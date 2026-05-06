//! Per-session FIFO dispatcher (ADR‑0024).
//!
//! Один `Arc<SessionDispatcher>` per session. Внутри:
//!
//! * `Arc<Semaphore>` capacity = 1 — admission slot;
//! * `AtomicU32 inflight` — счётчик inflight (для `session.list`);
//! * `AtomicU8 state` — Open/Drained.
//!
//! При вызове `enqueue`:
//!
//! 1. Ждём admission permit. На этом этапе допускаются:
//!    - `cancellation` → `CancelledWhileQueued`;
//!    - `deadline` истёк → `TimedOutWhileQueued`;
//!    - dispatcher переведён в Drained → `SessionGone`.
//! 2. С permit'ом: `inflight += 1`, шлём через `ConnectionHandle::dispatch_request`
//!    outbound `tool.call`. Ждём ответ до `deadline`. Если в течение ожидания
//!    приходит `cancellation` — шлём `tool.cancel(id)` и **продолжаем ждать
//!    terminal** (контракт §7.2 спеки и ADR‑0021/0024).
//! 3. После terminal — `inflight -= 1`, permit отпускается через `Drop`.

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;

use thiserror::Error;
use tokio::sync::Semaphore;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::session_manager::connection::{ConnectionCallError, ConnectionHandle};
use crate::session_manager::protocol::{
    error_codes, methods, JsonRpcError, ToolCallParams, ToolCallResult, ToolCancelParams,
};

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum DispatcherError {
    /// Cancel пришёл, пока вызов ещё в очереди — клиент его не получил.
    #[error("cancelled while queued")]
    CancelledWhileQueued,
    /// Cancel пришёл во время inflight — клиенту ушёл `tool.cancel`, но
    /// terminal всё равно дождались. Возвращается, если клиент сам ответил
    /// JSON‑RPC error c кодом `-32800 cancelled`. Если клиент успел ответить
    /// результатом до получения tool.cancel — вернётся `Ok(...)`.
    #[error("cancelled during execution")]
    Cancelled,
    /// Дедлайн истёк до получения admission slot'а.
    #[error("timed out while queued")]
    TimedOutWhileQueued,
    /// Дедлайн истёк после получения slot'а (вызов уже отправлен клиенту,
    /// но ответ не пришёл). Pending‑слот в ConnectionHandle вычищен.
    #[error("timed out while running")]
    TimedOutWhileRunning,
    /// Сессия в Disconnected/Gone (либо новая `enqueue` после `drain`, либо
    /// разрыв во время inflight).
    #[error("session_gone")]
    SessionGone,
    /// JSON‑RPC error от клиента (бизнес‑провал инструмента).
    #[error("client error: {0:?}")]
    ClientError(JsonRpcError),
    /// Ошибка десериализации `ToolCallResult` из ответа клиента.
    #[error("invalid tool.call result: {0}")]
    InvalidResult(String),
    /// Внутренняя ошибка — writer закрыт в момент отправки. Обрабатывается как
    /// SessionGone сверху, но различается в логе.
    #[error("writer closed")]
    WriterClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum DispatcherState {
    Open = 0,
    Drained = 1,
}

#[derive(Debug)]
#[allow(dead_code)]
pub struct SessionDispatcher {
    admission: Arc<Semaphore>,
    inflight: AtomicU32,
    state: AtomicU8,
}

#[allow(dead_code)]
impl SessionDispatcher {
    pub fn new() -> Self {
        Self {
            admission: Arc::new(Semaphore::new(1)),
            inflight: AtomicU32::new(0),
            state: AtomicU8::new(DispatcherState::Open as u8),
        }
    }

    pub fn is_drained(&self) -> bool {
        self.state.load(Ordering::Acquire) == DispatcherState::Drained as u8
    }

    pub fn inflight(&self) -> u32 {
        self.inflight.load(Ordering::Relaxed)
    }

    /// Переводит dispatcher в Drained: новые `enqueue` отвергаются `SessionGone`.
    /// Pending‑вызовы дренируются через `ConnectionHandle::drain_pending` —
    /// dispatcher на это полагается, сам ничего не отменяет.
    pub fn drain(&self) {
        self.state
            .store(DispatcherState::Drained as u8, Ordering::Release);
    }

    /// FIFO-вызов tool через эту сессию. См. §7 спеки и ADR‑0024.
    pub async fn enqueue(
        self: &Arc<Self>,
        connection: Arc<ConnectionHandle>,
        params: ToolCallParams,
        deadline: Instant,
        cancellation: CancellationToken,
    ) -> Result<ToolCallResult, DispatcherError> {
        if self.is_drained() {
            return Err(DispatcherError::SessionGone);
        }

        // 1. ждём admission permit с учётом cancellation/deadline/drain.
        let admission = Arc::clone(&self.admission);
        let permit = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(DispatcherError::CancelledWhileQueued),
            _ = tokio::time::sleep_until(deadline) => return Err(DispatcherError::TimedOutWhileQueued),
            permit = admission.acquire_owned() => permit,
        };
        let _permit = permit.map_err(|_| DispatcherError::SessionGone)?;

        // Между ожиданием и acquire состояние могло измениться.
        if self.is_drained() {
            return Err(DispatcherError::SessionGone);
        }

        self.inflight.fetch_add(1, Ordering::Relaxed);
        let _guard = InflightGuard { dispatcher: self };

        // 2. шлём outbound tool.call.
        let value = serde_json::to_value(&params).map_err(|e| {
            DispatcherError::InvalidResult(format!("serialize tool.call params: {e}"))
        })?;
        let (req_id, mut rx) = connection
            .dispatch_request(methods::TOOL_CALL, value)
            .map_err(map_call_err_send)?;

        // 3. ждём terminal с одновременным наблюдением cancellation/deadline.
        let mut cancel_sent = false;
        let outcome = loop {
            tokio::select! {
                biased;
                resp = &mut rx => match resp {
                    Ok(Ok(value)) => break Ok(value),
                    Ok(Err(jsonrpc_err)) => {
                        if jsonrpc_err.code == error_codes::TOOL_CANCELLED {
                            break Err(DispatcherError::Cancelled);
                        }
                        break Err(DispatcherError::ClientError(jsonrpc_err));
                    }
                    Err(_recv_closed) => break Err(DispatcherError::SessionGone),
                },
                _ = tokio::time::sleep_until(deadline) => {
                    connection.cancel_pending(&req_id);
                    break Err(DispatcherError::TimedOutWhileRunning);
                }
                _ = cancellation.cancelled(), if !cancel_sent => {
                    cancel_sent = true;
                    let cancel_params = ToolCancelParams {
                        id: req_id.clone(),
                        reason: Some("cancelled by manager".to_owned()),
                    };
                    let v = serde_json::to_value(&cancel_params).expect("cancel params");
                    // Шлём cancel "в пустоту" — нам ответ tool.cancel по сути не нужен.
                    if let Ok((_cancel_id, _cancel_rx)) =
                        connection.dispatch_request(methods::TOOL_CANCEL, v)
                    {
                        // не ждём ответа — продолжаем ждать tool.call terminal.
                    }
                    // продолжаем цикл — ждём rx или deadline.
                }
            }
        };

        match outcome {
            Ok(value) => {
                let parsed: ToolCallResult = serde_json::from_value(value).map_err(|e| {
                    DispatcherError::InvalidResult(format!("decode tool.call result: {e}"))
                })?;
                Ok(parsed)
            }
            Err(err) => Err(err),
        }
    }
}

impl Default for SessionDispatcher {
    fn default() -> Self {
        Self::new()
    }
}

struct InflightGuard<'a> {
    dispatcher: &'a SessionDispatcher,
}

impl<'a> Drop for InflightGuard<'a> {
    fn drop(&mut self) {
        self.dispatcher.inflight.fetch_sub(1, Ordering::Relaxed);
    }
}

fn map_call_err_send(err: ConnectionCallError) -> DispatcherError {
    match err {
        ConnectionCallError::Disconnected => DispatcherError::SessionGone,
        ConnectionCallError::WriterClosed => DispatcherError::WriterClosed,
        // dispatch_request не возвращает Timeout/Rejected (это терминальные).
        ConnectionCallError::Timeout => DispatcherError::TimedOutWhileRunning,
        ConnectionCallError::Rejected(e) => DispatcherError::ClientError(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_manager::protocol::{Id, ToolContent, WireMessage};
    use serde_json::json;
    use std::time::Duration;
    use tokio::sync::mpsc;

    fn handle_with_outbound() -> (Arc<ConnectionHandle>, mpsc::UnboundedReceiver<WireMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(ConnectionHandle::new(tx)), rx)
    }

    fn ok_call_params() -> ToolCallParams {
        ToolCallParams {
            name: "echo".to_owned(),
            arguments: json!({"x": 1}),
        }
    }

    #[tokio::test]
    async fn enqueue_round_trips_tool_call_via_connection() {
        let (conn, mut outbound_rx) = handle_with_outbound();
        let dispatcher = Arc::new(SessionDispatcher::new());

        let cancel = CancellationToken::new();
        let task = {
            let dispatcher = Arc::clone(&dispatcher);
            let conn = Arc::clone(&conn);
            tokio::spawn(async move {
                dispatcher
                    .enqueue(
                        conn,
                        ok_call_params(),
                        Instant::now() + Duration::from_secs(2),
                        cancel,
                    )
                    .await
            })
        };

        // Поймать outbound и ответить.
        let outbound = outbound_rx.recv().await.expect("outbound");
        let id = match outbound {
            WireMessage::Request { id, method, .. } => {
                assert_eq!(method, methods::TOOL_CALL);
                id
            }
            other => panic!("expected request, got {other:?}"),
        };
        let result_value = serde_json::to_value(ToolCallResult {
            content: vec![ToolContent::Text {
                text: "hi".to_owned(),
            }],
            is_error: false,
            structured_content: None,
        })
        .unwrap();
        conn.complete_response(id, Ok(result_value));

        let result = task.await.unwrap().unwrap();
        assert_eq!(result.content.len(), 1);
        assert_eq!(dispatcher.inflight(), 0);
    }

    #[tokio::test]
    async fn enqueue_sequentializes_two_calls() {
        let (conn, mut outbound_rx) = handle_with_outbound();
        let dispatcher = Arc::new(SessionDispatcher::new());

        let cancel = CancellationToken::new();
        let make = || {
            let dispatcher = Arc::clone(&dispatcher);
            let conn = Arc::clone(&conn);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                dispatcher
                    .enqueue(
                        conn,
                        ok_call_params(),
                        Instant::now() + Duration::from_secs(5),
                        cancel,
                    )
                    .await
            })
        };
        let t1 = make();
        let t2 = make();

        // Через 50 мс должна была пройти ровно одна заявка (admission=1).
        tokio::time::sleep(Duration::from_millis(50)).await;
        let first = outbound_rx.recv().await.expect("first outbound dispatched");
        assert!(
            outbound_rx.try_recv().is_err(),
            "second outbound must wait for first to terminate"
        );

        // Отвечаем на первый.
        let id1 = if let WireMessage::Request { id, .. } = first {
            id
        } else {
            panic!()
        };
        let r1 = serde_json::to_value(ToolCallResult::default()).unwrap();
        conn.complete_response(id1, Ok(r1.clone()));
        t1.await.unwrap().unwrap();

        // Теперь должна пойти вторая.
        let second = outbound_rx.recv().await.expect("second outbound");
        let id2 = if let WireMessage::Request { id, .. } = second {
            id
        } else {
            panic!()
        };
        conn.complete_response(id2, Ok(r1));
        t2.await.unwrap().unwrap();

        assert_eq!(dispatcher.inflight(), 0);
    }

    #[tokio::test]
    async fn cancel_before_admission_returns_cancelled_while_queued() {
        let (conn, _outbound_rx) = handle_with_outbound();
        let dispatcher = Arc::new(SessionDispatcher::new());

        // Занимаем admission "первой" задачей; она дойдёт до отправки и
        // упрётся в run-deadline 200мс — этого достаточно для теста.
        let blocker = {
            let d = Arc::clone(&dispatcher);
            let c = Arc::clone(&conn);
            tokio::spawn(async move {
                let _ = d
                    .enqueue(
                        c,
                        ok_call_params(),
                        Instant::now() + Duration::from_millis(500),
                        CancellationToken::new(),
                    )
                    .await;
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Вторая задача стоит в очереди → отменяем её ДО того, как blocker отпустит slot.
        let cancel = CancellationToken::new();
        let queued = {
            let d = Arc::clone(&dispatcher);
            let c = Arc::clone(&conn);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                d.enqueue(
                    c,
                    ok_call_params(),
                    Instant::now() + Duration::from_secs(60),
                    cancel,
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        cancel.cancel();

        let result = queued.await.unwrap();
        assert!(matches!(result, Err(DispatcherError::CancelledWhileQueued)));

        let _ = blocker.await;
    }

    #[tokio::test]
    async fn enqueue_after_drain_returns_session_gone() {
        let (conn, _rx) = handle_with_outbound();
        let dispatcher = Arc::new(SessionDispatcher::new());
        dispatcher.drain();

        let res = dispatcher
            .enqueue(
                conn,
                ok_call_params(),
                Instant::now() + Duration::from_secs(1),
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(res, Err(DispatcherError::SessionGone)));
    }

    #[tokio::test]
    async fn timeout_while_running_clears_pending() {
        let (conn, mut rx) = handle_with_outbound();
        let dispatcher = Arc::new(SessionDispatcher::new());

        let task = {
            let d = Arc::clone(&dispatcher);
            let c = Arc::clone(&conn);
            tokio::spawn(async move {
                d.enqueue(
                    c,
                    ok_call_params(),
                    Instant::now() + Duration::from_millis(80),
                    CancellationToken::new(),
                )
                .await
            })
        };
        // Клиент игнорирует запрос.
        let _ = rx.recv().await;
        let result = task.await.unwrap();
        assert!(matches!(result, Err(DispatcherError::TimedOutWhileRunning)));
        assert_eq!(conn.pending_len(), 0);
        assert_eq!(dispatcher.inflight(), 0);
    }

    #[tokio::test]
    async fn cancel_during_inflight_sends_tool_cancel_then_waits() {
        let (conn, mut rx) = handle_with_outbound();
        let dispatcher = Arc::new(SessionDispatcher::new());

        let cancel = CancellationToken::new();
        let task = {
            let d = Arc::clone(&dispatcher);
            let c = Arc::clone(&conn);
            let cancel = cancel.clone();
            tokio::spawn(async move {
                d.enqueue(
                    c,
                    ok_call_params(),
                    Instant::now() + Duration::from_secs(5),
                    cancel,
                )
                .await
            })
        };

        // Ловим outbound tool.call.
        let call = rx.recv().await.unwrap();
        let call_id = if let WireMessage::Request { id, method, .. } = call {
            assert_eq!(method, methods::TOOL_CALL);
            id
        } else {
            panic!()
        };

        // Cancel'им снаружи; dispatcher должен послать tool.cancel и
        // продолжить ждать ответ на исходный tool.call.
        cancel.cancel();
        let cancel_msg = rx.recv().await.unwrap();
        let cancel_id_in_payload = match cancel_msg {
            WireMessage::Request { method, params, .. } => {
                assert_eq!(method, methods::TOOL_CANCEL);
                let p: ToolCancelParams = serde_json::from_value(params).unwrap();
                p.id
            }
            _ => panic!("expected tool.cancel"),
        };
        assert_eq!(cancel_id_in_payload, call_id);

        // Клиент отвечает на tool.call с cancellation-кодом.
        conn.complete_response(
            call_id,
            Err(JsonRpcError::new(error_codes::TOOL_CANCELLED, "cancelled")),
        );

        let result = task.await.unwrap();
        assert!(matches!(result, Err(DispatcherError::Cancelled)));
        // Гасим побочный pending от tool.cancel (мы его не дождались).
        let _ = Id::String("ignored".to_owned());
    }
}

//! `ConnectionHandle` — outbound id‑correlation для bidirectional JSON‑RPC.
//!
//! Один `Arc<ConnectionHandle>` живёт на каждое WS‑соединение менеджера.
//! Хранит:
//!
//! * `outbound_tx` — mpsc в writer‑task транспорта (отправка `WireMessage`);
//! * `pending` — таблицу `Id → oneshot::Sender`, ожидающих ответ от клиента;
//! * `next_id` — монотонный генератор outbound‑id'ов в неймспейсе менеджера
//!   (`mgr-<u64>`) — гарантирует, что серверные id не пересекутся с клиентскими.
//!
//! Жизненный цикл:
//!
//! * на disconnect (WS drop, ошибка, `session.bye`) — `drain_pending` завершает
//!   все ожидающие вызовы ошибкой `Disconnected`;
//! * соответствует ADR‑0023.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;

use serde_json::Value;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::session_manager::protocol::{Id, JsonRpcError, WireMessage};

/// Ошибка outbound‑вызова через `ConnectionHandle::call`.
#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum ConnectionCallError {
    /// Клиент вернул JSON‑RPC error.
    #[error("json-rpc error: {0:?}")]
    Rejected(JsonRpcError),
    /// Дедлайн истёк до получения ответа.
    #[error("call timed out")]
    Timeout,
    /// Соединение разорвано / сессия в `Disconnected`.
    #[error("session_gone")]
    Disconnected,
    /// Канал writer'а закрыт (writer-task завершилась).
    #[error("writer channel closed")]
    WriterClosed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ConnectionState {
    Active = 0,
    Disconnected = 1,
}

/// Handle к одному WS‑соединению.
#[derive(Debug)]
#[allow(dead_code)]
pub struct ConnectionHandle {
    /// Sender в writer‑task. На `drain_pending` зануляется — это закрывает
    /// `mpsc::UnboundedReceiver` writer'а (когда все sender'ы дропнуты), и
    /// writer корректно завершается. Иначе writer ждал бы вечно.
    outbound_tx: Mutex<Option<mpsc::UnboundedSender<WireMessage>>>,
    pending: Mutex<HashMap<Id, oneshot::Sender<Result<Value, JsonRpcError>>>>,
    next_id: AtomicU64,
    state: AtomicU8,
}

#[allow(dead_code)]
impl ConnectionHandle {
    pub fn new(outbound_tx: mpsc::UnboundedSender<WireMessage>) -> Self {
        Self {
            outbound_tx: Mutex::new(Some(outbound_tx)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            state: AtomicU8::new(ConnectionState::Active as u8),
        }
    }

    fn state(&self) -> ConnectionState {
        match self.state.load(Ordering::Acquire) {
            0 => ConnectionState::Active,
            _ => ConnectionState::Disconnected,
        }
    }

    fn is_active(&self) -> bool {
        self.state() == ConnectionState::Active
    }

    fn fresh_id(&self) -> Id {
        let n = self.next_id.fetch_add(1, Ordering::Relaxed);
        Id::String(format!("mgr-{n}"))
    }

    /// Шлёт outbound JSON‑RPC `Request` без ожидания ответа.
    /// Возвращает выданный `Id` и `oneshot::Receiver` для будущего `Response`.
    /// Низкоуровневое API — позволяет потребителю (диспетчеру) узнать `Id`,
    /// чтобы потом послать `tool.cancel(id)` или вычистить pending вручную.
    pub fn dispatch_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(Id, oneshot::Receiver<Result<Value, JsonRpcError>>), ConnectionCallError> {
        if !self.is_active() {
            return Err(ConnectionCallError::Disconnected);
        }
        let id = self.fresh_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.pending.lock().expect("pending poisoned");
            guard.insert(id.clone(), tx);
        }

        let msg = WireMessage::Request {
            id: id.clone(),
            method: method.to_owned(),
            params,
        };
        let send_result = match self.outbound_tx.lock().expect("outbound poisoned").as_ref() {
            Some(tx) => tx.send(msg),
            None => {
                self.pending.lock().expect("pending poisoned").remove(&id);
                return Err(ConnectionCallError::WriterClosed);
            }
        };
        if send_result.is_err() {
            self.pending.lock().expect("pending poisoned").remove(&id);
            return Err(ConnectionCallError::WriterClosed);
        }
        Ok((id, rx))
    }

    /// Снимает pending‑слот (используется диспетчером, когда он сам прервал
    /// ожидание — например, локальный deadline истёк). Идемпотентно.
    pub fn cancel_pending(&self, id: &Id) {
        self.pending.lock().expect("pending poisoned").remove(id);
    }

    /// Шлёт outbound JSON‑RPC `Request`, ждёт `Response` от клиента до `deadline`.
    /// Корреляция по сгенерированному `Id` — `pending`-таблица.
    pub async fn call(
        &self,
        method: &str,
        params: Value,
        deadline: Instant,
    ) -> Result<Value, ConnectionCallError> {
        let (id, rx) = self.dispatch_request(method, params)?;
        match tokio::time::timeout_at(deadline, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(jsonrpc_err))) => Err(ConnectionCallError::Rejected(jsonrpc_err)),
            Ok(Err(_recv_closed)) => {
                // sender был дропнут (drain_pending) → расцениваем как disconnect.
                Err(ConnectionCallError::Disconnected)
            }
            Err(_elapsed) => {
                self.cancel_pending(&id);
                Err(ConnectionCallError::Timeout)
            }
        }
    }

    /// Доставка `Response` от клиента в нужный oneshot‑слот.
    /// Возвращает `true`, если id найден; `false` — нет (нарушение протокола, дроп).
    pub fn complete_response(&self, id: Id, result: Result<Value, JsonRpcError>) -> bool {
        let sender = {
            let mut guard = self.pending.lock().expect("pending poisoned");
            guard.remove(&id)
        };
        match sender {
            Some(tx) => tx.send(result).is_ok(),
            None => false,
        }
    }

    /// Завершает все ожидающие вызовы `Disconnected`, переводит handle в state
    /// `Disconnected`, обнуляет outbound‑sender (writer завершится при
    /// дропе последнего sender'а). Идемпотентен.
    pub fn drain_pending(&self) {
        self.state
            .store(ConnectionState::Disconnected as u8, Ordering::Release);
        let drained: Vec<oneshot::Sender<Result<Value, JsonRpcError>>> = {
            let mut guard = self.pending.lock().expect("pending poisoned");
            guard.drain().map(|(_, tx)| tx).collect()
        };
        drop(drained);
        // Закрываем outbound: writer завершится, когда все sender'ы дропнуты.
        let _ = self.outbound_tx.lock().expect("outbound poisoned").take();
    }

    /// Количество ожидающих ответов (для тестов/телеметрии).
    pub fn pending_len(&self) -> usize {
        self.pending.lock().expect("pending poisoned").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;

    fn make_handle() -> (Arc<ConnectionHandle>, mpsc::UnboundedReceiver<WireMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(ConnectionHandle::new(tx)), rx)
    }

    #[tokio::test]
    async fn call_completes_when_response_arrives() {
        let (h, mut rx) = make_handle();
        let h2 = Arc::clone(&h);
        let task = tokio::spawn(async move {
            h2.call(
                "tool.call",
                json!({"name": "echo", "arguments": {}}),
                Instant::now() + Duration::from_secs(2),
            )
            .await
        });

        let outbound = rx.recv().await.expect("outbound");
        let id = match &outbound {
            WireMessage::Request { id, method, .. } => {
                assert_eq!(method, "tool.call");
                id.clone()
            }
            _ => panic!("expected request"),
        };
        h.complete_response(id, Ok(json!({"content": "hi"})));

        let result = task.await.unwrap().unwrap();
        assert_eq!(result, json!({"content": "hi"}));
        assert_eq!(h.pending_len(), 0);
    }

    #[tokio::test]
    async fn call_returns_rejected_on_jsonrpc_error() {
        let (h, mut rx) = make_handle();
        let h2 = Arc::clone(&h);
        let task = tokio::spawn(async move {
            h2.call(
                "tool.call",
                json!({}),
                Instant::now() + Duration::from_secs(2),
            )
            .await
        });
        let outbound = rx.recv().await.unwrap();
        let id = if let WireMessage::Request { id, .. } = outbound {
            id
        } else {
            panic!()
        };
        h.complete_response(id, Err(JsonRpcError::new(-32011, "session_gone")));

        let err = task.await.unwrap().unwrap_err();
        assert!(matches!(err, ConnectionCallError::Rejected(ref e) if e.code == -32011));
    }

    #[tokio::test]
    async fn call_times_out_when_no_response() {
        let (h, _rx) = make_handle();
        let res = h
            .call(
                "ping",
                json!({}),
                Instant::now() + Duration::from_millis(50),
            )
            .await;
        assert!(matches!(res, Err(ConnectionCallError::Timeout)));
        // pending освобождён.
        assert_eq!(h.pending_len(), 0);
    }

    #[tokio::test]
    async fn drain_pending_disconnects_inflight_calls() {
        let (h, _rx) = make_handle();
        let h2 = Arc::clone(&h);
        let task = tokio::spawn(async move {
            h2.call(
                "tool.call",
                json!({}),
                Instant::now() + Duration::from_secs(5),
            )
            .await
        });
        // Дать call'у успеть положиться в pending.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(h.pending_len(), 1);

        h.drain_pending();
        let err = task.await.unwrap().unwrap_err();
        assert!(matches!(err, ConnectionCallError::Disconnected));
        assert_eq!(h.pending_len(), 0);
    }

    #[tokio::test]
    async fn call_after_drain_returns_disconnected_immediately() {
        let (h, _rx) = make_handle();
        h.drain_pending();
        let res = h
            .call("ping", json!({}), Instant::now() + Duration::from_secs(1))
            .await;
        assert!(matches!(res, Err(ConnectionCallError::Disconnected)));
    }

    #[tokio::test]
    async fn complete_response_with_unknown_id_returns_false() {
        let (h, _rx) = make_handle();
        let id = Id::String("does-not-exist".to_owned());
        assert!(!h.complete_response(id, Ok(json!({}))));
    }

    #[tokio::test]
    async fn outbound_ids_are_unique_and_namespaced() {
        let (h, mut rx) = make_handle();
        let h2 = Arc::clone(&h);
        let _t1 = tokio::spawn({
            let h = Arc::clone(&h2);
            async move {
                let _ = h
                    .call(
                        "ping",
                        json!({}),
                        Instant::now() + Duration::from_millis(50),
                    )
                    .await;
            }
        });
        let _t2 = tokio::spawn({
            let h = Arc::clone(&h2);
            async move {
                let _ = h
                    .call(
                        "ping",
                        json!({}),
                        Instant::now() + Duration::from_millis(50),
                    )
                    .await;
            }
        });

        let m1 = rx.recv().await.unwrap();
        let m2 = rx.recv().await.unwrap();
        let id1 = match m1 {
            WireMessage::Request { id, .. } => id,
            _ => panic!(),
        };
        let id2 = match m2 {
            WireMessage::Request { id, .. } => id,
            _ => panic!(),
        };
        assert_ne!(id1, id2);
        let s1 = match id1 {
            Id::String(s) => s,
            _ => panic!("expected mgr-* string id"),
        };
        let s2 = match id2 {
            Id::String(s) => s,
            _ => panic!("expected mgr-* string id"),
        };
        assert!(s1.starts_with("mgr-"));
        assert!(s2.starts_with("mgr-"));
    }
}

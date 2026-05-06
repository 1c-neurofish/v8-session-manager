//! Жизненный цикл сессии: graceful shutdown через WS + idle‑sweeper.
//!
//! После урезания менеджера до агрегатора менеджер больше НЕ запускает
//! 1С‑процессы. Lifecycle оставлен только в WS‑части:
//!
//! - graceful shutdown сессии через `session.shutdown` JSON‑RPC по WS;
//! - idle sweeper, проверяющий `last_call_at` Active‑сессий;
//! - удаление записи из реестра по факту дисконнекта/timeout.
//!
//! Spawn‑specific код (`track_spawn`, `SpawnHandle`, kill matrix, force‑kill
//! по PID) удалён вместе с `routing.rs`/`spawn.rs`/`backend.rs`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::time::Instant as TokioInstant;

use serde_json::json;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::session_manager::connection::ConnectionCallError;
use crate::session_manager::protocol::{methods, SessionShutdownParams};
use crate::session_manager::registry::{SessionRegistry, SessionState};

/// Результат `kill_session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillOutcome {
    /// Клиент ответил на `session.shutdown` и был удалён из реестра.
    Graceful,
    /// `session.shutdown` не был подтверждён за `grace_ms`; запись всё равно
    /// удалена из реестра.
    GraceTimeout,
    /// Сессия не найдена в реестре.
    NotFound,
}

#[allow(dead_code)]
pub struct LifecycleManager {
    registry: Arc<SessionRegistry>,
    graceful_kill_grace_ms: u64,
    cancel_token: CancellationToken,
}

#[allow(dead_code)]
impl LifecycleManager {
    pub fn new(registry: Arc<SessionRegistry>, graceful_kill_grace_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            registry,
            graceful_kill_grace_ms,
            cancel_token: CancellationToken::new(),
        })
    }

    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel_token
    }

    pub fn registry(&self) -> &Arc<SessionRegistry> {
        &self.registry
    }

    /// Завершить сессию через WS `session.shutdown`. Идемпотентно.
    ///
    /// Если клиент успел подтвердить shutdown — `Graceful`.
    /// Если grace истёк — запись всё равно удаляется (`GraceTimeout`):
    /// форсить процесс по PID менеджер больше не умеет.
    pub async fn shutdown_session(&self, session_id: &str) -> KillOutcome {
        let record = match self.registry.get(session_id) {
            Some(r) => r,
            None => return KillOutcome::NotFound,
        };
        // Снимаем generation того коннекта, который мы собираемся гасить.
        // На момент финального `remove_if_generation` запись могла измениться
        // из-за soft reconnect — тогда удалять её НЕЛЬЗЯ (см. WARN-4).
        let snapshot_generation = record.connection_generation;

        let mut outcome = KillOutcome::Graceful;

        if matches!(record.state, SessionState::Active) {
            if let Some(conn) = &record.connection {
                let params = SessionShutdownParams {
                    reason: Some("graceful_shutdown".to_owned()),
                    grace_ms: Some(self.graceful_kill_grace_ms),
                };
                let params_json = serde_json::to_value(&params).unwrap_or_else(|_| json!({}));
                let deadline =
                    TokioInstant::now() + Duration::from_millis(self.graceful_kill_grace_ms);
                match conn
                    .call(methods::SESSION_SHUTDOWN, params_json, deadline)
                    .await
                {
                    Ok(_) => {
                        debug!(
                            session_id = %session_id,
                            "client acknowledged session.shutdown"
                        );
                    }
                    Err(ConnectionCallError::Timeout) => {
                        warn!(
                            session_id = %session_id,
                            grace_ms = self.graceful_kill_grace_ms,
                            "session.shutdown timed out — recording GraceTimeout"
                        );
                        outcome = KillOutcome::GraceTimeout;
                    }
                    Err(err) => {
                        warn!(
                            session_id = %session_id,
                            error = ?err,
                            "session.shutdown failed — proceeding to drain"
                        );
                        outcome = KillOutcome::GraceTimeout;
                    }
                }
                conn.drain_pending();
            }
            record.dispatcher.drain();
        } else {
            // Disconnected: уговаривать некого, drain.
            record.dispatcher.drain();
            if let Some(conn) = &record.connection {
                conn.drain_pending();
            }
        }

        // Удаляем только если запись всё ещё та, которую мы драйнили.
        // Если за время WS round-trip успел произойти soft reconnect, запись
        // несёт уже более высокое поколение — оставляем её.
        let removed = self
            .registry
            .remove_if_generation(session_id, snapshot_generation);
        if removed.is_none() {
            debug!(
                session_id = %session_id,
                generation = snapshot_generation,
                "shutdown_session: record changed (soft reconnect?), skipping remove"
            );
        }

        info!(
            session_id = %session_id,
            outcome = ?outcome,
            "session shutdown via WS"
        );
        outcome
    }
}

/// Запустить idle‑sweeper. Возвращает `JoinHandle`, который можно
/// дождаться при graceful shutdown менеджера. Sweeper останавливается
/// при `lifecycle.cancel_token().cancel()`.
#[allow(dead_code)]
pub fn run_idle_sweeper(
    lifecycle: Arc<LifecycleManager>,
    idle_timeout: Duration,
    tick: Duration,
) -> JoinHandle<()> {
    let cancel = lifecycle.cancel_token().clone();
    tokio::spawn(async move {
        if idle_timeout.is_zero() {
            return; // отключено
        }
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    debug!("idle sweeper cancelled");
                    return;
                }
                _ = tokio::time::sleep(tick) => {
                    sweep_once(&lifecycle, idle_timeout).await;
                }
            }
        }
    })
}

#[allow(dead_code)]
async fn sweep_once(lifecycle: &Arc<LifecycleManager>, idle_timeout: Duration) {
    let now = Instant::now();
    // После удаления spawn-логики менеджер не различает ManagerSpawned vs
    // SelfRegistered — все сессии walk-in. Idle-sweep применяется ко всем
    // Active-записям без активных вызовов, у которых last_call_at старше
    // idle_timeout (WARN-3 «dead idle sweeper»).
    let candidates: Vec<String> = lifecycle
        .registry
        .snapshot()
        .into_iter()
        .filter(|rec| {
            matches!(rec.state, SessionState::Active)
                && rec.dispatcher.inflight() == 0
                && now.duration_since(rec.last_call_at) >= idle_timeout
        })
        .map(|rec| rec.session_id)
        .collect();
    for session_id in candidates {
        info!(session_id = %session_id, "idle timeout exceeded — graceful shutdown");
        let _ = lifecycle.shutdown_session(&session_id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_manager::connection::ConnectionHandle;
    use crate::session_manager::protocol::{SessionRegisterParams, WireMessage};
    use tokio::sync::mpsc;

    /// Помощник: регистрирует сессию через `register()`, возвращает
    /// (session_id, lifecycle, outbound_rx, conn_handle).
    async fn register_session(
        graceful_grace_ms: u64,
    ) -> (
        String,
        Arc<LifecycleManager>,
        mpsc::UnboundedReceiver<WireMessage>,
        Arc<ConnectionHandle>,
    ) {
        let registry = Arc::new(SessionRegistry::new());
        let lifecycle = LifecycleManager::new(Arc::clone(&registry), graceful_grace_ms);

        let (tx, rx) = mpsc::unbounded_channel::<WireMessage>();
        let conn = Arc::new(ConnectionHandle::new(tx));

        let uid = uuid::Uuid::new_v4().to_string();
        let p = SessionRegisterParams {
            client_uid: uid.clone(),
            kind: "mock".to_owned(),
            version: "0.1".to_owned(),
            tools: Vec::new(),
            host_id: None,
            pid: None,
            resources: None,
            prompts: None,
            extras: None,
        };
        registry
            .register(p, Instant::now(), Some(Arc::clone(&conn)))
            .unwrap();

        (uid, lifecycle, rx, conn)
    }

    #[tokio::test]
    async fn graceful_shutdown_sends_session_shutdown_and_removes_after_ack() {
        let (session_id, lifecycle, mut rx, conn) = register_session(2_000).await;

        let session_id_clone = session_id.clone();
        let lifecycle_clone = Arc::clone(&lifecycle);
        let conn_clone = Arc::clone(&conn);
        let task = tokio::spawn(async move { lifecycle_clone.shutdown_session(&session_id_clone).await });

        let outbound = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("outbound timeout")
            .expect("outbound channel closed");
        let (id, method) = match outbound {
            WireMessage::Request { id, method, .. } => (id, method),
            other => panic!("expected Request, got {other:?}"),
        };
        assert_eq!(method, methods::SESSION_SHUTDOWN);
        conn_clone.complete_response(id, Ok(json!({})));

        let outcome = task.await.unwrap();
        assert_eq!(outcome, KillOutcome::Graceful);
        assert!(lifecycle.registry.get(&session_id).is_none());
    }

    #[tokio::test]
    async fn graceful_shutdown_records_grace_timeout_when_unanswered() {
        let (session_id, lifecycle, mut rx, _conn) = register_session(150).await;

        let session_id_clone = session_id.clone();
        let lifecycle_clone = Arc::clone(&lifecycle);
        let task = tokio::spawn(async move { lifecycle_clone.shutdown_session(&session_id_clone).await });

        let _outbound = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("outbound timeout")
            .expect("outbound");

        let outcome = task.await.unwrap();
        assert_eq!(outcome, KillOutcome::GraceTimeout);
        assert!(lifecycle.registry.get(&session_id).is_none());
    }

    #[tokio::test]
    async fn idle_sweeper_does_not_reap_recently_called_sessions() {
        // BLOCKER-1: bump_last_call после enqueue должен сдвигать last_call_at,
        // и sweep_once с idle_timeout > (now - last_call_at) НЕ должен
        // удалять busy-сессию.
        let (session_id, lifecycle, _rx, _conn) = register_session(1_000).await;

        // Эмулируем proxy-call: bump_last_call в момент now.
        let bumped = lifecycle
            .registry
            .bump_last_call(&session_id, Instant::now());
        assert!(bumped);

        // idle_timeout = 1 час: запись свежая, реапить нечего.
        sweep_once(&lifecycle, Duration::from_secs(3600)).await;
        assert!(
            lifecycle.registry.get(&session_id).is_some(),
            "active session was incorrectly reaped by idle sweeper"
        );
    }

    #[tokio::test]
    async fn idle_sweeper_reaps_truly_idle_sessions() {
        // Контр-проверка: сессия без bump'а и с last_call_at в прошлом —
        // sweep_once должен её снести через graceful shutdown.
        let registry = Arc::new(SessionRegistry::new());
        let lifecycle = LifecycleManager::new(Arc::clone(&registry), 100);

        let (tx, mut rx) = mpsc::unbounded_channel::<WireMessage>();
        let conn = Arc::new(ConnectionHandle::new(tx));

        let uid = uuid::Uuid::new_v4().to_string();
        let p = SessionRegisterParams {
            client_uid: uid.clone(),
            kind: "mock".to_owned(),
            version: "0.1".to_owned(),
            tools: Vec::new(),
            host_id: None,
            pid: None,
            resources: None,
            prompts: None,
            extras: None,
        };
        // Регистрируем с last_call_at в прошлом.
        let past = Instant::now() - Duration::from_secs(3600);
        registry.register(p, past, Some(Arc::clone(&conn))).unwrap();

        // sweep_once запускает graceful shutdown — он шлёт session.shutdown по WS.
        // Запускаем sweep в фоне, отвечаем на shutdown — и проверяем удаление.
        let lifecycle_clone = Arc::clone(&lifecycle);
        let conn_clone = Arc::clone(&conn);
        let sweep_task = tokio::spawn(async move {
            sweep_once(&lifecycle_clone, Duration::from_secs(60)).await;
        });

        // Принимаем session.shutdown и подтверждаем его — иначе будет grace_timeout.
        let outbound = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("outbound timeout")
            .expect("outbound channel closed");
        if let WireMessage::Request { id, method, .. } = outbound {
            assert_eq!(method, methods::SESSION_SHUTDOWN);
            conn_clone.complete_response(id, Ok(json!({})));
        } else {
            panic!("expected SESSION_SHUTDOWN request");
        }

        sweep_task.await.unwrap();
        assert!(registry.get(&uid).is_none(), "idle session should be reaped");
    }

    #[tokio::test]
    async fn shutdown_unknown_session_is_not_found() {
        let registry = Arc::new(SessionRegistry::new());
        let lifecycle = LifecycleManager::new(Arc::clone(&registry), 100);
        let outcome = lifecycle.shutdown_session("nope").await;
        assert_eq!(outcome, KillOutcome::NotFound);
    }
}

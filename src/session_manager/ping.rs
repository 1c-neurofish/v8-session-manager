//! Application-level ping initiator.
//!
//! Менеджер периодически шлёт клиенту JSON-RPC `ping` через
//! [`ConnectionHandle::call`]. Это liveness-проверка прикладного слоя
//! (event-loop BSL): WS-уровневые ping/pong от tokio_tungstenite не
//! гарантируют, что клиентский обработчик в 1С не завис в long-running
//! BSL-операции.
//!
//! Контракт:
//!
//! * Если клиент ответил в пределах `timeout` — задача спит `interval`
//!   и повторяет цикл.
//! * Если ответ не пришёл за `timeout` — сессия помечается
//!   [`SessionState::Disconnected`] через `mark_disconnected_if_generation`.
//!   После grace timeout ([`run_grace_sweeper`](super::transport)) запись
//!   удаляется.
//! * Если соединение уже разорвано (`Disconnected` / `WriterClosed`) —
//!   задача мирно выходит.
//! * Generation-aware: если за время ожидания ответа произошёл
//!   soft-reconnect (`connection_generation` сменился), `mark_disconnected`
//!   на старом поколении — no-op, новая инкарнация продолжает жить.
//!
//! Spawn привязан к конкретному `ConnectionHandle`: на soft-reconnect
//! предыдущая ping-task сама завершится по `Disconnected` (handle уже
//! drained), а транспорт спаунит новую task на свежем handle.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::session_manager::connection::{ConnectionCallError, ConnectionHandle};
use crate::session_manager::registry::SessionRegistry;

/// Конфиг ping-задачи. `interval == 0` — пинг отключён.
#[derive(Debug, Clone, Copy)]
pub struct AppPingConfig {
    pub interval: Duration,
    pub timeout: Duration,
}

impl AppPingConfig {
    pub fn from_ms(interval_ms: u64, timeout_ms: u64) -> Self {
        Self {
            interval: Duration::from_millis(interval_ms),
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    pub fn is_disabled(&self) -> bool {
        self.interval.is_zero()
    }
}

/// Запускает фоновую ping-задачу.
///
/// Возвращает `None`, если `cfg.interval == 0` (пинг выключен).
/// Иначе — `Some(JoinHandle)`.
pub fn spawn_app_ping_task(
    connection: Arc<ConnectionHandle>,
    registry: Arc<SessionRegistry>,
    session_id: String,
    generation: u64,
    cfg: AppPingConfig,
) -> Option<JoinHandle<()>> {
    if cfg.is_disabled() {
        debug!(session_id = %session_id, "app ping disabled (interval == 0)");
        return None;
    }
    Some(tokio::spawn(run_app_ping_loop(
        connection,
        registry,
        session_id,
        generation,
        cfg,
    )))
}

async fn run_app_ping_loop(
    connection: Arc<ConnectionHandle>,
    registry: Arc<SessionRegistry>,
    session_id: String,
    generation: u64,
    cfg: AppPingConfig,
) {
    debug!(
        session_id = %session_id,
        generation,
        interval_ms = cfg.interval.as_millis() as u64,
        timeout_ms = cfg.timeout.as_millis() as u64,
        "app ping loop started",
    );
    loop {
        tokio::time::sleep(cfg.interval).await;
        let deadline = tokio::time::Instant::now() + cfg.timeout;
        match connection
            .call("ping", serde_json::json!({}), deadline)
            .await
        {
            Ok(_) => continue,
            Err(ConnectionCallError::Rejected(err)) => {
                // BSL handler не должен возвращать ошибку, но если вернул —
                // факт ответа уже доказывает что peer жив. Логируем и
                // продолжаем.
                warn!(
                    session_id = %session_id,
                    code = err.code,
                    msg = %err.message,
                    "app ping rejected by client; treating as alive",
                );
                continue;
            }
            Err(err @ ConnectionCallError::Timeout)
            | Err(err @ ConnectionCallError::WriterClosed) => {
                // Timeout — peer не ответил вовремя; WriterClosed — writer
                // task умерла раньше, чем reader заметил разрыв. В обоих
                // случаях peer фактически мёртв, и reader-loop может
                // тянуться (полузакрытый сокет, drained writer без EOF).
                // Чтобы не оставлять запись в Active навсегда, помечаем
                // сессию Disconnected сами; при race с soft reconnect
                // generation-aware вариант — no-op.
                info!(
                    session_id = %session_id,
                    generation,
                    timeout_ms = cfg.timeout.as_millis() as u64,
                    error = %err,
                    "app ping unhealthy — marking session disconnected",
                );
                let did_mark = registry.mark_disconnected_if_generation(
                    &session_id,
                    generation,
                    Instant::now(),
                );
                if !did_mark {
                    debug!(
                        session_id = %session_id,
                        generation,
                        "ping unhealthy but mark_disconnected was no-op (soft reconnect or removed)",
                    );
                }
                connection.drain_pending();
                return;
            }
            Err(ConnectionCallError::Disconnected) => {
                // ConnectionHandle уже в Disconnected — кто-то (mark_disconnected
                // от reader/transport) уже обработал разрыв. Просто выходим.
                debug!(
                    session_id = %session_id,
                    generation,
                    "app ping loop exiting: connection already gone",
                );
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_manager::protocol::WireMessage;
    use crate::session_manager::registry::SessionRecord;
    use serde_json::json;
    use std::time::Instant;
    use tokio::sync::mpsc;

    fn make_handle() -> (Arc<ConnectionHandle>, mpsc::UnboundedReceiver<WireMessage>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Arc::new(ConnectionHandle::new(tx)), rx)
    }

    fn register_session(
        registry: &Arc<SessionRegistry>,
        session_id: &str,
        connection: Arc<ConnectionHandle>,
    ) -> u64 {
        use crate::session_manager::protocol::SessionRegisterParams;
        registry
            .register(
                SessionRegisterParams {
                    client_uid: session_id.to_owned(),
                    kind: "client".to_owned(),
                    version: "0.0.0".to_owned(),
                    tools: vec![],
                    resources: None,
                    prompts: None,
                    extras: None,
                    host_id: None,
                    pid: None,
                },
                Instant::now(),
                Some(connection),
            )
            .expect("register");
        registry
            .get(session_id)
            .expect("record present")
            .connection_generation
    }

    #[tokio::test(start_paused = true)]
    async fn ping_marks_disconnected_after_timeout_with_no_response() {
        let (handle, mut rx) = make_handle();
        let registry = Arc::new(SessionRegistry::new());
        let generation = register_session(&registry, "sess-1", Arc::clone(&handle));

        let join = spawn_app_ping_task(
            Arc::clone(&handle),
            Arc::clone(&registry),
            "sess-1".to_owned(),
            generation,
            AppPingConfig::from_ms(50, 30),
        )
        .expect("task spawned");

        // Дать первой итерации цикла отправить ping и ждать ответ.
        // (interval=50, timeout=30, итого 80 мс до mark_disconnected)
        tokio::time::sleep(Duration::from_millis(120)).await;

        // Подтверждаем, что ping-фрейм действительно ушёл наружу.
        let msg = rx.try_recv().expect("ping frame should be sent");
        assert!(matches!(msg, WireMessage::Request { ref method, .. } if method == "ping"));

        join.await.expect("ping task finished");

        // Сессия должна быть помечена Disconnected.
        let rec: SessionRecord = registry.get("sess-1").expect("still in registry");
        assert_eq!(
            rec.state,
            crate::session_manager::registry::SessionState::Disconnected
        );
    }

    #[tokio::test(start_paused = true)]
    async fn ping_keeps_session_active_when_client_responds() {
        let (handle, mut rx) = make_handle();
        let registry = Arc::new(SessionRegistry::new());
        let generation = register_session(&registry, "sess-2", Arc::clone(&handle));

        let join = spawn_app_ping_task(
            Arc::clone(&handle),
            Arc::clone(&registry),
            "sess-2".to_owned(),
            generation,
            AppPingConfig::from_ms(40, 200),
        )
        .expect("task spawned");

        // Прокидываем 5 итераций отвечая каждый раз.
        for _ in 0..5 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let frame = rx.try_recv().expect("ping frame");
            let id = match frame {
                WireMessage::Request { id, method, .. } => {
                    assert_eq!(method, "ping");
                    id
                }
                other => panic!("expected ping request, got {other:?}"),
            };
            assert!(handle.complete_response(id, Ok(json!({}))));
        }

        // Сессия должна оставаться Active.
        let rec = registry.get("sess-2").expect("still in registry");
        assert_eq!(
            rec.state,
            crate::session_manager::registry::SessionState::Active
        );

        // Останавливаем ping-loop корректно — drain снимает task.
        handle.drain_pending();
        join.await.expect("ping task exits after drain");
    }

    #[tokio::test(start_paused = true)]
    async fn ping_exits_quietly_after_drain_pending() {
        let (handle, _rx) = make_handle();
        let registry = Arc::new(SessionRegistry::new());
        let generation = register_session(&registry, "sess-3", Arc::clone(&handle));

        let join = spawn_app_ping_task(
            Arc::clone(&handle),
            Arc::clone(&registry),
            "sess-3".to_owned(),
            generation,
            AppPingConfig::from_ms(50, 50),
        )
        .expect("task spawned");

        handle.drain_pending();
        // Подождать первый sleep + первый call.
        tokio::time::sleep(Duration::from_millis(80)).await;

        join.await.expect("ping task exits without panicking");
        // Сессию ping не трогает — до Disconnected её довёл бы лишь timeout,
        // а call вернул сразу Disconnected (drain_pending уже снял sender).
    }

    #[tokio::test(start_paused = true)]
    async fn ping_does_not_clobber_after_soft_reconnect() {
        // Симулируем: старая generation ловит timeout, но новая Active-запись
        // уже зарегистрирована — mark_disconnected_if_generation должен быть
        // no-op, новая запись остаётся Active.
        let (handle_old, _rx_old) = make_handle();
        let (handle_new, _rx_new) = make_handle();
        let registry = Arc::new(SessionRegistry::new());
        let old_gen = register_session(&registry, "sess-4", Arc::clone(&handle_old));

        // soft reconnect: drain old + повторный register.
        registry.mark_disconnected("sess-4", Instant::now());
        let new_gen = register_session(&registry, "sess-4", Arc::clone(&handle_new));
        assert!(new_gen > old_gen);

        // Старая ping-task на old_gen стартует уже после reconnect.
        let join = spawn_app_ping_task(
            Arc::clone(&handle_old),
            Arc::clone(&registry),
            "sess-4".to_owned(),
            old_gen,
            AppPingConfig::from_ms(40, 30),
        )
        .expect("task spawned");

        tokio::time::sleep(Duration::from_millis(100)).await;
        join.await.expect("old ping task finished");

        let rec = registry.get("sess-4").expect("still registered");
        // Новая инкарнация должна остаться Active.
        assert_eq!(
            rec.state,
            crate::session_manager::registry::SessionState::Active
        );
        assert_eq!(rec.connection_generation, new_gen);
    }

    #[test]
    fn ping_disabled_when_interval_zero() {
        let (handle, _rx) = make_handle();
        let registry = Arc::new(SessionRegistry::new());
        let task = spawn_app_ping_task(
            handle,
            registry,
            "sess-zero".to_owned(),
            1,
            AppPingConfig::from_ms(0, 1000),
        );
        assert!(task.is_none());
    }

}

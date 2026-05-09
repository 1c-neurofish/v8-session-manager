//! In-memory реестр сессий менеджера.
//!
//! После урезания (#5/post-extraction) менеджер не запускает 1С‑процессы и
//! состояние `Spawning`/`SessionOrigin::ManagerSpawned` упразднено. Все
//! записи реестра — walk‑in (`SelfRegistered`), создаются только в ответ
//! на входящий WS `session.register`.
//!
//! Состояния:
//!  - `Active` — WS живой, клиент зарегистрирован.
//!  - `Disconnected` — WS разорван, ждём soft reconnect в течение
//!    `reconnection_grace_secs`.
//!  - `Gone` отдельно не материализуется — записи, дошедшие до
//!    терминала (grace timeout / `session.bye` / hard remove), удаляются
//!    из `HashMap`.
//!
//! `session_id` ≡ `client_uid`. Коллизия uid в состоянии `Active` приводит
//! к ошибке `-32010 session_uid_collision`; коллизия uid в состоянии
//! `Disconnected` интерпретируется как soft reconnect.
//!
//! Все операции потокобезопасны: внутри [`SessionRegistry`] живёт
//! `RwLock<HashMap<…>>`. Транспорт держит `Arc<…>` и вызывает методы
//! напрямую.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use crate::session_manager::connection::ConnectionHandle;
use crate::session_manager::dispatcher::SessionDispatcher;
use crate::session_manager::protocol::{SessionRegisterParams, ToolDescriptor};

/// Состояние записи сессии.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// WS живой, клиент зарегистрирован.
    Active,
    /// WS разорван, ждём soft reconnect в течение `reconnection_grace_secs`.
    Disconnected,
}

/// Запись сессии в реестре.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SessionRecord {
    pub session_id: String,
    pub kind: String,
    pub version: String,
    /// Имя информационной базы 1С (naming v2 от 2026-05-09). Берётся из
    /// `session.register.infobase_name`; обновляется при soft reconnect.
    pub infobase_name: String,
    /// Внутренний номер сеанса 1С (`НомерСеанса()` платформы).
    /// Обновляется при soft reconnect — фактический номер в новом сеансе
    /// 1С может отличаться от номера предыдущего сеанса.
    pub ib_session_number: u32,
    pub tools: Vec<ToolDescriptor>,
    pub state: SessionState,
    /// Идентификатор хоста, на котором работает процесс. Берётся из поля
    /// `host_id` в `session.register` (fallback `"unknown"`).
    pub host_id: String,
    /// PID процесса (из поля `pid` в `session.register`, `None` если не передан).
    pub pid: Option<u32>,
    /// Время регистрации (первичной, не reconnect).
    pub registered_at: Instant,
    /// Момент последнего проксированного `tool.call`.
    pub last_call_at: Instant,
    /// Момент перехода в `Disconnected`. `None` для `Active`.
    pub disconnected_at: Option<Instant>,
    /// Поколение коннекта: инкрементируется при каждой регистрации/реконнекте.
    /// Используется в `lifecycle::shutdown_session` для cmpxchg-подобного
    /// удаления — чтобы не удалить запись soft‑reconnected клиента, пока мы
    /// драйнили старый коннект.
    pub connection_generation: u64,
    /// Handle к WS‑соединению. `Some` для `Active`, `None` для `Disconnected`.
    /// На soft reconnect перезаписывается новым handle; pending старого handle
    /// уже задрейнен в `drain_pending`.
    pub connection: Option<Arc<ConnectionHandle>>,
    /// Per‑session FIFO dispatcher (ADR‑0024). Источник правды по inflight —
    /// `dispatcher.inflight()`; в `SessionRecord` отдельного счётчика нет.
    pub dispatcher: Arc<SessionDispatcher>,
}

/// Результат `register`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    /// Новая сессия.
    Created,
    /// Soft reconnect к существующей `Disconnected`-записи.
    Reconnected,
}

/// Ошибка `register`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegisterError {
    /// `client_uid` уже есть в реестре в состоянии `Active`.
    #[error("session_uid_collision: client_uid '{0}' is already active")]
    UidCollision(String),
}

/// In-memory реестр сессий.
#[derive(Debug, Default)]
pub struct SessionRegistry {
    inner: RwLock<HashMap<String, SessionRecord>>,
    /// Сигнал для notifier'а MCP `tools/list_changed` (ADR-0026).
    /// Инкрементируется при register/mark_disconnected/remove/update_tools.
    tools_epoch: AtomicU64,
    tools_changed: Notify,
    /// Сигнал «состав реестра изменился» (register/remove). Notifier MCP
    /// `tools/list_changed` сюда не подписывается.
    session_changed: Notify,
    /// Глобальный монотонный счётчик для `connection_generation`.
    next_generation: AtomicU64,
}

#[allow(dead_code)]
impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Текущая эпоха публичного набора tools (увеличивается при изменении).
    /// Используется notifier'ом для coalescing и тестами.
    pub fn tools_epoch(&self) -> u64 {
        self.tools_epoch.load(Ordering::Acquire)
    }

    /// Сигнал-канал для notifier'а. Notifier ждёт `tools_changed_signal().notified()`.
    pub fn tools_changed_signal(&self) -> &Notify {
        &self.tools_changed
    }

    fn mark_tools_changed(&self) {
        self.tools_epoch.fetch_add(1, Ordering::Release);
        self.tools_changed.notify_one();
    }

    /// Сигнал «состав реестра изменился».
    pub fn session_changed_signal(&self) -> &Notify {
        &self.session_changed
    }

    fn mark_session_changed(&self) {
        self.session_changed.notify_waiters();
    }

    fn next_generation(&self) -> u64 {
        // Начинаем нумерацию с 1, чтобы 0 оставался «никогда не было коннекта».
        self.next_generation.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Зарегистрировать сессию или soft‑reconnect к существующей `Disconnected`.
    ///
    /// * `Created` — новая запись, состояние `Active`.
    /// * `Reconnected` — найдена `Disconnected`-запись с тем же `client_uid`,
    ///   `tools` и `version` обновлены, `state → Active`, `disconnected_at → None`,
    ///   `connection_generation` увеличен.
    /// * `Err(UidCollision)` — `client_uid` занят живой `Active`-записью.
    pub fn register(
        &self,
        params: SessionRegisterParams,
        now: Instant,
        connection: Option<Arc<ConnectionHandle>>,
    ) -> Result<RegisterOutcome, RegisterError> {
        let mut guard = self.inner.write().expect("registry poisoned");
        if let Some(existing) = guard.get_mut(&params.client_uid) {
            return match existing.state {
                SessionState::Active => Err(RegisterError::UidCollision(params.client_uid.clone())),
                SessionState::Disconnected => {
                    let new_gen = self.next_generation();
                    existing.state = SessionState::Active;
                    existing.disconnected_at = None;
                    existing.tools = params.tools;
                    existing.version = params.version;
                    existing.kind = params.kind;
                    existing.infobase_name = params.infobase_name;
                    existing.ib_session_number = params.ib_session_number;
                    existing.connection = connection;
                    existing.connection_generation = new_gen;
                    if let Some(hid) = params.host_id {
                        existing.host_id = hid;
                    }
                    if params.pid.is_some() {
                        existing.pid = params.pid;
                    }
                    // Старый dispatcher был drained — пересоздаём свежий.
                    existing.dispatcher = Arc::new(SessionDispatcher::new());
                    drop(guard);
                    self.mark_tools_changed();
                    self.mark_session_changed();
                    Ok(RegisterOutcome::Reconnected)
                }
            };
        }

        // Новая walk-in запись.
        let generation = self.next_generation();
        let record = SessionRecord {
            session_id: params.client_uid.clone(),
            kind: params.kind,
            version: params.version,
            infobase_name: params.infobase_name,
            ib_session_number: params.ib_session_number,
            tools: params.tools,
            state: SessionState::Active,
            host_id: params.host_id.unwrap_or_else(|| "unknown".to_owned()),
            pid: params.pid,
            registered_at: now,
            last_call_at: now,
            disconnected_at: None,
            connection_generation: generation,
            connection,
            dispatcher: Arc::new(SessionDispatcher::new()),
        };
        guard.insert(params.client_uid, record);
        drop(guard);
        self.mark_tools_changed();
        self.mark_session_changed();
        Ok(RegisterOutcome::Created)
    }

    /// Перевести запись в `Disconnected`. No‑op, если запись отсутствует или уже `Disconnected`.
    pub fn mark_disconnected(&self, session_id: &str, now: Instant) -> bool {
        let changed = {
            let mut guard = self.inner.write().expect("registry poisoned");
            match guard.get_mut(session_id) {
                Some(rec) if rec.state == SessionState::Active => {
                    rec.state = SessionState::Disconnected;
                    rec.disconnected_at = Some(now);
                    if let Some(conn) = rec.connection.take() {
                        conn.drain_pending();
                    }
                    rec.dispatcher.drain();
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.mark_tools_changed();
            self.mark_session_changed();
        }
        changed
    }

    /// Сдвинуть `last_call_at` сессии на `now`. Используется при enqueue
    /// прокси‑вызова, чтобы idle sweeper не реапил busy сессии (BLOCKER‑1).
    /// Возвращает `true`, если запись существует.
    pub fn bump_last_call(&self, session_id: &str, now: Instant) -> bool {
        let mut guard = self.inner.write().expect("registry poisoned");
        match guard.get_mut(session_id) {
            Some(rec) => {
                rec.last_call_at = now;
                true
            }
            None => false,
        }
    }

    /// Атомарный `mark_disconnected` с проверкой `connection_generation`.
    /// Помечает запись Disconnected только если её `connection_generation`
    /// совпадает с `expected_generation`. Защищает от ситуации, когда
    /// «зомби» WS‑task старого коннекта закрывается уже после soft reconnect:
    /// на старом поколении дисконнект — no-op, свежая Active‑запись остаётся.
    /// Возвращает `true`, если переход состоялся.
    pub fn mark_disconnected_if_generation(
        &self,
        session_id: &str,
        expected_generation: u64,
        now: Instant,
    ) -> bool {
        let changed = {
            let mut guard = self.inner.write().expect("registry poisoned");
            match guard.get_mut(session_id) {
                Some(rec)
                    if rec.state == SessionState::Active
                        && rec.connection_generation == expected_generation =>
                {
                    rec.state = SessionState::Disconnected;
                    rec.disconnected_at = Some(now);
                    if let Some(conn) = rec.connection.take() {
                        conn.drain_pending();
                    }
                    rec.dispatcher.drain();
                    true
                }
                _ => false,
            }
        };
        if changed {
            self.mark_tools_changed();
            self.mark_session_changed();
        }
        changed
    }

    /// Жёстко удалить запись (после `session.bye`, kill, grace timeout).
    pub fn remove(&self, session_id: &str) -> Option<SessionRecord> {
        let removed = self
            .inner
            .write()
            .expect("registry poisoned")
            .remove(session_id);
        if removed.is_some() {
            self.mark_tools_changed();
            self.mark_session_changed();
        }
        removed
    }

    /// Атомарное удаление с проверкой `connection_generation`. Удаляет запись
    /// только если её `connection_generation` совпадает с `expected_generation`.
    /// Используется `lifecycle::shutdown_session`, чтобы не удалить запись
    /// soft‑reconnected клиента (см. WARN-4 «shutdown race»).
    pub fn remove_if_generation(
        &self,
        session_id: &str,
        expected_generation: u64,
    ) -> Option<SessionRecord> {
        let removed = {
            let mut guard = self.inner.write().expect("registry poisoned");
            match guard.get(session_id) {
                Some(rec) if rec.connection_generation == expected_generation => {
                    guard.remove(session_id)
                }
                _ => None,
            }
        };
        if removed.is_some() {
            self.mark_tools_changed();
            self.mark_session_changed();
        }
        removed
    }

    /// Обновить набор tools (для `session.tools_changed`).
    ///
    /// Diff-gate: если новый набор семантически эквивалентен текущему,
    /// `mark_tools_changed` не вызывается — это защищает AI-клиентов от
    /// ложных `notifications/tools/list_changed` при пере-открытии форм
    /// (например, Vanessa MCPVA → закрытие → повторное открытие).
    /// Возвращает `true`, если сессия найдена (даже когда tools идентичны).
    pub fn update_tools(&self, session_id: &str, tools: Vec<ToolDescriptor>) -> bool {
        let outcome = {
            let mut guard = self.inner.write().expect("registry poisoned");
            match guard.get_mut(session_id) {
                Some(rec) => {
                    let same = rec.tools == tools;
                    if !same {
                        rec.tools = tools;
                    }
                    Some(same)
                }
                None => None,
            }
        };
        match outcome {
            Some(false) => {
                self.mark_tools_changed();
                true
            }
            Some(true) => true,
            None => false,
        }
    }

    /// Удалить `Disconnected`-сессии, у которых истёк `reconnection_grace`.
    /// Возвращает их `session_id` (для логирования / уведомлений).
    pub fn sweep_expired_disconnects(&self, now: Instant, grace: Duration) -> Vec<String> {
        let expired = {
            let mut guard = self.inner.write().expect("registry poisoned");
            let expired: Vec<String> = guard
                .iter()
                .filter_map(|(id, rec)| match (rec.state, rec.disconnected_at) {
                    (SessionState::Disconnected, Some(t)) if now.duration_since(t) >= grace => {
                        Some(id.clone())
                    }
                    _ => None,
                })
                .collect();
            for id in &expired {
                guard.remove(id);
            }
            expired
        };
        if !expired.is_empty() {
            self.mark_tools_changed();
        }
        expired
    }

    /// Снимок реестра (для `session.list` и тестов).
    pub fn snapshot(&self) -> Vec<SessionRecord> {
        self.inner
            .read()
            .expect("registry poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Получить запись по `session_id` (клон).
    pub fn get(&self, session_id: &str) -> Option<SessionRecord> {
        self.inner
            .read()
            .expect("registry poisoned")
            .get(session_id)
            .cloned()
    }

    pub fn len(&self) -> usize {
        self.inner.read().expect("registry poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn params(uid: &str, kind: &str, tool_name: &str) -> SessionRegisterParams {
        SessionRegisterParams {
            client_uid: uid.to_owned(),
            kind: kind.to_owned(),
            version: "1.0.0".to_owned(),
            infobase_name: "test_db".to_owned(),
            ib_session_number: 1,
            tools: vec![ToolDescriptor {
                name: tool_name.to_owned(),
                description: None,
                input_schema: json!({ "type": "object" }),
            }],
            host_id: None,
            pid: None,
            resources: None,
            prompts: None,
            extras: None,
        }
    }

    #[test]
    fn register_creates_new_active_session() {
        let reg = SessionRegistry::new();
        let now = Instant::now();
        let outcome = reg
            .register(params("uid-1", "client", "echo"), now, None)
            .unwrap();
        assert_eq!(outcome, RegisterOutcome::Created);
        let rec = reg.get("uid-1").expect("record");
        assert_eq!(rec.session_id, "uid-1");
        assert_eq!(rec.state, SessionState::Active);
        assert_eq!(rec.tools.len(), 1);
        assert!(rec.disconnected_at.is_none());
        assert_eq!(rec.connection_generation, 1);
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn register_with_active_uid_returns_collision() {
        let reg = SessionRegistry::new();
        let now = Instant::now();
        reg.register(params("uid-1", "client", "echo"), now, None)
            .unwrap();
        let err = reg
            .register(params("uid-1", "client", "echo2"), now, None)
            .unwrap_err();
        assert_eq!(err, RegisterError::UidCollision("uid-1".to_owned()));
        // Реестр не должен мутировать после ошибки.
        let rec = reg.get("uid-1").unwrap();
        assert_eq!(rec.tools[0].name, "echo");
    }

    #[test]
    fn mark_disconnected_transitions_to_disconnected() {
        let reg = SessionRegistry::new();
        let t0 = Instant::now();
        reg.register(params("uid-1", "client", "echo"), t0, None)
            .unwrap();
        let t1 = t0 + Duration::from_secs(1);
        assert!(reg.mark_disconnected("uid-1", t1));
        let rec = reg.get("uid-1").unwrap();
        assert_eq!(rec.state, SessionState::Disconnected);
        assert_eq!(rec.disconnected_at, Some(t1));

        // Повторный mark_disconnected — no-op.
        let t2 = t1 + Duration::from_secs(1);
        assert!(!reg.mark_disconnected("uid-1", t2));
    }

    #[test]
    fn register_with_disconnected_uid_performs_soft_reconnect() {
        let reg = SessionRegistry::new();
        let t0 = Instant::now();
        reg.register(params("uid-1", "client", "echo"), t0, None)
            .unwrap();
        let gen_before = reg.get("uid-1").unwrap().connection_generation;
        reg.mark_disconnected("uid-1", t0 + Duration::from_secs(1));

        let mut p2 = params("uid-1", "client", "echo_v2");
        p2.version = "1.1.0".to_owned();
        let outcome = reg.register(p2, t0 + Duration::from_secs(2), None).unwrap();
        assert_eq!(outcome, RegisterOutcome::Reconnected);

        let rec = reg.get("uid-1").unwrap();
        assert_eq!(rec.state, SessionState::Active);
        assert!(rec.disconnected_at.is_none());
        assert_eq!(rec.tools[0].name, "echo_v2");
        assert_eq!(rec.version, "1.1.0");
        assert!(rec.connection_generation > gen_before);
        // session_id не меняется при reconnect.
        assert_eq!(rec.session_id, "uid-1");
        // Реестр всё ещё содержит ровно одну запись.
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn reconnect_updates_infobase_and_ib_session_number() {
        // Naming v2: при soft reconnect значения из нового register'а
        // перетирают старые — у нового сеанса 1С может быть другой
        // ib_session_number.
        let reg = SessionRegistry::new();
        let t0 = Instant::now();
        let mut first = params("uid-1", "client", "echo");
        first.infobase_name = "db_a".to_owned();
        first.ib_session_number = 7;
        reg.register(first, t0, None).unwrap();

        reg.mark_disconnected("uid-1", t0 + Duration::from_secs(1));

        let mut second = params("uid-1", "client", "echo");
        second.infobase_name = "db_a".to_owned();
        second.ib_session_number = 99;
        reg.register(second, t0 + Duration::from_secs(2), None)
            .unwrap();

        let rec = reg.get("uid-1").unwrap();
        assert_eq!(rec.infobase_name, "db_a");
        assert_eq!(rec.ib_session_number, 99);
    }

    #[test]
    fn sweep_removes_only_expired_disconnects() {
        let reg = SessionRegistry::new();
        let t0 = Instant::now();
        reg.register(params("uid-old", "client", "a"), t0, None)
            .unwrap();
        reg.register(params("uid-fresh", "client", "b"), t0, None)
            .unwrap();
        reg.register(params("uid-active", "client", "c"), t0, None)
            .unwrap();

        reg.mark_disconnected("uid-old", t0);
        reg.mark_disconnected("uid-fresh", t0 + Duration::from_secs(25));

        let grace = Duration::from_secs(30);
        let expired = reg.sweep_expired_disconnects(t0 + Duration::from_secs(31), grace);

        assert_eq!(expired, vec!["uid-old".to_owned()]);
        assert!(reg.get("uid-old").is_none());
        assert!(reg.get("uid-fresh").is_some());
        assert!(reg.get("uid-active").is_some());
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn remove_drops_record() {
        let reg = SessionRegistry::new();
        let t0 = Instant::now();
        reg.register(params("uid-1", "client", "echo"), t0, None)
            .unwrap();
        let removed = reg.remove("uid-1").expect("record");
        assert_eq!(removed.session_id, "uid-1");
        assert!(reg.is_empty());
    }

    #[test]
    fn remove_if_generation_only_removes_matching() {
        let reg = SessionRegistry::new();
        let t0 = Instant::now();
        reg.register(params("uid-1", "client", "echo"), t0, None)
            .unwrap();
        let gen1 = reg.get("uid-1").unwrap().connection_generation;
        // Stale gen — не удаляет.
        assert!(reg.remove_if_generation("uid-1", gen1 + 99).is_none());
        assert!(reg.get("uid-1").is_some());
        // Совпадающий gen — удаляет.
        let removed = reg.remove_if_generation("uid-1", gen1).expect("removed");
        assert_eq!(removed.session_id, "uid-1");
        assert!(reg.is_empty());
    }

    #[test]
    fn remove_if_generation_after_reconnect_protects_new_record() {
        let reg = SessionRegistry::new();
        let t0 = Instant::now();
        reg.register(params("uid-1", "client", "echo"), t0, None)
            .unwrap();
        let stale_gen = reg.get("uid-1").unwrap().connection_generation;
        reg.mark_disconnected("uid-1", t0 + Duration::from_secs(1));
        // Soft reconnect — поднимает поколение.
        reg.register(params("uid-1", "client", "echo"), t0 + Duration::from_secs(2), None)
            .unwrap();
        let new_gen = reg.get("uid-1").unwrap().connection_generation;
        assert!(new_gen > stale_gen);
        // Старый shutdown_session с stale gen НЕ должен удалить новую запись.
        assert!(reg.remove_if_generation("uid-1", stale_gen).is_none());
        assert!(reg.get("uid-1").is_some());
    }

    #[test]
    fn bump_last_call_updates_timestamp() {
        let reg = SessionRegistry::new();
        let t0 = Instant::now();
        reg.register(params("uid-1", "client", "echo"), t0, None)
            .unwrap();
        let before = reg.get("uid-1").unwrap().last_call_at;
        let t1 = t0 + Duration::from_secs(5);
        assert!(reg.bump_last_call("uid-1", t1));
        let after = reg.get("uid-1").unwrap().last_call_at;
        assert_eq!(after, t1);
        assert!(after > before);
    }

    #[test]
    fn bump_last_call_missing_session_returns_false() {
        let reg = SessionRegistry::new();
        assert!(!reg.bump_last_call("nope", Instant::now()));
    }

    #[test]
    fn mark_disconnected_if_generation_skips_stale_generation() {
        let reg = SessionRegistry::new();
        let t0 = Instant::now();
        reg.register(params("uid-1", "client", "echo"), t0, None)
            .unwrap();
        let stale_gen = reg.get("uid-1").unwrap().connection_generation;
        reg.mark_disconnected("uid-1", t0 + Duration::from_secs(1));
        // Soft reconnect — поднимает поколение.
        reg.register(
            params("uid-1", "client", "echo"),
            t0 + Duration::from_secs(2),
            None,
        )
        .unwrap();
        let new_gen = reg.get("uid-1").unwrap().connection_generation;
        assert!(new_gen > stale_gen);
        // Старый WS-task с stale gen не должен переводить свежую Active в Disconnected.
        assert!(!reg.mark_disconnected_if_generation(
            "uid-1",
            stale_gen,
            t0 + Duration::from_secs(3),
        ));
        assert_eq!(reg.get("uid-1").unwrap().state, SessionState::Active);
        // А свежий WS-task со своим поколением — переводит.
        assert!(reg.mark_disconnected_if_generation(
            "uid-1",
            new_gen,
            t0 + Duration::from_secs(4),
        ));
        assert_eq!(reg.get("uid-1").unwrap().state, SessionState::Disconnected);
    }

    #[test]
    fn update_tools_replaces_set() {
        let reg = SessionRegistry::new();
        let t0 = Instant::now();
        reg.register(params("uid-1", "client", "echo"), t0, None)
            .unwrap();
        let new_tools = vec![
            ToolDescriptor {
                name: "a".to_owned(),
                description: None,
                input_schema: json!({}),
            },
            ToolDescriptor {
                name: "b".to_owned(),
                description: Some("desc".to_owned()),
                input_schema: json!({}),
            },
        ];
        assert!(reg.update_tools("uid-1", new_tools.clone()));
        let rec = reg.get("uid-1").unwrap();
        assert_eq!(rec.tools, new_tools);
    }

    #[test]
    fn update_tools_diff_gate_skips_signal_on_identical_set() {
        let reg = SessionRegistry::new();
        let t0 = Instant::now();
        reg.register(params("uid-1", "client", "echo"), t0, None)
            .unwrap();
        let new_tools = vec![ToolDescriptor {
            name: "x".to_owned(),
            description: None,
            input_schema: json!({}),
        }];
        // первый update — должен инкрементировать epoch
        let epoch_before_first = reg.tools_epoch();
        assert!(reg.update_tools("uid-1", new_tools.clone()));
        let epoch_after_first = reg.tools_epoch();
        assert!(epoch_after_first > epoch_before_first);

        // второй update с идентичным набором — epoch НЕ должен меняться
        assert!(reg.update_tools("uid-1", new_tools.clone()));
        let epoch_after_second = reg.tools_epoch();
        assert_eq!(epoch_after_first, epoch_after_second);

        // третий update с реально другим набором — снова инкремент
        let other_tools = vec![ToolDescriptor {
            name: "y".to_owned(),
            description: None,
            input_schema: json!({}),
        }];
        assert!(reg.update_tools("uid-1", other_tools));
        assert!(reg.tools_epoch() > epoch_after_second);
    }

    #[test]
    fn snapshot_returns_all_records() {
        let reg = SessionRegistry::new();
        let t0 = Instant::now();
        reg.register(params("uid-1", "client", "x"), t0, None)
            .unwrap();
        reg.register(params("uid-2", "yaxunit_runner", "y"), t0, None)
            .unwrap();
        let mut ids: Vec<String> = reg.snapshot().into_iter().map(|r| r.session_id).collect();
        ids.sort();
        assert_eq!(ids, vec!["uid-1".to_owned(), "uid-2".to_owned()]);
    }
}

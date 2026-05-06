//! `EnvCarrier` — MVP реализация ADR‑0020 «Доставка `manager_url` через ENV».
//!
//! При spawn'е дочернего 1С‑клиента (или мока в тестах) менеджер передаёт
//! три обязательные переменные окружения, которые читает встраиваемое
//! расширение `web-transport-addin` (см. ADR‑0020 §4):
//!
//! | Переменная | Назначение |
//! |---|---|
//! | `V8_SESSION_MANAGER_URL` | URL WS‑эндпойнта менеджера, на который клиент должен подключиться |
//! | `V8_SESSION_CLIENT_UID`  | Стабильный UID клиента, используемый для soft reconnect (ADR‑0022) |
//! | `V8_SESSION_KIND`        | Kind, который клиент передаст в `session.register` |
//!
//! Этап 3 MVP: только ENV‑путь. Composite‑путь (params‑file + CLI) — этап 5.

use std::collections::BTreeMap;

/// Имена переменных окружения, через которые менеджер передаёт параметры.
#[allow(dead_code)]
pub mod env_keys {
    pub const MANAGER_URL: &str = "V8_SESSION_MANAGER_URL";
    pub const CLIENT_UID: &str = "V8_SESSION_CLIENT_UID";
    pub const KIND: &str = "V8_SESSION_KIND";
}

/// Параметры, которые менеджер должен донести до spawn'енного клиента.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvCarrier {
    pub manager_url: String,
    pub client_uid: String,
    pub kind: String,
}

#[allow(dead_code)]
impl EnvCarrier {
    pub fn new(
        manager_url: impl Into<String>,
        client_uid: impl Into<String>,
        kind: impl Into<String>,
    ) -> Self {
        Self {
            manager_url: manager_url.into(),
            client_uid: client_uid.into(),
            kind: kind.into(),
        }
    }

    /// Преобразует carrier в набор пар `(имя, значение)` для `Command::envs`.
    pub fn to_env_pairs(&self) -> [(&'static str, String); 3] {
        [
            (env_keys::MANAGER_URL, self.manager_url.clone()),
            (env_keys::CLIENT_UID, self.client_uid.clone()),
            (env_keys::KIND, self.kind.clone()),
        ]
    }

    /// Подставляет `${manager_url}`, `${client_uid}`, `${kind}` в строку
    /// (используется для аргументов командной строки шаблона).
    pub fn interpolate(&self, value: &str) -> String {
        value
            .replace("${manager_url}", &self.manager_url)
            .replace("${client_uid}", &self.client_uid)
            .replace("${kind}", &self.kind)
    }

    /// Прочитать carrier из переменных окружения (для отладки и для mock_client).
    pub fn from_env_map(env: &BTreeMap<String, String>) -> Option<Self> {
        let manager_url = env.get(env_keys::MANAGER_URL)?.clone();
        let client_uid = env.get(env_keys::CLIENT_UID)?.clone();
        let kind = env.get(env_keys::KIND)?.clone();
        Some(Self {
            manager_url,
            client_uid,
            kind,
        })
    }

    /// Прочитать carrier из текущего процесса. Возвращает `None`, если
    /// хотя бы одна обязательная переменная отсутствует.
    pub fn from_process_env() -> Option<Self> {
        let manager_url = std::env::var(env_keys::MANAGER_URL).ok()?;
        let client_uid = std::env::var(env_keys::CLIENT_UID).ok()?;
        let kind = std::env::var(env_keys::KIND).ok()?;
        Some(Self {
            manager_url,
            client_uid,
            kind,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_env_pairs_returns_three_required_keys() {
        let carrier = EnvCarrier::new("ws://localhost:4000/ws", "uid-1", "yaxunit_runner");
        let pairs = carrier.to_env_pairs();
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0].0, "V8_SESSION_MANAGER_URL");
        assert_eq!(pairs[0].1, "ws://localhost:4000/ws");
        assert_eq!(pairs[1].0, "V8_SESSION_CLIENT_UID");
        assert_eq!(pairs[1].1, "uid-1");
        assert_eq!(pairs[2].0, "V8_SESSION_KIND");
        assert_eq!(pairs[2].1, "yaxunit_runner");
    }

    #[test]
    fn interpolate_substitutes_all_three_placeholders() {
        let carrier = EnvCarrier::new("ws://h:4000/ws", "uid-42", "mock");
        let out = carrier.interpolate("--url=${manager_url} --uid=${client_uid} --kind=${kind}");
        assert_eq!(out, "--url=ws://h:4000/ws --uid=uid-42 --kind=mock");
    }

    #[test]
    fn interpolate_leaves_unrelated_text_untouched() {
        let carrier = EnvCarrier::new("u", "c", "k");
        let out = carrier.interpolate("/path/to/file --flag");
        assert_eq!(out, "/path/to/file --flag");
    }

    #[test]
    fn from_env_map_round_trips() {
        let carrier = EnvCarrier::new("u", "c", "k");
        let map: BTreeMap<String, String> = carrier
            .to_env_pairs()
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        let restored = EnvCarrier::from_env_map(&map).expect("round trip");
        assert_eq!(restored, carrier);
    }

    #[test]
    fn from_env_map_returns_none_when_any_var_missing() {
        let mut map = BTreeMap::new();
        map.insert(env_keys::MANAGER_URL.to_owned(), "u".to_owned());
        map.insert(env_keys::CLIENT_UID.to_owned(), "c".to_owned());
        // KIND отсутствует
        assert!(EnvCarrier::from_env_map(&map).is_none());
    }
}

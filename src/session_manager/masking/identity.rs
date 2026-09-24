//! Deployment-owned резолюция identity: WS‑сессия ↔ база сервиса маскирования.
//!
//! Сопоставление задаётся конфигом по имени: `masking.identity_bindings[]`
//! привязывает `client_uid` сессии (например `server-gbig_pam_ai`) к
//! `database_id` в сервисе маскирования. Менеджер не доверяет identity,
//! присланной адаптером: регистрация не несёт ни UUID установки, ни
//! маршрутной привязки — источник истины только конфиг.

use std::collections::HashMap;

use uuid::Uuid;

use crate::config::model::MaskingIdentityBinding;
use crate::session_manager::registry::SessionRecord;

/// Проверенный routing identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedDatabaseIdentity {
    /// UUID базы в сервисе маскирования.
    pub database_id: Uuid,
}

/// Резолвер deployment bindings из конфига.
#[derive(Debug, Clone)]
pub struct IdentityResolver {
    /// `client_uid` сессии → `database_id` сервиса маскирования.
    bindings: HashMap<String, Uuid>,
}

impl IdentityResolver {
    /// Строит resolver из deployment bindings конфига.
    /// Повторяющиеся имена сессий отклоняются валидатором конфига; при
    /// невозможном дубле здесь последняя привязка побеждает (config — source
    /// of truth), что не расширяет доступ.
    pub fn new(bindings: &[MaskingIdentityBinding]) -> Self {
        let mut map = HashMap::with_capacity(bindings.len());
        for binding in bindings {
            if let Ok(database_id) = Uuid::parse_str(&binding.database_id) {
                map.insert(binding.session.clone(), database_id);
            }
        }
        Self { bindings: map }
    }

    /// Резолвит database identity по имени сессии.
    ///
    /// `None` для любой сессии, которой нет в конфиге: в masking-контуре
    /// существуют только заранее известные маршруты.
    pub fn verify(&self, record: &SessionRecord) -> Option<VerifiedDatabaseIdentity> {
        self.bindings
            .get(record.session_id.as_str())
            .map(|database_id| VerifiedDatabaseIdentity {
                database_id: *database_id,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_manager::protocol::{SessionRegisterParams, ToolDescriptor, ToolVisibility};
    use crate::session_manager::registry::SessionRegistry;
    use std::time::Instant;

    fn record(session: &str) -> SessionRecord {
        let registry = SessionRegistry::new();
        registry
            .register(
                SessionRegisterParams {
                    client_uid: session.to_owned(),
                    kind: "server".to_owned(),
                    version: "1".to_owned(),
                    infobase_name: "db".to_owned(),
                    ib_session_number: 1,
                    tools: vec![ToolDescriptor {
                        name: "mcp_internal_masking_metadata_feed".to_owned(),
                        description: None,
                        input_schema: serde_json::json!({"type":"object"}),
                        visibility: ToolVisibility::Internal,
                    }],
                    config_id: None,
                    host_id: None,
                    pid: None,
                    resources: None,
                    prompts: None,
                    extras: None,
                },
                Instant::now(),
                None,
            )
            .unwrap();
        registry.get(session).unwrap()
    }

    #[test]
    fn database_resolves_by_session_name() {
        let database = Uuid::new_v4();
        let resolver = IdentityResolver::new(&[MaskingIdentityBinding {
            session: "server-gbig_pam_ai".to_owned(),
            database_id: database.to_string(),
        }]);

        assert_eq!(
            resolver.verify(&record("server-gbig_pam_ai")),
            Some(VerifiedDatabaseIdentity {
                database_id: database
            })
        );
        assert_eq!(resolver.verify(&record("other-session")), None);
    }
}

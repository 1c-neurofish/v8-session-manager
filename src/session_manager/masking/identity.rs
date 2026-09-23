use std::collections::HashMap;

use uuid::Uuid;

use crate::config::model::MaskingIdentityBinding;
use crate::session_manager::protocol::{SessionRegisterParams, ToolDescriptor, ToolVisibility};
use crate::session_manager::registry::{SessionRecord, TrustedRouteContext};

/// Проверенная manager-ом identity базы.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedDatabaseIdentity {
    database_id: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegistrationTrustError;

impl VerifiedDatabaseIdentity {
    pub fn database_id(&self) -> Uuid {
        self.database_id
    }
}

/// Immutable deployment bindings для проверки WS registration route.
#[derive(Debug, Clone)]
pub struct IdentityResolver {
    bindings: HashMap<Uuid, ResolvedBinding>,
}

#[derive(Debug, Clone)]
struct ResolvedBinding {
    database_id: Uuid,
    expected_kind: String,
    expected_config_id: String,
    expected_host_id: Option<String>,
    allowed_internal_tools: Vec<String>,
}

impl IdentityResolver {
    pub fn new(bindings: &[MaskingIdentityBinding]) -> Self {
        let bindings = bindings
            .iter()
            .filter_map(|binding| {
                let instance_id = Uuid::parse_str(&binding.database_instance_id).ok()?;
                let database_id = Uuid::parse_str(&binding.database_id).ok()?;
                Some((
                    instance_id,
                    ResolvedBinding {
                        database_id,
                        expected_kind: binding.expected_kind.clone(),
                        expected_config_id: binding.expected_config_id.clone(),
                        expected_host_id: binding.expected_host_id.clone(),
                        allowed_internal_tools: binding.allowed_internal_tools.clone(),
                    },
                ))
            })
            .collect();
        Self { bindings }
    }

    /// Identity выдаётся только при полном совпадении UUID и ожидаемого route.
    pub fn verify(&self, record: &SessionRecord) -> Option<VerifiedDatabaseIdentity> {
        let trusted = record.trusted_route.as_ref()?;
        let instance_id = record.database_instance_id.as_deref()?;
        let instance_id = Uuid::parse_str(instance_id).ok()?;
        let binding = self.bindings.get(&instance_id)?;
        if trusted.database_instance_id != instance_id
            || trusted.database_id != binding.database_id
            || record.kind != binding.expected_kind
            || record.config_id != binding.expected_config_id
            || binding
                .expected_host_id
                .as_ref()
                .is_some_and(|host| host != &record.host_id)
        {
            return None;
        }
        Some(VerifiedDatabaseIdentity {
            database_id: trusted.database_id,
        })
    }

    /// Проверка registration tuple выполняется до помещения сессии в registry.
    pub fn validate_registration(
        &self,
        params: &SessionRegisterParams,
    ) -> Result<Option<TrustedRouteContext>, RegistrationTrustError> {
        let has_internal = params
            .tools
            .iter()
            .any(|tool| tool.visibility == ToolVisibility::Internal);
        let Some(instance_id) = params.database_instance_id.as_deref() else {
            return if has_internal {
                Err(RegistrationTrustError)
            } else {
                Ok(None)
            };
        };
        let instance_id = Uuid::parse_str(instance_id).map_err(|_| RegistrationTrustError)?;
        let binding = self
            .bindings
            .get(&instance_id)
            .ok_or(RegistrationTrustError)?;
        let config_id = params.config_id.as_deref().unwrap_or(&params.kind);
        let host_id = params.host_id.as_deref().unwrap_or("unknown");
        if params.kind != binding.expected_kind
            || config_id != binding.expected_config_id
            || binding
                .expected_host_id
                .as_deref()
                .is_some_and(|expected| expected != host_id)
            || params.tools.iter().any(|tool| {
                tool.visibility == ToolVisibility::Internal
                    && !binding
                        .allowed_internal_tools
                        .iter()
                        .any(|allowed| allowed == &tool.name)
            })
        {
            return Err(RegistrationTrustError);
        }
        Ok(Some(TrustedRouteContext {
            database_instance_id: instance_id,
            database_id: binding.database_id,
        }))
    }

    pub fn validate_tool_update(&self, record: &SessionRecord, tools: &[ToolDescriptor]) -> bool {
        let Some(trusted) = record.trusted_route.as_ref() else {
            return !tools
                .iter()
                .any(|tool| tool.visibility == ToolVisibility::Internal);
        };
        let Some(binding) = self.bindings.get(&trusted.database_instance_id) else {
            return false;
        };
        tools.iter().all(|tool| {
            tool.visibility != ToolVisibility::Internal
                || binding
                    .allowed_internal_tools
                    .iter()
                    .any(|allowed| allowed == &tool.name)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_manager::protocol::ToolDescriptor;
    use serde_json::json;

    fn registration(instance_id: Option<String>, tool_name: &str) -> SessionRegisterParams {
        SessionRegisterParams {
            client_uid: "server-1".to_owned(),
            kind: "server".to_owned(),
            version: "1".to_owned(),
            infobase_name: "db".to_owned(),
            ib_session_number: 1,
            database_instance_id: instance_id,
            tools: vec![ToolDescriptor {
                name: tool_name.to_owned(),
                description: None,
                input_schema: json!({"type":"object"}),
                visibility: ToolVisibility::Internal,
            }],
            config_id: Some("server".to_owned()),
            host_id: Some("host".to_owned()),
            pid: None,
            resources: None,
            prompts: None,
            extras: None,
        }
    }

    #[test]
    fn registration_requires_exact_binding_before_internal_tool_is_trusted() {
        let instance = Uuid::new_v4();
        let database = Uuid::new_v4();
        let resolver = IdentityResolver::new(&[MaskingIdentityBinding {
            database_instance_id: instance.to_string(),
            database_id: database.to_string(),
            expected_kind: "server".to_owned(),
            expected_config_id: "server".to_owned(),
            expected_host_id: Some("host".to_owned()),
            allowed_internal_tools: vec!["allowed-internal".to_owned()],
        }]);

        let trusted = resolver
            .validate_registration(&registration(
                Some(instance.to_string()),
                "allowed-internal",
            ))
            .unwrap()
            .unwrap();
        assert_eq!(trusted.database_id, database);
        assert!(resolver
            .validate_registration(&registration(Some(instance.to_string()), "not-allowed"))
            .is_err());
        assert!(resolver
            .validate_registration(&registration(None, "allowed-internal"))
            .is_err());
    }
}

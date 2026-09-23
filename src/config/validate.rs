use std::net::SocketAddr;
use std::time::Duration;

use thiserror::Error;

use crate::config::model::AppConfig;

#[derive(Debug, Error)]
pub enum ConfigValidationError {
    #[error("workPath is empty")]
    EmptyWorkPath,

    #[error("mcp.http.bind_address '{0}' is not a valid socket address")]
    InvalidHttpBindAddress(String),

    #[error("mcp.http.path must start with '/' but got '{0}'")]
    InvalidHttpPath(String),

    #[error("mcp.session_manager.bind_address '{0}' is not a valid socket address")]
    InvalidSessionManagerBindAddress(String),

    #[error("mcp.session_manager.path must start with '/' but got '{0}'")]
    InvalidSessionManagerPath(String),

    #[error("mcp.metrics.bind_address '{0}' is not a valid socket address")]
    InvalidMetricsBindAddress(String),

    #[error("tools_cache.cache_life_period must be >= 1s (got {0:?})")]
    ToolsCacheLifePeriodTooSmall(Duration),

    #[error("masking.socket_path must be an absolute non-empty path")]
    InvalidMaskingSocketPath,

    #[error("masking.{0} must be greater than zero")]
    InvalidMaskingTimeout(&'static str),

    #[error("masking.managed_tools must contain unique non-empty names")]
    InvalidManagedTools,

    #[error("masking.identity_bindings must be non-empty when masking is enabled")]
    MissingMaskingIdentityBindings,

    #[error("masking broker verification settings are invalid")]
    InvalidMaskingBrokerSettings,

    #[error("invalid masking identity binding: {0}")]
    InvalidMaskingIdentityBinding(String),
}

pub fn validate(config: &AppConfig) -> Result<(), ConfigValidationError> {
    if config.work_path.as_os_str().is_empty() {
        return Err(ConfigValidationError::EmptyWorkPath);
    }

    let http = &config.mcp.http;
    if http.bind_address.parse::<SocketAddr>().is_err() {
        return Err(ConfigValidationError::InvalidHttpBindAddress(
            http.bind_address.clone(),
        ));
    }
    if !http.path.starts_with('/') {
        return Err(ConfigValidationError::InvalidHttpPath(http.path.clone()));
    }

    if let Some(sm) = &config.mcp.session_manager {
        if sm.bind_address.parse::<SocketAddr>().is_err() {
            return Err(ConfigValidationError::InvalidSessionManagerBindAddress(
                sm.bind_address.clone(),
            ));
        }
        if !sm.path.starts_with('/') {
            return Err(ConfigValidationError::InvalidSessionManagerPath(
                sm.path.clone(),
            ));
        }
    }

    if let Some(addr) = &config.mcp.metrics.bind_address {
        if !addr.is_empty() && addr.parse::<SocketAddr>().is_err() {
            return Err(ConfigValidationError::InvalidMetricsBindAddress(
                addr.clone(),
            ));
        }
    }

    if config.tools_cache.enabled && config.tools_cache.cache_life_period < Duration::from_secs(1) {
        return Err(ConfigValidationError::ToolsCacheLifePeriodTooSmall(
            config.tools_cache.cache_life_period,
        ));
    }

    if config.masking.enabled {
        if config.masking.socket_path.as_os_str().is_empty()
            || !config.masking.socket_path.is_absolute()
        {
            return Err(ConfigValidationError::InvalidMaskingSocketPath);
        }
        if config.masking.preflight_timeout_ms == 0 {
            return Err(ConfigValidationError::InvalidMaskingTimeout(
                "preflight_timeout_ms",
            ));
        }
        if config.masking.finalize_timeout_ms == 0 {
            return Err(ConfigValidationError::InvalidMaskingTimeout(
                "finalize_timeout_ms",
            ));
        }
        if config.masking.feed_chunk_timeout_ms == 0 {
            return Err(ConfigValidationError::InvalidMaskingTimeout(
                "feed_chunk_timeout_ms",
            ));
        }
        if config.masking.feed_activate_timeout_ms == 0 {
            return Err(ConfigValidationError::InvalidMaskingTimeout(
                "feed_activate_timeout_ms",
            ));
        }
        let mut tools = std::collections::HashSet::new();
        if config.masking.managed_tools.is_empty()
            || config
                .masking
                .managed_tools
                .iter()
                .any(|name| name.is_empty() || !tools.insert(name))
        {
            return Err(ConfigValidationError::InvalidManagedTools);
        }
        let required_tools = [
            "execute_query",
            "find_references_to_object",
            "get_object_by_link",
            "get_metadata",
            "get_access_rights",
            "get_link_of_object",
        ]
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
        if tools.len() != required_tools.len()
            || !tools
                .iter()
                .all(|name| required_tools.contains(name.as_str()))
        {
            return Err(ConfigValidationError::InvalidManagedTools);
        }
        if config.masking.identity_bindings.is_empty() {
            return Err(ConfigValidationError::MissingMaskingIdentityBindings);
        }
        if !config.masking.broker_public_key_path.is_absolute()
            || config.masking.broker_issuer.is_empty()
            || config.masking.broker_audience != "v8-session-manager"
            || config.masking.broker_max_assertion_ttl_secs == 0
            || config.masking.broker_max_assertion_ttl_secs > 300
            || config.masking.conversation_assertion_header.is_empty()
            || config
                .masking
                .conversation_assertion_header
                .parse::<axum::http::HeaderName>()
                .is_err()
        {
            return Err(ConfigValidationError::InvalidMaskingBrokerSettings);
        }
        let mut instances = std::collections::HashSet::new();
        let mut databases = std::collections::HashSet::new();
        let required_internal_tools = [
            "mcp_internal_masking_metadata_feed",
            "mcp_internal_masking_dictionary_feed",
        ]
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
        for binding in &config.masking.identity_bindings {
            let instance = uuid::Uuid::parse_str(&binding.database_instance_id).map_err(|_| {
                ConfigValidationError::InvalidMaskingIdentityBinding(
                    "database_instance_id must be UUID".to_owned(),
                )
            })?;
            let database = uuid::Uuid::parse_str(&binding.database_id).map_err(|_| {
                ConfigValidationError::InvalidMaskingIdentityBinding(
                    "database_id must be UUID".to_owned(),
                )
            })?;
            let allowed_internal_tools = binding
                .allowed_internal_tools
                .iter()
                .map(String::as_str)
                .collect::<std::collections::HashSet<_>>();
            if binding.expected_kind.is_empty()
                || binding.expected_config_id.is_empty()
                || allowed_internal_tools != required_internal_tools
                || !instances.insert(instance)
                || !databases.insert(database)
            {
                return Err(ConfigValidationError::InvalidMaskingIdentityBinding(
                    "bindings must have unique IDs, an exact route, and the two hidden feed tools"
                        .to_owned(),
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::{AppConfig, McpConfig, ToolsCacheConfig};
    use std::path::PathBuf;

    fn base_config() -> AppConfig {
        AppConfig {
            work_path: PathBuf::from("/tmp/v8sm-test"),
            mcp: McpConfig::default(),
            tools_cache: ToolsCacheConfig::default(),
            masking: crate::config::model::MaskingConfig::default(),
        }
    }

    #[test]
    fn default_tools_cache_validates() {
        let cfg = base_config();
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn rejects_cache_life_period_below_1s_when_enabled() {
        let mut cfg = base_config();
        cfg.tools_cache.cache_life_period = Duration::from_millis(500);
        let err = validate(&cfg).expect_err("should reject");
        assert!(matches!(
            err,
            ConfigValidationError::ToolsCacheLifePeriodTooSmall(_)
        ));
    }

    /// Edge: ровно 1s (минимум по контракту) принимается.
    #[test]
    fn accepts_cache_life_period_exactly_1s() {
        let mut cfg = base_config();
        cfg.tools_cache.cache_life_period = Duration::from_secs(1);
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn allows_small_cache_life_when_disabled() {
        // Disabled cache: validator не давит, чтобы можно было выключить через
        // env override без правки cache_life_period.
        let mut cfg = base_config();
        cfg.tools_cache.enabled = false;
        cfg.tools_cache.cache_life_period = Duration::from_millis(0);
        assert!(validate(&cfg).is_ok());
    }

    #[test]
    fn masking_requires_exact_internal_feed_allowlist() {
        let mut cfg = base_config();
        cfg.masking.enabled = true;
        cfg.masking.identity_bindings = vec![crate::config::model::MaskingIdentityBinding {
            database_instance_id: uuid::Uuid::new_v4().to_string(),
            database_id: uuid::Uuid::new_v4().to_string(),
            expected_kind: "server".to_owned(),
            expected_config_id: "server".to_owned(),
            expected_host_id: None,
            allowed_internal_tools: vec![
                "mcp_internal_masking_metadata_feed".to_owned(),
                "mcp_internal_masking_dictionary_feed".to_owned(),
            ],
        }];
        assert!(validate(&cfg).is_ok());

        cfg.masking.identity_bindings[0].allowed_internal_tools[1] =
            "agent_visible_tool".to_owned();
        assert!(matches!(
            validate(&cfg),
            Err(ConfigValidationError::InvalidMaskingIdentityBinding(_))
        ));
    }
}

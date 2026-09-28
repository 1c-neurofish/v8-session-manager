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

    #[error("masking.internal_listen_path must be an absolute non-empty path")]
    InvalidMaskingInternalListenPath,

    #[error("masking.{0} must be greater than zero")]
    InvalidMaskingTimeout(&'static str),

    #[error("masking.internal_tools must contain unique non-empty names")]
    InvalidInternalTools,

    #[error("masking.service_expected_uid is required when masking is enabled")]
    MissingServiceExpectedUid,
    //++agent TASK-225 [26.09.2026] N: `identity_bindings` и YAML-настройки
    // RAS удалены — маршрут по (GUID кластера, GUID ИБ), RAS-резолюция
    // конфигурируется env (см. masking::ras), пустой/отсутствующий RAS —
    // штатный режим, не ошибка валидации.
    //++agent TASK-225
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
        if config.masking.internal_listen_path.as_os_str().is_empty()
            || !config.masking.internal_listen_path.is_absolute()
        {
            return Err(ConfigValidationError::InvalidMaskingInternalListenPath);
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
        if config.masking.internal_call_timeout_ms == 0 {
            return Err(ConfigValidationError::InvalidMaskingTimeout(
                "internal_call_timeout_ms",
            ));
        }
        //++agent TASK-225 [25.09.2026]
        // managed_tools устарел: при enabled=true через gate идут ВСЕ
        // публичные proxy-вызовы, список не влияет на маршрут и не
        // валидируется (старое требование ровно шести имён удалено).
        //++agent TASK-225
        let mut internal_tools = std::collections::HashSet::new();
        if config.masking.internal_tools.is_empty()
            || config
                .masking
                .internal_tools
                .iter()
                .any(|name| name.is_empty() || !internal_tools.insert(name))
        {
            return Err(ConfigValidationError::InvalidInternalTools);
        }
        if config.masking.service_expected_uid.is_none() {
            return Err(ConfigValidationError::MissingServiceExpectedUid);
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
    fn masking_enabled_requires_service_uid() {
        let mut cfg = base_config();
        cfg.masking.enabled = true;
        cfg.masking.socket_path = PathBuf::from("/run/mask.sock");
        cfg.masking.internal_listen_path = PathBuf::from("/run/mask-manager.sock");
        // TASK-225/N: identity_bindings удалены — маршрут по координатам
        // кластера (cluster_server + RAS-резолвленные GUID-ы), YAML-привязок
        // и RAS-конфига больше нет: RAS задаётся env и опционален.
        assert!(matches!(
            validate(&cfg),
            Err(ConfigValidationError::MissingServiceExpectedUid)
        ));

        cfg.masking.service_expected_uid = Some(994);
        assert!(validate(&cfg).is_ok());
    }
}

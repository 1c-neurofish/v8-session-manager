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
}

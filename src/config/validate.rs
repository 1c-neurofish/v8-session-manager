use std::net::SocketAddr;

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

    Ok(())
}

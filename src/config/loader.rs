use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::model::AppConfig;
use crate::config::validate::{validate, ConfigValidationError};

const DEFAULT_CONFIG_FILE_NAME: &str = "v8project.yaml";

#[derive(Debug, Error)]
pub enum ConfigLoadError {
    #[error("config file not found: {0}")]
    NotFound(PathBuf),

    #[error("failed to read config file '{path}': {source}")]
    ReadError { path: PathBuf, source: io::Error },

    #[error("failed to parse YAML config '{path}': {source}")]
    ParseError {
        path: PathBuf,
        source: serde_yaml::Error,
    },

    #[error("config validation failed: {0}")]
    ValidationError(#[from] ConfigValidationError),
}

/// Result of `read_config`: parsed YAML + the directory of the config file
/// (для последующего резолва относительных путей в `workPath` и т.п.).
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: AppConfig,
    pub config_dir: PathBuf,
}

/// Read & parse the YAML config WITHOUT validation.
///
/// CLI overrides (`--bind`/`--path`/`--mcp-http`/`--workdir`) применяются
/// вызывающим кодом (`app::run`) ПОСЛЕ чтения, ДО валидации. Это закрывает
/// codex WARN «invalid `--path sessions` пролезает через validate».
///
/// `workdir_override` здесь используется ТОЛЬКО для резолва относительного
/// пути к конфиг-файлу (если `--config` не задан или относителен). На
/// `config.work_path` он не влияет — override применяется отдельно через
/// `apply_workdir_override` в `app::run` уже ПОСЛЕ `resolve_workpath`,
/// чтобы относительный `--workdir` интерпретировался относительно cwd
/// процесса, а не директории конфига (стандартный CLI-контракт, WARN-1).
pub fn read_config(
    config_path: Option<&str>,
    workdir_override: Option<&str>,
) -> Result<LoadedConfig, ConfigLoadError> {
    let resolved = resolve_config_path(config_path, workdir_override);
    if !resolved.exists() {
        return Err(ConfigLoadError::NotFound(resolved));
    }
    let raw = fs::read_to_string(&resolved).map_err(|source| ConfigLoadError::ReadError {
        path: resolved.clone(),
        source,
    })?;
    let config: AppConfig =
        serde_yaml::from_str(&raw).map_err(|source| ConfigLoadError::ParseError {
            path: resolved.clone(),
            source,
        })?;

    let config_dir = resolved
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    Ok(LoadedConfig { config, config_dir })
}

/// Резолвит `config.work_path` относительно директории файла конфига,
/// если путь относительный.
///
/// Закрывает codex WARN: «при запуске из другой cwd логи молча уезжают».
/// Если путь уже абсолютный — возвращает без изменений.
pub fn resolve_workpath(config: &mut AppConfig, config_dir: &Path) {
    if config.work_path.as_os_str().is_empty() {
        return;
    }
    if config.work_path.is_relative() {
        config.work_path = config_dir.join(&config.work_path);
    }
}

/// Final validation step. Должен вызываться ПОСЛЕ применения CLI overrides
/// и резолва workPath.
pub fn finalize_config(config: &AppConfig) -> Result<(), ConfigLoadError> {
    validate(config)?;
    Ok(())
}

fn resolve_config_path(path: Option<&str>, workdir: Option<&str>) -> PathBuf {
    if let Some(p) = path {
        let pb = PathBuf::from(p);
        if pb.is_absolute() {
            return pb;
        }
        // Резолв относительного --config относительно --workdir, если задан.
        if let Some(wd) = workdir {
            return PathBuf::from(wd).join(pb);
        }
        return pb;
    }
    let base: PathBuf = workdir
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(".").to_path_buf());
    base.join(DEFAULT_CONFIG_FILE_NAME)
}

use clap::Parser;
use tracing::{debug, error};

use crate::cli::args::Cli;
use crate::config::loader::{finalize_config, read_config, resolve_workpath, LoadedConfig};
use crate::output::exit_codes;

pub fn run() -> i32 {
    let cli = Cli::parse();

    install_panic_hook();

    let LoadedConfig {
        mut config,
        config_dir,
    } = match read_config(cli.config.as_deref(), cli.workdir.as_deref()) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!("{err}");
            return exit_codes::VALIDATION_ERROR;
        }
    };

    // 1) Резолвим YAML.workPath относительно директории файла конфига
    // (стандарт «config-relative paths»).
    resolve_workpath(&mut config, &config_dir);

    // 2) `--workdir` перетирает workPath ПОСЛЕ резолва: относительный
    // `--workdir build` интерпретируется относительно cwd процесса
    // (стандартный CLI-контракт, WARN-1).
    if let Some(wd) = cli.workdir.as_deref() {
        config.work_path = std::path::PathBuf::from(wd);
    }

    // 3) Применяем остальные CLI overrides ДО валидации (закрывает WARN
    // «--path sessions пролезает в axum»).
    apply_cli_overrides(&mut config, &cli);

    // 4) Валидация финального конфига.
    if let Err(err) = finalize_config(&config) {
        eprintln!("{err}");
        return exit_codes::VALIDATION_ERROR;
    }

    let level = cli.log_level.as_deref().unwrap_or("info");
    if let Err(err) =
        crate::support::logging::init_action_logging(level, "json", false, &config.work_path)
    {
        eprintln!("{err}");
        return exit_codes::RUNTIME_ERROR;
    }

    debug!(
        work_path = %config.work_path.display(),
        bind = ?cli.bind,
        path = ?cli.path,
        mcp_http = ?cli.mcp_http,
        "starting v8-session-manager"
    );

    match crate::mcp::server::serve_session_manager(config) {
        Ok(()) => 0,
        Err(err) => {
            error!("{err}");
            eprintln!("{err}");
            exit_codes::RUNTIME_ERROR
        }
    }
}

/// Применяет `--bind`, `--path`, `--mcp-http` из CLI к конфигу.
/// Должен вызываться ДО `finalize_config`, иначе невалидные значения
/// (например, `--path sessions` без ведущего слеша) пройдут валидатор.
fn apply_cli_overrides(config: &mut crate::config::model::AppConfig, cli: &Cli) {
    if let Some(http) = cli.mcp_http.clone() {
        config.mcp.http.bind_address = http;
    }

    if cli.bind.is_some() || cli.path.is_some() {
        let mut sm = config.mcp.session_manager.clone().unwrap_or_default();
        if let Some(bind) = cli.bind.clone() {
            sm.bind_address = bind;
        }
        if let Some(path) = cli.path.clone() {
            sm.path = path;
        }
        config.mcp.session_manager = Some(sm);
    }
}

fn install_panic_hook() {
    std::panic::set_hook(Box::new(|panic_info| {
        eprintln!("{panic_info}");
    }));
}

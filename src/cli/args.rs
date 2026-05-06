use clap::Parser;

/// CLI for the v8-session-manager binary.
///
/// Запускает менеджер клиентских сессий: WS transport (по умолчанию `:4000/sessions`)
/// и MCP HTTP server (по умолчанию `:4001/mcp`), которые делят один и тот же
/// `Arc<SessionRegistry>`.
#[derive(Debug, Parser)]
#[command(name = "v8-session-manager", about = "1С client session manager (WS + MCP HTTP)")]
pub struct Cli {
    /// Path to YAML config file. Defaults to ./v8project.yaml.
    #[arg(long, env = "V8SM_CONFIG")]
    pub config: Option<String>,

    /// Override working directory (used to resolve config and to write logs).
    #[arg(long)]
    pub workdir: Option<String>,

    /// Log level.
    #[arg(
        long,
        default_value = "info",
        value_parser = ["error", "warn", "info", "debug", "trace"]
    )]
    pub log_level: Option<String>,

    /// Override WS bind address (default: from config `mcp.session_manager.bind_address`).
    #[arg(long)]
    pub bind: Option<String>,

    /// Override WS path (default: `/sessions`).
    #[arg(long)]
    pub path: Option<String>,

    /// Override MCP HTTP bind address (default: from config `mcp.http.bind_address`).
    #[arg(long = "mcp-http")]
    pub mcp_http: Option<String>,
}

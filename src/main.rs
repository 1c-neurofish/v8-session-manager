mod app;
mod cli;
mod config;
mod mcp;
mod output;
mod session_manager;
mod support;

use std::process;

fn main() {
    let exit_code = app::run();
    process::exit(exit_code);
}

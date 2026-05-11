use std::process;

use v8_session_manager::app;

fn main() {
    let exit_code = app::run();
    process::exit(exit_code);
}

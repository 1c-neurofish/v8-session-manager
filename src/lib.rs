//! Internal crate library re-exporting modules of `v8-session-manager` for use
//! by integration tests in `tests/`. The binary entry point lives in
//! [`crate::main`](`main.rs`) and consumes the same modules through `mod` paths.
//!
//! Not a stable public API: the only consumers are integration tests inside
//! this repo.

pub mod app;
pub mod cli;
pub mod config;
pub mod mcp;
pub mod output;
pub mod session_manager;
pub mod support;

//! Fail-closed интеграция с внешним сервисом маскирования.

pub mod client;
pub mod gate;
pub mod identity;
pub mod internal;
pub mod ras;

pub use gate::{CallerInfo, MaskingCallContext, MaskingFailure, MaskingGate};

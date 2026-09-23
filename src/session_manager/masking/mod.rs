//! Fail-closed интеграция с внешним сервисом маскирования.

pub mod client;
pub mod feed;
pub mod gate;
pub mod identity;

pub use gate::{MaskingCallContext, MaskingFailure, MaskingGate, TrustedConversationContext};

//! Model ids used when `install` or `--auto-review` has no explicit model.

/// Conversation model (`--model`).
pub const DEFAULT_MODEL: &str = "gpt-6-astra";
/// Automatic approval reviewer (bare `--auto-review`; otherwise disabled).
pub const DEFAULT_AUTO_REVIEW: &str = "gpt-5.6-luna";

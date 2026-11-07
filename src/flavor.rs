//! How a provider exposes its model catalog.
//!
//! A leaf module: this enum depends on nothing, which is the point. It used to
//! live in [`crate::config`], and `credits` importing it from there made
//! `config → workbuddy_auth → credits → config` a cycle. Moving the enum out
//! breaks that without touching either module's behaviour.

/// How a provider exposes its model catalog.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ModelsFlavor {
    /// Standard OpenAI `GET {base}/v1/models` returning `{"data":[{"id":…}]}`.
    #[default]
    OpenAI,
    /// Vendor-specific config endpoint (WorkBuddy `/v3/config`) whose models
    /// must be filtered by `agents[name=cli].models`.
    WorkBuddyConfig,
}

//! Shared startup configuration for application entry points.
//!
//! TOML supplies new-session defaults, never mutable global session state.
//! Stored session definitions remain authoritative when reopening. Relative file
//! paths are resolved against the configuration directory. Credentials are names
//! of environment variables and are resolved only by session assembly.

mod file;
mod mcp;
mod model;
mod onboarding;

pub use crate::agents::ModelProfile;
pub use file::{Config, ConfigError, ConfigSource, user_tars_dir};
pub(crate) use file::{process_home_dir, resolve_user_tars_dir};
pub use model::{
    DefinitionOverrides, DynamicCustomOverride, PreludeOverrides, StaticCustomOverride, parse_mcp,
    parse_tools,
};
pub use onboarding::{
    OnboardingError, OnboardingInput, default_config_path, default_store_dir,
    default_tools_for_current_platform, default_tools_for_sandbox, render_initial_config,
    write_initial_config,
};

#[cfg(test)]
mod tests;

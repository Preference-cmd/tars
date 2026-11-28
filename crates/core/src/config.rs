//! Shared startup configuration for application entry points.
//!
//! TOML supplies new-session defaults, never mutable global session state.
//! Stored session definitions remain authoritative when reopening. Relative file
//! paths are resolved against the configuration directory. Credentials are names
//! of credential store entries or environment variables and are resolved only by
//! session assembly.

mod credentials;
mod file;
mod mcp;
mod model;
mod model_discovery;
mod onboarding;
mod providers;

pub use crate::agents::ModelProfile;
pub use credentials::{
    CredentialsError, CredentialsFileStatus, CredentialsStore, MAX_CREDENTIALS_FILE_BYTES,
    credentials_path_for_config, default_credentials_path,
};
pub use file::{Config, ConfigError, ConfigSource, user_tars_dir};
pub(crate) use file::{process_home_dir, resolve_user_tars_dir};
pub use model::{
    DefinitionOverrides, DynamicCustomOverride, PreludeOverrides, StaticCustomOverride, parse_mcp,
    parse_tools,
};
pub use model_discovery::{
    DiscoveredModel, ModelDiscoveryError, derive_models_url, fetch_available_models,
};
pub use onboarding::{
    OnboardingError, OnboardingInput, default_config_path, default_store_dir,
    default_tools_for_current_platform, default_tools_for_sandbox, render_initial_config,
    write_initial_config,
};
pub use providers::{
    PROVIDER_PRESETS, ProfileUpsertInput, ProviderPreset, find_provider_preset,
    upsert_profile_in_config,
};

pub(super) fn toml_string(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{0008}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{000C}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 || c == '\u{007F}' => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests;

//! Shared startup configuration for application entry points.
//!
//! TOML supplies new-session defaults, never mutable global session state.
//! Stored session definitions remain authoritative when reopening. Relative file
//! paths are resolved against the configuration directory. Credentials are names
//! of environment variables and are resolved only by session assembly.

mod file;
mod model;

pub use file::{Config, ConfigError, ConfigSource};
pub use model::DefinitionOverrides;

#[cfg(test)]
mod tests;

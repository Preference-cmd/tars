//! Internal application modules shared by the TARS executable and its consumers.
//!
//! This package is not published as a Rust SDK. Causa owns material contracts
//! and individual runs; TARS owns work admission, sessions and explicit storage.
#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod agent;
pub mod agents;
pub mod application;
pub(crate) mod assembly;
pub mod collaboration;
pub mod compact;
pub mod config;
pub mod definition;
pub mod doctor;
pub mod events;
pub mod external_tools;
pub(crate) mod fs_atomic;
pub mod material;
mod mcp;
pub mod permission;
pub mod policy;
pub mod prelude;
pub use tars_sandbox as sandbox;
pub mod session;
pub mod skills;
mod store;
pub mod time;
pub(crate) mod tools;
pub mod usage;
pub mod workspace;

pub use application::{MAX_SESSION_TITLE_CHARS, validate_session_title};
pub use mcp::{McpServerStatus, McpSkippedTool, McpStartError};
pub use time::{created_at_from_id, format_timestamp_utc};
pub use workspace::WorkspaceError;

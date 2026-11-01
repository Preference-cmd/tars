//! Internal application modules shared by the TARS executable and its consumers.
//!
//! This package is not published as a Rust SDK. Causa owns material contracts
//! and individual runs; TARS owns work admission, sessions and explicit storage.
#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod application;
pub(crate) mod assembly;
pub mod compact;
pub mod config;
pub mod definition;
pub mod material;
mod mcp;
pub mod permission;
pub use tars_sandbox as sandbox;
pub mod session;
mod store;
pub(crate) mod tools;
pub(crate) mod workspace;

pub use mcp::{McpServerStatus, McpSkippedTool};

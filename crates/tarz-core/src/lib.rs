//! Internal application modules shared by the Tarz executable and its consumers.
//!
//! This package is not published as a Rust SDK. Causa owns material contracts
//! and individual runs; Tarz owns work admission, sessions and explicit storage.
#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod application;
pub(crate) mod assembly;
pub mod config;
pub mod definition;
pub mod material;
pub mod permission;
pub mod session;
mod store;
pub(crate) mod tools;
pub(crate) mod workspace;

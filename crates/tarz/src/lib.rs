//! Internal application modules shared by the Tarz executable and its consumers.
//!
//! This package is not published as a Rust SDK. Causa owns material contracts
//! and individual runs; Tarz owns work admission, sessions and explicit storage.
#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod material;
pub mod session;

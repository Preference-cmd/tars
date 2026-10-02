//! Shared provider assembly and single-work execution, independent of UI and stdio.

mod execution;
mod model;

pub use execution::{RunReport, run_once, text_input};
pub use model::{ModelConfig, Provider};

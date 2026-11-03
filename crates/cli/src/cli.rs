//! Command-line entry point. Agent execution is implemented separately.

mod approval;
mod command;
mod configuration;
mod execute;
mod managed_execution;
mod options;
mod session;
mod tools;
pub(crate) mod usage_json;

pub(crate) use command::run;

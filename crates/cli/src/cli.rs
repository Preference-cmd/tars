//! Command-line entry point. Agent execution is implemented separately.

mod approval;
mod auth;
mod command;
mod completions;
mod configuration;
mod doctor;
mod execute;
mod managed_execution;
mod options;
mod output_format;
mod print;
mod session;
mod tools;
mod trust;
pub(crate) mod usage_json;

pub(crate) use command::run;

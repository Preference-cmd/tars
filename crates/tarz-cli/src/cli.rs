//! Command-line entry point. Agent execution is implemented separately.

mod approval;
mod command;
mod configuration;
mod execute;
mod managed_execution;
mod options;
mod session;

pub(crate) use command::run;

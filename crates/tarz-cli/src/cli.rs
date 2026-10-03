//! Command-line entry point. Agent execution is implemented separately.

mod command;
mod execute;
mod managed_execution;
mod options;
mod session;

pub(crate) use command::run;

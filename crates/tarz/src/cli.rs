//! Command-line entry point. Agent execution is implemented separately.

mod command;
mod execute;
mod options;

pub(crate) use command::run;

//! Native terminal interface for TARS.
#![deny(unsafe_code)]
#![deny(missing_docs)]

mod app;
mod input;
mod markdown;
mod options;
mod preview;
mod render;
mod terminal;
mod transcript;

use futures_util::FutureExt;
use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::panic::AssertUnwindSafe;
use std::process::ExitCode;

/// Run the interactive TARS terminal interface with the provided CLI arguments.
///
/// Parses interactive options (`--help` and `--version` exit immediately with
/// [`ExitCode::SUCCESS`]; argument errors exit with code `2`), verifies that
/// both stdin and stdout are interactive terminals, and only then constructs
/// the Tokio multi-threaded runtime and enters the terminal screen.
pub fn run(args: impl IntoIterator<Item = OsString>) -> ExitCode {
    let mut parsed_args = Vec::new();
    for arg in args {
        match arg.into_string() {
            Ok(text) => parsed_args.push(text),
            Err(invalid) => {
                eprintln!(
                    "TARS: invalid UTF-8 argument: {}",
                    invalid.to_string_lossy()
                );
                return ExitCode::from(2);
            }
        }
    }
    let options = match options::parse(parsed_args.into_iter()) {
        Ok(Some(options)) => options,
        Ok(None) => return ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("TARS: {error}");
            return ExitCode::from(2);
        }
    };
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("TARS requires an interactive terminal on stdin and stdout");
        return ExitCode::FAILURE;
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("TARS: failed to initialize async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    runtime.block_on(run_interactive(options))
}

async fn run_interactive(options: options::Options) -> ExitCode {
    let mut app = app::App::new(options);
    let mut terminal = match terminal::Screen::enter() {
        Ok(terminal) => terminal,
        Err(error) => {
            eprintln!("TARS: terminal initialization failed: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = AssertUnwindSafe(app.run(&mut terminal.terminal))
        .catch_unwind()
        .await;
    drop(terminal);
    let error = match result {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(format!("terminal event loop failed: {error}")),
        Err(_) => Some("terminal event loop panicked".into()),
    };
    if let Some(error) = error {
        eprintln!("TARS: {error}");
        // Without an input surface, try saving, then export actual materials
        // before releasing resources; this is not an interactive quit failure.
        for report in app.emergency_close().await {
            eprintln!("TARS: {report}");
        }
        ExitCode::FAILURE
    } else {
        for report in &app.exit_reports {
            eprintln!("TARS: {report}");
        }
        ExitCode::SUCCESS
    }
}

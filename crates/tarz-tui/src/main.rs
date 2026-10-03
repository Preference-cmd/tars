//! Experimental native terminal consumer of Tarz's shared application core.
#![deny(unsafe_code)]

mod app;
mod input;
mod options;
mod render;
mod terminal;
mod transcript;

use futures_util::FutureExt;
use std::io::{self, IsTerminal};
use std::panic::AssertUnwindSafe;
use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let options = match options::parse(std::env::args().skip(1)) {
        Ok(Some(options)) => options,
        Ok(None) => return ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Tarz TUI: {error}");
            return ExitCode::from(2);
        }
    };
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        eprintln!("Tarz TUI requires an interactive terminal on stdin and stdout");
        return ExitCode::FAILURE;
    }
    let mut app = app::App::new(options);
    let mut terminal = match terminal::Screen::enter() {
        Ok(terminal) => terminal,
        Err(error) => {
            eprintln!("Tarz TUI: terminal initialization failed: {error}");
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
        eprintln!("Tarz TUI: {error}");
        // Without an input surface, try saving, then export actual materials before
        // releasing resources. This is separate from an interactive quit failure.
        for report in app.emergency_close().await {
            eprintln!("Tarz TUI: {report}");
        }
        ExitCode::FAILURE
    } else {
        for report in &app.exit_reports {
            eprintln!("Tarz TUI: {report}");
        }
        ExitCode::SUCCESS
    }
}

use std::ffi::OsString;
use std::io::{self, Write};
use std::process::ExitCode;

const HELP: &str = "Tarz — agent application\n\n\
Usage: tarz [--help | --version]\n\n\
Options:\n  -h, --help     Show this help\n  -V, --version  Show the version\n\n\
This development build provides the application entry point only.\n\
Interactive agent sessions are not available yet.\n";

pub(crate) fn run(mut args: impl Iterator<Item = OsString>) -> ExitCode {
    let first = args.next();
    if args.next().is_some() {
        return usage_error();
    }
    let output = match first.as_deref().and_then(|arg| arg.to_str()) {
        None if first.is_none() => HELP.to_owned(),
        Some("-h" | "--help") => HELP.to_owned(),
        Some("-V" | "--version") => format!("tarz {}\n", env!("CARGO_PKG_VERSION")),
        _ => return usage_error(),
    };
    match io::stdout().lock().write_all(output.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

fn usage_error() -> ExitCode {
    let _ = writeln!(
        io::stderr().lock(),
        "Unsupported arguments. Use tarz --help."
    );
    ExitCode::from(2)
}

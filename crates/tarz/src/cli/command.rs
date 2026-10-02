use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

const HELP: &str = "Tarz — agent application\n\n\
Usage: tarz [--help | --version]\n       tarz demo [--save <file>]\n\n\
Commands:\n  demo           Run the offline single-session demonstration\n\n\
Options:\n  -h, --help     Show this help\n  -V, --version  Show the version\n\n\
The demo uses a scripted model and a local tool; it makes no network calls.\n\
--save replaces the selected file with Context materials only.\n\
Interactive online sessions are not available yet.\n";

pub(crate) fn run(args: impl Iterator<Item = OsString>) -> ExitCode {
    let args: Vec<_> = args.collect();
    match args.as_slice() {
        [] => output(HELP),
        [one] if one == "-h" || one == "--help" => output(HELP),
        [one] if one == "-V" || one == "--version" => {
            output(&format!("tarz {}\n", env!("CARGO_PKG_VERSION")))
        }
        [one] if one == "demo" => demo(None),
        [one, option, path] if one == "demo" && option == "--save" && !path.is_empty() => {
            demo(Some(path.into()))
        }
        _ => {
            let _ = writeln!(
                io::stderr().lock(),
                "Unsupported arguments. Use tarz --help."
            );
            ExitCode::from(2)
        }
    }
}

fn demo(path: Option<PathBuf>) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return failure(error),
    };
    match runtime.block_on(crate::demo::run(path)) {
        Ok(report) => output(&report),
        Err(error) => failure(error),
    }
}

fn output(text: &str) -> ExitCode {
    match io::stdout().lock().write_all(text.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

fn failure(error: impl std::fmt::Display) -> ExitCode {
    let _ = writeln!(io::stderr().lock(), "Tarz: {error}");
    ExitCode::FAILURE
}

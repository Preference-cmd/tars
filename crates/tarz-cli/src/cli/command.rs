use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

const HELP: &str = "Tarz — agent application

Usage: tarz [--help | --version]
       tarz demo [--save <file>]
       tarz run --provider <protocol> --model <id> (--input <text> | --stdin) [options]
       tarz session <create | run | list> [options]

Commands:
  demo           Run the offline single-session demonstration
  run            Execute one work with a real model and explicitly selected workspace tools
  session        Save and reopen sessions; see tarz session --help

Run options:
  --provider     openai-chat | openai-responses | anthropic (or configured profile)
  --model        Model identifier (or configured profile)
  --input        User text; mutually exclusive with --stdin
  --stdin        Read UTF-8 user text until EOF
  --api-key-env  Credential variable (default OPENAI_API_KEY / ANTHROPIC_API_KEY)
  --endpoint     Full HTTP endpoint override, including the path
  --timeout-secs Work deadline, 1–86400 seconds (default 120)
  --max-tokens   Output token limit per model call (no default; required by anthropic)
  --reasoning-effort  none | minimal | low | medium | high | xhigh (default: provider's)
  --workspace    Root directory for read-only tools; relative to current directory
  --tools        Comma-separated list_directory,read_file,search_text (default empty)
  --load         Import a Context material file into a new session
  --save         Replace a material file after completion or controlled interruption
  --json         Output a structured result instead of answer text
  --config       Shared TOML configuration file
  --profile      Named model preset (otherwise default_profile)
  --no-config    Ignore global configuration; conflicts with --config

Options:
  -h, --help     Show this help
  -V, --version  Show the version

The demo uses a scripted model and a local tool; it makes no network calls.
Run/session read --config, then TARZ_CONFIG, then the global config path.
Explicit options override configured fields. Saved sessions keep their definition.
Exit codes: 0 complete, 1 execution/configuration/file error, 2 arguments,
124 deadline, 130 cancelled. Ctrl-C cancels work and waits for returned materials.
";

pub(crate) fn run(args: impl Iterator<Item = OsString>) -> ExitCode {
    let args: Vec<_> = args.collect();
    match args.as_slice() {
        [] => output(HELP),
        [one] if one == "-h" || one == "--help" => output(HELP),
        [one] if one == "-V" || one == "--version" => {
            output(&format!("tarz {}\n", env!("CARGO_PKG_VERSION")))
        }
        [one, help] if one == "run" && (help == "--help" || help == "-h") => output(HELP),
        [one] if one == "session" => output(super::session::HELP),
        [one, help] if one == "session" && (help == "--help" || help == "-h") => {
            output(super::session::HELP)
        }
        [one, command, help]
            if one == "session"
                && matches!(
                    command.to_str(),
                    Some("create" | "run" | "list" | "export-materials")
                )
                && (help == "--help" || help == "-h") =>
        {
            output(super::session::HELP)
        }
        [one, rest @ ..] if one == "session" => match super::session::SessionCommand::parse(rest) {
            Ok(command) => super::session::run(command),
            Err(error) => {
                let _ = writeln!(
                    io::stderr().lock(),
                    "Tarz: {error}. Use tarz session --help."
                );
                ExitCode::from(2)
            }
        },
        [one, rest @ ..] if one == "run" => match super::options::RunOptions::parse(rest) {
            Ok(options) => super::execute::run(options),
            Err(error) => {
                let _ = writeln!(io::stderr().lock(), "Tarz: {error}. Use tarz run --help.");
                ExitCode::from(2)
            }
        },
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

pub(super) fn output(text: &str) -> ExitCode {
    match io::stdout().lock().write_all(text.as_bytes()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

pub(super) fn failure(error: impl std::fmt::Display) -> ExitCode {
    let _ = writeln!(io::stderr().lock(), "Tarz: {error}");
    ExitCode::FAILURE
}

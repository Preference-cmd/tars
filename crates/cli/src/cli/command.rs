use std::ffi::OsString;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

const HELP: &str = "TARS — agent application

Usage: tars [--help | --version]
       tars demo [--save <file>]
       tars run --provider <protocol> --model <id> (--input <text> | --stdin) [options]
       tars session <create | run | list | show | compact | history | repair | export-materials> [options]
       tars tools <status | install> [options]

Commands:
  demo           Run the offline single-session demonstration
  run            Execute one work with a real model and explicitly selected workspace tools
  session        Save and reopen sessions; see tars session --help
  tools          Check or install the external search tool; see tars tools --help

Run options:
  --provider     openai-chat | openai-responses | anthropic (or configured profile)
  --model        Model identifier (or configured profile)
  --input        User text; mutually exclusive with --stdin
  --stdin        Read UTF-8 user text until EOF
  --api-key-env  Credential variable (default OPENAI_API_KEY / ANTHROPIC_API_KEY)
  --endpoint     Full HTTP endpoint override, including the path
  --timeout-secs Work deadline, 1–86400 seconds (default 120)
  --max-tokens   Output token limit per model call (no default; required by anthropic)
  --reasoning-effort  Effort text sent as written, e.g. low, high (default: provider's)
  --extra-body   JSON object of further request fields, sent as written
  --render-options  JSON object of the protocol's render options, e.g. max_tokens_field
  --max-model-attempts  Model calls per round on transient or rate-limit failure (default 1)
  --context-window   Model context window in tokens; omitted leaves it unknown and
                 disables automatic compaction
  --auto-compact on | off | PERCENT: automatic compaction switch, or a trigger
                 percentage in 10–95 that also enables it (default on, 70)
  --workspace    Root directory for workspace tools; relative to current directory
  --tools        Comma-separated list_directory,read_file,search_text,write_file,edit_file,run_command (default empty)
  --command-network  Command network: none or open (default none)
  --mcp          Comma-separated ids from mcp.json (default: profile; empty disables)
  --permission-mode  read-only | ask | accept-edits | full-access (default: profile or ask)
                 Initial permission mode for transient work. In ask mode, asks for
                 confirmation when stdin and stderr are interactive terminals and input is
                 not from --stdin; otherwise rejects without blocking.
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
Run/session/tools read --config, then TARS_CONFIG, then the global config path.
Explicit options override configured fields. Saved sessions keep their definition.
Material export and --load import contain only the current Context, never the
replaced history segments.
Exit codes: 0 complete, 1 execution/configuration/file error, 2 arguments,
124 deadline, 130 cancelled. Ctrl-C cancels work and waits for returned materials.
";

pub(crate) fn run(args: impl Iterator<Item = OsString>) -> ExitCode {
    let args: Vec<_> = args.collect();
    match args.as_slice() {
        [] => output(HELP),
        [one] if one == "-h" || one == "--help" => output(HELP),
        [one] if one == "-V" || one == "--version" => {
            output(&format!("tars {}\n", env!("CARGO_PKG_VERSION")))
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
                    Some(
                        "create"
                            | "run"
                            | "list"
                            | "show"
                            | "compact"
                            | "history"
                            | "repair"
                            | "export-materials"
                    )
                )
                && (help == "--help" || help == "-h") =>
        {
            output(super::session::HELP)
        }
        [one, rest @ ..] if one == "session" => match super::session::SessionCommand::parse(rest) {
            Ok(command) => super::session::run(command),
            Err(error) => {
                let hint = if error.is_argument() {
                    ". Use tars session --help."
                } else {
                    ""
                };
                let _ = writeln!(io::stderr().lock(), "TARS: {}{hint}", error.message());
                error.exit_code()
            }
        },
        [one, rest @ ..] if one == "run" => match super::options::RunOptions::parse(rest) {
            Ok(options) => super::execute::run(options),
            Err(error) => {
                let hint = if error.is_argument() {
                    ". Use tars run --help."
                } else {
                    ""
                };
                let _ = writeln!(io::stderr().lock(), "TARS: {}{hint}", error.message());
                error.exit_code()
            }
        },
        [one] if one == "tools" => output(super::tools::HELP),
        [one, help] if one == "tools" && (help == "--help" || help == "-h") => {
            output(super::tools::HELP)
        }
        [one, rest @ ..] if one == "tools" => match super::tools::ToolsCommand::parse(rest) {
            Ok(command) => super::tools::run(command),
            Err(error) => {
                let hint = if error.is_argument() {
                    ". Use tars tools --help."
                } else {
                    ""
                };
                let _ = writeln!(io::stderr().lock(), "TARS: {}{hint}", error.message());
                error.exit_code()
            }
        },
        [one] if one == "demo" => demo(None),
        [one, option, path] if one == "demo" && option == "--save" && !path.is_empty() => {
            demo(Some(path.into()))
        }
        _ => {
            let _ = writeln!(
                io::stderr().lock(),
                "Unsupported arguments. Use tars --help."
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
    let _ = writeln!(io::stderr().lock(), "TARS: {error}");
    ExitCode::FAILURE
}

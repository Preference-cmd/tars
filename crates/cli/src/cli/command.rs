use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

const HELP: &str = "TARS — agent application

Usage: tars [interactive options]
       tars -p [prompt] [options]
       tars run --provider <protocol> --model <id> (--input <text> | --stdin | --image <path>) [options]
       tars session <create | run | list | show | compact | history | repair | rename | delete | fork | export-materials | export-markdown | import-legacy> [options]
       tars trust <list | revoke> [options]
       tars tools <status | install> [options]
       tars doctor [options]
       tars completions <bash | zsh | fish>

Interactive mode (default when stdin and stdout are terminals):
  --config FILE       Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config (requires --store)
  --profile NAME      Named model preset (otherwise default_profile)
  --store DIR         Session store directory (defaults to ~/.tars/data or $TARS_HOME/data)
  --recovery-dir DIR  Recovery export directory (default configured or current directory)
  --workspace DIR     Workspace root directory for the session
  -c, --continue      Continue the most recently updated saved session
  -r, --resume [SEL]  Resume a saved session by ID/prefix, or open session picker
  --code-execution    Enable sandboxed read-only run_code
  --no-code-execution Disable run_code even when enabled in configuration

Interactive slash commands:
  /doctor    Check configuration, credentials, storage, sandbox and tools
  /trust     Show folder trust and workspace .tars trust status
  /init      Analyze the workspace and create or update AGENTS.md
  /new       Create a session with model settings (F2)
  /clear     Start a fresh session with the current model preset and workspace
  /resume    Open the session picker or jump to a session by ID/prefix
  /model     Switch model profile in a new session
  /sessions  Find and open a saved or active session
  /agents    List session agents and switch transcript view
  /mode      Show or change the permission mode (Shift-Tab cycles)
  /copy      Copy last reply, fenced code block, or session ID
  /compact   Summarize and replace the current context
  /help      Keyboard shortcuts and session behavior

Commands:
  -p, --print    Run headlessly with positional prompt or piped stdin; see tars -p --help
  run            Execute one work with a real model and explicitly selected workspace tools; see tars run --help
  session        Save and reopen sessions; see tars session --help
  trust          List or revoke trusted workspace folders; see tars trust --help
  tools          Check or install the external search tool; see tars tools --help
  doctor         Run read-only environment and configuration diagnostics; see tars doctor --help
  completions    Generate static shell completion scripts; see tars completions --help

Run options:
  --provider     openai-chat | openai-responses | anthropic (or configured profile)
  --model        Model identifier (or configured profile)
  --input        User text; mutually exclusive with --stdin
  --stdin        Read UTF-8 user text until EOF
  --image        Attach a local PNG/JPEG/GIF/WebP image file (repeatable, max 8;
                 text may be omitted when at least one --image is provided)
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
  --prompt-cache on | off: prompt caching switch (default on)
  --code-execution     Enable sandboxed read-only run_code (conflicts with --no-code-execution)
  --no-code-execution  Disable run_code even when enabled in configuration
  --workspace    Root directory for workspace tools; relative to current directory
  --tools        Comma-separated list_directory,read_file,search_text,find_files,write_file,edit_file,run_command (default empty)
  --command-network  Command network: none or open (default none)
  --mcp          Comma-separated ids from mcp.json (default: profile; empty disables)
  --permission-mode  read-only | ask | accept-edits | full-access (default: profile or ask)
                 Initial permission mode for transient work. In ask mode, asks for
                 confirmation when stdin and stderr are interactive terminals and input is
                 not from --stdin; otherwise rejects without blocking.
  --system       Inline static custom system prompt (conflicts with --system-file)
  --system-file  UTF-8 static custom system prompt file, relative to cwd
  --dynamic-context       Inline dynamic custom context (conflicts with --dynamic-context-file)
  --dynamic-context-file  UTF-8 dynamic custom context file, relative to cwd
  --skill-dir    Additional skill directory, relative to cwd (repeatable; ordered after
                 configured dirs and before <workspace>/.tars/skills)
  --no-skills    Disable skills discovery, <available_skills> injection, and load_skill
                 (conflicts with --skill-dir)
  --load         Import a Context material file into a new session
  --save         Replace a material file after completion or controlled interruption
  --json         Output a structured result instead of answer text
  --stream       Stream incremental response text to stdout and status to stderr
  --stream-json  Stream NDJSON progress events and terminal result to stdout
  --output-format  text | json | stream-json (conflicts with --json/--stream/--stream-json)
  --config       Shared TOML configuration file
  --profile      Named model preset (otherwise default_profile)
  --no-config    Ignore global configuration; conflicts with --config

Options:
  -h, --help     Show this help
  -V, --version  Show the version

Run/session/tools read --config, then TARS_CONFIG, then the global config path.
Explicit options override configured fields. Saved sessions keep their definition.
Material export and --load import contain only the current Context, never the
replaced blocks kept in history records.
Exit codes: 0 complete, 1 execution/configuration/file error, 2 arguments,
124 deadline, 130 cancelled. Ctrl-C cancels work and waits for returned materials.
";

pub(crate) fn run(args: impl Iterator<Item = OsString>) -> ExitCode {
    let args: Vec<_> = args.collect();
    match args.as_slice() {
        [] => {
            if io::stdin().is_terminal() && io::stdout().is_terminal() {
                tars_tui::run(std::iter::empty::<OsString>())
            } else {
                output(HELP)
            }
        }
        [one] if one == "-h" || one == "--help" => output(HELP),
        [one] if one == "-V" || one == "--version" => {
            output(&format!("tars {}\n", env!("CARGO_PKG_VERSION")))
        }
        [one] if one == "run" => output(super::options::HELP),
        [one, help] if one == "run" && (help == "--help" || help == "-h") => {
            output(super::options::HELP)
        }
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
        [one] if one == "session" => output(super::session::HELP),
        [one, help] if one == "session" && (help == "--help" || help == "-h") => {
            output(super::session::HELP)
        }
        [one, command, help]
            if one == "session"
                && is_session_subcommand(command.to_str())
                && (help == "--help" || help == "-h") =>
        {
            output(super::session::subcommand_help(
                command.to_str().unwrap_or(""),
            ))
        }
        [one, command, _, help]
            if one == "session"
                && is_session_subcommand(command.to_str())
                && (help == "--help" || help == "-h") =>
        {
            output(super::session::subcommand_help(
                command.to_str().unwrap_or(""),
            ))
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
        [one] if one == "trust" => output(super::trust::HELP),
        [one, help] if one == "trust" && (help == "--help" || help == "-h") => {
            output(super::trust::HELP)
        }
        [one, command, rest @ ..]
            if one == "trust"
                && matches!(command.to_str(), Some("list" | "revoke"))
                && rest.iter().any(|arg| arg == "--help" || arg == "-h") =>
        {
            output(super::trust::subcommand_help(
                command.to_str().unwrap_or(""),
            ))
        }
        [one, rest @ ..] if one == "trust" => match super::trust::TrustCommand::parse(rest) {
            Ok(command) => super::trust::run(command),
            Err(error) => {
                let hint = if error.is_argument() {
                    ". Use tars trust --help."
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
        [one, command, help]
            if one == "tools"
                && matches!(command.to_str(), Some("status" | "install"))
                && (help == "--help" || help == "-h") =>
        {
            output(super::tools::subcommand_help(
                command.to_str().unwrap_or(""),
            ))
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
        [one, help] if one == "doctor" && (help == "--help" || help == "-h") => {
            output(super::doctor::HELP)
        }
        [one, rest @ ..] if one == "doctor" => match super::doctor::DoctorOptions::parse(rest) {
            Ok(options) => super::doctor::run(options),
            Err(error) => {
                let hint = if error.is_argument() {
                    ". Use tars doctor --help."
                } else {
                    ""
                };
                let _ = writeln!(io::stderr().lock(), "TARS: {}{hint}", error.message());
                error.exit_code()
            }
        },
        [one, help] if one == "completions" && (help == "--help" || help == "-h") => {
            output(super::completions::HELP)
        }
        [one, rest @ ..] if one == "completions" => {
            match super::completions::CompletionsCommand::parse(rest) {
                Ok(command) => super::completions::run(command),
                Err(error) => {
                    let hint = if error.is_argument() {
                        ". Use tars completions --help."
                    } else {
                        ""
                    };
                    let _ = writeln!(io::stderr().lock(), "TARS: {}{hint}", error.message());
                    error.exit_code()
                }
            }
        }
        _ if super::print::is_print_invocation(&args) => {
            match super::print::PrintCommand::parse(&args) {
                Ok(command) => super::print::run(command),
                Err(error) => {
                    let hint = if error.is_argument() {
                        ". Use tars -p --help."
                    } else {
                        ""
                    };
                    let _ = writeln!(io::stderr().lock(), "TARS: {}{hint}", error.message());
                    error.exit_code()
                }
            }
        }
        _ if is_interactive_invocation(&args) => tars_tui::run(args),
        _ => {
            let _ = writeln!(
                io::stderr().lock(),
                "Unsupported arguments. Use tars --help."
            );
            ExitCode::from(2)
        }
    }
}

/// Returns whether top-level `args` form an interactive TUI invocation.
///
/// Scans tokens before `--`, skipping values consumed by value-taking flags
/// (including an optional selector after `-r` / `--resume` when the next token
/// does not start with `-`). Standalone `-h`/`--help` and `-V`/`--version` are
/// handled earlier; here they are permitted only alongside interactive flags.
fn is_interactive_invocation(args: &[OsString]) -> bool {
    let mut saw_interactive_flag = false;
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        let Some(token) = arg.to_str() else {
            return false;
        };
        if token == "--" {
            break;
        }
        match token {
            "--config" | "--profile" | "--store" | "--recovery-dir" | "--workspace" => {
                saw_interactive_flag = true;
                if iter
                    .peek()
                    .and_then(|next| next.to_str())
                    .is_some_and(|next| !next.starts_with("--"))
                {
                    let _ = iter.next();
                }
            }
            "-r" | "--resume" => {
                saw_interactive_flag = true;
                if iter
                    .peek()
                    .and_then(|next| next.to_str())
                    .is_some_and(|next| !next.starts_with('-'))
                {
                    let _ = iter.next();
                }
            }
            "--no-config" | "--code-execution" | "--no-code-execution" | "-c" | "--continue" => {
                saw_interactive_flag = true;
            }
            "-h" | "--help" | "-V" | "--version" => {}
            _ => return false,
        }
    }
    saw_interactive_flag
}

fn is_session_subcommand(command: Option<&str>) -> bool {
    matches!(
        command,
        Some(
            "create"
                | "run"
                | "list"
                | "show"
                | "compact"
                | "history"
                | "repair"
                | "rename"
                | "delete"
                | "fork"
                | "export-materials"
                | "export-markdown"
                | "import-legacy"
        )
    )
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

use std::ffi::OsString;
use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;

const HELP: &str = "TARS — agent application

Usage: tars [interactive options]
       tars run --provider <protocol> --model <id> (--input <text> | --stdin | --image <path>) [options]
       tars session <create | run | list | show | compact | history | repair | rename | delete | fork | export-materials | import-legacy> [options]
       tars tools <status | install> [options]

Interactive mode (default when stdin and stdout are terminals):
  --config FILE       Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config (requires --store)
  --profile NAME      Named model preset (otherwise default_profile)
  --store DIR         Session store directory (defaults to ~/.tars/data or $TARS_HOME/data)
  --recovery-dir DIR  Recovery export directory (default configured or current directory)
  --code-execution    Enable sandboxed read-only run_code
  --no-code-execution Disable run_code even when enabled in configuration

Interactive slash commands:
  /doctor    Check configuration, credentials, storage, sandbox and tools
  /init      Analyze the workspace and create or update AGENTS.md
  /new       Create a session with model settings (F2)
  /sessions  Find and open a saved or active session
  /agents    List session agents and switch transcript view
  /mode      Show or change the permission mode (Shift-Tab cycles)
  /compact   Summarize and replace the current context
  /help      Keyboard shortcuts and session behavior

Commands:
  run            Execute one work with a real model and explicitly selected workspace tools
  session        Save and reopen sessions; see tars session --help
  tools          Check or install the external search tool; see tars tools --help

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
        [first, ..]
            if matches!(
                first.to_str(),
                Some(
                    "--config"
                        | "--no-config"
                        | "--profile"
                        | "--store"
                        | "--recovery-dir"
                        | "--code-execution"
                        | "--no-code-execution"
                )
            ) =>
        {
            tars_tui::run(args)
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
                            | "rename"
                            | "delete"
                            | "fork"
                            | "export-materials"
                            | "import-legacy"
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
        _ => {
            let _ = writeln!(
                io::stderr().lock(),
                "Unsupported arguments. Use tars --help."
            );
            ExitCode::from(2)
        }
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

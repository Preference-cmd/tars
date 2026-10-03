//! Persistent session command parsing and presentation.

mod execute;
mod options;

pub(super) use execute::run;
pub(super) use options::SessionCommand;

pub(super) const HELP: &str = "Persistent Tarz sessions

Usage: tarz session create --store <directory> --provider <protocol> --model <id> [options]
       tarz session run <conversation-id> --store <directory> (--input <text> | --stdin) [options]
       tarz session list --store <directory> [--json]

Create options:
  --provider          openai-chat | openai-responses | anthropic (required)
  --model             Explicit model identifier (required)
  --api-key-env       Credential variable, resolved when a session opens
  --endpoint          Full HTTP(S) URL without credentials, query or fragment
  --timeout-secs      Work deadline, 1–86400 seconds (default 120)
  --capacity          Retained work record count (default 256; not a byte limit)
  --max-model-rounds  Per-work model call limit (default 10)
  --max-tool-calls    Per-work tool call limit (default 64)
  --load              Initial Context material file; creates a new conversation

Run options:
  --input             User text; mutually exclusive with --stdin
  --stdin             Read UTF-8 user text until EOF
  --recovery-dir      Material recovery directory on save failure (default current directory)

Common options:
  --store             Explicit local store directory (required)
  --json              Structured output

Create saves the fixed definition and materials without calling a model.
Run reopens that definition, executes, saves, and closes the local instance.
Model and execution options cannot be overridden during run.
Saved records contain no request-key table, old work results or live execution.
On save failure, run exports returned Context to a new recovery file and exits 1.
Recovery export failure is reported separately; no successful recovery is implied.
";

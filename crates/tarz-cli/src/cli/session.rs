//! Persistent session command parsing and presentation.

mod execute;
mod options;
mod repair;

pub(super) use execute::run;
pub(super) use options::SessionCommand;

pub(super) const HELP: &str = "Persistent Tarz sessions

Usage: tarz session create --store <directory> --provider <protocol> --model <id> [options]
       tarz session run <conversation-id> --store <directory> (--input <text> | --stdin) [options]
       tarz session list --store <directory> [--json]
       tarz session repair <conversation-id> --store <directory> [--dry-run | --apply --expected-revision N] [--json]
       tarz session export-materials --record <file> --output <new-file>

Create options:
  --provider          openai-chat | openai-responses | anthropic (or profile)
  --model             Model identifier (or profile)
  --profile           Named model preset (otherwise default_profile)
  --api-key-env       Credential variable, resolved when a session opens
  --endpoint          Full HTTP(S) URL without credentials, query or fragment
  --timeout-secs      Work deadline, 1–86400 seconds (default 120)
  --capacity          Retained work record count (default 256; not a byte limit)
  --max-model-rounds  Per-work model call limit (default 10)
  --max-tool-calls    Per-work tool call limit (default 64)
  --workspace         Root directory for selected read-only tools
  --tools             Comma-separated list_directory,read_file,search_text (default empty)
  --load              Initial Context material file; creates a new conversation

Run options:
  --input             User text; mutually exclusive with --stdin
  --stdin             Read UTF-8 user text until EOF
  --recovery-dir      Material recovery directory on save failure (default current directory)

Common options:
  --store             Local store directory (or configured store)
  --config            Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config
  --json              Structured output

Repair options:
  --dry-run           Preview removal of unpaired declarations (default)
  --apply             Apply the current candidate, then wait for saving
  --expected-revision Saved revision reported by preview; required with --apply
  --recovery-dir      Material export directory if saving the repair fails

Repair never calls a model or executes tools. It opens the saved definition,
so its configured credential environment and workspace must remain available.
It cannot recover a previous process's uncommitted results. Removing declarations
from future requests does not establish whether their operations occurred.
Repair does not send a new input; use session run explicitly after saving.

export-materials extracts validated Context from a v1/v2 session record offline.
It neither loads configuration nor opens a session; an existing output is refused.
Create saves the fixed definition and materials without calling a model.
Run reopens that definition, executes, saves, and closes the local instance.
Model and execution options cannot be overridden during run.
Saved records contain no request-key table, old work results or live execution.
On save failure, run exports returned Context to a new recovery file and exits 1.
Recovery export failure is reported separately; no successful recovery is implied.
";

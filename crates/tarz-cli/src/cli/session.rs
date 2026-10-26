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
  --max-tokens        Output token limit per model call (no default; required by anthropic)
  --reasoning-effort  Effort text sent as written, e.g. low, high (default: provider's)
  --extra-body   JSON object of further request fields, sent as written
  --render-options    JSON object of the protocol's render options, e.g. max_tokens_field
  --max-model-attempts  Model calls per round on transient or rate-limit failure (default 1)
  --workspace         Root directory for selected workspace tools
  --tools             Comma-separated list_directory,read_file,search_text,write_file,edit_file (default empty)
  --mcp               Comma-separated MCP server ids from mcp.json (default: profile; empty disables)
  --permission-mode   read-only | ask | accept-edits | full-access (default: profile or ask)
                      Initial permission mode saved in the definition. In ask mode, asks
                      when stdin and stderr are terminals and input is not from --stdin;
                      otherwise rejects without blocking.
  --load              Initial Context material file; creates a new conversation
  --recovery-dir      Material recovery directory on initial save failure
                      (default: configured recovery_dir, else current directory)

Run options:
  --input             User text; mutually exclusive with --stdin
  --stdin             Read UTF-8 user text until EOF
  --permission-mode   read-only | ask | accept-edits | full-access
                      Change and save the current permission mode for this session.
                      Reopening reverts full-access to ask unless created as full-access.
  --recovery-dir      Material recovery directory on save failure
                      (default: configured recovery_dir, else current directory)

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
                      (default: configured recovery_dir, else current directory)

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

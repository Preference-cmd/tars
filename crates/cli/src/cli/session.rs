//! Persistent session command parsing and presentation.

mod compaction;
mod execute;
mod options;
mod repair;
mod show;

pub(super) use execute::{run, run_saved_with_format};
pub(super) use options::SessionCommand;

pub(super) fn subcommand_help(command: &str) -> &'static str {
    match command {
        "create" => CREATE_HELP,
        "run" => RUN_HELP,
        "list" => LIST_HELP,
        "show" => SHOW_HELP,
        "compact" => COMPACT_HELP,
        "history" => HISTORY_HELP,
        "repair" => REPAIR_HELP,
        "rename" => RENAME_HELP,
        "delete" => DELETE_HELP,
        "fork" => FORK_HELP,
        "export-materials" => EXPORT_MATERIALS_HELP,
        "export-markdown" => EXPORT_MARKDOWN_HELP,
        "import-legacy" => IMPORT_LEGACY_HELP,
        _ => HELP,
    }
}

const CREATE_HELP: &str = "Create a persistent TARS session without calling a model

Usage: tars session create --store <directory> --provider <protocol> --model <id> [options]

Options:
  --provider          openai-chat | openai-responses | anthropic (or profile)
  --model             Model identifier (or profile)
  --profile           Named model preset (otherwise default_profile)
  --api-key-env       Credential variable, resolved when a session opens
  --endpoint          Full HTTP(S) URL without credentials, query or fragment
  --timeout-secs      Work deadline, 1–86400 seconds (default 120)
  --capacity          Retained work record count (default 256)
  --max-model-rounds  Per-work model call limit (default 10)
  --max-tool-calls    Per-work tool call limit (default 64)
  --max-tokens        Output token limit per model call (no default; required by anthropic)
  --reasoning-effort  Effort text sent as written, e.g. low, high (default: provider's)
  --extra-body        JSON object of further request fields, sent as written
  --render-options    JSON object of the protocol's render options, e.g. max_tokens_field
  --max-model-attempts  Model calls per round on transient or rate-limit failure (default 1)
  --context-window    Model context window in tokens; omitted leaves it unknown
  --auto-compact      on | off | PERCENT (10–95, default on, 70)
  --prompt-cache      on | off: prompt caching switch (default on)
  --code-execution    Enable sandboxed read-only run_code (conflicts with --no-code-execution)
  --no-code-execution Disable run_code even when enabled in configuration
  --workspace         Root directory for selected workspace tools
  --tools             Comma-separated list_directory,read_file,search_text,find_files,write_file,edit_file,run_command (default empty)
  --command-network   Command network: none or open (default none)
  --mcp               Comma-separated MCP server ids from mcp.json (default: profile; empty disables)
  --permission-mode   read-only | ask | accept-edits | full-access (default: profile or ask)
  --system            Inline static custom system prompt (conflicts with --system-file)
  --system-file       UTF-8 static custom system prompt file, relative to cwd
  --dynamic-context       Inline dynamic custom context (conflicts with --dynamic-context-file)
  --dynamic-context-file  UTF-8 dynamic custom context file, relative to cwd
  --skill-dir         Additional skill directory, relative to cwd (repeatable)
  --no-skills         Disable skills discovery (conflicts with --skill-dir)
  --load              Initial Context material file; creates a new conversation
  --trust-workspace-rules   Trust allow rules in <workspace>/.tars/permissions.toml
  --trust-workspace-agents  Trust presets in <workspace>/.tars/agents/
  --recovery-dir      Material recovery directory on initial save failure
  --store             Local store directory (or configured store)
  --config            Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config
  --json              Structured output
  -h, --help          Show this help
";

const RUN_HELP: &str = "Run one turn on a saved persistent session

Usage: tars session run <conversation-id> --store <directory> (--input <text> | --stdin | --image <path>) [options]

Options:
  --input             User text; mutually exclusive with --stdin
  --stdin             Read UTF-8 user text until EOF
  --image             Attach a local PNG/JPEG/GIF/WebP image file (repeatable, max 8)
  --permission-mode   read-only | ask | accept-edits | full-access
  --trust-workspace-rules   Trust allow rules in <workspace>/.tars/permissions.toml
  --trust-workspace-agents  Trust presets in <workspace>/.tars/agents/
  --recovery-dir      Material recovery directory on save failure
  --store             Local store directory (or configured store)
  --config            Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config
  --json              Structured JSON result object
  --stream            Stream incremental response text to stdout and status to stderr
  --stream-json       Stream NDJSON progress events and terminal result to stdout
  --output-format     text | json | stream-json (conflicts with --json/--stream/--stream-json)
  -h, --help          Show this help
";

const LIST_HELP: &str = "List sessions in a store

Usage: tars session list --store <directory> [--config <file> | --no-config] [--json]

Options:
  --store             Local store directory (or configured store)
  --config            Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config
  --json              Structured output
  -h, --help          Show this help
";

const SHOW_HELP: &str = "Inspect a saved session or one of its agents

Usage: tars session show <conversation-id> --store <directory> [--agent <id-or-name>] [--json]

Options:
  --agent             Inspect one agent by UUID agent_id or unique name
  --store             Local store directory (or configured store)
  --config            Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config
  --json              Structured output
  -h, --help          Show this help
";

const COMPACT_HELP: &str = "Summarize and replace the current context of a saved session

Usage: tars session compact <conversation-id> --store <directory> [--recovery-dir <dir>] [--json]

Options:
  --recovery-dir      Material recovery directory if saving fails
  --store             Local store directory (or configured store)
  --config            Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config
  --json              Structured output
  -h, --help          Show this help
";

const HISTORY_HELP: &str = "Print the expanded full conversation history of a saved session

Usage: tars session history <conversation-id> --store <directory> [--agent <id-or-name>] [--last N] [--json]

Options:
  --agent             Expand history for one agent by UUID agent_id or unique name
  --last N            Print only the newest N blocks
  --store             Local store directory (or configured store)
  --config            Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config
  --json              Print one JSON block per line
  -h, --help          Show this help
";

const REPAIR_HELP: &str = "Inspect or remove unpaired tool declarations from a saved session

Usage: tars session repair <conversation-id> --store <directory> [--dry-run | --apply --expected-revision N] [--json]

Options:
  --dry-run           Preview removal of unpaired declarations (default)
  --apply             Apply the current candidate, then wait for saving
  --expected-revision Saved revision reported by preview; required with --apply
  --recovery-dir      Material export directory if saving the repair fails
  --store             Local store directory (or configured store)
  --config            Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config
  --json              Structured output
  -h, --help          Show this help
";

const RENAME_HELP: &str = "Update or clear the title of an unlocked session

Usage: tars session rename <conversation-id> --store <directory> (--title <text> | --clear) [--json]

Options:
  --title             New session title (1–160 characters, no control characters)
  --clear             Clear the saved session title (conflicts with --title)
  --store             Local store directory (or configured store)
  --config            Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config
  --json              Structured output
  -h, --help          Show this help
";

const DELETE_HELP: &str = "Permanently remove an unlocked session from the store

Usage: tars session delete <conversation-id> --store <directory> [--json]

Options:
  --store             Local store directory (or configured store)
  --config            Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config
  --json              Structured output
  -h, --help          Show this help
";

const FORK_HELP: &str = "Fork a session into an isolated copy at revision 1

Usage: tars session fork <conversation-id> --store <directory> [--title <text>] [--json]

Options:
  --title             Optional title for the forked session (default: \"<source> (fork)\")
  --store             Local store directory (or configured store)
  --config            Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config
  --json              Structured output
  -h, --help          Show this help
";

const EXPORT_MATERIALS_HELP: &str = "Export validated Context materials from a session or legacy record

Usage: tars session export-materials <conversation-id> --store <directory> (--output <new-file> | <new-file>) [--json]
       tars session export-materials --record <file> --output <new-file> [--json]

Options:
  --output            New destination file path (existing files are refused)
  --record            Offline legacy v1/v2/v2.4 record file
  --store             Local store directory (or configured store)
  --config            Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config
  --json              Structured output
  -h, --help          Show this help
";

const EXPORT_MARKDOWN_HELP: &str = "Export a session's expanded conversation as GitHub-Flavored Markdown

Usage: tars session export-markdown <conversation-id> --store <directory> [--agent <id-or-name>] [--last N] [--output <new-file>] [--json]

Options:
  --agent             Export history for one agent by UUID agent_id or unique name
  --last N            Export only the newest N blocks
  --output            Write Markdown to a new destination file (existing files are refused)
  --store             Local store directory (or configured store)
  --config            Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config
  --json              Structured output
  -h, --help          Show this help
";

const IMPORT_LEGACY_HELP: &str = "Import a validated tars-session-v2.4 JSON record into SQLite

Usage: tars session import-legacy --record <file> --store <directory> [--json]

Options:
  --record            Source tars-session-v2.4 JSON record file
  --store             Local store directory (or configured store)
  --config            Shared TOML configuration file
  --no-config         Ignore global configuration; conflicts with --config
  --json              Structured output
  -h, --help          Show this help
";

pub(super) const HELP: &str = "Persistent TARS sessions

Usage: tars session create --store <directory> --provider <protocol> --model <id> [options]
       tars session run <conversation-id> --store <directory> (--input <text> | --stdin | --image <path>) [options]
       tars session list --store <directory> [--json]
       tars session show <conversation-id> --store <directory> [--agent <id-or-name>] [--json]
       tars session compact <conversation-id> --store <directory> [--json]
       tars session history <conversation-id> --store <directory> [--agent <id-or-name>] [--last N] [--json]
       tars session repair <conversation-id> --store <directory> [--dry-run | --apply --expected-revision N] [--json]
       tars session rename <conversation-id> --store <directory> (--title <text> | --clear) [--json]
       tars session delete <conversation-id> --store <directory> [--json]
       tars session fork <conversation-id> --store <directory> [--title <text>] [--json]
       tars session export-materials <conversation-id> --store <directory> (--output <new-file> | <new-file>) [--json]
       tars session export-materials --record <file> --output <new-file> [--json]
       tars session export-markdown <conversation-id> --store <directory> [--agent <id-or-name>] [--last N] [--output <new-file>] [--json]
       tars session import-legacy --record <file> --store <directory> [--json]

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
  --context-window    Model context window in tokens; omitted leaves it unknown
  --auto-compact      on | off | PERCENT: automatic compaction switch, or a trigger
                      percentage in 10–95 that also enables it (default on, 70)
  --prompt-cache      on | off: prompt caching switch (default on)
  --code-execution    Enable sandboxed read-only run_code (conflicts with --no-code-execution)
  --no-code-execution Disable run_code even when enabled in configuration
  --workspace         Root directory for selected workspace tools
  --tools             Comma-separated list_directory,read_file,search_text,find_files,write_file,edit_file,run_command (default empty)
  --command-network   Command network: none or open (default none)
  --mcp               Comma-separated MCP server ids from mcp.json (default: profile; empty disables)
  --permission-mode   read-only | ask | accept-edits | full-access (default: profile or ask)
                      Initial permission mode saved in the definition. In ask mode, asks
                      when stdin and stderr are terminals and input is not from --stdin;
                      otherwise rejects without blocking.
  --system            Inline static custom system prompt (conflicts with --system-file)
  --system-file       UTF-8 static custom system prompt file, relative to cwd
  --dynamic-context       Inline dynamic custom context (conflicts with --dynamic-context-file)
  --dynamic-context-file  UTF-8 dynamic custom context file, relative to cwd
  --skill-dir         Additional skill directory, relative to cwd (repeatable; ordered after
                      configured dirs and before <workspace>/.tars/skills)
  --no-skills         Disable skills discovery, <available_skills> injection, and load_skill
                      (conflicts with --skill-dir)
  --load              Initial Context material file; creates a new conversation
  --trust-workspace-rules
                      Trust and activate allow rules in <workspace>/.tars/permissions.toml
  --trust-workspace-agents
                      Trust and activate workspace agent presets in <workspace>/.tars/agents/
  --recovery-dir      Material recovery directory on initial save failure
                      (default: configured recovery_dir, else current directory)

Run options:
  --input             User text; mutually exclusive with --stdin
  --stdin             Read UTF-8 user text until EOF
  --image             Attach a local PNG/JPEG/GIF/WebP image file (repeatable, max 8;
                      text may be omitted when at least one --image is provided)
  --stream            Stream incremental response text to stdout and status to stderr
  --stream-json       Stream NDJSON progress events and terminal result to stdout
  --output-format     text | json | stream-json (conflicts with --json/--stream/--stream-json)
  --permission-mode   read-only | ask | accept-edits | full-access
                      Change and save the current permission mode for this session.
                      Reopening reverts full-access to ask unless created as full-access.
  --trust-workspace-rules
                      Trust and activate allow rules in <workspace>/.tars/permissions.toml
  --trust-workspace-agents
                      Trust and activate workspace agent presets in <workspace>/.tars/agents/
  --recovery-dir      Material recovery directory on save failure
                      (default: configured recovery_dir, else current directory)

Show options:
  --agent             Inspect one agent by UUID agent_id or unique name

Compact options:
  --recovery-dir      Material recovery directory if saving the compacted
                      materials fails (default: configured recovery_dir, else
                      current directory)

History options:
  --agent             Expand history for one agent by UUID agent_id or unique name
  --last N            Print only the newest N blocks

Export-markdown options:
  --agent             Export history for one agent by UUID agent_id or unique name
  --last N            Export only the newest N blocks
  --output            Write Markdown to a new file (existing files are refused)

Rename options:
  --title             New session title (1–160 characters, no control characters)
  --clear             Clear the saved session title (conflicts with --title)

Fork options:
  --title             Optional title for the forked session (default: \"<source> (fork)\")

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

Show reports the saved revision, block count, context usage, history record
count and retained-work capacity (`Works: 3 / 256`) without changing the record
or calling a model.
Compact replaces the current Context with a brief written by the session's own
model and saves it; the replaced conversation is kept as a read-only history
record. A busy session is refused, and materials with unpaired declarations
must be repaired first.
History prints the expanded full conversation with block status; --last N
prints only the newest N blocks; --json prints one block per line.
Rename updates or clears the saved title of an unlocked session.
Delete permanently removes an unlocked session and its owned rows while preserving workspace trust.
Fork creates a physically isolated copy of a session's entry materials, history, and artifacts at revision 1.
Material export and --load import contain only the current Context, never the
history records.

export-materials exports validated Context (and sidecar .media/ blobs when present)
from a SQLite session, or extracts Context offline from a legacy v1/v2/v2.4 record.
An existing output file is always refused.
export-markdown renders the expanded conversation (skipping internal prelude
blocks by default) to stdout or a new Markdown file (existing files are refused).
import-legacy imports a validated tars-session-v2.4 JSON record (and any
referenced .history/.artifacts files) into SQLite without modifying the source.
Create saves the fixed definition and materials without calling a model.
Run reopens that definition, executes, saves, and closes the local instance.
Model and execution options cannot be overridden during run.
Saved records contain no request-key table, old work results or live execution.
On save failure, run exports returned Context to a new recovery file and exits 1.
Recovery export failure is reported separately; no successful recovery is implied.
";

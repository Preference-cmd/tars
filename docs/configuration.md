# Shared TOML configuration

CLI and TUI share `tars-core::config`. The configuration is read at process start as immutable defaults for new sessions; reopening a saved session still uses the saved `SessionDefinition`. Editing the TOML never hot-reloads active sessions and never rewrites existing records.

Configuration file selection order:

1. `--config FILE`.
2. The `TARS_CONFIG` environment variable.
3. Unix/macOS: the absolute path `$XDG_CONFIG_HOME/tars/config.toml`, or `~/.config/tars/config.toml` when that variable is unset; Windows: `%APPDATA%/TARS/config.toml`.

The current project's configuration file is never searched for automatically. A missing default path means an empty configuration; a missing file for an explicit path or `TARS_CONFIG`, or invalid configuration, refuses to start. `--no-config` ignores file and environment selection and is mutually exclusive with `--config`. Reading, parsing, validating the configuration file and resolving presets fail with exit code 1; argument errors such as unknown arguments or missing argument values return 2. Help and the offline demo do not read the configuration.

See the root [config.example.toml](../config.example.toml):

```toml
version = 1
store = "data"
recovery_dir = "."
default_profile = "work"

# [external_tools]
# rg = "managed"               # managed | system | an absolute path

[profiles.work]
provider = "openai-chat"
model = "your-model-id"
api_key_env = "OPENAI_API_KEY"
# endpoint = "http://127.0.0.1:8080/v1/chat/completions"
timeout_secs = 120
capacity = 256
max_model_rounds = 10
max_tool_calls = 64
# max_tokens = 8192
# reasoning_effort = "medium"
# max_model_attempts = 3
# permission_mode = "ask"      # read-only | ask | accept-edits | full-access
# context_window = 200000      # model window in tokens; omitted means unknown
# auto_compact = true          # automatic compaction switch; default true
# compact_threshold = 70       # trigger percentage of the window, 10..95
# prompt_cache = true          # prompt cache switch; default true
# mcp = ["git"]                # ids defined in mcp.json; omitted or empty selects none
# [profiles.work.extra_body.thinking]
# type = "adaptive"
# [profiles.work.render_options]
# max_tokens_field = "max_tokens"
# [profiles.work.write_limits]
# max_edit_file_bytes = 1048576
# max_write_bytes = 524288
# max_edit_segments = 16
```

`version` must be 1; `store`, `recovery_dir`, `default_profile` and `profiles` may be omitted. Every model preset must contain a provider and a non-empty model. The provider is `openai-chat`, `openai-responses` or `anthropic`. When the remaining fields are omitted the table defaults apply: deadline 120 seconds, 256 retained works, at most 10 model calls and 64 tool declarations per work; `api_key_env` defaults to OPENAI_API_KEY or ANTHROPIC_API_KEY depending on the protocol. `max_tokens` (the output token limit for each model call) and `reasoning_effort` have no defaults and are not sent when omitted; the `anthropic` protocol requires `max_tokens`, and a request is rejected before sending when it is not configured. The corresponding command-line arguments are `--max-tokens` and `--reasoning-effort`. `reasoning_effort` is text that is written as-is into that protocol's strength field in the request; TARS keeps no list of accepted values because they vary by model and service (common ones are `low`, `medium` and `high`, and some services use `off` to disable reasoning), and an error returned when the service does not accept the value is shown as-is. `extra_body` is a table merged as-is into the request body for settings that exist only in a particular protocol or model, such as the `anthropic` `thinking` object. When a key collides with one TARS writes itself, if both sides are tables they are merged key by key; for example, `reasoning = { summary = "auto" }` on the `openai-responses` protocol coexists with the strength written by `reasoning_effort`; otherwise the value TARS writes wins (such as `model`, messages and tools), and the table's values are ignored without an error. The command-line argument is `--extra-body`, whose value is a JSON object that wholly replaces the preset's table when given. On the `anthropic` protocol, setting only `reasoning_effort` does not automatically add a `thinking` object; write it in `extra_body` when needed. `max_model_attempts` is the total number of attempts (including the first) allowed for each model round on transient failures or rate limits; omitted or 1 means no retry, and the command-line argument is `--max-model-attempts`. Retries happen only before any output is visible, and the wait never exceeds the work deadline. Saved session records that lack these fields read them as unconfigured.

`permission_mode` is the initial permission mode for a new session and defaults to `ask`; the choices are `read-only`, `ask`, `accept-edits` and `full-access`. The command-line counterpart is `--permission-mode`, and the TUI new-session form also has the field. Runtime switching, the meaning of the four modes and the saving rules are in [Workspace tools](workspace-tools.md#confirmation-and-permission-modes).

`prompt_cache` is a boolean setting that controls whether the session requests prefix prompt caching from supported providers (e.g., Anthropic ephemeral cache control breakpoints). It defaults to `true`. The command-line counterpart is `--prompt-cache on|off` on `run` and `session create` (explicit command-line option overrides profile setting, which overrides the default `true`). The TUI new-session form also provides a visible "Prompt cache (on | off)" field. `prompt_cache` is frozen in the `SessionDefinition` at session creation and cannot be changed at runtime; reopening a session always restores the saved value. When `prompt_cache` is enabled, turn runs and compaction brief requests ask for `CacheDirective::StablePrefix`: Anthropic requests gain `cache_control` breakpoints, while OpenAI Chat and Responses requests stay byte-for-byte the same, since those servers cache on their own. When disabled, no cache directive is set and every request is as it was without the switch.

`write_limits` may only be lowered within the supported limits (defaults are in [Workspace tools](workspace-tools.md#limits)); the fields are `max_edit_file_bytes`, `max_write_bytes` and `max_edit_segments`. There are no command-line arguments for them, and omitted fields use the defaults. Neither `tool_limits` nor `write_limits` can raise a hard limit.

`render_options` is a table that sets the selected protocol's rendering options in Causa. For example, `openai-chat` writes the output limit as `max_completion_tokens` by default; when a service only accepts `max_tokens`, write `max_tokens_field = "max_tokens"` and the request then contains only `max_tokens`. The available keys and values follow the rendering option type of that protocol in the Causa version in use (`ChatRenderOptions`, `ResponsesRenderOptions`, `AnthropicRenderOptions`); TARS lists no separate catalog and does not choose per service. Omitted keys use Causa's defaults. Unknown keys, invalid values and keys that do not belong to the selected protocol fail when the configuration is loaded or the session is created, without sending a request, and the error message does not show the table contents. So when switching protocols with `--provider`, the preset's table must also be valid for the new protocol. The command-line argument is `--render-options`, whose value is a JSON object that wholly replaces the preset's table when given. A session record saves the keys and values that were written, and reads this as an empty table when absent.

A transient `tars run` retains only its single work, with a fixed capacity of 1.

All presets are validated at load time; unknown fields, duplicate keys, invalid types, invalid presets, empty paths and unsupported versions all fail. A file is at most 1 MiB. Parse diagnostics show only line and column and never echo TOML values; a plaintext `api_key` field is not accepted. The actual credential is read from the named environment variable when the session is opened.

Paths do not expand `~` or `$VAR`. Relative `store` and `recovery_dir` are based on the directory containing the TOML file; relative command-line paths are based on the current working directory. The store is created according to the existing Store rules, while recovery_dir must already exist. Without a configured store, the persistent CLI and the TUI both require an explicit `--store`.

```sh
tars run --config ./config.toml --profile work --input 'Analyze the current problem'
tars session create --config ./config.toml --model another-model --json
tars session list --config ./config.toml --json
tars-tui --config ./config.toml --profile work
```

Selecting `--profile NAME` takes precedence over default_profile. Arguments override the selected preset field by field; endpoint, credential reference, deadline and limits that are not overridden still come from the preset. CLI model arguments apply to `run` and `session create`; `session run` accepts neither `--profile` nor model overrides. `--store` and `--recovery-dir` override the corresponding paths in the configuration.

The TUI new-session form is filled from the selected preset, and changes affect only the new session at hand; capacity/call limits, generation settings and `render_options` that the preset does not display are preserved as-is. In the first version the user edits the configuration file, and the TUI neither saves nor overwrites the global configuration automatically. A future GUI should reuse the same loading and parsing module.

## Long-session compaction

A Context only grows, so a long conversation eventually exceeds the model's context window. TARS may replace an older part of the Context with a working brief written by the session's own model and keep the newest material verbatim; the replaced conversation is kept as read-only history rewrite records and is never sent to the model again. The policy belongs to TARS; Causa supplies only the per-round `ContextPreparer` contract.

Three preset fields control it, and the `create`/`run` arguments override them for one new session:

| Setting | Meaning | Default |
| --- | --- | --- |
| `context_window` | Model context window in tokens | unset, so the window is unknown |
| `auto_compact` | Whether the session compacts automatically | `true` |
| `compact_threshold` | Trigger percentage of the window, 10 to 95 | `70` |

`context_window` is frozen in the definition with the model. `auto_compact` and `compact_threshold` start from the definition and the running values are stored in the session record only when they differ from it, following the permission mode. The command-line arguments are `--context-window N` and `--auto-compact on|off|PERCENT`; a percentage sets the threshold and enables automatic compaction. The TUI `/auto_compact` command reads or changes the running values without touching the shared TOML. Because running values are part of the record, the next save carries them and reopening restores them.

When `context_window` is unknown, automatic compaction never triggers (older records have no window); manual `tars session compact` and the TUI `/compact` still work. The request size is estimated from the previous upstream prompt total (uncached input plus cache read and write as reported) plus one token per three bytes of material added since that request; a session without upstream usage estimates from bytes alone. A round whose response reports no usage keeps the previous pair, so its material is still counted from bytes, and the summary request's own usage is never recorded.

Automatic compaction runs before a model round when the estimate reaches the threshold percentage of the window, or when the estimate plus `max_tokens` no longer fits the window. At most one automatic attempt happens per round; when it is abandoned (empty brief, no smaller estimate, request failure, unwritable history, a fixed prefix too large to help) the Context is unchanged and the round still goes out. A round that ends on the provider's context-window limit forces one compaction attempt on the next round even when automatic compaction is off. Manual compaction is refused while the session is busy, while a save is blocked, and while interrupted tool material still needs repair.

A summary never touches the fixed prefix: the leading run of system text blocks (with any tool declarations inside that run), which every protocol renders as the system prompt, and the tool state the request declares. Briefs of earlier summaries and the tool declarations between them stay as they are too; with the fixed prefix they form the head. A summary replaces the interval from the end of the head to a cut point `k` and keeps everything from `k` on verbatim as the retained tail. A cut point is a user message or the block after a completed tool round, so a call never loses its result and the parallel calls of one round stay together. `k` is the earliest cut point after the head whose tail fits the retained-tail budget; when none fits, the tail is empty. The budget is a quarter of what the trigger line (`context_window` × `compact_threshold`%) leaves after the fixed prefix, at most 20,000 tokens, and 20,000 tokens when the window is unknown. The fixed prefix is estimated as the leading system blocks plus one declaration of every current tool, since Anthropic sends the whole tool state with each request. When the fixed prefix alone is over half the trigger line no brief can help: automatic compaction reports `prefix_too_large` once per session and does not try again in later rounds, and manual compaction refuses with that reason each time.

The blocks a work submitted are protected while it runs, from its first round on, so a large request is never summarized away while it is being answered. Protected blocks inside the interval keep their ids, content and order; the interval is summarized around them in one rewrite and one record. At the start of the interval the summary inserts a tool declaration of the changes the interval made (omitted when it made none), so the request keeps offering the same tools, and one user message with a fixed prefix, `meta.source = "tars:compaction"`, holding the model's brief. The brief ends with one line naming its own rewrite record: `Original messages: recall id=<record id>`. The model therefore sees the brief, the request it is still answering and the newest material, and can read back the original messages with `recall`. A later summary adds its brief after the earlier ones; briefs are not merged. An interval that holds nothing but protected blocks and tool declarations is abandoned as `no_shrink` before any model call, and so is an edit that would not make the Context smaller.

The brief request is the normal request up to `k`, unchanged, followed by one instruction, so the provider can serve most of it from the prompt cache of the previous round. The instruction asks for a brief of the conversation after the earlier briefs, says that the newest messages stay verbatim and, when the request in progress is protected, asks the model not to copy it and to end with what of it is already done. When the material up to `k` is over 70% of the window, the earliest interval blocks are left out of the request: the kept part starts at a cut point (at a user message when the head has none), is preceded by a declaration of the tool changes the left-out part made, and still carries the head and the protected blocks. The history record holds all original blocks either way.

All Context rewrites go through a shared commit path: the operation writes the rewrite record to `<store>/sessions/<conversation id>.history/<record id>.json` in the `tars-history-v1.1` format and fsyncs it, applies all range replacements in a single `context.edit() ... commit()`, and updates the in-memory index; an edit failure removes the file. Ranges are ascending and non-overlapping, and each inserts at least one block, so expansion can put its originals back where that block stands. A protected block is put back as the last inserted block of the range that starts at it, with its own id, so it is both an original and an inserted block of that range; the removed block count leaves it out, and expansion shows it once, with the status of the place it stands. The record stores format, conversation id, record id, created time, trigger, `kind` (`summarize` | `elide` | `merge`), estimates before and after, and ordered `replacements` containing each range's original `ContextBlock`s and inserted block ids. The session record's `history` field lists the entries in order with record id, created time, trigger, kind, removed block count, and inserted block ids. Files the record does not reference are removed under the write lock when the session is opened, and a referenced file that is missing or unreadable is reported when read during conversation expansion. Tool results in the Context may reference original blocks of this session's rewrite records using artifact identifiers of the form `<record id>/<block id>`; such references are checked against the session's history index on open (reading zero record files) and verified against the rewrite record file on save, while references to records of other sessions are refused. When a tool result is shortened by compaction, it carries an artifact reference and a model-facing note with the block recall id: `Shortened by compaction; full original: recall id=<record id>/<block id>`. Material export (`tars session export-materials`), recovery exports and TUI export drop references to this session's history records and add a note stating that the full original stays in the original session and cannot be read back here; the exported file contains self-contained Context and loads into a new session with `--load`. A transient `tars run` has no store, keeps no history and therefore never compacts.

The running compaction values and the history index are part of the `tars-session-v2.2` record. Compaction does not reset the session's saved usage totals; see [Session lifecycle](session-lifecycle.md).

### Compaction recall tool

Persistent sessions automatically declare the built-in `recall` tool on every turn, regardless of whether workspace tools are configured or `--tools` is specified. Transient `tars run` invocations do not declare `recall`. Reopening an older session whose stored Context lacked `recall` declares it on the next turn, with Causa recording the `ToolChange`.

`recall` is classified as `KnownReadOnly` and is not sandboxed (it reads the session's internal history records, not the workspace). It executes without confirmation in all four permission modes (including `read-only` and `ask`).

The tool provides three calling forms with a maximum output cap of 64 KiB (truncated in the middle on char boundaries with an omission notice if exceeded):
- **List blocks in a record** (`id = "<record id>"`): Lists the original blocks in order with block index, role/kind label, ref `<record id>/<block id>`, character count, and a 200-character preview with collapsed whitespace. Paged by block offset and limit (`offset?: integer >= 0`, `limit?: integer >= 1`, default limit 100, maximum 200). If more blocks remain, ends with `more: recall id=<record id> offset=<next>`.
- **Read an original block** (`id = "<record id>/<block id>"`): Returns the exact full text (`block_to_text`) of that original block. Paged by Unicode scalar values (`offset?: integer >= 0`, `limit?: integer >= 1`, default limit 12,000, maximum 12,000). If more text remains, ends with `more: recall id=<ref> offset=<next>`.
- **Search across history** (`query = "<string>"`): Searches original block text across all records in the session, newest record first and within each record from last block to first. Matching is per line: a literal substring by default, or a regular expression in [Rust `regex` syntax](https://docs.rs/regex/latest/regex/#syntax) with `regex?: bool = true` (patterns up to 1,000 characters; the engine matches in linear time and bounds the compiled size). Both are case-insensitive by default, with `case_sensitive?: bool = false`; `regex` and `case_sensitive` are refused together with `id`. Optional parameters include `role?: "user"|"assistant"|"system"|"tool"`, `kind?: "message"|"tool_call"|"tool_result"|"tool_change"`, `context?: integer 0..=3` (attaches previews of up to `n` blocks before and after each hit within the same record), and `limit?: integer 1..=20` (default 20).

Ids come from brief pointers (`Original messages: recall id=<record id>`), shortened-result notes (`recall id=<record id>/<block id>`), listings and search hits.

Labels read `role/kind`, for example `assistant/tool_call` or `tool/tool_result`: tool calls count as `assistant`, tool results as `tool` and tool changes as `system`. A call runs under the session's `tool_limits.call_timeout_secs` and returns promptly as cancelled or timed out when the turn is cancelled or the deadline passes; the record reading stops at the next block.

Any block originally inserted by an earlier rewrite (such as an earlier brief inside a merge record or a shortened tool result inside a summarize record) includes the next-layer pointer line `original: recall id=<record id>` directing the model to the earlier record. Plain original blocks carry no such line.

## External tools (search)

The built-in `search_text` runs the external `rg` executable. `[external_tools] rg` selects where it comes from; it is a top-level table, not a preset field, and applies to every entry point of the process:

| Value | Meaning | Check |
| --- | --- | --- |
| `managed` (default) | The pinned ripgrep 15.2.0 installed by TARS under `$XDG_DATA_HOME/tars/tools/ripgrep-15.2.0-<target>/rg`, falling back to `~/.local/share/tars/tools/...` | SHA-256 of the installed executable compiled into TARS; a mismatch means unavailable |
| `system` | The first `rg` found in an absolute `PATH` entry when TARS starts; empty and relative entries are skipped so a repository's own `./rg` never runs | Version recorded (a packaging suffix such as `-1` is ignored); a major version below 14 is unavailable |
| an absolute path | That executable | Must exist; its version is recorded |

Resolution happens once per process and is cached; session binding uses the resolved absolute path and never re-resolves behind the caller's back. A managed file that fails its check, a missing file, an old `system` version or an unreadable path all make search unavailable with a short reason; no source falls back to the others. A configured absolute path is trusted as given, so it is not version-checked the way `system` is.

Only `tars tools install rg` (or the TUI startup consent) downloads anything, and only for the managed source: HTTPS only, restricted to `github.com` and GitHub release asset hosts including redirects, at most 10 MiB and 120 seconds, proxies from the environment, hash verified before extraction, only the single `rg` entry extracted from the `.tar.gz` with link and path escapes rejected, mode 0755, atomic rename, and one `rg --version` after installation. Any failure leaves nothing at the final path. `[external_tools] rg = "system"` or an absolute path disables the managed installation and `tars tools install rg` refuses with a notice; switch the source back to `managed` to install.

## MCP servers

MCP servers are defined in a separate `mcp.json`, kept apart from `config.toml`: common `mcpServers` JSON fragments can be pasted directly, and `config.toml` only stores the preset selection. See [mcp.example.json](../mcp.example.json) for a complete example.

File locations:

- The default is `mcp.json` in the same directory as `config.toml`; when `TARS_CONFIG` points at another file, that file's directory is used.
- `TARS_MCP_CONFIG` can name it separately; when set, that file must exist, and a relative path is based on the current directory.
- Isolated invocations with `--no-config` read neither `mcp.json` nor `TARS_MCP_CONFIG`.
- A missing default file means no servers; other read, parse or validation errors fail startup.

```json
{
  "mcpServers": {
    "git": {
      "command": "uvx",
      "args": ["mcp-server-git", "--repository", "."],
      "env": { "LOG_LEVEL": "warn", "GIT_AUTHOR_NAME": "${GIT_AUTHOR_NAME}" },
      "startupTimeoutSec": 10,
      "toolTimeoutSec": 60,
      "enabledTools": ["git_status", "git_diff"],
      "approval": "prompt",
      "required": false,
      "tools": { "git_status": { "approval": "approve" } }
    },
    "docs": {
      "type": "http",
      "url": "https://example.invalid/mcp",
      "headers": { "Authorization": "Bearer ${DOCS_MCP_TOKEN}" },
      "approval": "writes"
    }
  }
}
```

Only `mcpServers` is allowed at the top level, and its value is an object keyed by server id. Ids allow only lowercase letters, digits and `-`, at most 16 characters; each session selects at most 8 servers. Every entry must have exactly one of `command` or `url`:

- `command`, `args` and `env` are for stdio servers; `headers` does not apply to stdio.
- `url` and `headers` are for HTTP servers; `args` and `env` do not apply to HTTP.
- `type` may be omitted and is inferred from `command`/`url`; when written explicitly it accepts only `stdio`, `http` and `streamable-http`. `sse` is not supported.

| Field | Default | Description |
| --- | --- | --- |
| `startupTimeoutSec` | 10 | Startup deadline, 1–60 seconds |
| `toolTimeoutSec` | 60 | Per-call deadline, 1–3600 seconds |
| `enabledTools` / `disabledTools` | empty | Only allow or exclude the listed tools; the two cannot appear together, and names must be non-empty and unique |
| `approval` | `prompt` | `prompt` asks every time; `writes` treats tools the server declares read-only and not destructive as read-only and asks for the rest; `approve` does not ask |
| `tools.<name>.approval` | inherits the server | Overrides `approval` per tool; currently only this key may be written |
| `required` | `false` | When `true`, a startup failure fails the session open without a model request |

The tool names the model sees are `mcp_{id}_{tool}`. Tools longer than 64 bytes, containing invalid characters or sharing a name with a built-in tool are skipped and reported in the interface instead of failing the whole server. Tools added after the server sends `tools/list_changed` remain subject to the same filtering and confirmation settings.

### Variables and secrets

Every element of `args`, every value of `env`, and the values of `url` and `headers` may use `${VAR}` or `${VAR:-default}`, expanded from the TARS process environment when the session is opened. `command` does not support variables. When `${VAR}` is undefined and has no default, that server fails to start with a reason that includes the variable name but not the value; with `required = true` the whole open fails. The session snapshot saves the unexpanded form, and expanded values never enter records, listings or exports.

To reduce the risk of "pasting a token into a record just by following the docs":

- The `Authorization` header value must be `<scheme> ${VAR}` (for example `Bearer ${DOCS_MCP_TOKEN}`), and the variable must not have a default. Headers whose name contains `authorization` after splitting on `-` are treated the same way.
- Request headers whose name contains `TOKEN`, `KEY`, `APIKEY`, `SECRET`, `PASSWORD`, `PASSWD`, `CREDENTIAL` or `CREDENTIALS` after splitting on `-` must have a single `${VAR}` value without a default.
- `env` variables whose name contains one of those words after splitting on `_` follow the same rule.
- This is a name-based heuristic check: literal secrets inside `args` cannot be detected, so still use `${VAR}` there.

HTTP headers cannot override the transport's own `Accept`, `Content-Type`, `Last-Event-Id` and `Mcp-*` headers, nor be duplicated; invalid names or values fail at load time. Variables expanded inside a URL must be URL-safe characters; TARS does not percent-encode, so put content that needs encoding into headers.

Common foreign keys `disabled`, `autoApprove`, `alwaysAllow`, `trust` and `description` are ignored with a one-time notice at load time; they never relax TARS's `approval` or server selection. Unknown keys that would change behavior such as `cwd` and `timeout` fail (`timeout` points to the two timeout fields), and other unknown keys also fail. A `[mcp_servers.*]` table in `config.toml` fails loading with a notice to move it to `mcp.json`. Error messages never echo `env`, header or URL values.

### Selection, lifecycle and boundaries

A preset selects defined ids with `mcp = ["git"]`; CLI `run` and `session create` override the preset with `--mcp git,docs`, where an empty string enables none; the TUI new-session form has an MCP field. None are enabled by default. The configuration snapshot of the selected servers is saved with the new session definition, and later changes to `mcp.json` do not affect existing sessions; reopening reconnects from the snapshot, so rotated variable values take effect.

Opening a session starts the selected servers, and closing shuts the connections down and waits for stdio child processes to exit; `session create` and `session repair` only save the definition or materials and do not start servers. When startup fails, or listing tools fails or times out after startup, that server's tools are absent, the interface shows the reason, and the other tools remain available. A stdio child process receives only the variables listed in `PATH`, `HOME`, `LANG` and `env`, not TARS's entire environment, so model credentials do not leak to third-party servers. The working directory defaults to the session workspace root; without a configured workspace it inherits the TARS process's current directory.

**MCP servers run with the user's privileges, and TARS's workspace boundary does not constrain them.** A server can reach whatever user files, network and processes it can access; `read-only` mode can only block the call, not sandbox the server. The `writes` level trusts the server's read-only declarations; TARS cannot detect a server that declares a writing tool as read-only, which is the cost of choosing that level.

A single result is at most 64 KiB after serialization; larger content is converted to text with the beginning and end kept, the middle cut, and the omitted byte count reported. A tool-call timeout is handled as Causa's `UnknownOutcome` and may already have had an effect on the server side. The combination of `approval` and permission modes is in [Workspace tools](workspace-tools.md#confirmation-and-permission-modes).

## Workspace and built-in tools

A preset may add `workspace_root = "../project"` and `tools`. The built-in tools are `list_directory`, `read_file`, `search_text`, `write_file`, `edit_file` and `run_command`; tools default to empty, and enabling tools requires a root directory. `search_text` also requires a platform sandbox backend and a ready `rg`; see [External tools](#external-tools-search) for the source selection. `run_command` also takes the optional `[profiles.NAME.command]` table: `network` (`none`/`open`, default `none`), `default_timeout_secs` (default 120) and `max_timeout_secs` (default 600, hard limit 600); the command-line counterpart is `--command-network`, and the other two come from the preset. CLI `run` and `session create` accept `--workspace DIR --tools NAME,NAME`; an explicit `--tools ''` clears the preset. The TUI new-session form fills Workspace, Tools, initial permission mode and MCP selection from the preset, and changes affect only this new session; when the current Tools include `run_command`, an additional read-only command summary line is shown and cannot be edited. Reopening still uses the original definition and accepts no tool overrides.

A relative workspace likewise uses the configuration directory/CLI cwd base, but is canonicalized to a physical absolute directory and checked for existence when first resolved; after saving, reopening does not resolve aliases again. Unknown/duplicate tools, a missing root or exceeded limits are rejected. `[profiles.NAME.tool_limits]` can lower `max_concurrent_operations`, `call_timeout_secs`, `max_output_bytes`, `max_directory_entries`, `max_file_scan_bytes`, `max_search_files`, `max_search_depth`, `max_search_scan_bytes` and `max_search_matches` individually; `[profiles.NAME.write_limits]` controls `max_edit_file_bytes`, `max_write_bytes` and `max_edit_segments`. Omitted fields use the defaults, and the persistent definition saves the complete effective values. Values, paths and confirmation boundaries are in [Workspace tools](workspace-tools.md).

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

A preset may add `workspace_root = "../project"` and `tools`. The built-in tools are `list_directory`, `read_file`, `search_text`, `write_file`, `edit_file` and `run_command`; tools default to empty, and enabling tools requires a root directory. `run_command` also takes the optional `[profiles.NAME.command]` table: `network` (`none`/`open`, default `none`), `default_timeout_secs` (default 120) and `max_timeout_secs` (default 600, hard limit 600); the command-line counterpart is `--command-network`, and the other two come from the preset. CLI `run` and `session create` accept `--workspace DIR --tools NAME,NAME`; an explicit `--tools ''` clears the preset. The TUI new-session form fills Workspace, Tools, initial permission mode and MCP selection from the preset, and changes affect only this new session; when the current Tools include `run_command`, an additional read-only command summary line is shown and cannot be edited. Reopening still uses the original definition and accepts no tool overrides.

A relative workspace likewise uses the configuration directory/CLI cwd base, but is canonicalized to a physical absolute directory and checked for existence when first resolved; after saving, reopening does not resolve aliases again. Unknown/duplicate tools, a missing root or exceeded limits are rejected. `[profiles.NAME.tool_limits]` can lower `max_concurrent_operations`, `call_timeout_secs`, `max_output_bytes`, `max_directory_entries`, `max_file_scan_bytes`, `max_search_files`, `max_search_depth`, `max_search_scan_bytes` and `max_search_matches` individually; `[profiles.NAME.write_limits]` controls `max_edit_file_bytes`, `max_write_bytes` and `max_edit_segments`. Omitted fields use the defaults, and the persistent definition saves the complete effective values. Values, paths and confirmation boundaries are in [Workspace tools](workspace-tools.md).

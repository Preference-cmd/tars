# TARS

TARS is the validation project for Causa: a real agent application that exercises Causa's public contracts and feeds the problems found in use back into Causa to drive improvements. Validation comes first; the application is the vehicle for it. TARS ships as an installable, runnable program and is not published to crates.io.

## Purpose

- **Validate.** Causa provides only the contracts for context materials and single-run execution; sessions, saving, tool policy and the interface are left to the host. TARS is such a host and uses those contracts under real conditions: persistent sessions, continuation after a cancel, workspace tools with write confirmation, and live runs against real endpoints of the three wire protocols. It answers whether those contracts are sufficient and behave correctly inside a real application.
- **Improve.** For each problem found, first decide whether it belongs to TARS, Causa, or their wiring. Causa problems are fixed in the Causa repository under its rules and released there; TARS then upgrades the dependency and reruns the check that found the problem. For example, the Chat output-limit field in Causa 0.0.7 came from TARS's real-endpoint tests, and the tool hints and handler data in 0.1.2 came from TARS's write-tool design.
- **The application is the vehicle.** TARS is a CLI and TUI application that can actually be used, because only real use produces valid evidence. When ordering work, prefer work that exercises Causa; work that only serves distribution (install channels, GUI, more platforms) comes later.

Boundaries:

- TARS uses Causa only through its public facade at the version published on crates.io; it does not copy or vendor Causa sources, so what is validated is exactly what external users get.
- When improving Causa, do not put TARS policy into it. For example, permission modes belong to TARS; Causa carries only tool hints and per-round host data, while TARS keeps the vocabulary and the decision.
- A passing TARS test only shows that this particular wiring works; it does not replace Causa's own tests.

## Current status

A single-session work loop is implemented, along with an offline CLI demo: atomic submission, same-key replay, per-work assembly, cancellation and result observation, and material editing with save/import. The demo uses a scripted model and real local tools, runs through Session and the Causa runner, and needs no credentials or network.

`tars run` supports single text execution against a real provider, JSON output, cancellation/deadline and explicit material saving. `tars session create/run/list` automatically save the frozen definition and the latest materials through the shared SessionHost, and can reopen the same session. `tars-tui` provides a native terminal prototype with temporary streaming text preview; both ends share the TOML configuration and can explicitly choose the workspace and workspace tools. The standalone MCP configuration file `mcp.json` (with `${VAR}` expansion and literal-secret checks), preset/CLI/TUI selection, definition snapshots and runtime tools are wired (tools only; no resources/prompts/sampling). MCP calls run, ask (in the terminal or the TUI confirmation panel) or are rejected according to the `approval` setting in `mcp.json` and the current permission mode; verification against real third-party MCP servers is still pending. The built-in `run_command` runs in the macOS Seatbelt sandbox (no network by default, writable workspace, read-only `.git` hooks and config, unreadable/unwritable credential locations); a single command can request to run outside the sandbox with a reason and is asked or allowed according to the permission mode. The built-in `search_text` runs the external `rg` in a read-only sandbox, so it follows the workspace's ignore rules and supports regular expressions, case-insensitive matching, globs and context lines; `rg` is not bundled and is installed only after `tars tools install rg` asks for consent. The TUI `/status` and CLI `session create` display the sandbox backend and capabilities; both `run_command` and `search_text` are unavailable on Linux/Windows until those platforms have a sandbox backend (sessions containing them still open, and calls are rejected with the reason). GUI, parent/child agents and install channels are not implemented. CLI and TUI can write files: `ask` confirms in the terminal/confirmation panel and refuses in non-terminal contexts; `accept-edits` and `full-access` do not ask for file writes, while sandbox-outside requests for commands still ask under `accept-edits`. `tars-tui` has the write confirmation panel, command confirmation panel, `/mode` command and `Shift-Tab` mode switching wired. The current development version is `0.0.0`, with no public release yet.

## Build and run locally

Rust 1.96.0 is used (pinned by `rust-toolchain.toml`):

```sh
cargo build --locked
cargo run --locked -- --help
cargo run --locked -- demo
cargo run --locked -- demo --save materials.json
cargo build --locked --release
./target/release/tars demo
```

On Windows use `target\release\tars.exe`. No arguments prints help; argument errors exit with code 2, execution or file errors with 1. While the demo runs, Ctrl-C requests cancellation of the current work and waits for it to settle.

`demo` runs T1, summary editing, exact replay and T2, then saves materials and creates a new Session to continue execution. Temporary files are used by default and cleaned up afterwards; `--save` writes the materials to the given file, atomically replacing the original, and the parent directory must already exist. The file contains only Context; import preserves BlockIds but does not restore old work, request keys or external side effects. The application must save whatever history it needs; materials that are deleted without being saved cannot be recovered.

## Single model execution

First set `OPENAI_API_KEY` (OpenAI protocols) or `ANTHROPIC_API_KEY` (Anthropic) in the current shell, then specify the model ID explicitly:

```sh
./target/release/tars run --provider openai-responses --model "$MODEL_ID" --input 'Explain this design' --save materials.json
printf 'Continue analyzing the material' | ./target/release/tars run --provider openai-responses --model "$MODEL_ID" --stdin --load materials.json --save materials.json --json
```

You set `MODEL_ID` to a model available on the server. Protocol options are `openai-chat`, `openai-responses` and `anthropic`; `--endpoint` overrides the full URL (including the path), and `--api-key-env` names another credential environment variable. Credentials are never written into materials or result reports by the configuration layer. The default tool catalog is empty; use `--workspace DIR --tools list_directory,read_file,search_text,write_file,edit_file,run_command` to enable workspace tool capabilities, while still printing only the final result. `search_text` runs the external `rg` in a read-only sandbox and supports `regex`, `ignore_case`, `glob` and `context_lines`; `run_command` launches `/bin/bash -c` inside the sandbox by default; the command timeout defaults to 120 seconds and can be at most 600 seconds. `--command-network none|open` selects whether outbound network is denied by default or allowed. A command can carry `outside_sandbox` and a reason to request running outside the sandbox, and is confirmed or executed directly according to the permission mode. Reading an existing file before writing is mandatory; `--permission-mode` sets the initial permission mode, and `ask` refuses writes directly in non-terminal contexts. See [Workspace tools](docs/workspace-tools.md) for access scope, ignore rules, confirmation, limits and legacy record export.

Input must be exactly one of `--input <text>` or `--stdin` (read to EOF, UTF-8, not entirely whitespace). `--timeout-secs` defaults to 120 and accepts 1–86400, starting when the work is accepted and covering assembly and execution; it does not cover reading stdin, loading or saving files.

In normal mode stdout contains only the completion text, and diagnostics go to stderr. `--json` outputs `status`, `text`, `conversation_id`, `work_id`, `saved`, `error` and, when the provider disclosed token accounting, `usage` (`input_tokens`, `output_tokens` and any cache/reasoning counts); status is `not_started`, `completed`, `failed`, `cancelled` or `timed_out`. When a model call fails or ends with an unrecognized stop value, there are also `error_kind` and `error_detail`. `error_kind` is `transient`, `timed_out`, `cancelled`, `invalid_request`, `unknown_outcome`, `credential_rejected` for rejected credentials, `not_found` for a missing model or path, `permanent` for other rejections, `undecodable` when the service answered but the response could not be read, `unclassified` for a failure class unknown to this version, or `unrecognized_stop` when the service returned a stop value unknown to this version, in which case `error_detail` is that stop value. Otherwise `error_detail` is the raw message from the gateway, possibly containing server-provided text, and both fields are null when there is no such failure. In normal mode `error_detail` is written to stderr as `TARS: detail: …`. Argument errors go only to stderr. Execution and file results are separate: when the model completes but saving fails, the result is still `completed` with `saved=false`, non-empty `error` and exit code 1. Check the exit code to judge overall success.

Exit codes: 0 completed; 1 configuration, execution or file failure; 2 argument error; 124 work deadline; 130 cancelled. Ctrl-C during execution requests cancellation and waits for the real materials to be returned; `--save` saves the current materials after success, execution failure or controlled interruption, and without that option nothing is saved automatically. It does not save uncommitted output or uncommitted tool batches, nor does it restore old work or request keys. The default material interface rejects external media/asset references.

These paths are verified by local HTTP fixtures and real process tests without live credentials; minimal requests against a user-configured service, three-tool round trips and the BA09 three-protocol run with confirmation-enabled writes have also been done. Live samples do not replace interruption/race verification, and public installation has not been accepted yet.

## External tools

`search_text` needs the external `rg` executable. TARS does not bundle it and never downloads it silently:

```sh
./target/release/tars tools status          # source, path, version, state and the sandbox backend probe
./target/release/tars tools status --json
./target/release/tars tools install rg      # asks before downloading the pinned ripgrep 15.2.0
./target/release/tars tools install rg --yes
```

The default source is the managed copy under `$XDG_DATA_HOME/tars/tools/` (falling back to `~/.local/share/tars/tools/`); its SHA-256 is checked against the value compiled into TARS before it is ever executed. `[external_tools] rg = "managed" | "system" | "<absolute path>"` in the shared TOML selects another source. Only `tars tools install rg` performs network access; every other command, and a non-interactive run without `--yes`, never downloads, and a missing tool is reported with the remedy. The TUI asks the same question from its startup check before the conversation screen, ignores keys for 0.5 seconds, and shows the reason in the footer and `/status` when search stays unavailable. The managed directory is outside the command sandbox's writable roots, so a search never runs a file that `run_command` could replace. Linux and Windows have no sandbox backend yet, so the built-in search is unavailable there.

## Save and reopen a session

```sh
# First create, printing the ConversationId; creation itself does not call the model
./target/release/tars session create --store ./tars-data \
  --provider openai-chat --model "$MODEL_ID" --json

# Use the printed ID; provider/model/deadline come from the saved definition
./target/release/tars session run "$SESSION_ID" --store ./tars-data \
  --input 'Continue this session' --json
./target/release/tars session list --store ./tars-data --json

# Inspect or manage a long session's replaced material
./target/release/tars session show "$SESSION_ID" --store ./tars-data --json
./target/release/tars session compact "$SESSION_ID" --store ./tars-data
./target/release/tars session history "$SESSION_ID" --store ./tars-data --last 3
```

Set `--context-window N` (the model's token window) and `--auto-compact on|off|PERCENT` when creating a session, or put `context_window`, `auto_compact` and `compact_threshold` in a preset, to let TARS replace a Context that approaches the window with a model-written working brief. The replaced conversation is kept as read-only history segments next to the record, which `session history` reads in order (`--json` prints one segment document per line) and which the TUI shows above the current conversation. `session compact` performs the replacement manually; `session show` reports the current estimate, the settings, the segment count and retained-work occupancy. Material export, recovery exports and `--load` always contain the current Context only. Details are in [Configuration](docs/configuration.md#long-session-compaction) and [Current implementation](docs/README.md#compaction-and-history).

`--store` is given by an argument or the shared configuration. Records keep the frozen definition and the latest Context; reopening preserves the ConversationId and BlockIds but does not restore old work, request keys or uncommitted output. Environment-variable credentials are resolved again on every open; the record stores only the variable reference. The definition defaults to a 120-second deadline, 256 retained works, 10 model calls and 64 tool declarations; see `tars session --help` for details.

`session run` waits for the automatic save after execution succeeds, fails or is cancelled. JSON returns execution fields `status/text/execution_error/error_kind/error_detail` (and `usage` when the provider disclosed token accounting) separately from `saved/revision/save_error`, and also includes `recovery_file/recovery_importable/recovery_error/close_error/close_outcome` and `save_wait_abandoned`. When saving fails, the current materials are exported to `tars-recovery-<uuid>.json` and the command exits 1; the default export goes to the current directory, and `--recovery-dir` can name an existing directory. During the save wait, the first Ctrl-C prints a notice and the second stops waiting and exports through the same failure path. Export failure is reported separately and must not claim that materials were recovered. Recovery files can be imported as a new session with `--load` when creating; files over 64 MiB are still written completely but cannot be imported directly by this version, which the result reports explicitly. Closing confirms the original pending save; `close_outcome` distinguishes committed, uncommitted and still-unconfirmed. If closing confirms a commit whose earlier result was unknown, `saved/revision` updates to the confirmed fact while the original `save_error` and error exit remain.

Storage uses a local stable file lock: another process holding the same session is refused explicitly. Corrupt records are listed separately and make the command fail; they are not overwritten automatically. Each record is at most 64 MiB; this is not a process memory budget. The current scope excludes power-loss durability, network shares and cross-host writes; see [Current boundaries](docs/README.md) for details.

If a cancel leaves unpaired tool declarations, new input is preserved as-is with a prompt to handle the materials. In the TUI, F7 or `/repair` previews accepting the original result or removing the unpaired declaration, then Enter applies and saves. In the CLI, `session repair ID --store DIR --json` inspects it, and `--apply --expected-revision N` explicitly applies the removal mode. Repair does not rerun the old tool and the original work state is kept; after saving, send new input separately. Conditions, reopening and the retention scope of the original batch are described in [Continuation after interruption](docs/session-continuation.md).

## Project layout

The root is a Cargo workspace whose directory names omit the `tars-` prefix: `crates/core/` (package `tars-core`) provides the shared application core and configuration, `crates/sandbox/` (`tars-sandbox`) provides the platform sandbox policy, backends and process control, `crates/cli/` (`tars-cli`) provides `tars`, and `crates/tui/` (`tars-tui`) provides the `tars-tui` prototype. Default `cargo run/build` still selects the CLI; the TUI is started with `cargo run -p tars-tui -- --config /path/to/config.toml`, and everything is built with `cargo build --workspace --locked`. Entry points depend one-way on the core; `tars-core` depends one-way on the library package `tars-sandbox` and keeps the `tars_core::sandbox` path through `pub use tars_sandbox as sandbox`, and CLI/TUI do not depend on it directly. All packages have `publish = false`. Causa is used through the crates.io release `causa = "0.1"` with the runtime and providers features enabled, and the exact version is fixed by Cargo.lock; builds no longer need a sibling Causa checkout. Standalone distribution is handled separately.

CLI and TUI share the [TOML configuration](docs/configuration.md) and support the global default path, `TARS_CONFIG`, `--config` and named model presets; explicit arguments take precedence. Start from [config.example.toml](config.example.toml); MCP server definitions are in [mcp.example.json](mcp.example.json) and [Configuration](docs/configuration.md#mcp-servers). Reopening an existing session is unaffected by changes to global model presets.

- [Current structure, interface boundaries and checks](docs/README.md)
- [Shared configuration](docs/configuration.md)
- [Workspace tools: read/write, confirmation and record migration](docs/workspace-tools.md)
- [Material handling and continuation after interruption](docs/session-continuation.md)
- [TUI operation, keys and validation boundaries](docs/tui.md)

Causa owns Context and single-run execution; TARS owns sessions, work acceptance, dependency assembly and application interaction.

Development workflow material `.agents/` and helper `scripts/` stay on the local machine and are Git-ignored; they are not distributed with the application.

## License

This project is released under the [MIT license](LICENSE).

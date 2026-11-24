# TARS

TARS is an open-source, local-first AI agent written in Rust and built on [`causa`](https://crates.io/crates/causa) (`0.3`, via crates.io). Causa supplies the context and material contracts plus multi-protocol model execution across `openai-chat`, `openai-responses`, and `anthropic`, while TARS provides persistent sessions, workspace tools, sandboxing, compaction, and both a scriptable CLI (`tars`) and a native terminal interface (`tars-tui`). TARS is in early-stage development (`0.0.0`, built from source via Cargo).

## Key features

- **Multi-protocol providers:** Supports `openai-chat`, `openai-responses`, and `anthropic`, custom `--endpoint` URLs, and environment-variable-only credential references (`--api-key-env`) that are never written into session records or result reports.
- **Defense-in-depth workspace safety:** Four permission modes (`read-only`, `ask`, `accept-edits`, `full-access`), mandatory read-before-write file safety, macOS Seatbelt OS sandboxing for `run_command` and `search_text` (no outbound network by default, protected `.git` hooks/config/modules/worktrees, denied credential locations, and read-only `.tars/`), and two-layer command approval rules (`~/.tars/permissions.toml` + trust-verified `<workspace>/.tars/permissions.toml`, merged with `deny > ask > allow` precedence).
- **Long-session context & prompt-cache engineering:** Automatic large-output spilling, zero-LLM hot/cold micro-compaction, partial LLM working briefs with brief-chain merging, temporary frame-trimming fallback, prefix prompt caching (`--prompt-cache on|off`, default `on`), and `recall` over compacted history.
- **Persistent, crash-safe sessions:** Local JSON store (`tars-session-v2.4`) with OS exclusive file locks, atomic commits, recovery export on save failure (`tars-recovery-<uuid>.json`), and explicit `/repair` / `session repair` continuation for interrupted tool batches.
- **Extensible tool ecosystem:** Six workspace tools (`list_directory`, `read_file`, `search_text`, `write_file`, `edit_file`, `run_command`), persistent `recall`, multi-directory `agentskills.io` `load_skill`, opt-in QuickJS ES2020 read-only programmatic tool calling (`run_code`), and standalone `mcp.json` MCP server support (tools only; no resources, prompts, or sampling).
- **Written in Rust:** Rust `edition = 2024` (pinned to Rust `1.96.0` via `rust-toolchain.toml`), zero `unsafe` in TARS crates, and deterministic local fixture tests without live model credentials.

## Quick start

**Prerequisites:** Rust `1.96.0` (pinned by `rust-toolchain.toml`).

Build the workspace binaries from source:

```sh
cargo build --workspace --locked --release
```

This produces `./target/release/tars` and `./target/release/tars-tui` (`target\release\tars.exe` and `target\release\tars-tui.exe` on Windows).

Set a provider API key in your shell (`OPENAI_API_KEY` for OpenAI protocols or `ANTHROPIC_API_KEY` for Anthropic).

### Interactive TUI

Copy and customize [config.example.toml](config.example.toml), then launch `tars-tui`:

```sh
./target/release/tars-tui --config /path/to/config.toml
# Or run directly with Cargo:
cargo run -p tars-tui -- --config /path/to/config.toml
```

### CLI one-shot & persistent sessions

```sh
# One-shot execution with explicit material saving
./target/release/tars run --provider openai-responses --model "$MODEL_ID" \
  --input 'Explain this design' --save materials.json
printf 'Continue analyzing the material' | ./target/release/tars run \
  --provider openai-responses --model "$MODEL_ID" --stdin \
  --load materials.json --save materials.json --json

# Persistent sessions (creation itself does not call the model)
./target/release/tars session create --store ./tars-data \
  --provider openai-chat --model "$MODEL_ID" --json
./target/release/tars session run "$SESSION_ID" --store ./tars-data \
  --input 'Continue this session' --json
./target/release/tars session list --store ./tars-data --json
./target/release/tars session show "$SESSION_ID" --store ./tars-data --json
./target/release/tars session compact "$SESSION_ID" --store ./tars-data
./target/release/tars session history "$SESSION_ID" --store ./tars-data --last 3
```

### Optional `rg` setup for `search_text`

`search_text` uses external `rg` (ripgrep), which is not bundled and is never downloaded silently. Inspect status or install the pinned `15.2.0` release (consent-gated and SHA-256 verified):

```sh
./target/release/tars tools status
./target/release/tars tools install rg
```

## Usage overview

### Interactive TUI (`tars-tui`)

`tars-tui` provides a single-column Ratatui terminal interface with streaming response previews, inline confirmation panels for file writes, commands, and MCP calls, startup checks for the sandbox backend and `rg`, and lazy history expansion. Common commands and shortcuts include `/sessions`, `/mode` or `Shift-Tab`, `/rules`, `/tools`, `/compact`, `/auto_compact`, `/summary`, `/status`, and `F7` / `/repair`. See the [TUI guide](docs/tui.md).

### Persistent sessions, compaction & recovery

Configure `--store`, `--context-window`, `--auto-compact on|off|PERCENT`, and `--prompt-cache on|off` when creating a session (or via TOML presets), and inspect or manage sessions with `session show`, `session compact`, `session history`, and `session repair`. Records use OS exclusive `<uuid>.lock` files and a 64 MiB per-record limit; if saving fails, uncommitted materials export to `tars-recovery-<uuid>.json` (directory configurable via `--recovery-dir`) without overwriting existing files. See [Long-session compaction](docs/configuration.md#long-session-compaction), [Session lifecycle](docs/session-lifecycle.md), [Session continuation](docs/session-continuation.md), and [Compaction and history](docs/README.md#compaction-and-history).

### CLI automation & workspace tools

`tars run` supports `--input` or `--stdin`, `--load` / `--save`, and `--json`, with distinct exit codes (`0` completed, `1` configuration, execution, or file error, `2` argument error, `124` deadline, `130` cancelled). Workspace tools are opt-in via `--workspace DIR --tools ...` alongside `--permission-mode`, `--command-network none|open`, `--code-execution` / `--no-code-execution`, and unified prelude/skill controls (`--skill-dir`, `--no-skills`). CLI and TUI share TOML configuration (`~/.tars/`, `$TARS_HOME`, `TARS_CONFIG`, `--config`; see [config.example.toml](config.example.toml)) and `mcp.json` ([mcp.example.json](mcp.example.json)). See [Workspace tools](docs/workspace-tools.md) and [Configuration](docs/configuration.md).

## Platform support and current status

| Capability | macOS | Linux | Windows |
| --- | --- | --- | --- |
| Core runtime, CLI (`tars`), TUI (`tars-tui`), persistent sessions, MCP | Verified in CI | Verified in CI | Not currently in CI (unverified) |
| Workspace file tools (`list_directory`, `read_file`, `write_file`, `edit_file`) | Supported | Supported | Not yet implemented |
| Sandboxed `run_command` and `search_text` | Supported (Seatbelt) | Fail-closed (no sandbox backend yet) | Fail-closed (no sandbox backend yet) |

Current gaps and roadmap items not yet implemented:

- Prebuilt binaries and package-manager installers (currently source build via Cargo only)
- Linux and Windows sandbox backends for `run_command` and `search_text` (and restoring Windows CI)
- Multi-agent / subagent collaboration
- Desktop GUI
- Live verification against third-party MCP servers (current tests cover local fixtures)

## Architecture

| Crate | Package | Role |
| --- | --- | --- |
| `crates/core` | `tars-core` | Shared session host, configuration, tools, permission policy, compaction, and store |
| `crates/sandbox` | `tars-sandbox` | Platform-independent sandbox policy, capability probes, process trees, and Seatbelt backend |
| `crates/cli` | `tars-cli` | `tars` CLI binary |
| `crates/tui` | `tars-tui` | `tars-tui` native terminal interface |

Dependencies flow strictly one way (`tars-cli` / `tars-tui` → `tars-core` → `tars-sandbox`, with `tars-core` re-exporting `tars_core::sandbox`), and all workspace packages are `publish = false`.

**Causa dependency boundary:** The workspace (`tars-core`, `tars-cli`, and `tars-tui`; `tars-sandbox` depends on neither `causa` nor `tars-core`) consumes `causa 0.3` with the `runtime`, `providers`, and `extensions` features enabled (exact version locked in `Cargo.lock`) strictly through its public facade. TARS owns application policy—sessions, persistence, workspace tools, sandboxing, permission modes, compaction, and UIs—on top of Causa's `Context` and single-run execution primitives; general runtime or protocol improvements uncovered here are contributed upstream to Causa rather than mixing policy across the boundary.

## Documentation

- [Current implementation](docs/README.md) — code layout, module boundaries, and local verification checks
- [Shared configuration](docs/configuration.md) — TOML schema, presets, compaction, prelude/skills, external tools, and MCP
- [Workspace tools](docs/workspace-tools.md) — file safety, permission modes, command rules, Seatbelt sandbox, `run_code`, and record migration
- [Session lifecycle](docs/session-lifecycle.md) — session assembly, execution, saving, closing, and usage statistics
- [Session continuation](docs/session-continuation.md) — repairing interrupted tool batches
- [TUI guide](docs/tui.md) — terminal layout, keybindings, startup checks, and history display
- [config.example.toml](config.example.toml) & [mcp.example.json](mcp.example.json) — starter configuration examples

## Contributing

See [AGENTS.md](AGENTS.md) and [Local checks](docs/README.md#local-checks) for architecture conventions, layer boundaries, and verification commands. Local development workflow materials stay gitignored and are not part of the public repository.

## License

This project is released under the [MIT license](LICENSE).

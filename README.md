# TARS

TARS is an open-source, local-first AI agent written in Rust and built on [`causa`](https://crates.io/crates/causa) (`0.3`, via crates.io). Causa supplies the context and material contracts plus multi-protocol model execution across `openai-chat`, `openai-responses`, and `anthropic`, while TARS provides persistent sessions, workspace tools, sandboxing, compaction, and a single `tars` binary covering both an interactive terminal interface and scriptable CLI subcommands (`run`, `session`, `tools`). TARS is in early-stage development (`0.0.0`, built from source via Cargo).

## Key features

- **Multi-protocol providers:** `openai-chat`, `openai-responses`, and `anthropic` with custom endpoints and env-only API keys.
- **Workspace safety:** Four permission modes, read-before-write guards, macOS Seatbelt sandboxing, and layered command rules.
- **Long-session compaction:** Automatic output spilling, micro-compaction, working briefs, prompt caching, and `recall`.
- **Persistent sessions:** Local SQLite storage with OS file locks, atomic commits, recovery export, and interrupted-turn repair.
- **Programmatic tool calling (PTC):** Opt-in QuickJS ES2020 `run_code` sandbox for orchestrating read-only workspace tools and `recall` in one turn.
- **Multi-agent collaboration:** Built-in and `.tars/agents/*.md` sub-agents with monotonic narrowing, background tasks, and lock-free inspection.
- **Written in Rust:** Rust 2024 edition (`1.96.0`), zero `unsafe` in TARS crates, and deterministic local tests.

## Quick start

**Prerequisites:** Rust `1.96.0` (pinned by `rust-toolchain.toml`).

Build the unified `tars` binary from source:

```sh
cargo build --workspace --locked --release
```

This produces `./target/release/tars` (`target\release\tars.exe` on Windows).

Set a provider API key in your shell (`OPENAI_API_KEY` for OpenAI protocols or `ANTHROPIC_API_KEY` for Anthropic).

### Interactive TUI

Launch `tars` directly in an interactive terminal (zero-config default store under `~/.tars/data` or `$TARS_HOME/data`, with a guided first-run setup panel when unconfigured; press `Esc` to skip or use `/doctor`, `/init`, `/new`, `/sessions`, `/agents`, `/mode`, `/compact`, `/help`):

```sh
./target/release/tars
# Or with an explicit config file, or isolated --no-config --store:
./target/release/tars --config /path/to/config.toml
./target/release/tars --no-config --store ./tars-data
# Inspect all interactive options, slash commands, and CLI subcommands:
./target/release/tars -h
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

## Platform support and current status

| Capability | macOS | Linux | Windows |
| --- | --- | --- | --- |
| Core runtime, CLI & TUI (`tars`), persistent sessions, MCP | Verified in CI | Verified in CI | Not currently in CI (unverified) |
| Workspace file tools (`list_directory`, `read_file`, `find_files`, `write_file`, `edit_file`) | Supported | Supported | Not yet implemented |
| Sandboxed `run_command` and `search_text` | Supported (Seatbelt) | Fail-closed (no sandbox backend yet) | Fail-closed (no sandbox backend yet) |

Current gaps and roadmap items not yet implemented:

- Prebuilt binaries and package-manager installers (currently source build via Cargo only)
- Linux and Windows sandbox backends for `run_command` and `search_text` (and restoring Windows CI)
- Desktop GUI
- Live verification against third-party model and MCP endpoints (current tests cover local HTTP and MCP fixtures)

## Architecture

| Crate | Package | Role |
| --- | --- | --- |
| `crates/core` | `tars-core` | Shared session host, configuration, tools, permission policy, compaction, and store |
| `crates/sandbox` | `tars-sandbox` | Platform-independent sandbox policy, capability probes, process trees, and Seatbelt backend |
| `crates/cli` | `tars-cli` | `tars` unified binary (CLI subcommands + interactive TUI entry) |
| `crates/tui` | `tars-tui` | Native terminal interface library consumed by `tars-cli` |

Dependencies flow strictly one way (`tars-cli` → `tars-tui` → `tars-core` → `tars-sandbox`, with `tars-core` re-exporting `tars_core::sandbox`), and all workspace packages are `publish = false`.

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

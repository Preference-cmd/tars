# TARS

TARS is an open-source, local-first AI agent written in Rust and built on [`causa`](https://crates.io/crates/causa) (`0.3`, via crates.io). Causa supplies the context and material contracts plus multi-protocol model execution across `openai-chat`, `openai-responses`, and `anthropic`, while TARS provides persistent sessions, workspace tools, sandboxing, compaction, and a single `tars` binary covering both an interactive terminal interface and scriptable CLI commands (`-p`/`--print`, `run`, `session`, `tools`, `doctor`, `completions`). TARS is in early-stage development (`v0.1.0` prebuilt macOS/Linux binaries available via GitHub Releases, or build from source via Cargo).

## Key features

- **Multi-protocol providers:** `openai-chat`, `openai-responses`, and `anthropic` with custom endpoints, env-only API keys, and multimodal image attachments (PNG, JPEG, GIF, WebP).
- **Workspace safety:** Four permission modes, read-before-write guards, macOS Seatbelt and Linux Bubblewrap (`bwrap` + `seccomp-BPF`) sandboxing, and layered command rules.
- **Long-session compaction:** Automatic output spilling, micro-compaction, working briefs, prompt caching, and `recall`.
- **Persistent sessions:** Local SQLite storage with OS file locks, atomic commits, recovery export, and interrupted-turn repair.
- **Programmatic tool calling (PTC):** Opt-in QuickJS ES2020 `run_code` sandbox for orchestrating read-only workspace tools and `recall` in one turn.
- **Multi-agent collaboration:** Built-in and `.tars/agents/*.md` sub-agents with monotonic narrowing, background tasks, and lock-free inspection.
- **Written in Rust:** Rust 2024 edition (`1.96.0`), zero `unsafe` in TARS crates, and deterministic local tests.

## Quick start

### Install prebuilt binary (macOS & Linux)

Install the `v0.1.0` release binary with the POSIX installer:

```sh
curl -fsSL https://raw.githubusercontent.com/Preference-cmd/tars/v0.1.0/install.sh | sh
```

To pin a version or install into a custom directory (pass flags after `sh -s --`):

```sh
curl -fsSL https://raw.githubusercontent.com/Preference-cmd/tars/v0.1.0/install.sh | sh -s -- --version v0.1.0 --install-dir ~/.local/bin
```

`install.sh` installs by default into `~/.local/bin` (ensure it is on your `PATH`), performs idempotent atomic upgrades, enforces mandatory `SHA256SUMS.txt` verification and archive path-traversal checks, and never modifies shell rc files (`~/.zshrc`, `~/.bashrc`, `~/.profile`).

Supported prebuilt targets:

- **macOS:** `aarch64-apple-darwin`, `x86_64-apple-darwin` (note: macOS binaries are not yet Apple Developer ID signed or notarized)
- **Linux:** `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu` (`glibc >= 2.35`, built on `ubuntu:22.04`)

Manual archive verification (checksum & SLSA build provenance):

```sh
# macOS
shasum -a 256 -c SHA256SUMS.txt --ignore-missing
# Linux
sha256sum -c SHA256SUMS.txt --ignore-missing
# GitHub CLI SLSA provenance attestation
gh attestation verify tars-v0.1.0-<target>.tar.gz --repo Preference-cmd/tars
```

### Build from source

**Prerequisites:** Rust `1.96.0` (pinned by `rust-toolchain.toml`).

```sh
cargo build --workspace --locked --release
```

This produces `./target/release/tars` (`target\release\tars.exe` on Windows).

Set a provider API key in your shell (`OPENAI_API_KEY` for OpenAI protocols or `ANTHROPIC_API_KEY` for Anthropic).

The examples below assume `tars` is on your `PATH` (use `./target/release/tars` when running a local source build).

### Interactive TUI

Launch `tars` directly in an interactive terminal: when a default profile is configured (and `--no-config` is not passed), `tars` automatically creates and opens a ready session bound to the workspace (`--workspace DIR` or the current working directory) and sandbox-appropriate tools; when unconfigured with an empty store, it opens the guided first-run setup panel (`Esc` skips). Supports `-c`/`--continue` (most recently updated session), `-r`/`--resume [SELECTOR]` (session picker or ID/prefix match), slash commands (`/clear`, `/resume`, `/model`, `/copy`, `/doctor`, `/init`, `/new`, `/sessions`, `/agents`, `/queue`, `/image`, `/rename`, `/fork`, `/edit`, `/mode`, `/compact`, `/export-md`, `/help`), `Ctrl-O` to expand full tool results/diffs, and paste/drag-and-drop local image attachments:

```sh
# Launch directly (auto-starts a workspace-bound session when configured, or onboarding when unconfigured):
tars
tars --workspace /path/to/repo
# Continue the most recent session, open the session picker, or resume by ID/prefix:
tars -c
tars -r
tars -r "$SHORT_PREFIX"
# Or with an explicit config file, or isolated --no-config --store:
tars --config /path/to/config.toml
tars --no-config --store ./tars-data
# Inspect all interactive options, slash commands, and CLI subcommands:
tars -h
```

### Headless print mode (`tars -p`), persistent sessions & diagnostics

```sh
# Headless print mode: positional prompt, `--` for leading-dash prompts, and auto-piped stdin
tars -p --provider openai-responses --model "$MODEL_ID" "Summarize the workspace layout"
git diff | tars -p --profile fast "Review this diff"
tars -p -- -explain-this-flag

# Streaming text (--stream) or NDJSON events (--stream-json / --output-format stream-json)
tars -p --profile fast --stream "Write a short checklist"
tars -p --profile fast --output-format stream-json "Inspect the repo"

# Persistent continuation from `tars -p` (-c / --continue, or --resume <id|prefix|latest>)
tars -p -c "Follow up on the previous turn"
tars -p --resume "$SHORT_PREFIX" --store ./tars-data "Continue this session"

# Explicit one-shot execution with material saving and optional image attachments
tars run --provider openai-responses --model "$MODEL_ID" \
  --input 'Explain this design' --image diagram.png --save materials.json
printf 'Continue analyzing the material' | tars run \
  --provider openai-responses --model "$MODEL_ID" --stdin \
  --load materials.json --save materials.json --json

# Persistent sessions (creation itself does not call the model; supports full ID, unique prefix, or `latest`)
tars session create --store ./tars-data \
  --provider openai-chat --model "$MODEL_ID" --json
tars session run latest --store ./tars-data \
  --input 'Continue this session' --image screenshot.png --stream
tars session list --store ./tars-data --json
tars session show "$SHORT_PREFIX" --store ./tars-data --json
tars session rename "$SHORT_PREFIX" --store ./tars-data --title 'Design review' --json
tars session fork "$SHORT_PREFIX" --store ./tars-data --title 'Alternative branch' --json
tars session compact "$SHORT_PREFIX" --store ./tars-data
tars session history "$SHORT_PREFIX" --store ./tars-data --last 3
tars session export-materials "$SHORT_PREFIX" --store ./tars-data --output exported.json --json
tars session export-markdown "$SHORT_PREFIX" --store ./tars-data --output transcript.md --json
tars session delete "$SHORT_PREFIX" --store ./tars-data --json

# Read-only diagnostics and static shell completions
tars doctor --json
tars completions zsh
```

### Optional `rg` setup for `search_text`

`search_text` uses external `rg` (ripgrep), which is not bundled and is never downloaded silently. By default (`[external_tools] rg = "auto"`), TARS checks a SHA-256-verified managed copy first, falls back to a compatible `rg >= 14` on `PATH`, and otherwise prompts before downloading the pinned `15.2.0` release (macOS `aarch64`/`x86_64`, Linux `x86_64`/`aarch64`). Inspect status or install manually:

```sh
tars tools status
tars tools install rg
```

## Platform support and current status

| Capability | macOS | Linux | Windows |
| --- | --- | --- | --- |
| Core runtime, CLI & TUI (`tars`), persistent sessions, MCP | Verified in CI | Verified in CI | Not currently in CI (unverified) |
| Workspace file tools (`list_directory`, `read_file`, `find_files`, `write_file`, `edit_file`) | Supported | Supported | Not yet implemented |
| Sandboxed `run_command` and `search_text` | Supported (Seatbelt) | Supported (Bubblewrap `bwrap` + `seccomp-BPF`, requires unprivileged userns) | Fail-closed (no sandbox backend yet) |

Current gaps and roadmap items not yet implemented:

- Package-manager installers (Homebrew tap, Cargo-binstall), Windows prebuilt archives, and Apple Developer ID signing / notarization
- Windows workspace tools and sandbox backend for `run_command` and `search_text` (and restoring Windows CI)
- Desktop GUI
- Live verification against third-party model and MCP endpoints (current tests cover local HTTP and MCP fixtures)

## Architecture

| Crate | Package | Role |
| --- | --- | --- |
| `crates/core` | `tars-core` | Shared session host, configuration, tools, permission policy, compaction, and store |
| `crates/sandbox` | `tars-sandbox` | Platform-independent sandbox policy, capability probes, process trees, macOS Seatbelt, and Linux Bubblewrap + `seccomp-BPF` backends |
| `crates/cli` | `tars-cli` | `tars` unified binary (CLI subcommands + interactive TUI entry) |
| `crates/tui` | `tars-tui` | Native terminal interface library consumed by `tars-cli` |

Dependencies flow strictly one way (`tars-cli` → `tars-tui` → `tars-core` → `tars-sandbox`, with `tars-core` re-exporting `tars_core::sandbox`), and all workspace packages are `publish = false`.

**Causa dependency boundary:** The workspace (`tars-core`, `tars-cli`, and `tars-tui`; `tars-sandbox` depends on neither `causa` nor `tars-core`) consumes `causa 0.3` with the `runtime`, `providers`, and `extensions` features enabled (exact version locked in `Cargo.lock`) strictly through its public facade. TARS owns application policy—sessions, persistence, workspace tools, sandboxing, permission modes, compaction, and UIs—on top of Causa's `Context` and single-run execution primitives; general runtime or protocol improvements uncovered here are contributed upstream to Causa rather than mixing policy across the boundary.

## Documentation

- [Changelog](CHANGELOG.md) — release history and known limitations
- [Current implementation](docs/README.md) — code layout, module boundaries, and local verification checks
- [Shared configuration](docs/configuration.md) — TOML schema, presets, compaction, prelude/skills, external tools, and MCP
- [Workspace tools](docs/workspace-tools.md) — file safety, permission modes, command rules, platform sandboxes (Seatbelt/Bubblewrap), `run_code`, and record migration
- [Session lifecycle](docs/session-lifecycle.md) — session assembly, execution, saving, closing, and usage statistics
- [Session continuation](docs/session-continuation.md) — repairing interrupted tool batches
- [TUI guide](docs/tui.md) — terminal layout, keybindings, startup checks, and history display
- [config.example.toml](config.example.toml) & [mcp.example.json](mcp.example.json) — starter configuration examples

## Contributing

See [AGENTS.md](AGENTS.md) and [Local checks](docs/README.md#local-checks) for architecture conventions, layer boundaries, and verification commands. Local development workflow materials stay gitignored and are not part of the public repository.

## License

This project is released under the [MIT license](LICENSE).

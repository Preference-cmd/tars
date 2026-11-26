# Changelog

All notable changes to TARS will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Unified `tars` binary & TUI UX**: Single `tars` entry point combining interactive terminal mode (first-run setup wizard, `/doctor`, `/init`, `/new`, `/sessions`, `/agents`, `/mode`, `/compact`, `/summary`, `/tools`, `/status`, `/help`) with scriptable CLI subcommands (`run`, `session`, `tools`).
- **Multi-protocol `causa 0.3` runtime**: Support for `openai-chat`, `openai-responses`, and `anthropic` protocols with custom endpoints, environment-variable-only API keys, prompt caching (`CacheDirective::StablePrefix`), and bounded pre-`Done` mid-stream transient retry.
- **Workspace tools & platform sandboxing**: Built-in `list_directory`, `read_file`, `find_files` (`.gitignore`/`.ignore`-aware BFS discovery), `search_text`, `write_file`, `edit_file`, and `run_command` behind four permission modes (`read-only`, `ask`, `accept-edits`, `full-access`), read-before-write guards, layered command rules, macOS Seatbelt, and Linux Bubblewrap (`bwrap`) + `seccomp-BPF` sandboxes.
- **Consent-gated external `rg` resolution**: Default `[external_tools] rg = "auto"` checks a SHA-256-verified managed copy first, falls back to compatible `rg >= 14` on `PATH`, and prompts before downloading pinned ripgrep `15.2.0`.
- **Long-session compaction & recall**: Automatic and manual LLM summarization, zero-LLM micro-compaction (`micro-compact`) with cache-safety gates, temporary frame trimming fallback, persistent `recall` tool, and history expansion.
- **SQLite v3 store & multi-agent collaboration**: Embedded WAL SQLite store (`tars.db`) with OS advisory locks, `import-legacy` for `tars-session-v2.4` JSON files, unified prelude + multi-directory `load_skill` skills, and event-driven sub-agent delegation (`spawn_agent`, `message_agent`, `read_agent`, `cancel_agent`) with monotonic narrowing and `.tars/agents/*.md` workspace trust.
- **Opt-in programmatic tool calling (`run_code`)**: Sandboxed ES2020 QuickJS runtime for read-only multi-step orchestration across workspace tools, `recall`, and read-only MCP tools.
- **MCP client support**: Standalone `mcp.json` configuration with `${VAR}` expansion, stdio/HTTP transports, `McpToolAnnotations` classification, and per-tool approval policy.
- **Release pipeline & POSIX installer**: Added `[profile.dist]` in `Cargo.toml`, `.github/workflows/release.yml` (4-target native builds with `ubuntu:22.04` container glibc 2.35 baseline on Linux, whitelist archive checks, `SHA256SUMS.txt`, SLSA build provenance, and `workflow_dispatch` dry-run), and `install.sh` with mandatory SHA-256 verification and atomic replacement.

### Known limitations

- **Prebuilt releases**: Formal `v0.1.0` is not yet published; current `v0.1.0-rc.*` pre-release tags are for release-pipeline and installer verification, while building from source via Cargo remains the default until `v0.1.0` is published.
- **Windows support**: Windows workspace tools, sandbox backend (`run_command` / `search_text`), and Windows CI are not yet implemented or verified.
- **Desktop GUI**: Not yet built.
- **Distribution & signing**: No Homebrew tap, Cargo-binstall metadata, or Apple Developer ID signing / notarization yet.

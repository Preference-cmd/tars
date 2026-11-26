# Changelog

All notable changes to TARS will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **TUI rich-text GFM Markdown rendering (`pulldown-cmark` 0.13, `default-features = false`)**: Committed `Role::Assistant` messages (in the active transcript, expanded compaction history, and `/agents` read-only child views) render headings (`H1`–`H6`), nested inline styles (`bold`, `italic`, `strikethrough`, `code`), deduplicated links & image fallbacks, ordered/unordered/task lists with hanging-indent wrapping, nested blockquotes, adaptive horizontal rules, dynamic-width boxed tables (degrading cleanly to stacked cards below 20 columns), and framed code blocks using terminal-neutral styling, while isolating user prompts, tool summaries, system blocks, and raw XML as plain text and preserving `Rows` blocking-pool caching, `ScrollAnchor` stability, `O(1)` tail streaming `Preview`, and double-boundary untrusted text escaping.
- **Pure-Rust single-pass lexical syntax highlighter**: Zero-unsafe `O(n)` code-block and diff highlighter supporting 11 language groups (`rust`/`rs`, `typescript`/`ts`/`tsx`/`javascript`/`js`/`jsx`, `python`/`py`, `go`, `sh`/`bash`/`zsh`/`shell`, `json`/`jsonc`, `toml`, `yaml`/`yml`, `sql`, `c`/`cpp`/`c++`/`h`/`hpp`, and `diff`/`patch`), falling back linearly to plain text for unknown languages or blocks over `64 KiB`.
- **`/tools` & `/summary` detail panel readability**: `/tools` (F6) colors record headers and structurally expands and highlights unified diffs embedded in `write_file`/`edit_file` outputs (including lazily loaded spilled artifacts, immune to string-prefix spoofing) while staying strictly read-only; `/summary` renders the context summary chain as GFM Markdown.
- **Core & CLI session lifecycle (`rename`, `delete`, `fork`)**: Added `validate_session_title` (`MAX_SESSION_TITLE_CHARS = 160`, control-char rejection, explicit clear suppressing auto-derived titles), `SessionStore::{rename_session, delete_session, fork_session_with_lock_check}`, `ManagedSession::rename` (with in-flight save/compaction/close reconciliation), `SessionHost::{rename, delete, fork}`, and `tars session {rename, delete, fork}` CLI subcommands (`--json` included); `delete` unbinds `operations.parent_operation_id` for nested multi-agent cleanup while preserving `workspace_trust`, and `fork` creates physically isolated revision-1 clones (rewriting `HistoryRecord.conversation_id`, copying entry `artifacts`, resetting usage, and downgrading non-initial runtime `full-access` to `ask`) protected by `ForkGuard`.
- **TUI running-turn FIFO prompt queue & `/queue`**: Added per-session in-memory prompt queuing (`MAX_QUEUED_PROMPTS = 8`) during active turns with escaped footer preview (`Queued (N): <preview>`), automatic FIFO submission only after clean `completed` + saved + repair-free turns (without duplicating `input_history`), strict pause on cancellation/failure/timeout/`SaveBlocked`/needs-`/repair`, `/queue [list | pop | clear]`, and `/close`/`/quit` confirmation protection.
- **TUI `/rename`, `/fork`, and `/sessions` safe delete**: Added `/rename <title>` / `/rename --clear`, `/fork [title]` (opening and switching to the isolated fork while keeping the original intact), and `Ctrl-D` / `Delete` inside `/sessions` with `y`/`n` confirmation for unopened sessions (while keeping printable characters bound to `query` filtering).
- **TUI external `$EDITOR` editing (`Ctrl-G` / `/edit`)**: Resolves `$VISUAL` then `$EDITOR` without guessing fallbacks, tokenizes quotes/escapes and spawns directly via `Command::new` (no `sh -c`) against an exclusive `.md` temporary file closed before spawn, explicitly drops/recreates `EventStream` with unconditional terminal restoration, and enforces a `1 MiB` UTF-8 import limit while preserving the draft on errors or non-zero exits.

## [0.1.0] - 2026-11-26

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

### Fixed

- **External `rg` version probe on Linux**: Added a bounded retry for transient `ETXTBSY` (`ExecutableFileBusy`) errors when probing freshly written or installed `rg` binaries.

### Known limitations

- **Linux glibc baseline**: Prebuilt Linux release archives are built in `ubuntu:22.04` containers and require `glibc >= 2.35` (for example, Ubuntu 22.04+ or Debian 12+); older distributions should build from source.
- **Windows support**: Windows workspace tools, sandbox backend (`run_command` / `search_text`), prebuilt archives, and Windows CI are not yet implemented or verified.
- **Desktop GUI**: Not yet built.
- **Distribution & signing**: No Homebrew tap, Cargo-binstall metadata, or Apple Developer ID signing / notarization yet.

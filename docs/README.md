# Current implementation

TARS exists to validate and improve Causa; the application is the vehicle for validation, as described in [Purpose](../README.md#purpose). This page records only the current implementation.

2026-10-26: The shared SessionHost, frozen session definitions, the JSON Store, managed saving and the CLI are wired. `tars session create/run/list` can save a session and reopen the same session across processes; `tars run` uses the same host in transient mode. CLI and TUI prototype share the TOML configuration, and `tars-tui` has sessions, input, temporary streaming text preview, results, cancellation and save recovery wired; workspace tools (three read-only tools plus `write_file`/`edit_file`) and reliable status are wired; a write to an existing file must be preceded by a read, `ask` confirms in the terminal/confirmation panel and refuses in non-terminal contexts, while `accept-edits` and `full-access` do not ask; the four permission modes can be switched while running and are saved with the record, and `tars-tui` has the write confirmation panel, `/mode` command and `Shift-Tab` mode switching wired. MCP is wired through the standalone `mcp.json` configuration (with `${VAR}` expansion and literal-secret checks), preset/CLI/TUI selection, definition snapshots and tool calls through the built-in MCP client; MCP calls run, ask (in the terminal or the TUI confirmation panel) or are rejected according to the `approval` setting in `mcp.json` and the current permission mode. GUI, parent/child agents, installation and release remain in later scope.

2026-10-27: The sandbox module provides platform-independent policy, backend capability reports, process trees and the macOS Seatbelt backend; probes verify file, network, signal and credential boundaries. The layer is wired into `run_command` (macOS; permission category, definition/configuration and repair list are wired, Linux/Windows fail closed); complete documentation and real use are still step 3.

2026-10-28: Command tool status display is wired: the TUI `/status` shows the sandbox backend and each capability dimension, opening shows the notice when unavailable, and CLI `session create` prints the full status while running a command prints a short status; the TUI confirmation panel and the read-only command summary in the new-session form are covered. Tool-level file, network, signal, timeout/cancel/Drop tests and repair accept/never-rerun tests pass. The SC12 real-model `cargo test` hangs because the sandbox denies `~/.cargo/config.toml` and the Xcode SDK; the policy was not relaxed. The local cargo workaround and sandbox residuals are in [Workspace tools](workspace-tools.md#command-tool).

2026-10-29: The command sandbox changed to whole-machine read-only, credential locations and the current store directory still deny reads and writes, and `/dev` allows only four devices; `.git` was tightened from the whole tree read-only to hooks/config/modules/worktrees read-only with the entry itself not creatable, deletable or renamable, while objects/refs/index are writable again; `network = "open"` was tightened to IP egress plus the system DNS socket. Probes verify the Roots and Host read scopes separately. Same day, step 5: under `accept-edits`, sandboxed commands execute directly; a single command can request running outside the sandbox with a reason (`read-only` rejects, `ask`/`accept-edits` ask, `full-access` executes directly), and the execution layer checks the call permission before starting it through the backend as `unconfined`; `.git` locks only hooks, config, modules, worktrees and the entry. SC02, SC17 and SC18 are checked. [Workspace tools](workspace-tools.md#command-tool) is synchronized.

2026-10-31: The sandbox layer moved out of core into the standalone library package `crates/sandbox`, whose dependencies are only `tokio`, `thiserror`, `tempfile` and `rustix` on unix; it depends on neither Causa nor `tars-core`. `tars-core` depends on it and keeps the public `tars_core::sandbox` path in `lib.rs` through `pub use tars_sandbox as sandbox`; CLI/TUI do not depend on it directly. `exit_status_code` became a public function; backend injection, `RecordingBackend` and the function that opens Seatbelt with a given home directory moved into the `test-support` feature (enabled only by `tars-core`'s `[dev-dependencies]`; normal and release builds do not contain them). Sandbox unit tests moved with the package and the workspace test total is unchanged.

## Code structure

| Path | Current responsibility |
| --- | --- |
| `crates/core/src/application/` | Active session registry, opening/close supervision, instance view, save gate and versioned editing |
| `crates/core/src/definition/` | Serializable frozen definition and pure validation |
| `crates/core/src/config/` | Shared TOML loading, paths, named model presets and defaults for new definitions |
| `crates/core/src/workspace/`, `tools/` | Pinned root-directory handles, workspace tools (three read-only, two write and one command tool), bounded scanning and supervised file jobs |
| `crates/sandbox/src/` | Platform-independent sandbox policy, backend capability reports, process trees and the macOS Seatbelt backend; a standalone library package that depends on neither Causa nor `tars-core` and is used by `tars-core`'s `run_command` |
| `crates/core/src/mcp/` | A session's MCP servers: startup and shutdown, child-process environment, tool filtering, result limits and status |
| `crates/core/src/assembly/` | Resolve credentials on open, assemble gateway/runner and frozen execution options |
| `crates/core/src/session/` | Single-session owner/handle, original work acceptance, supervision, deduplication, reliable results and bounded transient observation |
| `crates/core/src/store/` | Private JSON backend, file lock while active, commit and confirmation |
| `crates/core/src/material/` | Explicit Context file import/export and reference validation |
| `crates/cli/src/cli/` | Command arguments, signals, presentation, recovery files and exit codes |
| `crates/cli/src/demo/` | A low-level Session demo with a scripted model and local tools |
| `crates/tui/src/` | Single-column conversation, command/session selection, input adaptation, streaming preview and actual material presentation |
| `crates/core/tests/`, `crates/cli/tests/` | Core contract consumers, real CLI and local HTTP tests |

The workspace contains `tars-core`, `tars-sandbox`, `tars-cli` and `tars-tui`, all with `publish=false`; the binaries are `tars` and `tars-tui`. Crate directory names omit the `tars-` prefix (`crates/core`, `crates/sandbox`, `crates/cli`, `crates/tui`) while package names stay unchanged. CLI/TUI depend only downward on the core and Causa (plus simple facts such as `BlockId`), and the core has no entry-point/UI/signal dependencies; `tars-core` depends one-way on the library package `tars-sandbox` and keeps the `tars_core::sandbox` path through `pub use tars_sandbox as sandbox`. The internal library makes no third-party SDK promise. It consumes the crates.io release `causa 0.1` through the public facade, with the exact version fixed by Cargo.lock, so builds need no sibling Causa checkout. No sources are included or copied. Independent CI/distribution is a later delivery step. Shared configuration and terminal boundaries are in [Configuration](configuration.md) and [TUI](tui.md).

Full behavior is in [Session lifecycle](session-lifecycle.md); usage is in the [README](../README.md). Per-item acceptance and local evidence live in the local design tree (`.agents/…`), which is not part of this repository.

## Shared application entry points

`SessionHost` exclusively owns the active registry and the underlying Session owners. `PreparedCreate` freezes identity before the async step, and `create/open` are registered and supervised by an independent task; dropping a wait or the `ManagedSession` view does not terminate the operation. `SessionTarget` distinguishes different open instances of the same ConversationId; an old target never acts on a reopened instance.

`ManagedSession` provides submit/observe/wait/cancel, subscribe_progress, status, versioned materials, atomic edit, wait_saved/retry_save and forget. The view does not expose the underlying owner/handle. The host provides explicit Save/DiscardLocal close and a single claim of the original uncommitted ToolBatch. Callers decide who may use the host; it is not a user authentication system.

Continuation after interruption handles materials explicitly through `inspect_repair/plan_repair/apply_repair`. The built-in tool assembly can accept the original result of a controlled interruption, and reopening without the original batch can remove unpaired declarations; new input first passes pairing admission, and new work starts only after the save is confirmed. CLI `session repair` and the TUI F7 key share this behavior; the full conditions, versions and retention boundaries are in [Continuation after interruption](session-continuation.md).

The frozen definition contains provider, model, endpoint, credential environment-variable reference, work deadline, retained capacity, model/tool call limits, plus workspace, tool selection, complete tool and write limits, initial permission mode and MCP snapshot. On open it is validated, resolved once and pinned to resources; creation and reopening do not call the model. Reopening keeps the definition and BlockIds, but work tables, request keys, WorkRefs, terminal states and uncommitted batches are not restored, and old work is never rerun automatically. The default tool catalog is empty; list_directory/read_file/search_text/write_file/edit_file/run_command can be enabled explicitly. Boundaries and record migration are in [Workspace tools](workspace-tools.md); local evidence lives in the local review tree (not in this repository).

The low-level `session` API is still used by the core and fixture consumers; the product CLI goes through the managed entry points. Its original contracts for deduplication, cancellation, typed results, batches and material return are unchanged.

## Streaming observation

The Session worker uses `TurnRunner::run_streaming`, while the original assembly observer still follows the synchronous borrow contract. The CLI keeps waiting and presents the final result; the TUI subscribes to `ManagedSession::subscribe_progress` before submit. Events are attributed by WorkRef and InvocationId and carry ModelStarted, body Text, ModelOutputReady and bounded tool status; tool status is also distinguished by declared BlockId. Currently neither reasoning nor unfinished tool arguments are shown.

The channel belongs to one open instance, has a capacity of 128 items, each body at most 16 KiB, truncated on UTF-8 boundaries with a `truncated` marker; bodies are not copied when there is no subscriber. Slow consumers receive Tokio broadcast Lagged and late subscribers get no history replay; subscribing or dropping an observer never cancels work or changes the final result. Event Arcs retained by the caller do not count against the channel capacity. Reliable terminal states still go through observe/wait and are separate from the losable transient preview.

The TUI processes at most 128 items every 50 ms, cycling through views; it keeps at most 256 KiB of body for the current work and current model round, and explicitly reports falling behind or exceeding the limit. End, failure, cancellation and close clear the preview, and the real conversation updates from the Context actually returned. ModelOutputReady only means the model delivered complete output; it does not mean materials were committed or saved successfully. The preview never enters the Store, material export or recovery files.

## Saving and closing

Execution terminal state and save state are independent facts. A persistent instance blocks new keys and edits after T1 is accepted until the save of the actual returned materials is confirmed; replay with the same key still works, and a conflicting key still returns Conflict. Finished can precede Saved. On failure the original result and pending snapshot are kept, and `retry_save` confirms/retries the same commit without calling the model. Versioned editing validates the candidate first; materials are unchanged when it is rejected, and a success saves automatically.

`close(Save)` waits for cancellation, material return and saving; if saving fails, the owner/lock are kept and saving can be retried. `DiscardLocal` waits for in-flight workers/I/O to stop, confirms the original pending commit operation and then abandons the local content without undoing records already on disk. CloseOutcome returns the facts: committed, uncommitted, not attempted or still unconfirmable, the last keeping the original error. Dropping the last host requests a supervised DiscardLocal cleanup; normal wait-for-settling still requires the Tokio runtime to stay alive, and a hard exit does not guarantee unsaved materials can be recovered.

The CLI persistent run waits for the save after success, failure and controlled interruption. During the save wait, the first Ctrl-C prints a notice on stderr, and the second stops waiting, exports the actual Context through the save-failure path and exits with an error. On save failure it follows the selected behavior: export the actual Context to a random `tars-recovery-<uuid>.json` that never overwrites an existing file, then exit with an error; export failure is reported separately. Transient runs export only with an explicit `--save`.

## Backend and resource boundaries

The local store directory is explicitly given by `--store DIR` or the shared TOML `store`; there is no automatically generated default store path. Records live at `DIR/sessions/<uuid>.json` in the `tars-session-v2` format; `workspace_root`, `tools` and `tool_limits` are required, while write limits, initial/current permission mode and the MCP snapshot may be omitted, in which case the default write limits, `ask` and no servers are read. v1 is not upgraded automatically; `session export-materials` can export the validated actual Context offline, and a new definition can then be used explicitly to create a session. The frozen definition, Context, ConversationId, revision and last_commit_id are written in one step; the initial revision is 1. Credential configuration stores only environment-variable names. User materials are saved as they are.

A stable `<uuid>.lock` file is separate from the replaceable record, and the OS exclusive lock is held from before reading until local worker/I/O stops. Busy, unsupported or I/O errors are all refused and never degrade to no lock. A commit validates the baseline and the full content, then uses a same-directory temporary file, content flush and atomic replacement; when confirmation is unclear the original operation is kept. list ignores locks and temporary files and reports corruption and active status per record; a listing is not an atomic snapshot of the whole directory.

The read/serialize limit for one record is 64 MiB; this is a file limit, not a memory budget. Recovery export does not truncate materials: files above the limit are still written completely, but the result explicitly reports that the current version cannot import them directly. The default host has at most 32 opening/active/blocked/closing slots and at most 4 concurrent store operations; each session has at most one pending commit. The CLI persistent definition retains 256 works by default, and the transient definition retains 1. Exhausted capacity is refused explicitly; there is no automatic eviction, unloading or work-history archiving.

Retention costs include the current Context, the original materials created at creation, the request's original input, terminal-state/unclaimed batch Arcs, pending and baseline copies, serialization buffers, and copies held by callers. Views/observers held after close may also retain the underlying work tables and materials; the active-slot limit does not bound these externally held objects. list must fully validate each record; scanning and memory still grow with the number of records. In sampled observations, peak RSS was about 6.9/24.9/158.7 MiB for 4.2 KiB/1.13 MiB/9.06 MiB material scenarios, and 93.8 MiB for 8 concurrently active medium sessions. This is a single local experiment and does not extrapolate to a throughput guarantee; reliable CPU cache metrics were not collected either. Raw sizes, latencies, copy/serialization and byte counts are in the local review records.

External media/artifact references are rejected by default. The explicit material extension interface can delegate reference validation to the caller, but the Store never copies assets automatically. The current verification scope is complete old/new records after a local process interruption; it does not promise directory-entry durability after power loss, network filesystem/sync-drive cross-host execution, or exactly-once remote side effects.

## Local checks

```sh
cargo fmt --all --check
cargo clippy --workspace --locked --all-targets -- -D warnings
cargo test --workspace --locked
cargo test -p tars-sandbox
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --locked --no-deps
cargo build -p tars-core --locked
cargo build --workspace --locked --release
./target/release/tars session --help
./target/release/tars-tui --help
./target/release/tars demo
# Local checkout only (the scripts/ tree is not part of this repository):
node scripts/verify-proposals.mjs --links
cargo deny check   # security advisories, licenses, sources; policy in deny.toml
```

Tests use local HTTP, scripted models/tools, real files, processes and deterministic barriers, with no live model credentials. Tests are split by platform: common cases run on Linux, macOS and Windows; workspace-tool and SIGINT cases compile only on Linux/macOS, and Seatbelt command cases compile only on macOS. Tool access is implemented only on Linux/macOS, with this round verified on macOS; Windows has no tool capability, and remote CI on each platform still needs its own platform evidence.

The local development workflow is in `.agents/pipelines/WORKFLOW.md`; `.agents/`, `scripts/` and `AGENTS.local.md` stay on the local machine and are not added to the repository or the application distribution.

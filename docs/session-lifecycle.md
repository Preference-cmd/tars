# Session assembly and execution

This page describes the current implementation as of 2026-10-05. Design and acceptance live in the local proposal tree, and the local tests and raw resource data live in the local implementation records (neither is part of this repository).

## 1. Creating and reopening

```text
CLI arguments → SessionDefinition + initial Context → PreparedCreate
  → SessionHost registers opening + starts independent supervision
  → persistent instance takes the stable file lock → reads and validates the latest complete record
  → assembly resolves credentials once and freezes gateway / runner / execution options
  → create: confirms initial revision 1; reopen: adopts the existing revision
  → publishes a ManagedSession bound to this SessionTarget
```

[Host](../crates/core/src/application/host.rs) registers and starts the supervision task while holding the directory lock, and concurrent calls with the same ID connect to the same instance. The ConversationId and initial commit_id for create are frozen by the synchronous PreparedCreate; a waiting caller leaving does not change them. A read/assembly failure followed by the task stopping removes the opening; an initial commit failure keeps a lookup-able InitializationBlocked with the original operation and publishes no committable view.

[Definition](../crates/core/src/definition/types.rs) is saveable data; only [assembly](../crates/core/src/assembly/provider.rs) resolves environment references. The definition is frozen in a live instance, and reopening resolves the same reference again; a connection never captures a particular WorkRef. Each work still receives options with its own identity, control and frozen policy. Construction and reopening send no model request.

## 2. Acceptance and execution

[ManagedSession::submit](../crates/core/src/application/managed.rs) first compares the complete arguments against the underlying unique request-key table, then checks the lifecycle and save gate. A retained key with the same arguments returns the original receipt, while different arguments return Conflict; a new key rejection returns the original request. New-work acceptance and the host active-slot registration happen inside the same application gate, so a worker that finishes first cannot let T2 pass the save.

The low-level [SessionHandle](../crates/core/src/session/handle.rs) allocates the TurnId, atomically appends the input, saves a copy of the original request and hands the Context to the worker. Each instance has at most one active work and no background submission queue. The work deadline starts at acceptance and covers per-work assembly and execution; it does not cover opening, stdin or disk I/O.

The [Worker](../crates/core/src/session/worker.rs) checks control, calls per-work assembly, and then hands off to the Causa public facade's `TurnRunner::run_streaming`. Causa handles the model/tool loop of one work and returns the actual Context, the typed TurnResult and the uncommitted ToolBatch; it does not own TARS directories, files or work history. Product assembly defaults to an empty tool catalog and can bind the six built-in workspace tools according to a frozen definition (three read-only, two write and one command tool; platform limits of the command tool are in [Workspace tools](workspace-tools.md)). Root resources are shared within an instance, while tool wrappers and the ToolReporter are created per WorkRef. Tool materials in an old Context never trigger a rerun of old tools.

## 3. Finished and Saved

The low-level supervisor waits on both normal and exceptional paths for that work's supervised file closures to actually exit, freezes the reliable tool state, then returns the materials and batch and publishes the original `Arc<WorkResult>`. Dropping a tool future requests that the operation stop but cannot replace real task reclamation. The host's [independent supervision path](../crates/core/src/application/supervisor.rs) waits until real materials are available, advances the MaterialVersion and fixes this immutable save operation.

```text
submit → Dirty → original WorkResult::Finished → freeze returned materials → Saving
                                                       ├─ confirmed → Saved → T2 can be accepted
                                                       └─ error → SaveBlocked
                                                                    ↓ retry_save
                                                               confirm/retry original commit
```

Save I/O runs in blocking tasks with a concurrency limit and does not hold the synchronous coordination lock. Waiters, cancellation, replay and observation remain responsive. Saved is a persistent fact; Finished only describes execution. A failed save never turns Completed into an execution failure; retrying never reruns the model. A transient instance stays NotTracked and still releases the active gate only after the materials are returned.

`read_materials` returns a copy of the actual idle materials with a version. `edit` validates against that MaterialVersion as the baseline and replaces all blocks in one step, rejecting old versions, old instances, Busy or unhandled saves; a valid persistent edit fixes a new snapshot and saves it, and arbitrary re-entrant edit callbacks are not accepted.

Material handling after interruption reuses the same edit/save gate: `inspect_repair` inspects read-only, `plan_repair` builds a non-rewritable candidate that passed protocol validation, and `apply_repair` checks the material version, open instance and original batch availability and commits once. take/forget invalidate existing plans. New-key pairing admission failure returns the original input without registering work, and retained-key replay still takes precedence. CLI/TUI operations and the original-result retention scope are in [Continuation after interruption](session-continuation.md).

## 4. JSON commits and confirmation

[Store](../crates/core/src/store/json.rs) is the concrete backend inside the core. The `tars-session-v2` record format saves ConversationId, revision, last_commit_id, definition and context; it accepts only canonical UUID filenames and never concatenates arbitrary input into a path. The definition's workspace_root, tools and tool_limits must be present in full; write limits, initial/current permission mode and the MCP snapshot may be omitted and are read with defaults when absent. Old v1 is rejected explicitly; offline material export is described in [Workspace tools](workspace-tools.md). Work tables, request keys, execution results, uncommitted batches, tasks and connections are not serialized. After Causa changed the serialized form of tool result status and artifact category to snake_case, all three paths that read stored Context (session records, material files and `export-materials`) go through [`material::vocabulary`](../crates/core/src/material/vocabulary.rs): it rewrites the old spellings of those two fields to the current spellings before decoding and leaves current data untouched, so the record format is not bumped; the next write uses the current spellings. After Causa 0.0.5 made the message role a fact of the block itself, the same place also rewrites the old `{"shape":"parts","value":[..]}` to `{"shape":"message","value":{"role":..,"parts":[..]}}`. The role follows what the old renderer inferred from `meta.source`: missing or `"assistant"` becomes `assistant`, `"system"` becomes `system` and everything else becomes `user`; the only exception is TARS's material-repair note (source `application:tars:material-repair`), which reads as `system`, matching how it is written now.

The writer holds the stable lock file; every in-flight blocking operation also holds it. A commit first reads the latest revision: the same commit_id requires identical complete content, otherwise the baseline must match. It writes a temporary file, syncs it and then replaces the final JSON. When the response is lost, it reads to confirm the original commit; only a complete old baseline with continuous identical write access can prove that nothing was committed. When the record has advanced or is unreadable it keeps the conflict/unconfirmed state and never generates a new identity or force-overwrites.

list validates the final records and reports bad JSON, format, identity, duplicate BlockIds, references and lock errors separately, without falling back to a default definition or an empty session. Temporary files never appear in listings, and cleanup requires the corresponding write access. Lock files are not deleted on close; externally deleting/replacing a lock file is not a supported coordination method.

## 5. Closing, faults and batches

The host's close checks the complete SessionTarget, marks Closing, closes new acceptance and requests cancellation, then waits for the real worker and I/O. Save releases after confirming the latest materials; on save failure the owner/write access is kept and a retry can continue closing. DiscardLocal also waits for in-flight writes to stop and only confirms the original pending commit without writing again. The returned CloseOutcome distinguishes Applied, NotApplied, NotSubmitted and Unresolved; an unconfirmed outcome keeps the original structured error, and close must never be treated as the record not existing. It does not roll back committed records. An opening can also be closed through its lookup target, in which case the original waiter never receives a committable handle; conflicting policy requests in progress are refused, and a blocked close with no in-flight operation may explicitly switch to DiscardLocal.

An old ManagedSession can only observe the facts it retained and never follows a redirected ConversationId. Dropping a view does not close the owner; dropping the last external host requests supervised cleanup and cannot replace an explicit await. Runtime or process exit does not guarantee a clean settling.

An owner/worker fault isolates new work, waits for confirmable task stops and keeps the fault and lock until it is explicitly handled. ContextUnavailable is reported truthfully, and an old disk version is never disguised as the materials returned by this run. The local file lock has no lease expiry while OS handles stay alive; external lock revocation and cross-host shared execution are not supported.

`SessionHost::take_uncommitted_tool_batch(target, work)` takes the original U once from the original owner inside the same coordination boundary, with no await, copy or tool execution. Arcs of observed results stay unchanged. forget is refused while an associated save is unhandled; after forget the request keys, terminal states and unclaimed batches are released while existing Arcs remain valid. An entry point that needs U must claim it before the final close; the Store never saves U additionally.

## 6. CLI consumers

`session create` only creates and confirms the initial record. `session run ID` reopens the saved definition, submits new work, waits for the independent save result and then closes; provider/model cannot be overridden transiently. `session list` needs no credential resolution. All of them require an explicit `--store`.

On save failure the CLI keeps the original execution status/text and additionally outputs save_error; according to the user's choice it exports a random, non-overwriting recovery Context and then exits 1. recovery_error, close_error and close_outcome are reported separately; a recovery file contains the actually returned materials, not the original work table or uncommitted content. After Ctrl-C cancels the work it still waits for the return and save; a second Ctrl-C during the save wait stops waiting, exports the actual Context and exits with an error.

`tars run` creates a transient host and still supports the original `--load/--save`: an import gets a new ConversationId, only an explicit `--save` exports Context, and the report and exit codes are unchanged. The low-level offline demo is a separate fixture used to observe the single-session contract.

## Transient streaming observation

`SessionHandle::subscribe_progress` and `ManagedSession::subscribe_progress` return this instance's broadcast receiver. Callers that need to observe new work subscribe before submit; each WorkProgress carries the WorkRef, InvocationId and ModelStarted, body Text or ModelOutputReady. Current limits, Lagged and truncation semantics are in [Current implementation](README.md#streaming-observation). The original `TurnRunOptions::observer` is composed with the call and never silently replaced.

These events are losable transient observations with no work-history or reopen-recovery meaning, and they are not material-editing instructions. Reliable WorkResult is still kept by the original work table; the synchronous observer does not wait for UI consumption. Only the Context returned by Causa enters the original save flow; text fragments before cancellation/EOF/error are never padded into complete model output. Model output being complete, work being terminal, and materials being saved remain distinct facts.

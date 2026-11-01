# Material handling and continuation after interruption

After a cancellation or deadline, tool declarations may already have entered Context while the actual result remains in the original work's uncommitted batch. TARS checks pairing before accepting new input; when unpaired declarations exist the original input is returned without appending materials or registering new work. Replay of an existing work with the same request key and arguments still takes precedence. A fully paired UnknownOutcome can continue to be used.

Material handling belongs to TARS's shared application layer, and CLI/TUI use the same checks and save gate. The whole flow is inspect, preview, apply explicitly, save, and then the user sends new input; it never restores old futures, executes old declarations or changes the original work's terminal state.

## Two handling options

| Choice | Conditions and result |
| --- | --- |
| Accept the original result | The same open instance still holds the original batch; the source must be a controlled cancellation or deadline of a formally assembled built-in tool. Keeps the original result's BlockId, status, output and notes, includes them into the materials in declaration order, and removes the remaining result-less declarations. |
| Remove unpaired declarations | Removes only declarations without a corresponding result and never fabricates a result. Available after reopening even without the original batch. Existing complete exchanges and other materials stay unchanged. |
| Keep the original materials | Merely close the preview; inspection and preview never edit, claim the batch, or request a model or tool. |

Removing a declaration only means excluding it from the materials of subsequent requests; it never proves the operation did not happen. Accepting an UnknownOutcome keeps the unknown state. New work never reruns historical declarations, but the model may propose new calls in a new response.

Accepting supports the single unfinished exchange at the tail of the materials, and the fixed six built-in tool sources (list_directory, read_file, search_text, write_file, edit_file, run_command). A batch containing MCP or other non-built-in calls cannot be accepted and can only have its unpaired declarations removed, because cancellation on the external server side is outside this application's control. Accepting an UnknownOutcome write call or removing an unpaired write declaration marks the possible-write notice and reminds the user to read the corresponding file again before continuing; an unknown outer-call result or a removed outer declaration marks the external-side-effect notice and reminds the user to check the corresponding server's state. Accepting an UnknownOutcome, TimedOut or Cancelled `run_command` result marks a "command may have executed" notice and reminds the user to check the workspace (also for sandbox-outside runs); Rejected results (including commands not started while queued) can be accepted as usual without that notice. A command that was still queued when cancelled started no process but may settle as UnknownOutcome and trigger the notice; this is a conservative boundary and does not mean the command really executed. Removing an unpaired `run_command` declaration shows the same notice. After applying the repair and saving, new work runs only the calls the model proposes anew; historical commands are never rerun. Automatic acceptance is refused for custom assembly, handler failures, rejected results, commit collisions, complex intermediate exchanges and unsupported media/assets; orphan results, duplicate results or ambiguous wire IDs show a specific diagnosis. Rejected results normally returned by a handler (precheck, mode rejection, user rejection, no confirmation channel) are ordinary results in the batch and can be accepted under a controlled cancellation or deadline. An abnormal work whose Context has been lost can only report the fault and cannot manufacture the materials returned by this run from old disk content.

Retained blocks keep their IDs, content, metadata and order unchanged. The candidate appends an `application:tars:material-repair` note after the valid exchange, listing the IDs of removed declarations and accepted results and the non-execution meaning. This note is application material, written with the `system` role, and all three protocols render it in place as a system message so the model never treats it as a user turn. The candidate must pass Context identity validation and the selected protocol's public pure-rendering entry point in Causa; local structural validation does not guarantee that the live service accepts every request argument.

## TUI

1. After the original work settles, open the material panel with F7 or `/repair`; opening it early waits for the original work and saving without requesting cancellation.
2. Inspect the diagnosis and original result summary. Press `a` to preview accepting the original result, or `r` to preview removing unpaired declarations.
3. Check the identities to be removed/accepted, and press Enter to apply that preview and wait for saving.
4. Esc returns to the kept draft; new work is created only when sent explicitly. The original Cancelled/TimedOut state is kept.

Switching sessions with Esc or F4 releases the current preview; an already-started apply/save continues under supervision. Refreshing with F7 discards the old preview. On save failure, Ctrl-R retries the same commit and Ctrl-E exports the actual current Context. Viewing or closing the panel never claims the original batch. Each instance has at most one material-handling task, and the whole UI holds at most one candidate; redrawing and resizing only read cached summary lines.

## CLI

```sh
# Dry-run by default; reads the saved definition and persistent revision, shows the candidate and changes nothing
tars session repair "$SESSION_ID" --store ./tars-data --json

# N must come from the preview above; the persistent revision is validated again under the write lock
tars session repair "$SESSION_ID" --store ./tars-data \
  --apply --expected-revision N --json

# After repairing and saving, submit new work separately
tars session run "$SESSION_ID" --store ./tars-data --input 'Continue analyzing'
```

`--dry-run` may be given explicitly and is mutually exclusive with `--apply`. apply requires a positive integer `--expected-revision` and refuses when the revision does not match. The command uses the saved definition and still needs to resolve its credential reference and workspace; inspection, planning and applying themselves send no model request.

A separate process cannot recover the previous instance's uncommitted batch, so the CLI offers only the removal mode. JSON reports diagnosis, plan, applied, saved, revision and save/export/close errors separately. When no repair is needed it returns `not_needed` and creates no new revision. On save failure it exports the actual Context already applied, confirms the close outcome and exits 1; the original error and the confirmed facts are retained separately. Text mode shows at most 128 declaration IDs, and JSON provides a bounded structured summary.

## Core and retention boundaries

`ManagedSession::inspect_repair(work)` returns the revision and a bounded diagnosis, `plan_repair(RepairMode)` returns a candidate that cannot be rewritten, and `apply_repair(&plan)` atomically applies that same candidate. A plan is bound to the open instance, the material generation and the batch claim/forget revision; material changes, reopening, a successful take/forget and repeated application all invalidate an old plan. Busy, Closing, Faulted and unconfirmed saves refuse application; after a persistent apply, `wait_saved` must be awaited, and retrying the save never executes the edit again.

Inspection and preview do not consume the original U; applying also never takes/forgets automatically. The original batch still represents the facts from when the old execution returned and exists only for the original owner's defined retention period. The Store, Context export and recovery files never additionally save the original U, work tables or request keys. Results accepted into Context are saved as ordinary materials; results kept only in U cannot be queried after reopening.

New-input admission uses a linear pairing scan and does not fully render the request on the synchronous input path. The selected protocol renderer is called only when inspecting existing tool history and building the candidate; copying, candidate construction and rendering happen in a TUI background task. Displays show at most 128 diagnoses/identities/results, each diagnosis at most 512 bytes and each original-result JSON summary at most 1024 bytes, while totals are preserved; the candidate and note still contain the complete identity list.

A plan holds a complete copy of the materials, and applying and saving need additional copies. The Store's 64 MiB limit is a file limit, not a memory budget. A single local macOS debug sample: about 9.1 MiB of materials with 8200 blocks, scan 0.42 ms, copy 1.83 ms, candidate 10.44 ms, independent render 5.30 ms, apply 4.28 ms, save wait 466.79 ms, process peak RSS 147.8 MiB. Releasing a plan does not mean the allocator immediately returns OS pages; there are no CPU cache metrics or maximum-scale throughput guarantees. The raw samples, three-protocol consumers, terminal and fault evidence live in the local review records (not in this repository).

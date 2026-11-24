use super::host::{
    BuiltinBindings, ReadOnlySubcallProvider, SandboxToolDescriptor, SubcallError, SubcallHost,
};
use super::sandbox::{bound_output_payload, run_quickjs_blocking};
use super::tool::RunCodeTool;
use crate::application::{ClosePolicy, ManagedSession, PreparedCreate, SessionHost, text_input};
use crate::compact::spill::SPILL_THRESHOLD_TOKENS;
use crate::definition::{
    CodeExecutionSettings, CommandSettings, CredentialRef, Provider, SessionDefinition,
    WorkspaceTool, WorkspaceToolLimits,
};
use crate::permission::{PermissionDecisionKind, PermissionMode};
use crate::prelude::PreludeDefinition;
use crate::session::{ToolPhase, ToolReporter, WorkObservation, WorkRef, WorkState};
use crate::tools::{ManagedTool, ToolResources, WorkspaceTools};
use async_trait::async_trait;
use causa::kernel::{
    BlockContent, BlockId, CallControl, Context, InvocationId, ModelGateway, ModelInvokeError,
    ModelItem, ModelOutput, ModelRequest, ModelResponse, ModelStopReason, RoundId, Tool, ToolBatch,
    ToolCallContext, ToolCallDraft, ToolCallPayload, ToolResultStatus, TurnId,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

enum ScriptStep {
    ToolCalls(Vec<(&'static str, Value)>),
    Answer(&'static str),
}

struct ScriptedGateway {
    steps: Mutex<VecDeque<ScriptStep>>,
    seen_tool_names: Mutex<Vec<Vec<String>>>,
}

impl ScriptedGateway {
    fn new(steps: Vec<ScriptStep>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps.into()),
            seen_tool_names: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl ModelGateway for ScriptedGateway {
    async fn invoke(
        &self,
        request: &ModelRequest,
        _control: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        let names = causa::kernel::current_tools(&request.frame.blocks)
            .into_iter()
            .map(|t| t.name)
            .collect::<Vec<_>>();
        self.seen_tool_names.lock().unwrap().push(names);
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(ScriptStep::Answer("done"));
        match step {
            ScriptStep::ToolCalls(calls) => {
                let items = calls
                    .into_iter()
                    .map(|(name, args)| ModelItem::tool_call(ToolCallDraft::new(name, args)))
                    .collect();
                Ok(ModelOutput::new(
                    ModelResponse { items },
                    ModelStopReason::ToolUse,
                ))
            }
            ScriptStep::Answer(text) => Ok(ModelOutput::new(
                ModelResponse {
                    items: vec![ModelItem::text(text)],
                },
                ModelStopReason::EndTurn,
            )),
        }
    }
}

fn base_definition(root: Option<&Path>, tools: Vec<WorkspaceTool>) -> SessionDefinition {
    let command = tools
        .contains(&WorkspaceTool::RunCommand)
        .then(CommandSettings::default);
    SessionDefinition {
        provider: Provider::OpenAiChat,
        model: "ptc-fixture".into(),
        endpoint: None,
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(30),
        retained_work_capacity: 8,
        max_model_rounds: 6,
        max_tool_calls: 16,
        max_tokens: None,
        reasoning_effort: None,
        temperature: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: root.map(|p| p.canonicalize().unwrap()),
        tools,
        tool_limits: WorkspaceToolLimits::default(),
        write_limits: None,
        permission_mode: None,
        command,
        mcp_servers: Vec::new(),
        context_window: None,
        auto_compact: true,
        compact_threshold: 70,
        prompt_cache: true,
        prelude: PreludeDefinition::default(),
        code_execution: Some(CodeExecutionSettings::default()),
    }
}

fn test_reporter(calls: &[ToolCallContext]) -> ToolReporter {
    let turn_id = TurnId::new(uuid::Uuid::now_v7().to_string());
    let reporter = ToolReporter::new(
        WorkRef {
            conversation_id: "ptc-test".into(),
            turn_id: turn_id.clone(),
        },
        tokio::sync::broadcast::channel(128).0,
    );
    if !calls.is_empty() {
        reporter.batch_ready(
            &InvocationId {
                turn_id,
                round_id: RoundId(0),
            },
            &ToolBatch::new(calls.to_vec()).unwrap(),
        );
    }
    reporter
}

fn make_call(name: &str, arguments: Value) -> ToolCallContext {
    ToolCallContext::new(
        BlockId::new(uuid::Uuid::now_v7()),
        ToolCallPayload::new(name, arguments),
    )
}

async fn finish_work(session: &ManagedSession, key: &str) -> WorkObservation {
    let receipt = session
        .submit(crate::session::SubmitRequest {
            request_key: key.into(),
            blocks: vec![text_input(key)],
        })
        .unwrap();
    let obs = session
        .wait(&receipt.work, Duration::from_secs(10))
        .await
        .unwrap();
    assert!(
        matches!(&obs.state, WorkState::Finished(_)),
        "obs: {:?}",
        obs.state
    );
    session.wait_saved().await.unwrap();
    obs
}

fn find_tool_result(
    session: &ManagedSession,
    call_block_id: BlockId,
) -> causa::kernel::ToolResultPayload {
    session
        .read_materials()
        .unwrap()
        .context
        .blocks()
        .iter()
        .find_map(|block| match block.content() {
            BlockContent::ToolResult(payload) if payload.call_block_id == call_block_id => {
                Some(payload.clone())
            }
            _ => None,
        })
        .expect("tool result block")
}

struct DummyReadOnlyProvider {
    delay: Duration,
}

#[async_trait]
impl ReadOnlySubcallProvider for DummyReadOnlyProvider {
    async fn list_tools(&self) -> Vec<SandboxToolDescriptor> {
        vec![SandboxToolDescriptor {
            name: "dummy_read".into(),
            description: "dummy read".into(),
            parameters: json!({"type":"object"}),
            source: "mcp".into(),
        }]
    }

    async fn describe_tool(&self, name: &str) -> Option<SandboxToolDescriptor> {
        (name == "dummy_read").then(|| SandboxToolDescriptor {
            name: "dummy_read".into(),
            description: "dummy read".into(),
            parameters: json!({"type":"object"}),
            source: "mcp".into(),
        })
    }

    async fn call_tool(
        &self,
        _name: &str,
        arguments: Value,
        _control: &CallControl,
    ) -> Result<Value, SubcallError> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        Ok(json!({ "echo": arguments }))
    }
}

#[test]
fn prepare_validates_code_parameter_and_rejects_unknown_fields() {
    let bindings = BuiltinBindings::from_session(&[WorkspaceTool::ReadFile], false);
    let host = SubcallHost::new(CodeExecutionSettings::default(), bindings, None, None);
    let reporter = test_reporter(&[]);
    let tool = RunCodeTool::new(host, CodeExecutionSettings::default(), reporter);

    assert_eq!(
        tool.classify(&json!({ "code": "return 1;" })).class,
        crate::policy::ToolPermissionClass::KnownReadOnly
    );

    let rt = tokio::runtime::Runtime::new().unwrap();
    let ctrl = CallControl::new(CancellationToken::new(), None);
    let mut batch = crate::tools::BatchPrepareState::new();

    for bad_args in [
        json!({}),
        json!({ "code": "" }),
        json!({ "code": "   \n\t " }),
        json!({ "code": 123 }),
        json!({ "code": "return 1;", "extra": true }),
        json!({ "code": "a\0b" }),
        json!({ "code": "x".repeat(64 * 1024 + 1) }),
    ] {
        let call = make_call("run_code", bad_args);
        let res = rt.block_on(tool.prepare(&call, &mut batch, &ctrl));
        assert!(res.is_err());
    }

    let ok_call = make_call("run_code", json!({ "code": "return 42;" }));
    assert!(
        rt.block_on(tool.prepare(&ok_call, &mut batch, &ctrl))
            .is_ok()
    );
}

#[tokio::test]
async fn assemble_omits_run_code_when_zero_readonly_tools_are_available() {
    let dir = tempfile::tempdir().unwrap();
    // Transient session (no recall) with no tools, and with write/command tools only -> 0 read-only tools.
    for tools in [
        vec![],
        vec![WorkspaceTool::WriteFile, WorkspaceTool::EditFile],
    ] {
        let gw = ScriptedGateway::new(vec![ScriptStep::Answer("ok")]);
        let host = SessionHost::new(None);
        host.set_test_gateway(Some(gw.clone()));
        let mut def = base_definition(Some(dir.path()), tools);
        def.permission_mode = Some(PermissionMode::ReadOnly);
        let session = host
            .create(PreparedCreate::transient(def, Context::default()))
            .await
            .unwrap();
        finish_work(&session, "req1").await;
        {
            let seen = gw.seen_tool_names.lock().unwrap();
            assert!(
                !seen[0].iter().any(|n| n == "run_code"),
                "run_code must not be advertised when 0 readonly tools are bound: {:?}",
                seen[0]
            );
        }
        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ptc03_four_permission_modes_run_without_approval_and_zero_ambient_capabilities() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "world\n").unwrap();
    let skill_dir = dir.path().join("skills").join("sample-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: sample-skill\ndescription: sample\n---\nbody\n",
    )
    .unwrap();

    for mode in [
        PermissionMode::ReadOnly,
        PermissionMode::Ask,
        PermissionMode::AcceptEdits,
        PermissionMode::FullAccess,
    ] {
        let js = r#"
const f = await read_file({ path: "hello.txt" });
return {
    text: f.text.trim(),
    write_file: typeof write_file,
    tools_write_file: typeof tools.write_file,
    edit_file: typeof edit_file,
    tools_edit_file: typeof tools.edit_file,
    run_command: typeof run_command,
    tools_run_command: typeof tools.run_command,
    run_code: typeof run_code,
    tools_run_code: typeof tools.run_code,
    load_skill: typeof load_skill,
    tools_load_skill: typeof tools.load_skill,
    recall: typeof recall,
    process: typeof process,
    require: typeof require,
    fetch: typeof fetch,
};
"#;
        let gw = ScriptedGateway::new(vec![
            ScriptStep::ToolCalls(vec![("run_code", json!({ "code": js }))]),
            ScriptStep::Answer("ok"),
        ]);
        let host = SessionHost::new(None);
        host.set_test_gateway(Some(gw));
        let mut tools = vec![
            WorkspaceTool::ReadFile,
            WorkspaceTool::WriteFile,
            WorkspaceTool::EditFile,
        ];
        if cfg!(target_os = "macos") {
            tools.push(WorkspaceTool::RunCommand);
        }
        let mut def = base_definition(Some(dir.path()), tools);
        def.permission_mode = Some(mode);
        def.prelude.skill_dirs = vec![dir.path().join("skills")];
        let session = host
            .create(PreparedCreate::transient(def, Context::default()))
            .await
            .unwrap();
        let mut approvals = session.connect_approvals();

        let obs = finish_work(&session, &format!("mode-{mode}")).await;
        assert!(
            approvals.try_recv().is_err(),
            "run_code must never prompt for approval in mode {mode}"
        );
        assert_eq!(obs.tools.calls.len(), 1);
        let progress = &obs.tools.calls[0];
        assert_eq!(progress.tool_name, "run_code");
        assert_eq!(progress.phase, ToolPhase::Committed);
        assert_eq!(progress.status, Some(ToolResultStatus::Succeeded));
        assert_eq!(
            progress.permission_decision.as_ref().map(|d| d.kind),
            Some(PermissionDecisionKind::DirectExecution)
        );

        let result = find_tool_result(&session, progress.call_block_id);
        let parsed: Value =
            serde_json::from_str(result.output.content["output"].as_str().unwrap()).unwrap();
        assert_eq!(parsed["text"], "world");
        for forbidden in [
            "write_file",
            "tools_write_file",
            "edit_file",
            "tools_edit_file",
            "run_command",
            "tools_run_command",
            "run_code",
            "tools_run_code",
            "load_skill",
            "tools_load_skill",
            "recall",
            "process",
            "require",
            "fetch",
        ] {
            assert_eq!(parsed[forbidden], "undefined", "{forbidden} leaked");
        }
        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ptc04_reuses_workspace_security_observations_and_keeps_snapshot_clean() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/target.txt"), "before line\n").unwrap();
    std::fs::write(dir.path().join("src/other.txt"), "other line\n").unwrap();

    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret\n").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret.txt"),
        dir.path().join("escape_link.txt"),
    )
    .unwrap();

    // Verify path escape, symlink escape, observations update, and ToolProgressSnapshot count.
    let js = r#"
const list = await list_directory(".");
let parentRejected = false;
try { await read_file("../etc/passwd"); } catch (e) { parentRejected = true; }
let symlinkRejected = false;
try { await read_file("escape_link.txt"); } catch (e) { symlinkRejected = true; }
const readTarget = await read_file("src/target.txt");
for (let i = 0; i < 8; i++) {
    await read_file("src/other.txt");
}
return {
    entries: list.entries.length,
    parentRejected,
    symlinkRejected,
    targetText: readTarget.text.trim(),
};
"#;
    let gw = ScriptedGateway::new(vec![
        ScriptStep::ToolCalls(vec![("run_code", json!({ "code": js }))]),
        // Next round: direct edit_file on src/target.txt WITHOUT direct read_file!
        ScriptStep::ToolCalls(vec![(
            "edit_file",
            json!({
                "path": "src/target.txt",
                "edits": [{ "old_text": "before line", "new_text": "after line" }],
            }),
        )]),
        ScriptStep::Answer("edited"),
    ]);
    let host = SessionHost::new(None);
    host.set_test_gateway(Some(gw));
    let mut def = base_definition(
        Some(dir.path()),
        vec![
            WorkspaceTool::ListDirectory,
            WorkspaceTool::ReadFile,
            WorkspaceTool::EditFile,
        ],
    );
    def.permission_mode = Some(PermissionMode::AcceptEdits);
    let session = host
        .create(PreparedCreate::transient(def, Context::default()))
        .await
        .unwrap();

    let obs = finish_work(&session, "obs-sync").await;
    // Must contain only the 2 top-level tool declarations (`run_code` and `edit_file`),
    // NOT the 11 inner subcalls!
    assert_eq!(obs.tools.calls.len(), 2, "calls: {:?}", obs.tools.calls);
    assert_eq!(obs.tools.calls[0].tool_name, "run_code");
    assert_eq!(obs.tools.calls[1].tool_name, "edit_file");
    assert_eq!(obs.tools.calls[1].status, Some(ToolResultStatus::Succeeded));

    let rc_res = find_tool_result(&session, obs.tools.calls[0].call_block_id);
    let rc_out: Value =
        serde_json::from_str(rc_res.output.content["output"].as_str().unwrap()).unwrap();
    assert_eq!(rc_out["parentRejected"], true);
    assert_eq!(rc_out["symlinkRejected"], true);
    assert_eq!(rc_out["targetText"], "before line");
    assert_eq!(rc_res.output.content["stats"]["subcalls"], 12);

    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/target.txt")).unwrap(),
        "after line\n"
    );

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ptc04_search_text_subcall_reuses_sandboxed_search_runtime() {
    use crate::sandbox::test_support::RecordingBackend;
    use crate::sandbox::{BackendId, BackendReport, Capability, Limitation, Sandbox};
    use crate::tools::SearchRuntime;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("alpha.rs"), "pub fn needle_fn() {}\n").unwrap();

    let rg = dir.path().join("fake-rg");
    std::fs::write(&rg, "#!/bin/sh\nexit 0\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&rg, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let backend = RecordingBackend::available(BackendReport::new(
        BackendId::RECORDING,
        Capability::Full,
        Capability::Full,
        Capability::Full,
        Capability::Full,
        Capability::Partial(Limitation::ProcessGroupEscape),
    ));
    let launches = backend.calls();
    let sandbox = Sandbox::open_with(Box::new(backend)).unwrap();
    let ws = crate::workspace::Workspace::open(&dir.path().canonicalize().unwrap()).unwrap();
    let search_rt =
        SearchRuntime::bind_with_parts(ws.clone(), dir.path(), Ok(rg), Ok(sandbox)).unwrap();
    let wt = WorkspaceTools::with_search(
        ws,
        WorkspaceToolLimits::default(),
        Default::default(),
        ToolResources::new(8),
        Some(search_rt),
    );
    let host = SubcallHost::new(
        CodeExecutionSettings::default(),
        BuiltinBindings::from_session(&[WorkspaceTool::SearchText], false),
        Some(wt),
        None,
    );
    let call = make_call(
        "run_code",
        json!({
            "code": r#"
let escaped = false;
try {
    await tools.search_text({ path: "../outside", query: "needle_fn" });
} catch (e) {
    escaped = true;
}
const res = await tools.search_text({ path: ".", query: "needle_fn" });
return { escaped, complete: res.complete, isArray: Array.isArray(res.matches) };
"#
        }),
    );
    let reporter = test_reporter(std::slice::from_ref(&call));
    let tool = RunCodeTool::new(host, CodeExecutionSettings::default(), reporter.clone());
    let payload = tool
        .execute(
            &call,
            &CallControl::new(CancellationToken::new(), None),
            None,
        )
        .await;
    reporter.drain().await;
    assert_eq!(
        payload.status,
        ToolResultStatus::Succeeded,
        "{:?}",
        payload.output.content
    );
    let parsed: Value =
        serde_json::from_str(payload.output.content["output"].as_str().unwrap()).unwrap();
    assert_eq!(parsed["escaped"], true);
    assert_eq!(parsed["complete"], true);
    assert_eq!(parsed["isArray"], true);
    assert_eq!(launches.lock().unwrap().len(), 1);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ptc05_recall_and_spill_processor_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("store");
    let ws_dir = dir.path().join("ws");
    std::fs::create_dir_all(&ws_dir).unwrap();

    // > 15 KiB file exceeds SPILL_THRESHOLD_TOKENS (4,000 tokens ~= 12,000 bytes).
    let big_line = format!("MARKER_HEAD_{}_MARKER_TAIL\n", "a".repeat(16_000));
    std::fs::write(ws_dir.join("big.txt"), &big_line).unwrap();
    assert!(
        crate::compact::estimate::estimate_bytes(big_line.len() as u64) > SPILL_THRESHOLD_TOKENS
    );

    // Work 1: inner read_file reads big.txt (must NOT spill to .artifacts/), returns short summary.
    // Work 2: outer run_code returns >15 KiB string (MUST spill to .artifacts/ while keeping stats).
    let gw = ScriptedGateway::new(vec![
        ScriptStep::ToolCalls(vec![(
            "run_code",
            json!({
                "code": r#"
const f = await read_file({ path: "big.txt", start_line: 1, max_lines: 10 });
return "len=" + f.text.length + ",head=" + f.text.slice(0, 11);
"#
            }),
        )]),
        ScriptStep::Answer("work1 done"),
        ScriptStep::ToolCalls(vec![(
            "run_code",
            json!({
                "code": r#"
const f = await read_file({ path: "big.txt", start_line: 1, max_lines: 10 });
return f.text;
"#
            }),
        )]),
        ScriptStep::Answer("work2 done"),
    ]);

    let host = SessionHost::new(Some(store_dir.clone()));
    host.set_test_gateway(Some(gw));
    let mut def = base_definition(Some(&ws_dir), vec![WorkspaceTool::ReadFile]);
    def.permission_mode = Some(PermissionMode::ReadOnly);
    let session = host
        .create(PreparedCreate::persistent(def, Context::default()))
        .await
        .unwrap();
    let conv_id = session.target().conversation_id().as_str().to_owned();

    // Work 1: inner read_file does not trigger OutputSpillProcessor.
    let obs1 = finish_work(&session, "w1").await;
    let res1 = find_tool_result(&session, obs1.tools.calls[0].call_block_id);
    assert!(res1.output.artifact.is_none());
    assert!(
        res1.output.content["output"]
            .as_str()
            .unwrap()
            .contains("MARKER_HEAD"),
        "got {:?}",
        res1.output.content
    );
    let artifact_count_after_w1 =
        crate::application::tests::support::artifact_row_count(&store_dir, &conv_id);
    assert_eq!(
        artifact_count_after_w1, 0,
        "inner read_file must not spill into artifacts"
    );

    // Work 2: outer run_code output > SPILL_THRESHOLD_TOKENS spills cleanly and retains stats.
    let obs2 = finish_work(&session, "w2").await;
    let res2 = find_tool_result(&session, obs2.tools.calls[0].call_block_id);
    let artifact = res2
        .output
        .artifact
        .expect("outer run_code output must spill");
    assert!(artifact.id.starts_with("artifact:"));
    assert_eq!(res2.output.content["stats"]["subcalls"], 1);
    assert_eq!(res2.output.content["truncated"], false);
    assert!(
        res2.output.content["output"]
            .as_str()
            .unwrap()
            .contains(&artifact.id)
    );

    // Work 3: script inside run_code can call `await recall({ id: artifact.id })` to read full text.
    // Re-attach a new gateway? Instead we can enqueue Work 3 via a fresh session reopen or include Work 3 in gw!
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    let artifact_id = artifact.id.clone();
    let recall_js = format!(
        r#"const full = await recall({{ id: "{artifact_id}", offset: 0, limit: 12000 }});
const rest = await recall({{ id: "{artifact_id}", offset: 12000, limit: 12000 }});
return {{ len: (full + rest).length, has_head: full.includes("MARKER_HEAD"), has_tail: rest.includes("MARKER_TAIL") }};"#
    );
    let gw2 = ScriptedGateway::new(vec![
        ScriptStep::ToolCalls(vec![("run_code", json!({ "code": recall_js }))]),
        ScriptStep::ToolCalls(vec![(
            "recall",
            json!({ "id": artifact.id, "offset": 0, "limit": 12000 }),
        )]),
        ScriptStep::Answer("recalled"),
    ]);
    let host2 = SessionHost::new(Some(store_dir));
    host2.set_test_gateway(Some(gw2));
    let reopened = host2
        .open(session.target().conversation_id().clone())
        .await
        .unwrap();
    let obs3 = finish_work(&reopened, "w3").await;
    let res3 = find_tool_result(&reopened, obs3.tools.calls[0].call_block_id);
    assert_eq!(res3.output.content["stats"]["by_tool"]["recall"], 2);
    let parsed: Value =
        serde_json::from_str(res3.output.content["output"].as_str().unwrap()).unwrap();
    assert_eq!(parsed["has_head"], true);
    assert_eq!(parsed["has_tail"], true);

    let ext_recall = find_tool_result(&reopened, obs3.tools.calls[1].call_block_id);
    assert_eq!(ext_recall.status, ToolResultStatus::Succeeded);
    assert!(
        ext_recall
            .output
            .content
            .as_str()
            .unwrap()
            .contains("MARKER_HEAD")
    );

    host2
        .close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ptc06_promise_all_concurrency_enforces_max_concurrency() {
    let settings = CodeExecutionSettings {
        max_concurrency: 2,
        ..CodeExecutionSettings::default()
    };
    let host = SubcallHost::new(settings, BuiltinBindings::default(), None, None)
        .with_extra_provider(Arc::new(DummyReadOnlyProvider {
            delay: Duration::from_millis(40),
        }));
    // Call 4 concurrent subcalls using a small helper exposed via extra_provider?
    // Wait: extra_provider isn't registered on globalThis unless bound or called directly;
    // let's also test real WorkspaceTools Promise.all below on Unix!
    let sess = host.new_run_session();
    let ctrl = CallControl::new(CancellationToken::new(), None);
    let futs = (0..4).map(|i| sess.call_tool("dummy_read", json!({ "i": i }), &ctrl));
    let results = futures_util::future::join_all(futs).await;
    assert!(results.iter().all(|r| r.is_ok()));
    let stats = sess.stats();
    assert_eq!(stats.subcalls, 4);
    assert_eq!(stats.max_in_flight, 2);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ptc06_promise_all_real_workspace_concurrency_and_bounds() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "a\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "b\n").unwrap();
    std::fs::write(dir.path().join("c.txt"), "c\n").unwrap();

    let ws = crate::workspace::Workspace::open(&dir.path().canonicalize().unwrap()).unwrap();
    let wt = WorkspaceTools::with_write_limits(
        ws,
        WorkspaceToolLimits {
            max_concurrent_operations: 4,
            ..Default::default()
        },
        Default::default(),
        ToolResources::new(4),
    );
    struct ReleaseGateOnDrop(Arc<crate::tools::execute::TestGate>);
    impl Drop for ReleaseGateOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    let gate = crate::tools::execute::TestGate::new(false);
    *wt.gate.lock().unwrap() = Some(gate.clone());
    let _release_guard = ReleaseGateOnDrop(gate.clone());

    let settings = CodeExecutionSettings {
        max_concurrency: 2,
        ..CodeExecutionSettings::default()
    };
    let host = SubcallHost::new(
        settings,
        BuiltinBindings::from_session(&[WorkspaceTool::ReadFile], false),
        Some(wt.clone()),
        None,
    );
    let call = make_call(
        "run_code",
        json!({
            "code": r#"
const [a, b, c] = await Promise.all([
    read_file("a.txt"),
    read_file("b.txt"),
    read_file("c.txt"),
]);
return [a.text.trim(), b.text.trim(), c.text.trim()].join(",");
"#
        }),
    );
    let reporter = test_reporter(std::slice::from_ref(&call));
    let tool = Arc::new(RunCodeTool::new(host, settings, reporter.clone()));
    let tool_clone = Arc::clone(&tool);
    let call_clone = call.clone();
    let exec_handle = tokio::spawn(async move {
        tool_clone
            .execute(
                &call_clone,
                &CallControl::new(CancellationToken::new(), None),
                None,
            )
            .await
    });

    // Wait for 2 concurrent read_file workers to hit the gate while the 3rd waits on max_concurrency=2.
    tokio::time::timeout(Duration::from_secs(2), async {
        while wt.metrics().active < 2 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("two concurrent read_file workers did not become active in time");
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(wt.metrics().active, 2);
    gate.release();

    let payload = exec_handle.await.unwrap();
    reporter.drain().await;
    assert_eq!(payload.status, ToolResultStatus::Succeeded);
    assert_eq!(payload.output.content["output"], "a,b,c");
    assert_eq!(payload.output.content["stats"]["subcalls"], 3);
    assert_eq!(payload.output.content["stats"]["max_in_flight"], 2);
    assert_eq!(wt.metrics().peak_active, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ptc06_timeout_oom_stack_subcall_limit_and_cancellation() {
    let settings = CodeExecutionSettings {
        timeout_secs: 1,
        memory_limit_bytes: 4 * 1024 * 1024,
        stack_limit_bytes: 64 * 1024,
        max_subcalls: 3,
        max_concurrency: 4,
        max_output_bytes: 1024,
    };
    let host = SubcallHost::new(
        settings,
        BuiltinBindings::from_session(&[WorkspaceTool::ReadFile], false),
        None,
        None,
    );

    // 1. Infinite loop interrupts within timeout_secs (1s) quickly.
    let t0 = Instant::now();
    let handle = tokio::runtime::Handle::current();
    let outcome_loop = tokio::task::spawn_blocking({
        let host = Arc::clone(&host);
        let handle = handle.clone();
        move || {
            run_quickjs_blocking(
                host,
                "while (true) {}".into(),
                settings,
                CallControl::new(CancellationToken::new(), None),
                handle,
            )
        }
    })
    .await
    .unwrap();
    assert!(!outcome_loop.ok);
    assert!(outcome_loop.timed_out);
    assert!(t0.elapsed() < Duration::from_secs(3));

    // 2. OOM (new Uint8Array exceeding 4 MiB).
    let outcome_oom = tokio::task::spawn_blocking({
        let host = Arc::clone(&host);
        let handle = handle.clone();
        move || {
            run_quickjs_blocking(
                host,
                "const b = new Uint8Array(16 * 1024 * 1024); return b.length;".into(),
                settings,
                CallControl::new(CancellationToken::new(), None),
                handle,
            )
        }
    })
    .await
    .unwrap();
    assert!(!outcome_oom.ok);
    assert!(
        outcome_oom
            .error
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains("memory"),
        "expected memory error, got {:?}",
        outcome_oom.error
    );

    // 3. Stack overflow.
    let outcome_stack = tokio::task::spawn_blocking({
        let host = Arc::clone(&host);
        let handle = handle.clone();
        move || {
            run_quickjs_blocking(
                host,
                "function f(n) { return f(n + 1); } return f(0);".into(),
                settings,
                CallControl::new(CancellationToken::new(), None),
                handle,
            )
        }
    })
    .await
    .unwrap();
    assert!(!outcome_stack.ok);
    assert!(
        outcome_stack
            .error
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains("stack"),
        "expected stack error, got {:?}",
        outcome_stack.error
    );

    // 4. Max output truncation at 1024 bytes.
    let outcome_trunc = tokio::task::spawn_blocking({
        let host = Arc::clone(&host);
        let handle = handle.clone();
        move || {
            run_quickjs_blocking(
                host,
                "return 'x'.repeat(5000);".into(),
                settings,
                CallControl::new(CancellationToken::new(), None),
                handle,
            )
        }
    })
    .await
    .unwrap();
    assert!(outcome_trunc.ok);
    assert!(outcome_trunc.truncated);
    assert!(outcome_trunc.output.len() < 1024);
    let _ = bound_output_payload(outcome_trunc.output, &outcome_trunc.stats, 1024);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ptc06_max_subcalls_uncatchable_in_try_catch_and_call_control_cancel() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    let ws = crate::workspace::Workspace::open(&dir.path().canonicalize().unwrap()).unwrap();
    let wt = WorkspaceTools::with_write_limits(
        ws,
        WorkspaceToolLimits::default(),
        Default::default(),
        ToolResources::new(8),
    );
    let settings = CodeExecutionSettings {
        max_subcalls: 3,
        timeout_secs: 10,
        ..CodeExecutionSettings::default()
    };
    let host = SubcallHost::new(
        settings,
        BuiltinBindings::from_session(&[WorkspaceTool::ReadFile], false),
        Some(wt.clone()),
        None,
    );

    // 1. Exceeding max_subcalls inside try/catch must abort uncatchably even with trailing code.
    let js_try_catch = r#"
for (let i = 0; i < 6; i++) {
    try {
        await read_file("a.txt");
    } catch (e) {}
}
return "escaped";
"#;
    let handle = tokio::runtime::Handle::current();
    let outcome = tokio::task::spawn_blocking({
        let host = Arc::clone(&host);
        let handle = handle.clone();
        move || {
            run_quickjs_blocking(
                host,
                js_try_catch.into(),
                settings,
                CallControl::new(CancellationToken::new(), None),
                handle,
            )
        }
    })
    .await
    .unwrap();
    assert!(!outcome.ok);
    assert_eq!(outcome.stats.subcalls, 3);
    assert_eq!(wt.metrics().started, 3);
    assert!(
        outcome
            .error
            .as_deref()
            .unwrap_or("")
            .contains("subcall limit"),
        "got {:?}",
        outcome.error
    );

    // 2. CallControl cancellation stops in-flight / subsequent subcalls cleanly.
    struct ReleaseGateOnDrop(Arc<crate::tools::execute::TestGate>);
    impl Drop for ReleaseGateOnDrop {
        fn drop(&mut self) {
            self.0.release();
        }
    }
    let gate = crate::tools::execute::TestGate::new(false);
    *wt.gate.lock().unwrap() = Some(gate.clone());
    let _release_guard = ReleaseGateOnDrop(gate.clone());
    let token = CancellationToken::new();
    let call = make_call(
        "run_code",
        json!({
            "code": r#"
await read_file("a.txt");
await read_file("a.txt");
await read_file("a.txt");
return "done";
"#
        }),
    );
    let reporter = test_reporter(std::slice::from_ref(&call));
    let tool = Arc::new(RunCodeTool::new(host, settings, reporter.clone()));
    let tool_clone = Arc::clone(&tool);
    let call_clone = call.clone();
    let ctrl = CallControl::new(token.clone(), None);
    let exec = tokio::spawn(async move { tool_clone.execute(&call_clone, &ctrl, None).await });

    tokio::time::timeout(Duration::from_secs(2), gate.reached.notified())
        .await
        .expect("worker did not reach gate in time");
    token.cancel();
    gate.release();

    let payload = exec.await.unwrap();
    reporter.drain().await;
    assert_eq!(payload.status, ToolResultStatus::Cancelled);
    assert_eq!(
        wt.metrics().started,
        4,
        "only 1 additional call before cancel"
    );
}

#[test]
fn ptc06_interrupted_promise_produces_zero_rquickjs_stdout_noise() {
    const CHILD_ENV: &str = "TARS_PTC_STDOUT_PROBE";
    if std::env::var_os(CHILD_ENV).is_some() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let settings = CodeExecutionSettings {
            timeout_secs: 1,
            max_subcalls: 1,
            ..CodeExecutionSettings::default()
        };
        let host = SubcallHost::new(settings, BuiltinBindings::default(), None, None)
            .with_extra_provider(Arc::new(DummyReadOnlyProvider {
                delay: Duration::from_millis(50),
            }));
        let outcome = rt.block_on(async move {
            let handle = tokio::runtime::Handle::current();
            let token = CancellationToken::new();
            let cancel_clone = token.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(10)).await;
                cancel_clone.cancel();
            });
            tokio::task::spawn_blocking(move || {
                run_quickjs_blocking(
                    host,
                    "while (true) {}".into(),
                    settings,
                    CallControl::new(token, None),
                    handle,
                )
            })
            .await
            .unwrap()
        });
        assert!(!outcome.ok);
        return;
    }

    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("tools::code_execution::tests::ptc06_interrupted_promise_produces_zero_rquickjs_stdout_noise")
        .arg("--nocapture")
        .env(CHILD_ENV, "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "child failed: {:?}", output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("promise handle function returned error")
            && !stdout.contains("Exception generated by QuickJS"),
        "unexpected rquickjs stdout noise: {stdout}"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ptc09_tools_namespace_lists_describes_calls_and_enforces_readonly_filter() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("note.txt"), "hello-ptc09\n").unwrap();
    let ws = crate::workspace::Workspace::open(&dir.path().canonicalize().unwrap()).unwrap();
    let wt = WorkspaceTools::with_write_limits(
        ws,
        WorkspaceToolLimits::default(),
        Default::default(),
        ToolResources::new(4),
    );
    let settings = CodeExecutionSettings::default();
    let host = SubcallHost::new(
        settings,
        BuiltinBindings::from_session(&[WorkspaceTool::ReadFile], false),
        Some(wt),
        None,
    )
    .with_extra_provider(Arc::new(DummyReadOnlyProvider {
        delay: Duration::ZERO,
    }));

    let call = make_call(
        "run_code",
        json!({
            "code": r#"
const list = await tools.list();
const rfDesc = await tools.describe("read_file");
const dummyDesc = await tools.describe("dummy_read");
const rfRes = await tools.call("read_file", { path: "note.txt" });
const dummyRes = await tools.call("dummy_read", { k: "v" });

let rejectedWriteDesc = false;
try { await tools.describe("write_file"); } catch (e) { rejectedWriteDesc = true; }
let rejectedWriteCall = false;
try { await tools.call("write_file", { path: "x.txt", content: "bad" }); } catch (e) { rejectedWriteCall = true; }
let rejectedUnknownDesc = false;
try { await tools.describe("nonexistent"); } catch (e) { rejectedUnknownDesc = true; }

return {
    list,
    rfDescName: rfDesc.name,
    rfDescHasProps: Boolean(rfDesc.parameters && rfDesc.parameters.properties && rfDesc.parameters.properties.path),
    dummyDescSource: dummyDesc.source,
    rfText: rfRes.text.trim(),
    dummyEcho: dummyRes.echo.k,
    rejectedWriteDesc,
    rejectedWriteCall,
    rejectedUnknownDesc,
};
"#
        }),
    );
    let reporter = test_reporter(std::slice::from_ref(&call));
    let tool = RunCodeTool::new(host, settings, reporter.clone());
    let payload = tool
        .execute(
            &call,
            &CallControl::new(CancellationToken::new(), None),
            None,
        )
        .await;
    reporter.drain().await;
    assert_eq!(
        payload.status,
        ToolResultStatus::Succeeded,
        "{:?}",
        payload.output.content
    );
    assert_eq!(payload.output.content["stats"]["subcalls"], 2);
    assert_eq!(payload.output.content["stats"]["by_tool"]["read_file"], 1);
    assert_eq!(payload.output.content["stats"]["by_tool"]["dummy_read"], 1);

    let out: Value =
        serde_json::from_str(payload.output.content["output"].as_str().unwrap()).unwrap();
    let list = out["list"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["name"], "read_file");
    assert_eq!(list[0]["source"], "builtin");
    assert_eq!(list[1]["name"], "dummy_read");
    assert_eq!(list[1]["source"], "mcp");
    assert_eq!(out["rfDescName"], "read_file");
    assert_eq!(out["rfDescHasProps"], true);
    assert_eq!(out["dummyDescSource"], "mcp");
    assert_eq!(out["rfText"], "hello-ptc09");
    assert_eq!(out["dummyEcho"], "v");
    assert_eq!(out["rejectedWriteDesc"], true);
    assert_eq!(out["rejectedWriteCall"], true);
    assert_eq!(out["rejectedUnknownDesc"], true);
}

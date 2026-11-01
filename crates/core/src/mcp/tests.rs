//! MCP runtime tests against the local stdio fixture and a scripted model.

use super::McpRuntime;
use super::source::{MAX_RESULT_BYTES, limit_result};
use crate::application::tests::workspace_http::{Fixture, Reply, ScriptedToolCall};
use crate::application::{
    ApplicationError, ClosePolicy, ManagedSession, PreparedCreate, RepairError, RepairMode,
    RepairWarningKind, SessionHost, text_input,
};
use crate::definition::{
    CredentialRef, McpApproval, McpServerConfig, McpToolSetting, Provider, SessionDefinition,
    WorkspaceTool, WorkspaceToolLimits,
};
use crate::permission::{
    ApprovalDecision, ApprovalSubject, PermissionDecisionKind, PermissionMode, ToolPermissionClass,
};
use crate::session::{SubmitRequest, WorkObservation, WorkResult, WorkState};
use causa::kernel::{
    BlockContent, BlockId, CallControl, CancellationToken, Context, DynamicToolSource,
    ToolCallContext, ToolCallPayload, ToolExecutionError, ToolOutput, ToolResultPayload,
    ToolResultStatus, Truncation,
};
use causa::runtime::{TurnInterruption, TurnResult};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ROLE: &str = "TARS_MCP_FIXTURE_ROLE";
const PID_FILE: &str = "TARS_MCP_FIXTURE_PID_FILE";
const EXIT_MARKER: &str = "TARS_MCP_FIXTURE_EXIT_MARKER";
const FIXTURE_FAILURE: &str = "fixture: missing configuration";

/// The fixture example next to this test binary; `cargo test` builds examples.
fn fixture_program() -> PathBuf {
    let exe = std::env::current_exe().expect("test binary path");
    let profile = exe
        .parent()
        .and_then(Path::parent)
        .expect("test binary under target/<profile>/deps");
    let program = profile.join("examples").join("mcp_stdio_fixture");
    assert!(
        program.is_file(),
        "MCP fixture not found at {}; run the tests through `cargo test`, which builds examples",
        program.display()
    );
    program
}

fn server(id: &str, role: &str) -> McpServerConfig {
    McpServerConfig {
        id: id.into(),
        command: Some(fixture_program().display().to_string()),
        args: Vec::new(),
        url: None,
        env: BTreeMap::from([(ROLE.to_owned(), role.to_owned())]),
        headers: BTreeMap::new(),
        startup_timeout_sec: 10,
        tool_timeout_sec: 10,
        enabled_tools: None,
        disabled_tools: None,
        approval: McpApproval::Prompt,
        tools: BTreeMap::new(),
        required: false,
    }
}

fn definition(endpoint: &str, root: &Path, servers: Vec<McpServerConfig>) -> SessionDefinition {
    SessionDefinition {
        provider: Provider::OpenAiChat,
        model: "mcp-fixture".into(),
        endpoint: Some(endpoint.into()),
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(20),
        retained_work_capacity: 8,
        max_model_rounds: 4,
        max_tool_calls: 5,
        max_tokens: None,
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: Some(root.canonicalize().expect("canonical workspace root")),
        tools: Vec::new(),
        tool_limits: WorkspaceToolLimits::default(),
        write_limits: None,
        permission_mode: Some(PermissionMode::FullAccess),
        command: None,
        mcp_servers: servers,
        context_window: None,
        auto_compact: true,
        compact_threshold: 70,
    }
}

async fn open(definition: SessionDefinition) -> (SessionHost, ManagedSession) {
    let host = SessionHost::new(None);
    let session = host
        .create(PreparedCreate::transient(definition, Context::default()))
        .await
        .expect("session opens");
    (host, session)
}

async fn run(session: &ManagedSession, key: &str) -> WorkObservation {
    let work = session
        .submit(SubmitRequest {
            request_key: key.into(),
            blocks: vec![text_input(key)],
        })
        .expect("submit")
        .work;
    let observation = session
        .wait(&work, Duration::from_secs(15))
        .await
        .expect("work finishes");
    assert!(matches!(&observation.state, WorkState::Finished(_)));
    session.wait_saved().await.expect("work saved");
    observation
}

fn call(name: &str, arguments: Value) -> ToolCallContext {
    ToolCallContext::new(
        BlockId::new(uuid::Uuid::now_v7()),
        ToolCallPayload::new(name, arguments),
    )
}

fn control() -> CallControl {
    CallControl::new(CancellationToken::new(), None)
}

async fn start(definition: &SessionDefinition) -> Arc<McpRuntime> {
    McpRuntime::start(definition)
        .await
        .expect("runtime starts")
        .expect("servers configured")
}

fn only_source(runtime: &McpRuntime) -> Arc<dyn DynamicToolSource> {
    let sources = runtime.sources();
    assert_eq!(sources.len(), 1);
    sources[0].clone()
}

async fn names(source: &Arc<dyn DynamicToolSource>) -> Vec<String> {
    source
        .describe()
        .await
        .expect("describe")
        .into_iter()
        .map(|description| description.definition.name)
        .collect()
}

fn text(payload: &ToolResultPayload) -> String {
    match &payload.output.content {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn request_tool_names(request: &Value) -> Vec<String> {
    request["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn tool_result(session: &ManagedSession, observation: &WorkObservation) -> ToolResultPayload {
    let call_block_id = observation.tools.calls[0].call_block_id;
    session
        .read_materials()
        .expect("materials")
        .context
        .blocks()
        .iter()
        .find_map(|block| match block.content() {
            BlockContent::ToolResult(payload) if payload.call_block_id == call_block_id => {
                Some(payload.clone())
            }
            _ => None,
        })
        .expect("tool result in context")
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .expect("run kill -0")
        .success()
}

/// Close returns once the child has exited; allow only the moment the
/// operating system needs to reap it.
async fn assert_exited_after_close(pid: &str) {
    let deadline = Instant::now() + Duration::from_millis(200);
    while alive(pid) {
        assert!(Instant::now() < deadline, "the server outlived close");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_until(what: &str, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn model_lists_and_calls_an_mcp_tool_and_sees_its_result() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(
        "openai-chat",
        vec![
            Reply::Call("mcp_fx_echo".into(), json!({"note": "marker-7"})),
            Reply::Answer,
        ],
        None,
    );
    let (host, session) = open(definition(
        &fixture.endpoint,
        root.path(),
        vec![server("fx", "serve")],
    ))
    .await;
    let observation = run(&session, "call echo").await;
    assert_eq!(
        observation.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );

    let requests = fixture.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let offered = request_tool_names(&requests[0]);
    for name in [
        "mcp_fx_echo",
        "mcp_fx_env",
        "mcp_fx_cwd",
        "mcp_fx_big",
        "mcp_fx_hang",
    ] {
        assert!(
            offered.contains(&name.to_owned()),
            "{name} not offered: {offered:?}"
        );
    }
    assert!(
        offered
            .iter()
            .all(|name| !name.contains('.') && name.len() <= 64),
        "an uncallable name reached the model: {offered:?}"
    );
    // The result text exists only in the tool's answer, not in the call.
    assert!(
        requests[1].to_string().contains("echo-result:"),
        "the second request lacks the tool result: {}",
        requests[1]
    );
    assert!(requests[1].to_string().contains("marker-7"));

    let status = session.mcp_status();
    assert_eq!(status.len(), 1);
    assert!(status[0].ready, "{status:?}");
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uncallable_tool_names_are_skipped_and_reported() {
    let root = tempfile::tempdir().unwrap();
    let runtime = start(&definition(
        "http://127.0.0.1:9/v1/chat/completions",
        root.path(),
        vec![server("fx", "serve")],
    ))
    .await;
    let source = only_source(&runtime);
    let listed = names(&source).await;
    assert_eq!(
        listed,
        [
            "mcp_fx_echo",
            "mcp_fx_env",
            "mcp_fx_cwd",
            "mcp_fx_big",
            "mcp_fx_hang",
            "mcp_fx_big_error",
            "mcp_fx_lookup",
            "mcp_fx_mutate",
            "mcp_fx_wipe",
            "mcp_fx_unlock",
        ]
    );
    let skipped: Vec<(String, String)> = runtime.status()[0]
        .skipped_tools
        .iter()
        .map(|tool| (tool.name.clone(), tool.reason.clone()))
        .collect();
    assert_eq!(
        skipped,
        [
            ("mcp_fx_bad.name".to_owned(), "invalid name".to_owned()),
            (
                "mcp_fx_a_tool_name_that_is_far_too_long_to_fit_any_provider_limit_xyz".to_owned(),
                "too long".to_owned()
            ),
        ]
    );
    let refused = source
        .invoke(&call("mcp_fx_bad.name", json!({})), &control(), None)
        .await;
    assert!(matches!(refused, Err(ToolExecutionError::UnknownTool(_))));
    runtime.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enabled_and_disabled_tools_filter_the_listing_and_calls() {
    let root = tempfile::tempdir().unwrap();
    let endpoint = "http://127.0.0.1:9/v1/chat/completions";

    let mut enabled = server("fx", "serve");
    enabled.enabled_tools = Some(vec!["echo".into()]);
    let runtime = start(&definition(endpoint, root.path(), vec![enabled])).await;
    let source = only_source(&runtime);
    assert_eq!(names(&source).await, ["mcp_fx_echo"]);
    let skipped = &runtime.status()[0].skipped_tools;
    assert!(
        skipped
            .iter()
            .any(|tool| tool.name == "mcp_fx_env" && tool.reason == "not enabled"),
        "{skipped:?}"
    );
    let refused = source
        .invoke(&call("mcp_fx_env", json!({})), &control(), None)
        .await;
    assert!(matches!(refused, Err(ToolExecutionError::UnknownTool(_))));
    let echoed = source
        .invoke(&call("mcp_fx_echo", json!({"n": 1})), &control(), None)
        .await
        .expect("enabled tool runs");
    assert!(text(&echoed).starts_with("echo-result:"));
    runtime.close().await;

    let mut disabled = server("fx", "serve");
    disabled.disabled_tools = Some(vec!["echo".into()]);
    let runtime = start(&definition(endpoint, root.path(), vec![disabled])).await;
    let source = only_source(&runtime);
    let listed = names(&source).await;
    assert!(!listed.contains(&"mcp_fx_echo".to_owned()), "{listed:?}");
    assert!(listed.contains(&"mcp_fx_env".to_owned()), "{listed:?}");
    assert!(
        runtime.status()[0]
            .skipped_tools
            .iter()
            .any(|tool| tool.name == "mcp_fx_echo" && tool.reason == "disabled")
    );
    let refused = source
        .invoke(&call("mcp_fx_echo", json!({})), &control(), None)
        .await;
    assert!(matches!(refused, Err(ToolExecutionError::UnknownTool(_))));
    runtime.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_results_lose_their_middle_within_the_limit() {
    let root = tempfile::tempdir().unwrap();
    let runtime = start(&definition(
        "http://127.0.0.1:9/v1/chat/completions",
        root.path(),
        vec![server("fx", "serve")],
    ))
    .await;
    let source = only_source(&runtime);
    let _ = names(&source).await;
    let payload = source
        .invoke(
            &call("mcp_fx_big", json!({"bytes": 300_000})),
            &control(),
            None,
        )
        .await
        .expect("big result");
    let serialized = serde_json::to_vec(&payload).unwrap().len();
    assert!(serialized <= MAX_RESULT_BYTES, "{serialized} bytes");
    assert_eq!(payload.output.truncation, Truncation::Middle);
    let body = text(&payload);
    assert!(body.starts_with("HEAD-MARK "), "{}", &body[..40]);
    assert!(body.ends_with(" TAIL-MARK"));
    assert!(body.contains("bytes omitted from the middle"));
    runtime.close().await;
}

fn omitted(body: &str) -> usize {
    let start = body.find("[... truncated: ").expect("notice") + "[... truncated: ".len();
    let end = start + body[start..].find(' ').expect("count end");
    body[start..end].parse().expect("count")
}

#[test]
fn truncation_cuts_on_character_boundaries_and_counts_the_omission() {
    // Every character here is multibyte or needs escaping, so the escaped
    // size is far above the byte length.
    let original: String = "é漢\"\\\n\u{1}🙂".repeat(20_000);
    let mut payload = ToolResultPayload::new(
        BlockId::new(uuid::Uuid::now_v7()),
        ToolResultStatus::Succeeded,
        ToolOutput::new(Value::String(original.clone())),
    );
    limit_result(&mut payload);
    assert!(serde_json::to_vec(&payload).unwrap().len() <= MAX_RESULT_BYTES);
    assert_eq!(payload.output.truncation, Truncation::Middle);
    let body = text(&payload);
    let notice_start = body.find("\n[... truncated: ").unwrap();
    let notice_end = body.find(" ...]\n").unwrap() + " ...]\n".len();
    let (head, tail) = (&body[..notice_start], &body[notice_end..]);
    assert!(original.starts_with(head) && !head.is_empty());
    assert!(original.ends_with(tail) && !tail.is_empty());
    assert_eq!(omitted(&body), original.len() - head.len() - tail.len());

    let structured = json!({ "rows": vec!["x".repeat(100); 2_000] });
    let mut payload = ToolResultPayload::new(
        BlockId::new(uuid::Uuid::now_v7()),
        ToolResultStatus::Succeeded,
        ToolOutput::new(structured.clone()),
    );
    limit_result(&mut payload);
    assert!(serde_json::to_vec(&payload).unwrap().len() <= MAX_RESULT_BYTES);
    assert!(text(&payload).starts_with("{\"rows\":[\"xxx"));

    let small = ToolResultPayload::new(
        BlockId::new(uuid::Uuid::now_v7()),
        ToolResultStatus::Succeeded,
        ToolOutput::new(json!({"ok": true})),
    );
    let mut unchanged = small.clone();
    limit_result(&mut unchanged);
    assert_eq!(unchanged, small);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stdio_server_sees_only_the_allowed_environment_and_starts_in_the_workspace() {
    // Cargo sets these for test processes; the first is forwarded, the second
    // stands for every variable the configuration does not name.
    let forwarded = "CARGO_PKG_NAME";
    let unlisted = "CARGO_MANIFEST_DIR";
    let forwarded_value = std::env::var(forwarded).expect("cargo sets CARGO_PKG_NAME");
    assert!(
        std::env::var_os(unlisted).is_some(),
        "cargo sets {unlisted}"
    );

    let root = tempfile::tempdir().unwrap();
    let mut config = server("fx", "serve");
    config
        .env
        .insert("TARS_FIXTURE_LITERAL".into(), "literal-value".into());
    config
        .env
        .insert(forwarded.into(), format!("${{{forwarded}}}"));
    let runtime = start(&definition(
        "http://127.0.0.1:9/v1/chat/completions",
        root.path(),
        vec![config],
    ))
    .await;
    let source = only_source(&runtime);
    let _ = names(&source).await;
    let payload = source
        .invoke(&call("mcp_fx_env", json!({})), &control(), None)
        .await
        .expect("env tool");
    let environment: BTreeMap<String, String> = serde_json::from_str(&text(&payload)).unwrap();

    let mut expected: Vec<&str> = ["PATH", "HOME", "LANG"]
        .into_iter()
        .filter(|name| std::env::var_os(name).is_some())
        .collect();
    expected.extend([ROLE, "TARS_FIXTURE_LITERAL", forwarded]);
    expected.sort_unstable();
    // On macOS, CoreFoundation (linked into the fixture) sets this in the
    // process's own environment when it loads; it is not inherited.
    let seen: Vec<&str> = environment
        .keys()
        .map(String::as_str)
        .filter(|name| *name != "__CF_USER_TEXT_ENCODING")
        .collect();
    assert_eq!(seen, expected, "the child saw unexpected variables");
    assert_eq!(environment["TARS_FIXTURE_LITERAL"], "literal-value");
    assert_eq!(environment[forwarded], forwarded_value);
    assert!(!environment.contains_key(unlisted));

    let cwd = source
        .invoke(&call("mcp_fx_cwd", json!({})), &control(), None)
        .await
        .expect("cwd tool");
    assert_eq!(
        PathBuf::from(text(&cwd)),
        root.path().canonicalize().unwrap()
    );
    runtime.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_server_is_absent_while_the_others_keep_working() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(
        "openai-chat",
        vec![
            Reply::Call("mcp_ok_echo".into(), json!({"note": "still-here"})),
            Reply::Answer,
        ],
        None,
    );
    let (host, session) = open(definition(
        &fixture.endpoint,
        root.path(),
        vec![server("bad", "fail"), server("ok", "serve")],
    ))
    .await;
    let status = session.mcp_status();
    assert_eq!(status.len(), 2);
    assert_eq!(status[0].id, "bad");
    assert!(!status[0].ready);
    let reason = status[0].reason.as_deref().unwrap_or_default();
    assert!(reason.contains(FIXTURE_FAILURE), "reason: {reason}");
    assert!(status[1].ready, "{status:?}");

    let observation = run(&session, "call the working server").await;
    assert_eq!(
        observation.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    let requests = fixture.requests.lock().unwrap().clone();
    let offered = request_tool_names(&requests[0]);
    assert!(offered.contains(&"mcp_ok_echo".to_owned()));
    assert!(offered.iter().all(|name| !name.starts_with("mcp_bad_")));
    assert!(requests[1].to_string().contains("echo-result:"));
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_required_server_failure_prevents_opening_before_any_model_request() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::new("openai-chat", vec![Reply::Answer], None);
    let marker = root.path().join("ok-exited");
    let mut ok = server("ok", "serve");
    ok.env
        .insert(EXIT_MARKER.into(), marker.display().to_string());
    let mut required = server("bad", "fail");
    required.required = true;
    let host = SessionHost::new(None);
    let Err(error) = host
        .create(PreparedCreate::transient(
            definition(&fixture.endpoint, root.path(), vec![ok, required]),
            Context::default(),
        ))
        .await
    else {
        panic!("a required server must start");
    };
    let message = error.to_string();
    assert!(message.contains("'bad'"), "{message}");
    assert!(message.contains(FIXTURE_FAILURE), "{message}");
    // The server that did start was shut down before the error returned.
    assert!(
        marker.exists(),
        "the started server outlived the failed open"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        fixture.accepted.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_startup_timeout_reports_the_reason_and_stops_the_process() {
    let root = tempfile::tempdir().unwrap();
    let pid_file = root.path().join("slow.pid");
    let mut slow = server("slow", "slow");
    slow.startup_timeout_sec = 1;
    slow.env
        .insert(PID_FILE.into(), pid_file.display().to_string());
    let started = Instant::now();
    let runtime = start(&definition(
        "http://127.0.0.1:9/v1/chat/completions",
        root.path(),
        vec![slow, server("ok", "serve")],
    ))
    .await;
    assert!(started.elapsed() < Duration::from_secs(8));
    let status = runtime.status();
    assert!(!status[0].ready);
    let reason = status[0].reason.as_deref().unwrap_or_default();
    assert!(reason.contains("timed out"), "reason: {reason}");
    assert!(status[1].ready);
    assert_eq!(runtime.sources().len(), 1);

    let pid = std::fs::read_to_string(&pid_file).expect("slow server wrote its pid");
    wait_until("the timed-out server exits", || !alive(&pid)).await;
    runtime.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_session_shuts_the_server_down() {
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("exited");
    let fixture = Fixture::new("openai-chat", vec![Reply::Answer], None);
    let mut config = server("fx", "serve");
    config
        .env
        .insert(EXIT_MARKER.into(), marker.display().to_string());
    let (host, session) = open(definition(&fixture.endpoint, root.path(), vec![config])).await;
    run(&session, "plain answer").await;
    assert!(!marker.exists());
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
    // The server writes the marker when the client ends the session and
    // exits right after; close returns only once the child has exited.
    assert!(
        marker.exists(),
        "close returned before the server shut down"
    );
    assert!(session.mcp_status().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tool_deadline_ends_the_call_with_an_unknown_outcome() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(
        "openai-chat",
        vec![Reply::Call("mcp_fx_hang".into(), json!({})), Reply::Answer],
        None,
    );
    let pid_file = root.path().join("hang.pid");
    let mut config = server("fx", "serve");
    config.tool_timeout_sec = 1;
    config
        .env
        .insert(PID_FILE.into(), pid_file.display().to_string());
    let (host, session) = open(definition(&fixture.endpoint, root.path(), vec![config])).await;
    let started = Instant::now();
    let observation = run(&session, "call hang").await;
    // The fixture answers after ten seconds; the one-second deadline wins.
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        observation.tools.calls[0].status,
        Some(ToolResultStatus::UnknownOutcome)
    );
    assert_eq!(
        tool_result(&session, &observation).status,
        ToolResultStatus::UnknownOutcome
    );
    // The server is still working on the call; close waits until it is gone.
    let pid = std::fs::read_to_string(&pid_file).expect("server wrote its pid");
    assert!(alive(&pid));
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
    assert_exited_after_close(&pid).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm13_prompt_denied_with_reason_and_turn_continues() {
    let root = tempfile::tempdir().unwrap();
    let log_file = root.path().join("call_log.txt");
    let mut fx = server("fx", "serve");
    fx.approval = McpApproval::Prompt;
    fx.env.insert(
        "TARS_MCP_FIXTURE_CALL_LOG".into(),
        log_file.display().to_string(),
    );

    let fixture = Fixture::new(
        "openai-chat",
        vec![
            Reply::Call("mcp_fx_echo".into(), json!({"text": "hello"})),
            Reply::Answer,
        ],
        None,
    );
    let mut def = definition(&fixture.endpoint, root.path(), vec![fx]);
    def.permission_mode = Some(PermissionMode::Ask);
    let (host, session) = open(def).await;

    let mut rx = session.connect_approvals();
    let approval_task = tokio::spawn(async move {
        let req = rx.recv().await.expect("mcp approval request");
        let ApprovalSubject::McpCall {
            server,
            tool,
            arguments,
        } = &req.subject
        else {
            panic!("expected McpCall");
        };
        assert_eq!(server, "fx");
        assert_eq!(tool, "echo");
        assert!(arguments.contains("hello"));
        req.reply
            .send(ApprovalDecision::Deny {
                reason: Some("security policy prevents call".into()),
            })
            .unwrap();
    });

    let observation = run(&session, "call prompt echo").await;
    approval_task.await.unwrap();

    let call = &observation.tools.calls[0];
    assert_eq!(call.status, Some(ToolResultStatus::Rejected));
    assert_eq!(
        call.permission_decision.as_ref().unwrap().kind,
        PermissionDecisionKind::AskedAndDenied
    );

    assert_eq!(std::fs::read_to_string(&log_file).unwrap_or_default(), "");

    let requests = fixture.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let req2_str = requests[1].to_string();
    assert!(
        req2_str.contains("user declined this tool call: security policy prevents call"),
        "expected declined text in model prompt: {req2_str}"
    );

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm13_tool_level_approve_in_ask_and_read_only() {
    let root = tempfile::tempdir().unwrap();
    let log_file = root.path().join("call_log.txt");
    let mut fx = server("fx", "serve");
    fx.approval = McpApproval::Prompt;
    fx.tools.insert(
        "echo".into(),
        McpToolSetting {
            approval: McpApproval::Approve,
        },
    );
    fx.env.insert(
        "TARS_MCP_FIXTURE_CALL_LOG".into(),
        log_file.display().to_string(),
    );

    let fixture1 = Fixture::new(
        "openai-chat",
        vec![
            Reply::Call("mcp_fx_echo".into(), json!({"msg": "approved"})),
            Reply::Answer,
        ],
        None,
    );
    let mut def1 = definition(&fixture1.endpoint, root.path(), vec![fx.clone()]);
    def1.permission_mode = Some(PermissionMode::Ask);
    let (host1, session1) = open(def1).await;
    let mut rx = session1.connect_approvals();

    let obs1 = run(&session1, "call approved echo in ask").await;
    assert!(rx.try_recv().is_err());
    assert_eq!(
        obs1.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(
        obs1.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::DirectExecution
    );
    assert_eq!(
        std::fs::read_to_string(&log_file)
            .unwrap_or_default()
            .trim(),
        "echo"
    );
    host1
        .close(&session1.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();

    std::fs::remove_file(&log_file).unwrap();
    let fixture2 = Fixture::new(
        "openai-chat",
        vec![
            Reply::Call("mcp_fx_echo".into(), json!({"msg": "approved"})),
            Reply::Answer,
        ],
        None,
    );
    let mut def2 = definition(&fixture2.endpoint, root.path(), vec![fx]);
    def2.permission_mode = Some(PermissionMode::ReadOnly);
    let (host2, session2) = open(def2).await;
    let obs2 = run(&session2, "call approved echo in ro").await;
    assert_eq!(obs2.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    assert_eq!(
        obs2.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::ModeRefused
    );
    let result = tool_result(&session2, &obs2);
    assert_eq!(
        result.output.content,
        json!({ "error": "the current permission mode 'read-only' does not allow this MCP tool call" })
    );
    assert_eq!(std::fs::read_to_string(&log_file).unwrap_or_default(), "");
    host2
        .close(&session2.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm13_writes_annotated_lookup_mutate_wipe() {
    let root = tempfile::tempdir().unwrap();
    let mut fx = server("fx", "serve");
    fx.approval = McpApproval::Writes;

    let fixture_ro = Fixture::new(
        "openai-chat",
        vec![
            Reply::Call("mcp_fx_lookup".into(), json!({})),
            Reply::Call("mcp_fx_mutate".into(), json!({})),
            Reply::Call("mcp_fx_wipe".into(), json!({})),
            Reply::Answer,
        ],
        None,
    );
    let mut def_ro = definition(&fixture_ro.endpoint, root.path(), vec![fx.clone()]);
    def_ro.permission_mode = Some(PermissionMode::ReadOnly);
    let (host_ro, session_ro) = open(def_ro).await;
    let obs_ro = run(&session_ro, "test writes in ro").await;
    assert_eq!(obs_ro.tools.calls.len(), 3);
    assert_eq!(
        obs_ro.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(
        obs_ro.tools.calls[1].status,
        Some(ToolResultStatus::Rejected)
    );
    assert_eq!(
        obs_ro.tools.calls[2].status,
        Some(ToolResultStatus::Rejected)
    );
    host_ro
        .close(&session_ro.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();

    let fixture_ask = Fixture::new(
        "openai-chat",
        vec![
            Reply::Call("mcp_fx_lookup".into(), json!({})),
            Reply::Call("mcp_fx_mutate".into(), json!({})),
            Reply::Call("mcp_fx_wipe".into(), json!({})),
            Reply::Answer,
        ],
        None,
    );
    let mut def_ask = definition(&fixture_ask.endpoint, root.path(), vec![fx.clone()]);
    def_ask.permission_mode = Some(PermissionMode::Ask);
    let (host_ask, session_ask) = open(def_ask).await;
    let mut rx = session_ask.connect_approvals();
    let approval_task = tokio::spawn(async move {
        let req1 = rx.recv().await.expect("req mutate");
        let ApprovalSubject::McpCall { tool, .. } = &req1.subject else {
            panic!()
        };
        assert_eq!(tool, "mutate");
        req1.reply.send(ApprovalDecision::Allow).unwrap();

        let req2 = rx.recv().await.expect("req wipe");
        let ApprovalSubject::McpCall { tool, .. } = &req2.subject else {
            panic!()
        };
        assert_eq!(tool, "wipe");
        req2.reply.send(ApprovalDecision::Allow).unwrap();
    });
    let obs_ask = run(&session_ask, "test writes in ask").await;
    approval_task.await.unwrap();
    assert_eq!(obs_ask.tools.calls.len(), 3);
    assert_eq!(
        obs_ask.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(
        obs_ask.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::DirectExecution
    );
    assert_eq!(
        obs_ask.tools.calls[1].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(
        obs_ask.tools.calls[1]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::AskedAndAllowed
    );
    assert_eq!(
        obs_ask.tools.calls[2].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(
        obs_ask.tools.calls[2]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::AskedAndAllowed
    );
    host_ask
        .close(&session_ask.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();

    let fixture_ae = Fixture::new(
        "openai-chat",
        vec![
            Reply::Call("mcp_fx_lookup".into(), json!({})),
            Reply::Call("mcp_fx_mutate".into(), json!({})),
            Reply::Call("mcp_fx_wipe".into(), json!({})),
            Reply::Answer,
        ],
        None,
    );
    let mut def_ae = definition(&fixture_ae.endpoint, root.path(), vec![fx.clone()]);
    def_ae.permission_mode = Some(PermissionMode::AcceptEdits);
    let (host_ae, session_ae) = open(def_ae).await;
    let mut rx = session_ae.connect_approvals();
    let approval_task_ae = tokio::spawn(async move {
        let req1 = rx.recv().await.expect("req mutate");
        let ApprovalSubject::McpCall { tool, .. } = &req1.subject else {
            panic!()
        };
        assert_eq!(tool, "mutate");
        req1.reply.send(ApprovalDecision::Allow).unwrap();

        let req2 = rx.recv().await.expect("req wipe");
        let ApprovalSubject::McpCall { tool, .. } = &req2.subject else {
            panic!()
        };
        assert_eq!(tool, "wipe");
        req2.reply.send(ApprovalDecision::Allow).unwrap();
    });
    let obs_ae = run(&session_ae, "test writes in accept-edits").await;
    approval_task_ae.await.unwrap();
    assert_eq!(obs_ae.tools.calls.len(), 3);
    assert_eq!(
        obs_ae.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(
        obs_ae.tools.calls[1].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(
        obs_ae.tools.calls[2].status,
        Some(ToolResultStatus::Succeeded)
    );
    host_ae
        .close(&session_ae.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();

    let fixture_fa = Fixture::new(
        "openai-chat",
        vec![
            Reply::Call("mcp_fx_lookup".into(), json!({})),
            Reply::Call("mcp_fx_mutate".into(), json!({})),
            Reply::Call("mcp_fx_wipe".into(), json!({})),
            Reply::Answer,
        ],
        None,
    );
    let mut def_fa = definition(&fixture_fa.endpoint, root.path(), vec![fx]);
    def_fa.permission_mode = Some(PermissionMode::FullAccess);
    let (host_fa, session_fa) = open(def_fa).await;
    let obs_fa = run(&session_fa, "test writes in full-access").await;
    assert_eq!(obs_fa.tools.calls.len(), 3);
    assert_eq!(
        obs_fa.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(
        obs_fa.tools.calls[1].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(
        obs_fa.tools.calls[2].status,
        Some(ToolResultStatus::Succeeded)
    );
    host_fa
        .close(&session_fa.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm13_auto_approve_in_mcp_json_still_asks_when_default_prompt() {
    let root = tempfile::tempdir().unwrap();
    let toml_path = root.path().join("config.toml");
    let json_path = root.path().join("mcp.json");
    std::fs::write(
        &toml_path,
        "version = 1
",
    )
    .unwrap();
    let config_json = serde_json::json!({
        "mcpServers": {
            "fx": {
                "command": fixture_program().display().to_string(),
                "env": { ROLE: "serve" },
                "autoApprove": ["mutate"]
            }
        }
    });
    std::fs::write(&json_path, serde_json::to_string(&config_json).unwrap()).unwrap();
    let config = crate::config::Config::load(crate::config::ConfigSource::File(toml_path)).unwrap();
    let servers: Vec<_> = config.mcp_servers().values().cloned().collect();
    assert_eq!(servers[0].approval, McpApproval::Prompt);
    assert!(!servers[0].tools.contains_key("mutate"));

    let fixture = Fixture::new(
        "openai-chat",
        vec![
            Reply::Call("mcp_fx_mutate".into(), json!({})),
            Reply::Answer,
        ],
        None,
    );
    let mut def = definition(&fixture.endpoint, root.path(), servers);
    def.permission_mode = Some(PermissionMode::Ask);
    let (host, session) = open(def).await;
    let mut rx = session.connect_approvals();
    let approval_task = tokio::spawn(async move {
        let req = rx.recv().await.expect("mutate asks");
        let ApprovalSubject::McpCall { tool, .. } = &req.subject else {
            panic!()
        };
        assert_eq!(tool, "mutate");
        req.reply.send(ApprovalDecision::Allow).unwrap();
    });
    let obs = run(&session, "call mutate").await;
    approval_task.await.unwrap();
    assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Succeeded));
    assert_eq!(
        obs.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::AskedAndAllowed
    );
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm13_legacy_saved_auto_approval_treated_as_approve() {
    let root = tempfile::tempdir().unwrap();
    let raw_config = serde_json::json!({
        "id": "fx",
        "command": fixture_program().display().to_string(),
        "env": { ROLE: "serve" },
        "startup_timeout_sec": 10,
        "tool_timeout_sec": 10,
        "required": false,
        "approval": "auto"
    });
    let server: McpServerConfig =
        serde_json::from_value(raw_config).expect("deserialize legacy auto");
    assert_eq!(server.approval, McpApproval::Approve);

    let fixture = Fixture::new(
        "openai-chat",
        vec![
            Reply::Call("mcp_fx_echo".into(), json!({"test": "legacy-auto"})),
            Reply::Answer,
        ],
        None,
    );
    let mut def = definition(&fixture.endpoint, root.path(), vec![server]);
    def.permission_mode = Some(PermissionMode::Ask);
    let (host, session) = open(def).await;
    let mut rx = session.connect_approvals();
    let obs = run(&session, "call legacy auto").await;
    assert!(rx.try_recv().is_err(), "must not ask approval for approve");
    assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Succeeded));
    assert_eq!(
        obs.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::DirectExecution
    );
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm13_list_changed_unlock_adds_late_tool() {
    let root = tempfile::tempdir().unwrap();
    let log_file = root.path().join("call_log.txt");
    let mut fx = server("fx", "serve");
    fx.approval = McpApproval::Prompt;
    fx.env.insert(
        "TARS_MCP_FIXTURE_CALL_LOG".into(),
        log_file.display().to_string(),
    );

    let fixture = Fixture::new(
        "openai-chat",
        vec![
            Reply::Call("mcp_fx_unlock".into(), json!({})),
            Reply::Answer,
            Reply::Call("mcp_fx_late".into(), json!({})),
            Reply::Answer,
        ],
        None,
    );
    let mut def = definition(&fixture.endpoint, root.path(), vec![fx]);
    def.permission_mode = Some(PermissionMode::Ask);
    let (host, session) = open(def).await;
    let mut rx = session.connect_approvals();

    let approval_task1 = tokio::spawn(async move {
        let req = rx.recv().await.expect("unlock request");
        req.reply.send(ApprovalDecision::Allow).unwrap();
        rx
    });
    let obs1 = run(&session, "unlock late tool").await;
    let mut rx = approval_task1.await.unwrap();
    assert_eq!(
        obs1.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );

    let approval_task2 = tokio::spawn(async move {
        let req = rx.recv().await.expect("late tool request");
        let ApprovalSubject::McpCall { tool, .. } = &req.subject else {
            panic!()
        };
        assert_eq!(tool, "late");
        req.reply.send(ApprovalDecision::Allow).unwrap();
    });
    let obs2 = run(&session, "call late tool").await;
    approval_task2.await.unwrap();
    assert_eq!(
        obs2.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );

    // The notification arrives before the result, so requests[1] is not
    // asserted: the second round's first request is already re-listed.
    {
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 4, "must have recorded exactly 4 requests");
        let round1_tools = requests[0]["tools"].as_array().expect("tools in round 1");
        assert!(
            !round1_tools
                .iter()
                .any(|t| t["function"]["name"] == "mcp_fx_late"),
            "mcp_fx_late must not be in round 1 tool definitions"
        );
        let round2_tools = requests[2]["tools"].as_array().expect("tools in round 2");
        assert!(
            round2_tools
                .iter()
                .any(|t| t["function"]["name"] == "mcp_fx_late"),
            "mcp_fx_late must be in round 2 tool definitions"
        );
    }

    let call_log = std::fs::read_to_string(&log_file).unwrap_or_default();
    assert!(
        call_log.contains("late"),
        "fixture log must contain late tool call"
    );

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();

    let _ = std::fs::remove_file(&log_file);

    let fixture_dis = Fixture::new(
        "openai-chat",
        vec![
            Reply::Call("mcp_fx_unlock".into(), json!({})),
            Reply::Answer,
            Reply::Call("mcp_fx_late".into(), json!({})),
            Reply::Answer,
        ],
        None,
    );
    let mut fx_dis = server("fx", "serve");
    fx_dis.approval = McpApproval::Prompt;
    fx_dis.disabled_tools = Some(vec!["late".into()]);
    fx_dis.env.insert(
        "TARS_MCP_FIXTURE_CALL_LOG".into(),
        log_file.display().to_string(),
    );
    let mut def_dis = definition(&fixture_dis.endpoint, root.path(), vec![fx_dis]);
    def_dis.permission_mode = Some(PermissionMode::Ask);
    let (host_dis, session_dis) = open(def_dis).await;
    let mut rx_dis = session_dis.connect_approvals();

    let approval_dis1 = tokio::spawn(async move {
        let req = rx_dis.recv().await.expect("unlock request");
        req.reply.send(ApprovalDecision::Allow).unwrap();
        rx_dis
    });
    let obs_dis1 = run(&session_dis, "unlock in disabled mode").await;
    let mut rx_dis = approval_dis1.await.unwrap();
    assert_eq!(
        obs_dis1.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );

    let obs_dis2 = run(&session_dis, "call disabled late").await;
    assert_eq!(
        obs_dis2.tools.calls[0].status,
        Some(ToolResultStatus::Rejected)
    );
    let res = tool_result(&session_dis, &obs_dis2);
    assert_eq!(
        res.output.content,
        json!({ "error": "tool 'mcp_fx_late' is not available" })
    );
    assert_eq!(
        obs_dis2.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::Unavailable
    );

    {
        let requests_dis = fixture_dis.requests.lock().unwrap();
        assert_eq!(
            requests_dis.len(),
            4,
            "disabled fixture must record exactly 4 requests"
        );
        let dis_round2_tools = requests_dis[2]["tools"]
            .as_array()
            .expect("tools in round 2");
        assert!(
            !dis_round2_tools
                .iter()
                .any(|t| t["function"]["name"] == "mcp_fx_late"),
            "disabled late tool must not appear in tool definitions"
        );
    }

    assert!(
        rx_dis.try_recv().is_err(),
        "no approval request should be sent for disabled tool"
    );

    let call_log = std::fs::read_to_string(&log_file).unwrap_or_default();
    assert!(call_log.contains("unlock"));
    assert!(
        !call_log.contains("late"),
        "fixture log must not contain late"
    );

    host_dis
        .close(&session_dis.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm13_unavailable_or_unregistered_tools_are_rejected() {
    for mode in [PermissionMode::Ask, PermissionMode::FullAccess] {
        let root = tempfile::tempdir().unwrap();
        let log_file = root.path().join("call_log.txt");
        let mut fx = server("fx", "serve");
        fx.approval = McpApproval::Prompt;
        fx.disabled_tools = Some(vec!["echo".into()]);
        fx.env.insert(
            "TARS_MCP_FIXTURE_CALL_LOG".into(),
            log_file.display().to_string(),
        );

        let tool_names = [
            "mcp_fx_nonexistent",
            "mcp_fx_echo",
            "unregistered_standalone_tool",
        ];

        let mut replies = Vec::new();
        for name in &tool_names {
            replies.push(Reply::Call((*name).into(), json!({ "arg": "value" })));
            replies.push(Reply::Answer);
        }

        let fixture = Fixture::new("openai-chat", replies, None);
        let mut def = definition(&fixture.endpoint, root.path(), vec![fx]);
        def.permission_mode = Some(mode);
        let (host, session) = open(def).await;
        let mut rx = session.connect_approvals();

        for name in &tool_names {
            let obs = run(&session, &format!("calling {name} in {mode:?}")).await;
            assert_eq!(
                obs.tools.calls[0].status,
                Some(ToolResultStatus::Rejected),
                "tool {name} in mode {mode:?} must be Rejected"
            );
            assert_eq!(
                obs.tools.calls[0]
                    .permission_decision
                    .as_ref()
                    .unwrap()
                    .kind,
                PermissionDecisionKind::Unavailable,
                "tool {name} in mode {mode:?} must record Unavailable decision"
            );
            let res = tool_result(&session, &obs);
            assert_eq!(
                res.output.content,
                json!({ "error": format!("tool '{name}' is not available") }),
                "tool {name} in mode {mode:?} error message mismatch"
            );
            assert!(
                rx.try_recv().is_err(),
                "no approval request should be sent for {name} in mode {mode:?}"
            );
            let log = std::fs::read_to_string(&log_file).unwrap_or_default();
            assert!(
                !log.contains(name) && !log.contains("echo") && !log.contains("nonexistent"),
                "fixture log must not contain {name}"
            );
        }

        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm13_mcp_call_cancelled_during_approval_wait() {
    let root = tempfile::tempdir().unwrap();
    let log_file = root.path().join("call_log.txt");

    let mut fx = server("fx", "serve");
    fx.approval = McpApproval::Prompt;
    fx.env.insert(
        "TARS_MCP_FIXTURE_CALL_LOG".into(),
        log_file.display().to_string(),
    );

    let replies = vec![
        Reply::Call("mcp_fx_echo".into(), json!({ "message": "cancel_me" })),
        Reply::Answer,
    ];
    let fixture = Fixture::new("openai-chat", replies, None);
    let mut def = definition(&fixture.endpoint, root.path(), vec![fx]);
    def.permission_mode = Some(PermissionMode::Ask);
    let (host, session) = open(def).await;

    let mut rx = session.connect_approvals();
    let entered = Arc::new(tokio::sync::Notify::new());
    let entered_clone = entered.clone();

    let approval_task = tokio::spawn(async move {
        let req = rx.recv().await.expect("approval request");
        assert!(matches!(
            req.subject,
            ApprovalSubject::McpCall { ref tool, .. } if tool == "echo"
        ));
        entered_clone.notify_one();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });

    let work = session
        .submit(SubmitRequest {
            request_key: "mcp_cancel".into(),
            blocks: vec![text_input("echo and cancel")],
        })
        .expect("submit")
        .work;

    entered.notified().await;
    session.cancel(&work).unwrap();

    let obs = session
        .wait(&work, Duration::from_secs(5))
        .await
        .expect("work finishes");
    approval_task.abort();

    use crate::session::ToolPhase;
    use crate::session::WorkResult;
    use causa::runtime::{TurnInterruption, TurnResult};
    assert!(matches!(
        obs.state,
        WorkState::Finished(ref r) if matches!(**r, WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::Cancelled { .. }
        }))
    ));
    assert_eq!(obs.tools.calls.len(), 1);
    let call = &obs.tools.calls[0];
    assert!(
        call.phase == ToolPhase::NotStarted || call.operation_not_started,
        "cancelled wait must record tool as not started"
    );
    let call_log = std::fs::read_to_string(&log_file).unwrap_or_default();
    assert!(
        !call_log.contains("echo"),
        "fixture log must not contain cancelled tool call"
    );
    assert!(call.permission_decision.is_none());

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm17_four_modes_six_categories_session_integration() {
    struct TestCase {
        tool_name: &'static str,
        payload: Value,
        is_mcp: bool,
        log_name: Option<&'static str>,
    }

    let cases = [
        TestCase {
            tool_name: "read_file",
            payload: json!({ "path": "file.txt" }),
            is_mcp: false,
            log_name: None,
        },
        TestCase {
            tool_name: "write_file",
            payload: json!({ "path": "file.txt", "content": "updated" }),
            is_mcp: false,
            log_name: None,
        },
        TestCase {
            tool_name: "mcp_p_echo",
            payload: json!({ "msg": "test" }),
            is_mcp: true,
            log_name: Some("echo"),
        },
        TestCase {
            tool_name: "mcp_w_lookup",
            payload: json!({}),
            is_mcp: true,
            log_name: Some("lookup"),
        },
        TestCase {
            tool_name: "mcp_w_mutate",
            payload: json!({}),
            is_mcp: true,
            log_name: Some("mutate"),
        },
        TestCase {
            tool_name: "mcp_a_echo",
            payload: json!({ "msg": "test" }),
            is_mcp: true,
            log_name: Some("echo"),
        },
    ];

    struct Expected {
        asked: bool,
        status: ToolResultStatus,
        decision_kind: PermissionDecisionKind,
        fixture_executed: bool,
    }

    let expectations: [[Expected; 6]; 4] = [
        // ReadOnly
        [
            Expected {
                asked: false,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::DirectExecution,
                fixture_executed: false,
            },
            Expected {
                asked: false,
                status: ToolResultStatus::Rejected,
                decision_kind: PermissionDecisionKind::ModeRefused,
                fixture_executed: false,
            },
            Expected {
                asked: false,
                status: ToolResultStatus::Rejected,
                decision_kind: PermissionDecisionKind::ModeRefused,
                fixture_executed: false,
            },
            Expected {
                asked: false,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::DirectExecution,
                fixture_executed: true,
            },
            Expected {
                asked: false,
                status: ToolResultStatus::Rejected,
                decision_kind: PermissionDecisionKind::ModeRefused,
                fixture_executed: false,
            },
            Expected {
                asked: false,
                status: ToolResultStatus::Rejected,
                decision_kind: PermissionDecisionKind::ModeRefused,
                fixture_executed: false,
            },
        ],
        // AcceptEdits
        [
            Expected {
                asked: false,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::DirectExecution,
                fixture_executed: false,
            },
            Expected {
                asked: false,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::DirectExecution,
                fixture_executed: false,
            },
            Expected {
                asked: true,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::AskedAndAllowed,
                fixture_executed: true,
            },
            Expected {
                asked: false,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::DirectExecution,
                fixture_executed: true,
            },
            Expected {
                asked: true,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::AskedAndAllowed,
                fixture_executed: true,
            },
            Expected {
                asked: false,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::DirectExecution,
                fixture_executed: true,
            },
        ],
        // Ask
        [
            Expected {
                asked: false,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::DirectExecution,
                fixture_executed: false,
            },
            Expected {
                asked: true,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::AskedAndAllowed,
                fixture_executed: false,
            },
            Expected {
                asked: true,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::AskedAndAllowed,
                fixture_executed: true,
            },
            Expected {
                asked: false,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::DirectExecution,
                fixture_executed: true,
            },
            Expected {
                asked: true,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::AskedAndAllowed,
                fixture_executed: true,
            },
            Expected {
                asked: false,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::DirectExecution,
                fixture_executed: true,
            },
        ],
        // FullAccess
        [
            Expected {
                asked: false,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::DirectExecution,
                fixture_executed: false,
            },
            Expected {
                asked: false,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::DirectExecution,
                fixture_executed: false,
            },
            Expected {
                asked: false,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::DirectExecution,
                fixture_executed: true,
            },
            Expected {
                asked: false,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::DirectExecution,
                fixture_executed: true,
            },
            Expected {
                asked: false,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::DirectExecution,
                fixture_executed: true,
            },
            Expected {
                asked: false,
                status: ToolResultStatus::Succeeded,
                decision_kind: PermissionDecisionKind::DirectExecution,
                fixture_executed: true,
            },
        ],
    ];

    let modes = [
        PermissionMode::ReadOnly,
        PermissionMode::AcceptEdits,
        PermissionMode::Ask,
        PermissionMode::FullAccess,
    ];

    for (mode_idx, &mode) in modes.iter().enumerate() {
        let root = tempfile::tempdir().unwrap();
        let log_file = root.path().join("call_log.txt");
        std::fs::write(root.path().join("file.txt"), "initial text").unwrap();

        let mut srv_p = server("p", "serve");
        srv_p.approval = McpApproval::Prompt;
        srv_p.env.insert(
            "TARS_MCP_FIXTURE_CALL_LOG".into(),
            log_file.display().to_string(),
        );

        let mut srv_w = server("w", "serve");
        srv_w.approval = McpApproval::Writes;
        srv_w.env.insert(
            "TARS_MCP_FIXTURE_CALL_LOG".into(),
            log_file.display().to_string(),
        );

        let mut srv_a = server("a", "serve");
        srv_a.approval = McpApproval::Approve;
        srv_a.env.insert(
            "TARS_MCP_FIXTURE_CALL_LOG".into(),
            log_file.display().to_string(),
        );

        let mut replies = Vec::new();
        for case in &cases {
            replies.push(Reply::Call(case.tool_name.into(), case.payload.clone()));
            replies.push(Reply::Answer);
        }

        let fixture = Fixture::new("openai-chat", replies, None);
        let mut def = definition(&fixture.endpoint, root.path(), vec![srv_p, srv_w, srv_a]);
        def.tools = vec![WorkspaceTool::ReadFile, WorkspaceTool::WriteFile];
        def.permission_mode = Some(mode);
        let (host, session) = open(def).await;
        let mut rx = session.connect_approvals();

        let (asked_tx, mut asked_rx) = tokio::sync::mpsc::unbounded_channel();
        let approver = tokio::spawn(async move {
            while let Some(req) = rx.recv().await {
                let _ = asked_tx.send(req.tool_name.clone());
                let _ = req.reply.send(ApprovalDecision::Allow);
            }
        });

        for (case_idx, case) in cases.iter().enumerate() {
            let _ = std::fs::write(&log_file, "");
            let exp = &expectations[mode_idx][case_idx];

            let obs = run(&session, &format!("turn {case_idx} for {mode:?}")).await;

            if exp.asked {
                let asked_name = asked_rx.try_recv().unwrap_or_else(|_| {
                    panic!(
                        "mode {:?}, case {}: expected approval request for {}",
                        mode, case_idx, case.tool_name
                    )
                });
                assert_eq!(asked_name, case.tool_name);
                assert!(
                    asked_rx.try_recv().is_err(),
                    "mode {:?}, case {}: only 1 approval expected",
                    mode,
                    case_idx
                );
            } else {
                assert!(
                    asked_rx.try_recv().is_err(),
                    "mode {:?}, case {}: expected NO approval request for {}",
                    mode,
                    case_idx,
                    case.tool_name
                );
            }

            assert_eq!(
                obs.tools.calls[0].status,
                Some(exp.status.clone()),
                "mode {:?}, case {} ({}) status mismatch",
                mode,
                case_idx,
                case.tool_name
            );

            let decision = obs.tools.calls[0]
                .permission_decision
                .as_ref()
                .expect("every call in this table records a decision");
            assert_eq!(
                decision.kind, exp.decision_kind,
                "mode {:?}, case {} ({}) decision kind mismatch",
                mode, case_idx, case.tool_name
            );
            // The mode is fixed at open, so every decision sees it at revision 0.
            assert_eq!(decision.mode, mode, "mode {mode:?}, case {case_idx}");
            assert_eq!(decision.revision, 0, "mode {mode:?}, case {case_idx}");

            if case.is_mcp {
                let log = std::fs::read_to_string(&log_file).unwrap_or_default();
                let expected_name = case.log_name.unwrap();
                if exp.fixture_executed {
                    assert!(
                        log.contains(expected_name),
                        "mode {:?}, case {} ({}) should have been logged in fixture, got: {}",
                        mode,
                        case_idx,
                        case.tool_name,
                        log
                    );
                } else {
                    assert!(
                        !log.contains(expected_name),
                        "mode {:?}, case {} ({}) should NOT have been logged in fixture, got: {}",
                        mode,
                        case_idx,
                        case.tool_name,
                        log
                    );
                }
            }
        }

        approver.abort();
        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }
}

#[test]
fn wm13_no_mcp_prefix_in_permission_logic() {
    let table = std::sync::Arc::new(crate::permission::McpPermissionTable::default());
    let hub = crate::permission::PermissionHub::with_mcp_table(
        crate::permission::PermissionMode::Ask,
        table.clone(),
    );
    let view = hub.view();
    assert_eq!(
        view.classify_tool("read_file"),
        Some(ToolPermissionClass::KnownReadOnly)
    );
    assert_eq!(
        view.classify_tool("write_file"),
        Some(ToolPermissionClass::BuiltinWrite)
    );
    assert_eq!(view.classify_tool("mcp_server_fake"), None);

    table.update_server_tools(
        "custom",
        vec![(
            "my_special_tool".into(),
            crate::permission::McpToolInfo {
                server_id: "custom".into(),
                raw_tool_name: "special".into(),
                category: ToolPermissionClass::McpPrompt,
            },
        )],
    );
    assert_eq!(
        view.classify_tool("my_special_tool"),
        Some(ToolPermissionClass::McpPrompt)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm17_accept_edits_does_not_affect_mcp_prompt_but_full_access_does() {
    let root = tempfile::tempdir().unwrap();
    let mut fx = server("fx", "serve");
    fx.approval = McpApproval::Prompt;

    let fixture = Fixture::new(
        "openai-chat",
        vec![
            Reply::Call("mcp_fx_echo".into(), json!({"t": 1})),
            Reply::Call("mcp_fx_echo".into(), json!({"t": 2})),
            Reply::Call("mcp_fx_echo".into(), json!({"t": 3})),
            Reply::Answer,
        ],
        None,
    );
    let mut def = definition(&fixture.endpoint, root.path(), vec![fx]);
    def.permission_mode = Some(PermissionMode::Ask);
    let (host, session) = open(def).await;

    let mut rx = session.connect_approvals();
    let session_clone = session.clone();

    let approval_task = tokio::spawn(async move {
        let req1 = rx.recv().await.expect("req1");
        let ApprovalSubject::McpCall { tool, .. } = &req1.subject else {
            panic!()
        };
        assert_eq!(tool, "echo");
        // McpPrompt still asks under AcceptEdits.
        session_clone
            .set_permission_mode(PermissionMode::AcceptEdits)
            .unwrap();
        req1.reply.send(ApprovalDecision::Allow).unwrap();

        let req2 = rx
            .recv()
            .await
            .expect("req2 must still ask under accept-edits");
        let ApprovalSubject::McpCall { tool, .. } = &req2.subject else {
            panic!()
        };
        assert_eq!(tool, "echo");
        // FullAccess executes directly, so no third request arrives.
        session_clone
            .set_permission_mode(PermissionMode::FullAccess)
            .unwrap();
        req2.reply.send(ApprovalDecision::Allow).unwrap();

        let next = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(next.is_err(), "call 3 must not ask under FullAccess");
    });

    let obs = run(&session, "mcp mode switches mid-batch").await;
    approval_task.await.unwrap();

    assert_eq!(obs.tools.calls.len(), 3);
    assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Succeeded));
    assert_eq!(
        obs.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::AskedAndAllowed
    );
    assert_eq!(obs.tools.calls[1].status, Some(ToolResultStatus::Succeeded));
    assert_eq!(
        obs.tools.calls[1]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::AskedAndAllowed
    );
    assert_eq!(obs.tools.calls[2].status, Some(ToolResultStatus::Succeeded));
    assert_eq!(
        obs.tools.calls[2]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::DirectExecution
    );

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_bearer_token_variable_fails_without_its_value() {
    let root = tempfile::tempdir().unwrap();
    let mut remote = server("remote", "serve");
    remote.command = None;
    remote.env.clear();
    remote.url = Some("http://127.0.0.1:9/mcp".into());
    remote.headers.insert(
        "Authorization".into(),
        "Bearer ${TARS_MCP_TEST_TOKEN_NEVER_SET}".into(),
    );
    let runtime = start(&definition(
        "http://127.0.0.1:9/v1/chat/completions",
        root.path(),
        vec![remote],
    ))
    .await;
    let status = runtime.status();
    assert!(!status[0].ready);
    let reason = status[0].reason.as_deref().unwrap_or_default();
    assert!(
        reason.contains("TARS_MCP_TEST_TOKEN_NEVER_SET")
            && (reason.contains("not set") || reason.contains("missing")),
        "reason: {reason}"
    );
    assert!(runtime.sources().is_empty());
    runtime.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_twice_or_dropping_without_close_is_safe() {
    let root = tempfile::tempdir().unwrap();
    let endpoint = "http://127.0.0.1:9/v1/chat/completions";
    let runtime = start(&definition(
        endpoint,
        root.path(),
        vec![server("fx", "serve")],
    ))
    .await;
    let source = only_source(&runtime);
    runtime.close().await;
    runtime.close().await;
    assert!(runtime.status().iter().all(|status| !status.ready));
    let after = source
        .invoke(&call("mcp_fx_echo", json!({})), &control(), None)
        .await;
    assert!(after.is_err(), "a closed server must not answer");
    assert!(names(&source).await.is_empty());

    // Dropping the last handle without close still ends the server: the
    // adapter closes its transport when dropped.
    let marker = root.path().join("dropped");
    let mut config = server("fx", "serve");
    config
        .env
        .insert(EXIT_MARKER.into(), marker.display().to_string());
    let runtime = start(&definition(endpoint, root.path(), vec![config])).await;
    drop(runtime);
    wait_until("the dropped server exits", || marker.exists()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_that_does_not_connect_servers_starts_no_process() {
    let root = tempfile::tempdir().unwrap();
    let pid_file = root.path().join("never.pid");
    let mut required = server("needed", "fail");
    required.required = true;
    required
        .env
        .insert(PID_FILE.into(), pid_file.display().to_string());
    let options = crate::application::HostOptions {
        connect_mcp_servers: false,
        ..Default::default()
    };
    let host = SessionHost::with_options(None, options).unwrap();
    let session = host
        .create(PreparedCreate::transient(
            definition(
                "http://127.0.0.1:9/v1/chat/completions",
                root.path(),
                vec![required],
            ),
            Context::default(),
        ))
        .await
        .expect("opens without connecting");
    assert!(session.mcp_status().is_empty());
    assert!(!pid_file.exists(), "a server process was started");
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_stops_a_server_that_is_still_busy_with_a_call() {
    let root = tempfile::tempdir().unwrap();
    let pid_file = root.path().join("busy.pid");
    let mut config = server("fx", "serve");
    config.tool_timeout_sec = 1;
    config
        .env
        .insert(PID_FILE.into(), pid_file.display().to_string());
    let runtime = start(&definition(
        "http://127.0.0.1:9/v1/chat/completions",
        root.path(),
        vec![config],
    ))
    .await;
    let source = only_source(&runtime);
    let _ = names(&source).await;
    // The call's deadline ends it here, but the server keeps working on it
    // and ignores the closed stdin until it answers.
    let timed_out = source
        .invoke(&call("mcp_fx_hang", json!({})), &control(), None)
        .await;
    assert!(
        matches!(timed_out, Err(ToolExecutionError::TimedOut)),
        "{timed_out:?}"
    );
    let pid = std::fs::read_to_string(&pid_file).expect("server wrote its pid");
    assert!(alive(&pid));
    runtime.close().await;
    assert_exited_after_close(&pid).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_server_error_is_cut_within_the_result_limit() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(
        "openai-chat",
        vec![
            Reply::Call("mcp_fx_big_error".into(), json!({"bytes": 300_000})),
            Reply::Answer,
        ],
        None,
    );
    let (host, session) = open(definition(
        &fixture.endpoint,
        root.path(),
        vec![server("fx", "serve")],
    ))
    .await;
    let observation = run(&session, "call big_error").await;
    assert_eq!(
        observation.tools.calls[0].status,
        Some(ToolResultStatus::Failed)
    );
    let result = tool_result(&session, &observation);
    let serialized = serde_json::to_vec(&result).unwrap().len();
    assert!(serialized <= MAX_RESULT_BYTES, "{serialized} bytes");
    let error = result.output.content["error"].as_str().expect("error text");
    assert!(
        error.contains("ERR-HEAD") && error.ends_with("ERR-TAIL"),
        "{}",
        &error[..80]
    );
    assert!(error.contains("bytes omitted from the middle"));
    let requests = fixture.requests.lock().unwrap().clone();
    assert!(requests[1].to_string().len() < MAX_RESULT_BYTES + 8 * 1024);
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[test]
fn error_text_is_cut_on_character_boundaries() {
    use super::source::{MAX_ERROR_BYTES, limit_error};
    let original = format!("head {} tail", "漢\"\n".repeat(20_000));
    let ToolExecutionError::Protocol(cut) =
        limit_error(ToolExecutionError::Protocol(original.clone()))
    else {
        panic!("variant kept");
    };
    assert!(serde_json::to_string(&cut).unwrap().len() <= MAX_ERROR_BYTES + 2);
    let start = cut.find("\n[... truncated: ").unwrap();
    let end = cut.find(" ...]\n").unwrap() + " ...]\n".len();
    assert!(original.starts_with(&cut[..start]));
    assert!(original.ends_with(&cut[end..]));
    assert_eq!(omitted(&cut), original.len() - start - (cut.len() - end));
    assert!(matches!(
        limit_error(ToolExecutionError::Protocol("short".into())),
        ToolExecutionError::Protocol(text) if text == "short"
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_server_that_stops_listing_does_not_hold_up_the_turn() {
    let root = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(
        "openai-chat",
        vec![
            Reply::Call("mcp_ok_echo".into(), json!({"note": "unblocked"})),
            Reply::Answer,
        ],
        None,
    );
    let mut stuck = server("stuck", "serve");
    stuck.startup_timeout_sec = 1;
    stuck
        .env
        .insert("TARS_MCP_FIXTURE_RELIST".into(), "hang".into());
    let (host, session) = open(definition(
        &fixture.endpoint,
        root.path(),
        vec![stuck, server("ok", "serve")],
    ))
    .await;
    assert!(session.mcp_status()[0].ready, "the first listing answers");
    let started = Instant::now();
    let observation = run(&session, "call the other server").await;
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        observation.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    let requests = fixture.requests.lock().unwrap().clone();
    let offered = request_tool_names(&requests[0]);
    assert!(offered.contains(&"mcp_ok_echo".to_owned()));
    assert!(offered.iter().all(|name| !name.starts_with("mcp_stuck_")));
    let status = session.mcp_status();
    assert!(!status[0].ready);
    let reason = status[0].reason.as_deref().unwrap_or_default();
    assert!(
        reason.contains("listing tools timed out"),
        "reason: {reason}"
    );
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stdio_server_env_expands_variables_and_defaults() {
    let forwarded = "CARGO_PKG_NAME";
    let forwarded_value = std::env::var(forwarded).expect("cargo sets CARGO_PKG_NAME");
    let root = tempfile::tempdir().unwrap();
    let mut config = server("fx", "serve");
    config
        .env
        .insert("TARS_FIXTURE_LITERAL".into(), "literal-value".into());
    config
        .env
        .insert("FORWARDED_VAR".into(), format!("${{{forwarded}}}"));
    config.env.insert(
        "DEFAULTED_VAR".into(),
        "${TARS_MCP_TEST_UNSET_VAR:-fallback_value}".into(),
    );
    let runtime = start(&definition(
        "http://127.0.0.1:9/v1/chat/completions",
        root.path(),
        vec![config],
    ))
    .await;
    let source = only_source(&runtime);
    let _ = names(&source).await;
    let payload = source
        .invoke(&call("mcp_fx_env", json!({})), &control(), None)
        .await
        .expect("env tool");
    let environment: BTreeMap<String, String> = serde_json::from_str(&text(&payload)).unwrap();
    assert_eq!(environment["FORWARDED_VAR"], forwarded_value);
    assert_eq!(environment["DEFAULTED_VAR"], "fallback_value");
    assert_eq!(environment["TARS_FIXTURE_LITERAL"], "literal-value");
    runtime.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_env_variable_fails_startup_and_required_server_prevents_session_open() {
    let root = tempfile::tempdir().unwrap();
    let mut config = server("fx", "serve");
    config.env.insert(
        "MISSING_VAR".into(),
        "${TARS_MCP_TEST_VAR_THAT_IS_NOT_SET}".into(),
    );
    config.required = true;

    let store_dir = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store_dir.path().to_path_buf()));
    let def = definition(
        "http://127.0.0.1:9/v1/chat/completions",
        root.path(),
        vec![config],
    );
    let Err(err) = host
        .create(PreparedCreate::transient(def, Context::default()))
        .await
    else {
        panic!("must fail");
    };
    let msg = err.to_string();
    assert!(msg.contains("TARS_MCP_TEST_VAR_THAT_IS_NOT_SET"));
    assert!(msg.contains("not set"));
}

async fn starts_with_post(socket: &tokio::net::TcpStream) -> bool {
    const PREFIX: &[u8] = b"POST ";
    let mut head = [0; PREFIX.len()];
    loop {
        match tokio::time::timeout(Duration::from_secs(5), socket.peek(&mut head)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return false,
            Ok(Ok(n)) if head[..n] != PREFIX[..n] => return false,
            Ok(Ok(n)) if n == PREFIX.len() => return true,
            Ok(Ok(_)) => tokio::time::sleep(Duration::from_millis(1)).await,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_server_headers_are_expanded_and_sent_over_network() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let token_var = "CARGO_PKG_NAME";
    let token_val = std::env::var(token_var).expect("CARGO_PKG_NAME is set by cargo");

    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        // Other local programs may connect to a new port; the MCP client's
        // first request is a POST.
        let mut accepted = None;
        while let Ok((socket, _)) = listener.accept().await {
            if starts_with_post(&socket).await {
                accepted = Some(socket);
                break;
            }
        }
        if let Some(mut socket) = accepted {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = tx.send(req);
            let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });

    let root = tempfile::tempdir().unwrap();
    let mut remote = server("http-srv", "serve");
    remote.command = None;
    remote.env.clear();
    remote.url = Some(format!("http://{addr}/mcp"));
    remote
        .headers
        .insert("Authorization".into(), format!("Bearer ${{{token_var}}}"));
    remote
        .headers
        .insert("X-Custom-Header".into(), "CustomVal".into());
    remote.startup_timeout_sec = 2;

    let runtime = start(&definition(
        "http://127.0.0.1:9/v1/chat/completions",
        root.path(),
        vec![remote],
    ))
    .await;

    let received_request = rx.await.unwrap();
    let lower_req = received_request.to_ascii_lowercase();
    assert!(lower_req.contains(&format!("authorization: bearer {token_val}").to_ascii_lowercase()));
    assert!(lower_req.contains("x-custom-header: customval"));

    let status = runtime.status();
    assert!(!status[0].ready);
    let reason = status[0].reason.as_deref().unwrap_or_default();
    assert!(!reason.contains(&token_val));

    runtime.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_server_failure_reason_redacts_expanded_url_variables() {
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };

    let root = tempfile::tempdir().unwrap();
    let mut remote = server("http-srv", "serve");
    remote.command = None;
    remote.env.clear();
    remote.url = Some(format!(
        "http://127.0.0.1:{port}/secret/${{TARS_TEST_UNSET_TOKEN:-prominent_secret_token_12345}}"
    ));
    remote.startup_timeout_sec = 2;

    let runtime = start(&definition(
        "http://127.0.0.1:9/v1/chat/completions",
        root.path(),
        vec![remote],
    ))
    .await;

    let status = runtime.status();
    assert!(!status[0].ready);
    let reason = status[0].reason.as_deref().unwrap_or_default();
    assert!(
        !reason.contains("prominent_secret_token_12345"),
        "reason echoed secret: {reason}"
    );
    assert!(
        reason.contains("${TARS_TEST_UNSET_TOKEN}"),
        "reason lacked placeholder: {reason}"
    );

    runtime.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_server_expanded_invalid_url_fails_startup_with_static_reason() {
    let root = tempfile::tempdir().unwrap();
    let mut remote = server("http-srv", "serve");
    remote.command = None;
    remote.env.clear();
    remote.url =
        Some("${TARS_TEST_UNSET_ENDPOINT:-ftp://invalid-scheme-marker.invalid/mcp}".to_string());
    remote.startup_timeout_sec = 2;

    let runtime = start(&definition(
        "http://127.0.0.1:9/v1/chat/completions",
        root.path(),
        vec![remote],
    ))
    .await;

    let status = runtime.status();
    assert!(!status[0].ready);
    let reason = status[0].reason.as_deref().unwrap_or_default();
    assert_eq!(reason, "expanded URL is not a valid HTTP endpoint");
    assert!(!reason.contains("invalid-scheme-marker"));

    runtime.close().await;
}

// Connection errors quote URLs percent-encoded, which redaction by the raw
// value would miss, so such values are refused before connecting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_url_variables_that_would_be_percent_encoded_are_refused() {
    for value in [
        "café_secret_marker",
        "has space_secret_marker",
        "quote\"_secret_marker",
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut remote = server("http-srv", "serve");
        remote.command = None;
        remote.env.clear();
        remote.url = Some(format!(
            "http://127.0.0.1:9/${{TARS_TEST_UNSET_PATH:-{value}}}/mcp"
        ));
        remote.startup_timeout_sec = 2;

        let runtime = start(&definition(
            "http://127.0.0.1:9/v1/chat/completions",
            root.path(),
            vec![remote],
        ))
        .await;

        let status = runtime.status();
        assert!(!status[0].ready);
        let reason = status[0].reason.as_deref().unwrap_or_default();
        assert_eq!(
            reason,
            "a variable in the URL expands to characters that are not allowed there"
        );
        assert!(!reason.contains("secret_marker"), "{reason}");

        runtime.close().await;
    }
}

/// Persistent session over the stdio fixture whose single batch calls the
/// hanging tool; cancels once that call reaches the server, leaving the batch
/// interrupted with the external call unresolved and its declaration unpaired.
async fn cancel_after_hang_reaches_the_server(
    root: &Path,
    tools: Vec<WorkspaceTool>,
    replies: Vec<Reply>,
) -> (
    SessionHost,
    ManagedSession,
    crate::session::WorkRef,
    Fixture,
) {
    let log_file = root.join("call_log.txt");
    let mut fx = server("fx", "serve");
    fx.env.insert(
        "TARS_MCP_FIXTURE_CALL_LOG".into(),
        log_file.display().to_string(),
    );

    let fixture = Fixture::new("openai-chat", replies, None);
    let mut def = definition(&fixture.endpoint, root, vec![fx]);
    let has_builtin = !tools.is_empty();
    def.tools = tools;
    let host = SessionHost::new(Some(root.join("store")));
    let session = host
        .create(PreparedCreate::persistent(def, Context::default()))
        .await
        .expect("session opens");

    let mut progress = session.subscribe_progress();
    let work = session
        .submit(SubmitRequest {
            request_key: "interrupted mcp call".into(),
            blocks: vec![text_input("call the server")],
        })
        .expect("submit")
        .work;
    // Built-in tools of the same batch must be recorded as completed before
    // the cancellation, so wait for their return event first.
    if has_builtin {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match progress.recv().await {
                    Ok(event) => {
                        if let crate::session::ProgressUpdate::Tool(tool) = &event.update
                            && tool.tool_name == "read_file"
                            && tool.phase == crate::session::ToolPhase::Returned
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        panic!("progress channel closed before read_file returned")
                    }
                }
            }
        })
        .await
        .expect("read_file returned timeout");
    }
    wait_until("the hang call reaches the server", || {
        std::fs::read_to_string(&log_file).is_ok_and(|log| log.contains("hang"))
    })
    .await;
    session.cancel(&work).expect("cancel");
    let observation = session
        .wait(&work, Duration::from_secs(10))
        .await
        .expect("work finishes");
    assert!(matches!(
        observation.state,
        WorkState::Finished(ref result)
            if matches!(**result, WorkResult::Run(TurnResult::Interrupted {
                cause: TurnInterruption::Cancelled { .. }
            }))
    ));
    session.wait_saved().await.expect("interrupted work saved");
    (host, session, work, fixture)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm09_mcp_call_cancelled_mid_execution_rejects_adoption_and_warns_external_side_effects() {
    let root = tempfile::tempdir().unwrap();
    let (host, session, work, fixture) = cancel_after_hang_reaches_the_server(
        root.path(),
        Vec::new(),
        vec![Reply::Call("mcp_fx_hang".into(), json!({})), Reply::Answer],
    )
    .await;

    // The preview keeps the batch observable but marks adoption unavailable,
    // and the unknown outcome carries an external-side-effect warning.
    let inspection = session.inspect_repair(Some(&work)).unwrap();
    let original = inspection.original.as_ref().unwrap();
    let adoption_error = original.adoption_error.as_ref().expect("adoption refused");
    assert!(matches!(adoption_error, RepairError::NonBuiltinCalls));
    assert!(
        adoption_error
            .to_string()
            .contains("external side effects may already have happened"),
        "{adoption_error}"
    );
    assert_eq!(original.completed_count, 1);
    assert_eq!(original.pending_count, 0);
    assert_eq!(original.write_warnings_count, 1);
    assert_eq!(original.external_warnings_count, 1);
    let preview = &original.results[0];
    assert_eq!(preview.status, ToolResultStatus::UnknownOutcome);
    let warning = preview.write_warning.as_ref().expect("external warning");
    assert_eq!(warning.kind, RepairWarningKind::External);
    assert_eq!(warning.tool_name, "mcp_fx_hang");
    assert_eq!(warning.path, "");

    assert!(matches!(
        session.plan_repair(RepairMode::AdoptOriginal(work.clone())),
        Err(ApplicationError::Repair(RepairError::NonBuiltinCalls))
    ));
    let plan = session.plan_repair(RepairMode::RemoveUnpaired).unwrap();
    let summary = plan.summary();
    assert_eq!(summary.removed_count, 1);
    assert_eq!(summary.write_warnings_count, 1);
    assert_eq!(summary.external_warnings_count, 1);
    assert_eq!(summary.write_warnings[0].kind, RepairWarningKind::External);
    assert_eq!(summary.write_warnings[0].tool_name, "mcp_fx_hang");

    session.apply_repair(&plan).unwrap();
    session.wait_saved().await.unwrap();

    let next = session
        .submit(SubmitRequest {
            request_key: "after repair".into(),
            blocks: vec![text_input("continue")],
        })
        .expect("submit")
        .work;
    let observation = session
        .wait(&next, Duration::from_secs(10))
        .await
        .expect("work finishes");
    assert!(matches!(observation.state, WorkState::Finished(_)));
    session.wait_saved().await.unwrap();
    let requests = fixture.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let messages = requests[1]["messages"].as_array().expect("messages");
    assert!(
        messages
            .iter()
            .all(|m| m.get("tool_calls").is_none() && m.get("tool_call_id").is_none()),
        "the removed declaration must not reach the model: {messages:?}"
    );

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm09_builtin_read_and_mcp_call_cancelled_cannot_be_partially_adopted() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("notes.txt"), "readable content\n").unwrap();
    let (host, session, work, fixture) = cancel_after_hang_reaches_the_server(
        root.path(),
        vec![WorkspaceTool::ReadFile],
        vec![
            Reply::Calls(vec![
                ScriptedToolCall {
                    name: "read_file".into(),
                    arguments: json!({"path": "notes.txt"}),
                },
                ScriptedToolCall {
                    name: "mcp_fx_hang".into(),
                    arguments: json!({}),
                },
            ]),
            Reply::Answer,
        ],
    )
    .await;

    let inspection = session.inspect_repair(Some(&work)).unwrap();
    let original = inspection.original.as_ref().unwrap();
    assert!(matches!(
        original.adoption_error,
        Some(RepairError::NonBuiltinCalls)
    ));
    assert_eq!(original.completed_count, 2);
    assert_eq!(original.write_warnings_count, 1);
    assert_eq!(original.external_warnings_count, 1);
    let read = original
        .results
        .iter()
        .find(|result| result.write_warning.is_none())
        .expect("read preview");
    assert_eq!(read.status, ToolResultStatus::Succeeded);
    let hang = original
        .results
        .iter()
        .find(|result| result.write_warning.is_some())
        .expect("external preview");
    assert_eq!(hang.status, ToolResultStatus::UnknownOutcome);
    let warning = hang.write_warning.as_ref().unwrap();
    assert_eq!(warning.kind, RepairWarningKind::External);
    assert_eq!(warning.tool_name, "mcp_fx_hang");

    // The completed read result cannot be adopted on its own: a batch with any
    // non-builtin call is removal-only.
    assert!(matches!(
        session.plan_repair(RepairMode::AdoptOriginal(work.clone())),
        Err(ApplicationError::Repair(RepairError::NonBuiltinCalls))
    ));
    let plan = session.plan_repair(RepairMode::RemoveUnpaired).unwrap();
    let summary = plan.summary();
    assert_eq!(summary.removed_count, 2);
    assert_eq!(summary.write_warnings_count, 1);
    assert_eq!(summary.external_warnings_count, 1);
    assert_eq!(summary.write_warnings[0].tool_name, "mcp_fx_hang");
    assert_eq!(summary.write_warnings[0].kind, RepairWarningKind::External);

    session.apply_repair(&plan).unwrap();
    session.wait_saved().await.unwrap();

    let next = session
        .submit(SubmitRequest {
            request_key: "after repair".into(),
            blocks: vec![text_input("continue")],
        })
        .expect("submit")
        .work;
    let observation = session
        .wait(&next, Duration::from_secs(10))
        .await
        .expect("work finishes");
    assert!(matches!(observation.state, WorkState::Finished(_)));
    session.wait_saved().await.unwrap();
    let requests = fixture.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let messages = requests[1]["messages"].as_array().expect("messages");
    assert!(
        messages
            .iter()
            .all(|m| m.get("tool_calls").is_none() && m.get("tool_call_id").is_none()),
        "no removed exchange may reach the model: {messages:?}"
    );

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

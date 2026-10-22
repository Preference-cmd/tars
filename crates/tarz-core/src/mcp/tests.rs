//! MCP runtime tests against the local stdio fixture and a scripted model.

use super::McpRuntime;
use super::source::{MAX_RESULT_BYTES, limit_result};
use crate::application::tests::workspace_http::{Fixture, Reply};
use crate::application::{ClosePolicy, ManagedSession, PreparedCreate, SessionHost, text_input};
use crate::definition::{
    CredentialRef, McpApproval, McpServerConfig, Provider, SessionDefinition, WorkspaceToolLimits,
};
use crate::permission::{PermissionDecisionKind, PermissionMode};
use crate::session::{SubmitRequest, WorkObservation, WorkState};
use causa::kernel::{
    BlockContent, BlockId, CallControl, CancellationToken, Context, DynamicToolSource,
    ToolCallContext, ToolCallPayload, ToolExecutionError, ToolOutput, ToolResultPayload,
    ToolResultStatus, Truncation,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ROLE: &str = "TARZ_MCP_FIXTURE_ROLE";
const PID_FILE: &str = "TARZ_MCP_FIXTURE_PID_FILE";
const EXIT_MARKER: &str = "TARZ_MCP_FIXTURE_EXIT_MARKER";
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
        env_vars: Vec::new(),
        bearer_token_env_var: None,
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
        mcp_servers: servers,
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
            "mcp_fx_big_error"
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

/// The omitted count in a truncation notice.
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

    // Structured content is cut through its serialized form.
    let structured = json!({ "rows": vec!["x".repeat(100); 2_000] });
    let mut payload = ToolResultPayload::new(
        BlockId::new(uuid::Uuid::now_v7()),
        ToolResultStatus::Succeeded,
        ToolOutput::new(structured.clone()),
    );
    limit_result(&mut payload);
    assert!(serde_json::to_vec(&payload).unwrap().len() <= MAX_RESULT_BYTES);
    assert!(text(&payload).starts_with("{\"rows\":[\"xxx"));

    // A result within the limit is left exactly as it was.
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
        .insert("TARZ_FIXTURE_LITERAL".into(), "literal-value".into());
    config.env_vars = vec![forwarded.into(), "TARZ_MCP_TEST_VARIABLE_NEVER_SET".into()];
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
    expected.extend([ROLE, "TARZ_FIXTURE_LITERAL", forwarded]);
    expected.sort_unstable();
    // On macOS, CoreFoundation (linked into the fixture) sets this in the
    // process's own environment when it loads; it is not inherited.
    let seen: Vec<&str> = environment
        .keys()
        .map(String::as_str)
        .filter(|name| *name != "__CF_USER_TEXT_ENCODING")
        .collect();
    assert_eq!(seen, expected, "the child saw unexpected variables");
    assert_eq!(environment["TARZ_FIXTURE_LITERAL"], "literal-value");
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
async fn mcp_calls_run_only_in_full_access_until_they_can_be_confirmed() {
    for mode in [
        PermissionMode::ReadOnly,
        PermissionMode::Ask,
        PermissionMode::AcceptEdits,
    ] {
        let error = format!("MCP tool calls run only in 'full-access' mode (current mode: {mode})");
        let root = tempfile::tempdir().unwrap();
        let fixture = Fixture::new(
            "openai-chat",
            vec![
                Reply::Call("mcp_fx_echo".into(), json!({"note": "refused"})),
                Reply::Answer,
            ],
            None,
        );
        let mut definition =
            definition(&fixture.endpoint, root.path(), vec![server("fx", "serve")]);
        definition.permission_mode = Some(mode);
        let (host, session) = open(definition).await;
        let observation = run(&session, "call echo").await;
        let progress = &observation.tools.calls[0];
        assert_eq!(
            progress.status,
            Some(ToolResultStatus::Rejected),
            "{mode:?}"
        );
        assert_eq!(
            progress.permission_decision.as_ref().unwrap().kind,
            PermissionDecisionKind::ModeRefused
        );
        let result = tool_result(&session, &observation);
        assert_eq!(result.output.content, json!({ "error": error }));
        let requests = fixture.requests.lock().unwrap().clone();
        assert!(!requests[1].to_string().contains("echo-result:"));
        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_bearer_token_variable_fails_without_its_value() {
    let root = tempfile::tempdir().unwrap();
    let mut remote = server("remote", "serve");
    remote.command = None;
    remote.env.clear();
    remote.url = Some("http://127.0.0.1:9/mcp".into());
    remote.bearer_token_env_var = Some("TARZ_MCP_TEST_TOKEN_NEVER_SET".into());
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
        reason.contains("TARZ_MCP_TEST_TOKEN_NEVER_SET") && reason.contains("missing"),
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
        .insert("TARZ_MCP_FIXTURE_RELIST".into(), "hang".into());
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

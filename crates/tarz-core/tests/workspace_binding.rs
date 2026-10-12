#![cfg(any(target_os = "linux", target_os = "macos"))]

#[path = "common/workspace_http.rs"]
mod http;

use causa::kernel::{BlockContent, BlockId, Context, ToolResultPayload, ToolResultStatus};
use causa::runtime::{TurnInterruption, TurnResult};
use http::{Fixture, Reply};
use serde_json::Value;
use std::{collections::HashSet, path::Path, sync::Arc, time::Duration};
use tarz_core::application::{
    ClosePolicy, ManagedSession, PreparedCreate, SessionHost, text_input,
};
use tarz_core::definition::{CredentialRef, Provider, SessionDefinition, WorkspaceTool};
use tarz_core::session::{
    ProgressUpdate, SessionError, SubmitRequest, ToolPhase, WorkObservation, WorkProgress, WorkRef,
    WorkResult, WorkState,
};
use tokio::sync::broadcast;

fn definition(provider: Provider, endpoint: &str, root: &Path) -> SessionDefinition {
    SessionDefinition {
        provider,
        model: "workspace-binding-fixture".into(),
        endpoint: Some(endpoint.into()),
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(10),
        retained_work_capacity: 8,
        max_model_rounds: 4,
        max_tool_calls: 3,
        max_tokens: (provider == Provider::Anthropic).then_some(1024),
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        workspace_root: Some(root.canonicalize().unwrap()),
        tools: vec![WorkspaceTool::ReadFile],
        tool_limits: Default::default(),
    }
}
fn submit(session: &ManagedSession, key: &str) -> WorkRef {
    session
        .submit(SubmitRequest {
            request_key: key.into(),
            blocks: vec![text_input(key)],
        })
        .unwrap()
        .work
}
async fn finish(session: &ManagedSession, work: &WorkRef) -> WorkObservation {
    let observation = session.wait(work, Duration::from_secs(5)).await.unwrap();
    assert!(matches!(&observation.state, WorkState::Finished(_)));
    session.wait_saved().await.unwrap();
    observation
}
fn completed(observation: &WorkObservation) {
    assert!(
        matches!(&observation.state, WorkState::Finished(result)
        if matches!(&**result, WorkResult::Run(TurnResult::Completed { .. }))),
        "{observation:?}"
    );
}
fn cancelled(observation: &WorkObservation) {
    assert!(matches!(&observation.state, WorkState::Finished(result)
    if matches!(&**result, WorkResult::Run(TurnResult::Interrupted {
        cause: TurnInterruption::Cancelled { .. }
    }))));
}
fn calls(observation: &WorkObservation, statuses: &[ToolResultStatus]) -> Vec<BlockId> {
    assert_eq!(observation.tools.calls.len(), statuses.len());
    observation
        .tools
        .calls
        .iter()
        .zip(statuses)
        .map(|(call, expected)| {
            assert_eq!(call.invocation_id.turn_id, observation.work.turn_id);
            assert_eq!(call.tool_name, "read_file");
            assert_eq!(call.phase, ToolPhase::Committed);
            assert_eq!(call.status.as_ref(), Some(expected));
            call.call_block_id
        })
        .collect()
}
fn events(receiver: &mut broadcast::Receiver<Arc<WorkProgress>>, work: &WorkRef) {
    let mut phases = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        assert_eq!(
            &event.work, work,
            "per-work wrapper retained a stale WorkRef"
        );
        assert_eq!(event.invocation_id.turn_id, work.turn_id);
        if let ProgressUpdate::Tool(tool) = &event.update {
            assert_eq!(tool.invocation_id, event.invocation_id);
            phases.push(tool.phase);
        }
    }
    assert!(phases.contains(&ToolPhase::Running));
    assert!(phases.contains(&ToolPhase::Committed));
}
fn results(context: &Context) -> Vec<&ToolResultPayload> {
    let declarations: HashSet<_> = context
        .blocks()
        .iter()
        .filter_map(|block| {
            matches!(block.content(), BlockContent::ToolCall(_)).then_some(block.id())
        })
        .collect();
    context
        .blocks()
        .iter()
        .filter_map(|block| match block.content() {
            BlockContent::ToolResult(result) => {
                assert!(declarations.contains(&result.call_block_id));
                Some(result)
            }
            _ => None,
        })
        .collect()
}
fn schemas(provider: &str, requests: &[Value]) {
    for request in requests {
        assert_eq!(request["stream"], true);
        let tools = request["tools"].as_array().unwrap();
        assert_eq!(
            tools.len(),
            1,
            "only selected tools may enter provider request"
        );
        let tool = if provider == "openai-chat" {
            &tools[0]["function"]
        } else {
            &tools[0]
        };
        assert_eq!(tool["name"], "read_file");
        let schema = if provider == "anthropic" {
            &tool["input_schema"]
        } else {
            &tool["parameters"]
        };
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["path"]["type"], "string");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_host_binds_each_work_and_instance_to_its_saved_root_and_fresh_control() {
    let storage = tempfile::tempdir().unwrap();
    let root_a = tempfile::tempdir().unwrap();
    let root_b = tempfile::tempdir().unwrap();
    let marker_a = format!("root A {}", uuid::Uuid::now_v7());
    let marker_b = format!("root B {}", uuid::Uuid::now_v7());
    std::fs::write(root_a.path().join("marker.txt"), format!("{marker_a}\n")).unwrap();
    std::fs::write(root_b.path().join("marker.txt"), format!("{marker_b}\n")).unwrap();
    let fixture_a = Fixture::new(
        "openai-chat",
        vec![
            Reply::Read("marker.txt"),
            Reply::Answer,
            Reply::Read("marker.txt"),
            Reply::Answer,
            Reply::Read("marker.txt"),
            Reply::Answer,
            Reply::Read("marker.txt"),
            Reply::Answer,
        ],
        Some(3),
    );
    let fixture_b = Fixture::new(
        "openai-chat",
        vec![Reply::Read("marker.txt"), Reply::Answer],
        None,
    );
    let frozen_a = definition(Provider::OpenAiChat, &fixture_a.endpoint, root_a.path());
    let host = SessionHost::new(Some(storage.path().to_owned()));
    let a = host
        .create(PreparedCreate::persistent(
            frozen_a.clone(),
            Context::default(),
        ))
        .await
        .unwrap();
    let b = host
        .create(PreparedCreate::persistent(
            definition(Provider::OpenAiChat, &fixture_b.endpoint, root_b.path()),
            Context::default(),
        ))
        .await
        .unwrap();
    let mut events_a = a.subscribe_progress();
    let mut events_b = b.subscribe_progress();
    let a1 = submit(&a, "A first");
    let first = finish(&a, &a1).await;
    completed(&first);
    let first_id = calls(&first, &[ToolResultStatus::Succeeded])[0];
    events(&mut events_a, &a1);
    assert!(
        fixture_a.requests.lock().unwrap()[1]
            .to_string()
            .contains(&marker_a)
    );

    let cancelled_work = submit(&a, "A cancel after actual read");
    tokio::select! {
        _ = fixture_a.wait_for_requests(4) => {},
        stopped = a.wait(&cancelled_work, Duration::from_secs(5)) => panic!("cancel fixture did not reach continuation: {stopped:?}"),
    }
    a.cancel(&cancelled_work).unwrap();
    let cancelled_result = finish(&a, &cancelled_work).await;
    cancelled(&cancelled_result);
    let cancelled_id = calls(&cancelled_result, &[ToolResultStatus::Succeeded])[0];
    events(&mut events_a, &cancelled_work);
    fixture_a.release();

    let marker_a_new = format!("updated root A {}", uuid::Uuid::now_v7());
    std::fs::write(
        root_a.path().join("marker.txt"),
        format!("{marker_a_new}\n"),
    )
    .unwrap();
    let a2 = submit(&a, "A second after cancellation");
    let b1 = submit(&b, "B first while A also runs");
    let (second, separate) = tokio::join!(finish(&a, &a2), finish(&b, &b1));
    completed(&second);
    completed(&separate);
    let second_id = calls(&second, &[ToolResultStatus::Succeeded])[0];
    let separate_id = calls(&separate, &[ToolResultStatus::Succeeded])[0];
    events(&mut events_a, &a2);
    events(&mut events_b, &b1);
    assert_ne!(a1, a2);
    assert_ne!(a2.conversation_id, b1.conversation_id);
    assert_eq!(
        first.tools.calls[0].call_block_id, first_id,
        "later work must not mutate retained facts"
    );
    assert!(
        fixture_a.requests.lock().unwrap()[5]
            .to_string()
            .contains(&marker_a_new)
    );
    {
        let b_requests = fixture_b.requests.lock().unwrap();
        assert!(b_requests[1].to_string().contains(&marker_b));
        assert!(!b_requests[1].to_string().contains(&marker_a));
        assert!(!b_requests[1].to_string().contains(&marker_a_new));
        schemas("openai-chat", &b_requests);
    }

    let saved = a.read_materials().unwrap().context;
    assert_eq!(
        results(&saved)
            .iter()
            .map(|result| result.call_block_id)
            .collect::<Vec<_>>(),
        vec![first_id, cancelled_id, second_id]
    );
    assert_eq!(
        results(&b.read_materials().unwrap().context)[0].call_block_id,
        separate_id
    );
    assert!(
        results(&saved)[2]
            .output
            .content
            .to_string()
            .contains(&marker_a_new)
    );
    let target = a.target();
    host.close(&target, ClosePolicy::Save).await.unwrap();
    let reopened = host.open(target.conversation_id().clone()).await.unwrap();
    assert_ne!(reopened.target(), target);
    assert_eq!(reopened.definition(), frozen_a);
    assert_eq!(
        reopened.read_materials().unwrap().context.blocks(),
        saved.blocks()
    );
    assert!(matches!(
        reopened.observe(&a1),
        Err(SessionError::NotFound { .. })
    ));
    assert_eq!(
        fixture_a.requests.lock().unwrap().len(),
        6,
        "opening must not replay old tool declarations"
    );
    let mut new_events = reopened.subscribe_progress();
    let a3 = submit(&reopened, "A new opened instance");
    let new_instance = finish(&reopened, &a3).await;
    completed(&new_instance);
    let new_id = calls(&new_instance, &[ToolResultStatus::Succeeded])[0];
    events(&mut new_events, &a3);
    assert!(matches!(
        events_a.try_recv(),
        Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed)
    ));
    assert_eq!(
        [first_id, cancelled_id, second_id, separate_id, new_id]
            .into_iter()
            .collect::<HashSet<_>>()
            .len(),
        5
    );
    assert!(
        results(&reopened.read_materials().unwrap().context)
            .last()
            .unwrap()
            .output
            .content
            .to_string()
            .contains(&marker_a_new)
    );
    {
        let a_requests = fixture_a.requests.lock().unwrap();
        assert_eq!(a_requests.len(), 8);
        schemas("openai-chat", &a_requests);
    }
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
    host.close(&b.target(), ClosePolicy::Save).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_three_protocols_preserve_actual_partial_failure_and_recover_after_cancellation() {
    for (name, provider) in [
        ("openai-chat", Provider::OpenAiChat),
        ("openai-responses", Provider::OpenAiResponses),
        ("anthropic", Provider::Anthropic),
    ] {
        let root = tempfile::tempdir().unwrap();
        let marker = format!("actual {name} content {}", uuid::Uuid::now_v7());
        std::fs::write(root.path().join("marker.txt"), format!("{marker}\n")).unwrap();
        let fixture = Fixture::new(
            name,
            vec![
                Reply::Read("marker.txt"),
                Reply::Answer,
                Reply::Read("marker.txt"),
                Reply::Read("missing.txt"),
                Reply::Answer,
            ],
            Some(1),
        );
        let host = SessionHost::new(None);
        let session = host
            .create(PreparedCreate::transient(
                definition(provider, &fixture.endpoint, root.path()),
                Context::default(),
            ))
            .await
            .unwrap();
        let cancelled_work = submit(&session, "cancel after actual tool material");
        tokio::select! {
            _ = fixture.wait_for_requests(2) => {},
            stopped = session.wait(&cancelled_work, Duration::from_secs(5)) => panic!("{name} cancel fixture did not reach continuation: {stopped:?}"),
        }
        session.cancel(&cancelled_work).unwrap();
        let stopped = finish(&session, &cancelled_work).await;
        cancelled(&stopped);
        let old_call = calls(&stopped, &[ToolResultStatus::Succeeded])[0];
        fixture.release();
        let resumed_work = submit(&session, "successful read then missing file");
        let observation = finish(&session, &resumed_work).await;
        completed(&observation);
        let new_calls = calls(
            &observation,
            &[ToolResultStatus::Succeeded, ToolResultStatus::Failed],
        );
        assert_eq!(
            [old_call, new_calls[0], new_calls[1]]
                .into_iter()
                .collect::<HashSet<_>>()
                .len(),
            3
        );
        let context = session.read_materials().unwrap().context;
        let outputs = results(&context);
        assert_eq!(
            outputs
                .iter()
                .map(|result| result.call_block_id)
                .collect::<Vec<_>>(),
            vec![old_call, new_calls[0], new_calls[1]]
        );
        assert_eq!(outputs[2].status, ToolResultStatus::Failed);
        assert!(outputs[1].output.content.to_string().contains(&marker));
        assert!(
            outputs[2]
                .output
                .content
                .to_string()
                .contains("workspace I/O failed")
        );
        {
            let requests = fixture.requests.lock().unwrap();
            assert_eq!(requests.len(), 5);
            schemas(name, &requests);
            assert!(requests[1].to_string().contains(&marker));
            assert!(requests[4].to_string().contains(&marker));
            assert!(requests[4].to_string().contains("workspace I/O failed"));
        }
        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }
}

#[test]
fn accepted_http_socket_waits_for_delayed_request_bytes() {
    use std::io::{Read, Write};
    use std::sync::atomic::Ordering;
    let fixture = Fixture::new("openai-chat", vec![Reply::Answer], None);
    let url = url::Url::parse(&fixture.endpoint).unwrap();
    let mut socket =
        std::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while fixture.accepted.load(Ordering::SeqCst) == 0 {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    // The accept barrier makes missing initial bytes intentional rather than
    // relying on scheduler luck to place accept before the client writes.
    std::thread::sleep(Duration::from_millis(20));
    socket
        .write_all(
            b"POST /v1/chat/completions HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\n{}",
        )
        .unwrap();
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    assert!(
        response.starts_with("HTTP/1.1 200 OK"),
        "fixture closed an accepted connection before reading its request: {response:?}"
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
}

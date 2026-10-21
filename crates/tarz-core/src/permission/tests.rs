use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use causa::kernel::{
    BlockContent, Context, ProcessorContext, ProcessorError, ToolBatch, ToolBatchProcessor,
    ToolHints, ToolResultStatus,
};
use causa::runtime::{TurnInterruption, TurnResult};
use serde_json::json;

use crate::application::{
    ApplicationError, ClosePolicy, ManagedSession, PreparedCreate, SessionHost, text_input,
};
use crate::definition::{
    CredentialRef, Provider, SessionDefinition, WorkspaceTool, WorkspaceToolLimits,
};
use crate::permission::{
    ApprovalDecision, ApprovalSubject, PermissionDecisionKind, PermissionMode, PermissionView,
};
use crate::session::ToolPhase;
use crate::session::{SubmitRequest, WorkObservation, WorkRef, WorkResult, WorkState};

use crate::application::tests::workspace_http::*;

fn base_definition(endpoint: &str, root: &Path) -> SessionDefinition {
    SessionDefinition {
        provider: Provider::OpenAiChat,
        model: "permission-fixture".into(),
        endpoint: Some(endpoint.into()),
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(10),
        retained_work_capacity: 8,
        max_model_rounds: 4,
        max_tool_calls: 5,
        max_tokens: None,
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: Some(root.canonicalize().unwrap()),
        tools: vec![
            WorkspaceTool::ReadFile,
            WorkspaceTool::WriteFile,
            WorkspaceTool::EditFile,
            WorkspaceTool::ListDirectory,
            WorkspaceTool::SearchText,
        ],
        tool_limits: WorkspaceToolLimits::default(),
        write_limits: None,
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

fn call_write(path: &str, content: &str) -> Reply {
    Reply::Call(
        "write_file".into(),
        json!({
            "path": path,
            "content": content,
        }),
    )
}

fn call_edit(path: &str, old_text: &str, new_text: &str) -> Reply {
    Reply::Call(
        "edit_file".into(),
        json!({
            "path": path,
            "edits": [{
                "old_text": old_text,
                "new_text": new_text,
            }],
        }),
    )
}

fn call_read(path: &str) -> Reply {
    Reply::Call(
        "read_file".into(),
        json!({
            "path": path,
        }),
    )
}

// 1. 4 Modes Table: write_file and read_file in the same batch across all 4 modes
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn four_modes_table_write_and_read() {
    for mode in [
        PermissionMode::ReadOnly,
        PermissionMode::Ask,
        PermissionMode::AcceptEdits,
        PermissionMode::FullAccess,
    ] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("existing.txt"), "hello world\n").unwrap();

        let replies = vec![
            Reply::Calls(vec![
                ScriptedToolCall {
                    name: "read_file".into(),
                    arguments: json!({"path": "existing.txt"}),
                },
                ScriptedToolCall {
                    name: "write_file".into(),
                    arguments: json!({"path": "created.txt", "content": "content"}),
                },
            ]),
            Reply::Answer,
        ];

        let fixture = Fixture::new("openai-chat", replies, None);
        let host = SessionHost::new(None);
        let session = host
            .create(PreparedCreate::transient(
                base_definition(&fixture.endpoint, root.path()),
                Context::default(),
            ))
            .await
            .unwrap();

        session.set_permission_mode(mode).unwrap();

        match mode {
            PermissionMode::ReadOnly => {
                let work = submit(&session, "batch readonly");
                let obs = finish(&session, &work).await;
                assert_eq!(obs.tools.calls.len(), 2);
                assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Succeeded));
                assert_eq!(obs.tools.calls[1].status, Some(ToolResultStatus::Rejected));
                let d = obs.tools.calls[1].permission_decision.as_ref().unwrap();
                assert_eq!(d.kind, PermissionDecisionKind::ModeRefused);

                let context = session.read_materials().unwrap().context;
                let payload = context
                    .blocks()
                    .iter()
                    .find_map(|b| match b.content() {
                        BlockContent::ToolResult(p)
                            if p.call_block_id == obs.tools.calls[1].call_block_id =>
                        {
                            Some(p)
                        }
                        _ => None,
                    })
                    .unwrap();
                assert!(payload.output.content.to_string().contains("read-only"));
                assert!(!root.path().join("created.txt").exists());
            }
            PermissionMode::Ask => {
                let mut rx = session.connect_approvals();
                let approval_task = tokio::spawn(async move {
                    if let Some(req) = rx.recv().await {
                        req.reply.send(ApprovalDecision::Allow).unwrap();
                    }
                });
                let work = submit(&session, "batch ask allow");
                let obs = finish(&session, &work).await;
                approval_task.await.unwrap();

                assert_eq!(obs.tools.calls.len(), 2);
                assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Succeeded));
                assert_eq!(obs.tools.calls[1].status, Some(ToolResultStatus::Succeeded));
                assert!(root.path().join("created.txt").exists());
                let d = obs.tools.calls[1].permission_decision.as_ref().unwrap();
                assert_eq!(d.kind, PermissionDecisionKind::AskedAndAllowed);
            }
            PermissionMode::AcceptEdits | PermissionMode::FullAccess => {
                let work = submit(&session, "batch direct execution");
                let obs = finish(&session, &work).await;

                assert_eq!(obs.tools.calls.len(), 2);
                assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Succeeded));
                assert_eq!(obs.tools.calls[1].status, Some(ToolResultStatus::Succeeded));
                assert!(root.path().join("created.txt").exists());
                let d = obs.tools.calls[1].permission_decision.as_ref().unwrap();
                assert_eq!(d.kind, PermissionDecisionKind::DirectExecution);
            }
        }

        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }
}

// 2. Core approval flow: Allow, Deny with reason, batch ordering, and rejection reason sent to model
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm05_core_allow_deny_and_batch_ordering() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![
        // Turn 1: two writes to different files
        Reply::Calls(vec![
            ScriptedToolCall {
                name: "write_file".into(),
                arguments: json!({"path": "file1.txt", "content": "one"}),
            },
            ScriptedToolCall {
                name: "write_file".into(),
                arguments: json!({"path": "file2.txt", "content": "two"}),
            },
        ]),
        Reply::Answer,
        // Turn 2: two writes to the same file in a single batch
        Reply::Calls(vec![
            ScriptedToolCall {
                name: "write_file".into(),
                arguments: json!({"path": "same.txt", "content": "v1"}),
            },
            ScriptedToolCall {
                name: "write_file".into(),
                arguments: json!({"path": "same.txt", "content": "v2"}),
            },
        ]),
        Reply::Answer,
    ];

    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let session = host
        .create(PreparedCreate::transient(
            base_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .unwrap();

    let mut rx = session.connect_approvals();

    // Turn 1: file1 Allow, file2 Deny with reason
    let approval_task1 = tokio::spawn(async move {
        // Request 1: file1
        let req1 = rx.recv().await.expect("req1");
        let ApprovalSubject::FileWrite { path, .. } = &req1.subject;
        assert_eq!(path, "file1.txt");
        req1.reply.send(ApprovalDecision::Allow).unwrap();

        // Request 2: file2
        let req2 = rx.recv().await.expect("req2");
        let ApprovalSubject::FileWrite { path, .. } = &req2.subject;
        assert_eq!(path, "file2.txt");
        req2.reply
            .send(ApprovalDecision::Deny {
                reason: Some("policy disallows file2".into()),
            })
            .unwrap();
        rx
    });

    let work1 = submit(&session, "two different files");
    let obs1 = finish(&session, &work1).await;
    let mut rx = approval_task1.await.unwrap();

    assert_eq!(obs1.tools.calls.len(), 2);
    assert_eq!(
        obs1.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(obs1.tools.calls[1].status, Some(ToolResultStatus::Rejected));
    assert!(root.path().join("file1.txt").exists());
    assert!(!root.path().join("file2.txt").exists());

    // Verify ToolProgress records decisions
    let d0 = obs1.tools.calls[0].permission_decision.as_ref().unwrap();
    assert_eq!(d0.mode, PermissionMode::Ask);
    assert_eq!(d0.kind, PermissionDecisionKind::AskedAndAllowed);

    let d1 = obs1.tools.calls[1].permission_decision.as_ref().unwrap();
    assert_eq!(d1.mode, PermissionMode::Ask);
    assert_eq!(d1.kind, PermissionDecisionKind::AskedAndDenied);

    // Verify rejection reason appears in the model follow-up request
    {
        let requests = fixture.requests.lock().unwrap();
        assert!(requests.len() >= 2);
        let req1_str = requests[1].to_string();
        assert!(
            req1_str.contains("user declined this write: policy disallows file2"),
            "model request payload must contain refusal reason: {req1_str}"
        );
    }

    // Turn 2: two writes to SAME file
    let approval_task2 = tokio::spawn(async move {
        let req = rx.recv().await.expect("req for same.txt");
        let ApprovalSubject::FileWrite { path, .. } = &req.subject;
        assert_eq!(path, "same.txt");
        req.reply.send(ApprovalDecision::Allow).unwrap();
        // There must NOT be a second request for same.txt
        let next = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(next.is_err(), "should not produce 2nd approval request");
        rx
    });

    let work2 = submit(&session, "same file twice in batch");
    let obs2 = finish(&session, &work2).await;
    let _rx = approval_task2.await.unwrap();

    assert_eq!(obs2.tools.calls.len(), 2);
    assert_eq!(
        obs2.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(obs2.tools.calls[1].status, Some(ToolResultStatus::Rejected));

    let d_same0 = obs2.tools.calls[0].permission_decision.as_ref().unwrap();
    assert_eq!(d_same0.kind, PermissionDecisionKind::AskedAndAllowed);

    // Second call rejected in precheck: must NOT record a permission decision
    assert!(obs2.tools.calls[1].permission_decision.is_none());

    let context = session.read_materials().unwrap().context;
    let payload = context
        .blocks()
        .iter()
        .find_map(|b| match b.content() {
            BlockContent::ToolResult(p) if p.call_block_id == obs2.tools.calls[1].call_block_id => {
                Some(p)
            }
            _ => None,
        })
        .unwrap();
    assert!(
        payload
            .output
            .content
            .to_string()
            .contains("same file can only be modified once")
    );

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

// 3. File modified between request and Allow -> Failed, external content preserved
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm04_file_modified_between_approval_and_allow() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("target.txt"), "original content\n").unwrap();

    let replies = vec![
        call_read("target.txt"),
        Reply::Answer,
        call_edit("target.txt", "original content", "my new content"),
        Reply::Answer,
    ];

    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let session = host
        .create(PreparedCreate::transient(
            base_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .unwrap();

    // Turn 1: read file
    let work1 = submit(&session, "read target");
    finish(&session, &work1).await;

    let mut rx = session.connect_approvals();
    let root_path = root.path().to_path_buf();

    let approval_task = tokio::spawn(async move {
        let req = rx.recv().await.expect("approval request");
        let ApprovalSubject::FileWrite { path, .. } = &req.subject;
        assert_eq!(path, "target.txt");
        // External third-party modification occurs before Allow
        std::fs::write(root_path.join("target.txt"), "external third party edit\n").unwrap();
        // Now send Allow
        req.reply.send(ApprovalDecision::Allow).unwrap();
    });

    let work2 = submit(&session, "edit target");
    let obs2 = finish(&session, &work2).await;
    approval_task.await.unwrap();

    assert_eq!(obs2.tools.calls.len(), 1);
    assert_eq!(obs2.tools.calls[0].status, Some(ToolResultStatus::Failed));

    let current = std::fs::read_to_string(root.path().join("target.txt")).unwrap();
    assert_eq!(current, "external third party edit\n");

    let context = session.read_materials().unwrap().context;
    let payload = context
        .blocks()
        .iter()
        .find_map(|b| match b.content() {
            BlockContent::ToolResult(p) if p.call_block_id == obs2.tools.calls[0].call_block_id => {
                Some(p)
            }
            _ => None,
        })
        .unwrap();
    assert!(
        payload
            .output
            .content
            .to_string()
            .contains("file changed after confirmation")
    );

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

// 4. Approval wait and cancellation
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm07_call_timeout_does_not_abort_approval_wait() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![call_write("timeout_test.txt", "success"), Reply::Answer];

    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let mut def = base_definition(&fixture.endpoint, root.path());
    def.tool_limits.call_timeout_secs = 1; // 1 second call timeout

    let session = host
        .create(PreparedCreate::transient(def, Context::default()))
        .await
        .unwrap();

    let mut rx = session.connect_approvals();
    let approval_task = tokio::spawn(async move {
        let req = rx.recv().await.expect("approval request");
        // Wait over 2 seconds (exceeding call_timeout_secs = 1)
        tokio::time::sleep(Duration::from_millis(2100)).await;
        req.reply.send(ApprovalDecision::Allow).unwrap();
    });

    let work = submit(&session, "write with delayed approval");
    let obs = finish(&session, &work).await;
    approval_task.await.unwrap();

    assert_eq!(obs.tools.calls.len(), 1);
    assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Succeeded));
    assert_eq!(
        std::fs::read_to_string(root.path().join("timeout_test.txt")).unwrap(),
        "success"
    );

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm07_cancel_during_approval_wait() {
    let root = tempfile::tempdir().unwrap();
    let store_root = tempfile::tempdir().unwrap();
    let replies = vec![call_write("cancel_test.txt", "never"), Reply::Answer];

    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(Some(store_root.path().into()));
    let session = host
        .create(PreparedCreate::persistent(
            base_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .unwrap();

    let mut rx = session.connect_approvals();
    let entered = Arc::new(tokio::sync::Notify::new());
    let entered_clone = entered.clone();

    let approval_task = tokio::spawn(async move {
        let _req = rx.recv().await.expect("approval request");
        entered_clone.notify_one();
        // Wait without answering until cancelled
        tokio::time::sleep(Duration::from_secs(5)).await;
    });

    let work = submit(&session, "write and cancel");
    entered.notified().await;
    session.cancel(&work).unwrap();

    let obs = session.wait(&work, Duration::from_secs(5)).await.unwrap();
    approval_task.abort();

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
    assert!(!root.path().join("cancel_test.txt").exists());

    session.wait_saved().await.unwrap();

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm07_work_deadline_truncates_approval_wait() {
    let root = tempfile::tempdir().unwrap();
    let store_root = tempfile::tempdir().unwrap();
    let replies = vec![call_write("deadline_test.txt", "never"), Reply::Answer];

    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(Some(store_root.path().into()));
    let mut def = base_definition(&fixture.endpoint, root.path());
    def.work_timeout_secs = Some(1); // 1 second work timeout

    let session = host
        .create(PreparedCreate::persistent(def, Context::default()))
        .await
        .unwrap();

    let mut rx = session.connect_approvals();
    let approval_task = tokio::spawn(async move {
        let _req = rx.recv().await.expect("approval request");
        // Sleep 2 seconds without answering, exceeding work_timeout_secs
        tokio::time::sleep(Duration::from_millis(2000)).await;
    });

    let work = submit(&session, "work timeout");
    let obs = session.wait(&work, Duration::from_secs(5)).await.unwrap();
    approval_task.abort();

    assert!(matches!(
        obs.state,
        WorkState::Finished(ref r) if matches!(**r, WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::DeadlineExceeded { .. }
        }))
    ));
    assert_eq!(obs.tools.calls.len(), 1);
    let call = &obs.tools.calls[0];
    assert!(
        call.phase == ToolPhase::NotStarted || call.operation_not_started,
        "deadline truncated wait must record tool as not started"
    );
    assert!(!root.path().join("deadline_test.txt").exists());

    session.wait_saved().await.unwrap();

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

// 5. No confirmation route returns Rejected without blocking
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_confirmation_route_unconnected_returns_rejected() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![call_write("no_handler.txt", "content"), Reply::Answer];

    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let session = host
        .create(PreparedCreate::transient(
            base_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .unwrap();

    // Never connected approvals
    let work = submit(&session, "no receiver");
    let obs = finish(&session, &work).await;

    assert_eq!(obs.tools.calls.len(), 1);
    assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    let d = obs.tools.calls[0].permission_decision.as_ref().unwrap();
    assert_eq!(d.kind, PermissionDecisionKind::NoConfirmationRoute);
    assert!(!root.path().join("no_handler.txt").exists());

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_confirmation_route_dropped_receiver_returns_rejected() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![call_write("dropped_rx.txt", "content"), Reply::Answer];

    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let session = host
        .create(PreparedCreate::transient(
            base_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .unwrap();

    // Connected then dropped receiver
    let rx = session.connect_approvals();
    drop(rx);

    let work = submit(&session, "dropped receiver");
    let obs = finish(&session, &work).await;

    assert_eq!(obs.tools.calls.len(), 1);
    assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    let d = obs.tools.calls[0].permission_decision.as_ref().unwrap();
    assert_eq!(d.kind, PermissionDecisionKind::NoConfirmationRoute);
    assert!(!root.path().join("dropped_rx.txt").exists());

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_confirmation_route_dropped_reply_handle_returns_rejected() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![call_write("dropped_reply.txt", "content"), Reply::Answer];

    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let session = host
        .create(PreparedCreate::transient(
            base_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .unwrap();

    let mut rx = session.connect_approvals();
    let drop_task = tokio::spawn(async move {
        let req = rx.recv().await.expect("req");
        drop(req.reply); // Drop reply handle explicitly
    });

    let work = submit(&session, "dropped reply");
    let obs = finish(&session, &work).await;
    drop_task.await.unwrap();

    assert_eq!(obs.tools.calls.len(), 1);
    assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    let d = obs.tools.calls[0].permission_decision.as_ref().unwrap();
    assert_eq!(d.kind, PermissionDecisionKind::NoConfirmationRoute);
    assert!(!root.path().join("dropped_reply.txt").exists());

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

// 6. Runtime mode switch
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_mode_switch_wm17() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![
        // Turn 1: batch with 2 writes
        Reply::Calls(vec![
            ScriptedToolCall {
                name: "write_file".into(),
                arguments: json!({"path": "file1.txt", "content": "1"}),
            },
            ScriptedToolCall {
                name: "write_file".into(),
                arguments: json!({"path": "file2.txt", "content": "2"}),
            },
        ]),
        Reply::Answer,
        // Turn 2: next batch with 1 write
        Reply::Calls(vec![ScriptedToolCall {
            name: "write_file".into(),
            arguments: json!({"path": "file3.txt", "content": "3"}),
        }]),
        Reply::Answer,
    ];

    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let session = host
        .create(PreparedCreate::transient(
            base_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .unwrap();

    let mut rx = session.connect_approvals();
    let session_clone = session.clone();

    let switch_task = tokio::spawn(async move {
        let req = rx.recv().await.expect("req for file1.txt");
        let ApprovalSubject::FileWrite { path, .. } = &req.subject;
        assert_eq!(path, "file1.txt");
        // Switch to AcceptEdits mid-batch before replying
        let (mode, rev) = session_clone
            .set_permission_mode(PermissionMode::AcceptEdits)
            .unwrap();
        assert_eq!(mode, PermissionMode::AcceptEdits);
        assert_eq!(rev, 1);
        req.reply.send(ApprovalDecision::Allow).unwrap();
        // Second write should NOT trigger an approval request
        let next = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
        assert!(next.is_err(), "second write must not ask under AcceptEdits");
        rx
    });

    let work1 = submit(&session, "switch during batch");
    let obs1 = finish(&session, &work1).await;
    let mut rx = switch_task.await.unwrap();

    assert_eq!(obs1.tools.calls.len(), 2);
    assert_eq!(
        obs1.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(
        obs1.tools.calls[1].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert!(root.path().join("file1.txt").exists());
    assert!(root.path().join("file2.txt").exists());

    let d0 = obs1.tools.calls[0].permission_decision.as_ref().unwrap();
    assert_eq!(d0.mode, PermissionMode::Ask);
    assert_eq!(d0.revision, 0);
    assert_eq!(d0.kind, PermissionDecisionKind::AskedAndAllowed);

    let d1 = obs1.tools.calls[1].permission_decision.as_ref().unwrap();
    assert_eq!(d1.mode, PermissionMode::AcceptEdits);
    assert_eq!(d1.revision, 1);
    assert_eq!(d1.kind, PermissionDecisionKind::DirectExecution);

    // Switch back to Ask
    let (current, rev) = session.set_permission_mode(PermissionMode::Ask).unwrap();
    assert_eq!(current, PermissionMode::Ask);
    assert_eq!(rev, 2);

    let ask_again_task = tokio::spawn(async move {
        let req = rx.recv().await.expect("req for file3.txt");
        let ApprovalSubject::FileWrite { path, .. } = &req.subject;
        assert_eq!(path, "file3.txt");
        req.reply.send(ApprovalDecision::Allow).unwrap();
    });

    let work2 = submit(&session, "ask again");
    let obs2 = finish(&session, &work2).await;
    ask_again_task.await.unwrap();

    assert_eq!(obs2.tools.calls.len(), 1);
    assert_eq!(
        obs2.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert!(root.path().join("file3.txt").exists());

    let d2 = obs2.tools.calls[0].permission_decision.as_ref().unwrap();
    assert_eq!(d2.mode, PermissionMode::Ask);
    assert_eq!(d2.revision, 2);
    assert_eq!(d2.kind, PermissionDecisionKind::AskedAndAllowed);

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

// 7. Processor chain order and hints
struct VerifyingProcessor {
    seen_mode: Mutex<Option<PermissionMode>>,
    seen_revision: Mutex<Option<u64>>,
    seen_hints: Mutex<Vec<(String, ToolHints)>>,
}

#[async_trait]
impl ToolBatchProcessor for VerifyingProcessor {
    async fn process(
        &self,
        batch: &mut ToolBatch,
        ctx: &ProcessorContext<'_>,
    ) -> Result<(), ProcessorError> {
        if let Some(view) = ctx.data::<PermissionView>() {
            *self.seen_mode.lock().unwrap() = Some(view.mode());
            *self.seen_revision.lock().unwrap() = Some(view.revision());
        }
        let mut hints = Vec::new();
        for entry in batch.calls() {
            let tool_name = entry.call().input.tool_name.clone();
            let hint = ctx.hints(&tool_name).cloned().unwrap_or_default();
            hints.push((tool_name, hint));
        }
        *self.seen_hints.lock().unwrap() = hints;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn processor_chain_order_and_hints_wm17() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("f.txt"), "hello\n").unwrap();

    let replies = vec![
        call_read("f.txt"),
        Reply::Answer,
        Reply::Calls(vec![
            ScriptedToolCall {
                name: "read_file".into(),
                arguments: json!({"path": "f.txt"}),
            },
            ScriptedToolCall {
                name: "list_directory".into(),
                arguments: json!({"path": "."}),
            },
            ScriptedToolCall {
                name: "search_text".into(),
                arguments: json!({"path": ".", "query": "hello"}),
            },
            ScriptedToolCall {
                name: "write_file".into(),
                arguments: json!({"path": "w.txt", "content": "1"}),
            },
            ScriptedToolCall {
                name: "edit_file".into(),
                arguments: json!({"path": "f.txt", "edits": [{"old_text": "hello\n", "new_text": "bye\n"}]}),
            },
        ]),
        Reply::Answer,
    ];

    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let session = host
        .create(PreparedCreate::transient(
            base_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .unwrap();

    // Mode switches: initial Ask(0) -> ReadOnly(1) -> AcceptEdits(2)
    let (m1, r1) = session
        .set_permission_mode(PermissionMode::ReadOnly)
        .unwrap();
    assert_eq!(m1, PermissionMode::ReadOnly);
    assert_eq!(r1, 1);
    let (m2, r2) = session
        .set_permission_mode(PermissionMode::AcceptEdits)
        .unwrap();
    assert_eq!(m2, PermissionMode::AcceptEdits);
    assert_eq!(r2, 2);

    let verifier = Arc::new(VerifyingProcessor {
        seen_mode: Mutex::new(None),
        seen_revision: Mutex::new(None),
        seen_hints: Mutex::new(Vec::new()),
    });

    session.add_test_before_processor(verifier.clone());

    let work0 = submit(&session, "read first");
    finish(&session, &work0).await;

    let work = submit(&session, "verify processor chain");
    let obs = finish(&session, &work).await;
    assert_eq!(obs.tools.calls.len(), 5);

    let seen_mode = *verifier.seen_mode.lock().unwrap();
    let seen_rev = *verifier.seen_revision.lock().unwrap();

    assert_eq!(seen_mode, Some(PermissionMode::AcceptEdits));
    assert_eq!(seen_rev, Some(2));

    // Assert that mode & revision read by test processor matches decisions on all calls
    for call in &obs.tools.calls {
        let decision = call
            .permission_decision
            .as_ref()
            .expect("decision must be recorded for each call");
        assert_eq!(decision.mode, seen_mode.unwrap());
        assert_eq!(decision.revision, seen_rev.unwrap());
        assert_eq!(decision.revision, 2); // equal to switch count
    }

    let hints = verifier.seen_hints.lock().unwrap().clone();
    assert_eq!(hints.len(), 5);

    // Assert complete tool hints for all five built-in tools
    // 1. read_file
    assert_eq!(hints[0].0, "read_file");
    assert_eq!(hints[0].1.read_only, Some(true));
    assert_eq!(hints[0].1.destructive, None);
    assert_eq!(hints[0].1.idempotent, None);
    assert_eq!(hints[0].1.open_world, Some(false));

    // 2. list_directory
    assert_eq!(hints[1].0, "list_directory");
    assert_eq!(hints[1].1.read_only, Some(true));
    assert_eq!(hints[1].1.destructive, None);
    assert_eq!(hints[1].1.idempotent, None);
    assert_eq!(hints[1].1.open_world, Some(false));

    // 3. search_text
    assert_eq!(hints[2].0, "search_text");
    assert_eq!(hints[2].1.read_only, Some(true));
    assert_eq!(hints[2].1.destructive, None);
    assert_eq!(hints[2].1.idempotent, None);
    assert_eq!(hints[2].1.open_world, Some(false));

    // 4. write_file
    assert_eq!(hints[3].0, "write_file");
    assert_eq!(hints[3].1.read_only, Some(false));
    assert_eq!(hints[3].1.destructive, Some(true));
    assert_eq!(hints[3].1.idempotent, Some(true));
    assert_eq!(hints[3].1.open_world, Some(false));

    // 5. edit_file
    assert_eq!(hints[4].0, "edit_file");
    assert_eq!(hints[4].1.read_only, Some(false));
    assert_eq!(hints[4].1.destructive, Some(true));
    assert_eq!(hints[4].1.idempotent, Some(false));
    assert_eq!(hints[4].1.open_world, Some(false));

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

// 8. Model prompts unaffected by mode switch
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompts_and_materials_unaffected_by_mode_switch_wm17() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![Reply::Answer, Reply::Answer];

    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let session = host
        .create(PreparedCreate::transient(
            base_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .unwrap();

    let work1 = submit(&session, "turn first question");
    finish(&session, &work1).await;

    let materials_before = session.read_materials().unwrap().context;

    // Switch mode
    session
        .set_permission_mode(PermissionMode::AcceptEdits)
        .unwrap();

    let materials_after = session.read_materials().unwrap().context;
    assert_eq!(
        materials_before.blocks(),
        materials_after.blocks(),
        "read_materials context must not change upon permission mode switch"
    );

    let work2 = submit(&session, "turn second question");
    finish(&session, &work2).await;

    // Verify requests captured by Fixture contain no permission instructions
    {
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for req in requests.iter() {
            let req_str = req.to_string();
            for forbidden in [
                "read-only",
                "accept-edits",
                "full-access",
                "PermissionMode",
                "PermissionView",
            ] {
                assert!(
                    !req_str.contains(forbidden),
                    "model request leaked permission term \"{forbidden}\": {req_str}"
                );
            }
        }
    }

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

// 9. Turn isolation: Prepared write from turn 1 cannot be executed in turn 2
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_isolation_prepared_write_cannot_be_executed_in_subsequent_turn() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![
        call_write("t1.txt", "v1"),
        Reply::Answer,
        call_write("t2.txt", "v2"),
        Reply::Answer,
    ];

    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let session = host
        .create(PreparedCreate::transient(
            base_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .unwrap();

    let mut rx = session.connect_approvals();

    // Turn 1: Deny write
    let deny_task = tokio::spawn(async move {
        let req = rx.recv().await.expect("req1");
        req.reply
            .send(ApprovalDecision::Deny {
                reason: Some("denied".into()),
            })
            .unwrap();
        rx
    });

    let work1 = submit(&session, "turn 1 write");
    let obs1 = finish(&session, &work1).await;
    let mut rx = deny_task.await.unwrap();

    assert_eq!(obs1.tools.calls[0].status, Some(ToolResultStatus::Rejected));

    // Turn 2: Allow write
    let allow_task = tokio::spawn(async move {
        let req = rx.recv().await.expect("req2");
        req.reply.send(ApprovalDecision::Allow).unwrap();
    });

    let work2 = submit(&session, "turn 2 write");
    let obs2 = finish(&session, &work2).await;
    allow_task.await.unwrap();

    assert_eq!(
        obs2.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert!(!root.path().join("t1.txt").exists());
    assert!(root.path().join("t2.txt").exists());

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

// 10. Unprepared write under Ask mode records Unprepared decision and returns Rejected
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unprepared_write_in_ask_mode_records_unprepared_decision() {
    use causa::kernel::{
        CallControl, InvocationId, RoundId, ToolCallContext, ToolCallPayload, TurnId,
    };
    use tokio_util::sync::CancellationToken;

    let call_id = causa::kernel::BlockId::new(uuid::Uuid::now_v7());
    let call = ToolCallContext::new(
        call_id,
        ToolCallPayload::new(
            "write_file",
            json!({
                "path": "unprepared.txt",
                "content": "hello",
            }),
        ),
    );
    let turn_id = TurnId::new(uuid::Uuid::now_v7().to_string());
    let work = WorkRef {
        conversation_id: "test".into(),
        turn_id: turn_id.clone(),
    };
    let reporter =
        crate::session::ToolReporter::new(work.clone(), tokio::sync::broadcast::channel(128).0);
    let batch_initial = ToolBatch::new(vec![call.clone()]).unwrap();
    reporter.batch_ready(
        &InvocationId {
            turn_id: turn_id.clone(),
            round_id: RoundId(0),
        },
        &batch_initial,
    );

    let hub = Arc::new(crate::permission::approval::ApprovalsHub::new());
    let prepared_empty = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

    let processor =
        crate::permission::PermissionProcessor::new(work, reporter.clone(), hub, prepared_empty);

    let mut batch = ToolBatch::new(vec![call.clone()]).unwrap();
    let declaration_order = [call.call_block_id];
    let control = CallControl::new(CancellationToken::new(), None);
    let view = crate::permission::PermissionHub::new(PermissionMode::Ask).view();
    let processor_data = causa::kernel::ProcessorData::new(view);
    let ctx = ProcessorContext::new(&turn_id, RoundId(0), &declaration_order, &control)
        .with_data(&processor_data);

    processor.process(&mut batch, &ctx).await.unwrap();

    let snap = reporter.snapshot();
    assert_eq!(snap.calls.len(), 1);
    let decision = snap.calls[0].permission_decision.as_ref().unwrap();
    assert_eq!(decision.mode, PermissionMode::Ask);
    assert_eq!(decision.kind, PermissionDecisionKind::Unprepared);

    let (_, result) = batch.results().first().unwrap().result().unwrap();
    assert_eq!(result.status, ToolResultStatus::Rejected);
    assert_eq!(
        result.output.content["error"],
        "write operation was not prepared by precheck"
    );
}

// 11. Session constructed without permissions behavior
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_without_permissions_behavior() {
    let conversation_id = crate::session::ConversationId::from("no-perm-test");
    let context = Context::default();
    let config = crate::session::SessionConfig {
        retained_work_capacity: 8,
        work_timeout: None,
    };

    let session = crate::session::Session::new(
        conversation_id,
        context,
        move |_work, _control| async {
            panic!("not called");
        },
        config,
    );

    // permission_mode returns None
    assert_eq!(session.permission_mode(), None);

    // set_permission_mode returns Err(ApplicationError::PermissionUnavailable)
    let res = session.set_permission_mode(PermissionMode::ReadOnly);
    assert!(matches!(res, Err(ApplicationError::PermissionUnavailable)));

    // connect_approvals returns immediately closed receiver
    let mut rx = session.connect_approvals();
    assert!(rx.recv().await.is_none());
}

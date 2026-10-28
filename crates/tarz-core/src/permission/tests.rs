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

use crate::application::SaveStatus;
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
use crate::store::{CommitStage, FaultAction};

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
        permission_mode: None,
        command: None,
        mcp_servers: Vec::new(),
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
        let ApprovalSubject::FileWrite { path, .. } = &req1.subject else {
            panic!("expected FileWrite")
        };
        assert_eq!(path, "file1.txt");
        req1.reply.send(ApprovalDecision::Allow).unwrap();

        // Request 2: file2
        let req2 = rx.recv().await.expect("req2");
        let ApprovalSubject::FileWrite { path, .. } = &req2.subject else {
            panic!("expected FileWrite")
        };
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
        let ApprovalSubject::FileWrite { path, .. } = &req.subject else {
            panic!("expected FileWrite")
        };
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
        let ApprovalSubject::FileWrite { path, .. } = &req.subject else {
            panic!("expected FileWrite")
        };
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
        let ApprovalSubject::FileWrite { path, .. } = &req.subject else {
            panic!("expected FileWrite")
        };
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
        let ApprovalSubject::FileWrite { path, .. } = &req.subject else {
            panic!("expected FileWrite")
        };
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

    let processor = crate::permission::PermissionProcessor::new(
        work,
        reporter.clone(),
        hub,
        crate::definition::CommandSettings::default(),
        prepared_empty,
    );

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

fn tool_result_error(session: &ManagedSession, call_block_id: causa::kernel::BlockId) -> String {
    let context = session.read_materials().unwrap().context;
    let payload = context
        .blocks()
        .iter()
        .find_map(|b| match b.content() {
            BlockContent::ToolResult(p) if p.call_block_id == call_block_id => Some(p),
            _ => None,
        })
        .unwrap();
    payload.output.content["error"]
        .as_str()
        .unwrap()
        .to_string()
}

// Old V2 record tests

#[tokio::test]
async fn legacy_v2_record_fixture_reopen_preserves_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("store");
    let sessions_dir = storage.join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();

    let id = uuid::Uuid::now_v7().to_string();
    let commit = uuid::Uuid::now_v7().to_string();
    let record_path = sessions_dir.join(format!("{id}.json"));
    let raw_json = format!(
        r#"{{"format":"tarz-session-v2","conversation_id":"{id}","revision":1,"last_commit_id":"{commit}","definition":{{"provider":"openai-chat","model":"legacy-model","endpoint":null,"credential":{{"environment":"PATH"}},"work_timeout_secs":null,"retained_work_capacity":16,"max_model_rounds":8,"max_tool_calls":0,"workspace_root":null,"tools":[],"tool_limits":{{"max_concurrent_operations":4,"call_timeout_secs":10,"max_output_bytes":65536,"max_directory_entries":200,"max_file_scan_bytes":2097152,"max_search_files":1000,"max_search_depth":32,"max_search_scan_bytes":8388608,"max_search_matches":200}}}},"context":{{"blocks":[]}}}}"#
    );
    std::fs::write(&record_path, raw_json.as_bytes()).unwrap();
    let original_bytes = std::fs::read(&record_path).unwrap();

    let host = SessionHost::new(Some(storage));
    let session = host.open(id.clone().into()).await.unwrap();

    assert!(session.definition().tools.is_empty());
    assert_eq!(session.definition().write_limits, None);
    assert_eq!(session.definition().permission_mode, None);
    assert_eq!(
        session.definition().initial_permission_mode(),
        PermissionMode::Ask
    );
    assert_eq!(session.permission_mode(), Some((PermissionMode::Ask, 0)));

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    let after_bytes = std::fs::read(&record_path).unwrap();
    assert_eq!(after_bytes, original_bytes);
}

#[tokio::test]
async fn field_writing_rules_and_omissions() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("store");

    // Part A: New session without new features
    let mut def = base_definition("https://example.test", dir.path());
    def.permission_mode = None;
    def.write_limits = None;

    let host = SessionHost::new(Some(storage.clone()));
    let prepared = PreparedCreate::persistent(def.clone(), Context::default());
    let id_a = prepared.conversation_id().clone();
    let session_a = host.create(prepared).await.unwrap();
    host.close(&session_a.target(), ClosePolicy::Save)
        .await
        .unwrap();

    let record_a_path = storage
        .join("sessions")
        .join(format!("{}.json", id_a.as_str()));
    let json_a: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record_a_path).unwrap()).unwrap();
    assert!(json_a.get("permission_mode").is_none());
    assert!(json_a["definition"].get("permission_mode").is_none());
    assert!(json_a["definition"].get("write_limits").is_none());

    // Part B: Current mode equals initial mode
    let mut def_b = base_definition("https://example.test", dir.path());
    def_b.permission_mode = Some(PermissionMode::AcceptEdits);

    let prepared_b = PreparedCreate::persistent(def_b, Context::default());
    let id_b = prepared_b.conversation_id().clone();
    let session_b = host.create(prepared_b).await.unwrap();
    session_b.wait_saved().await.unwrap();

    let record_b_path = storage
        .join("sessions")
        .join(format!("{}.json", id_b.as_str()));
    let json_b: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record_b_path).unwrap()).unwrap();
    assert_eq!(json_b["definition"]["permission_mode"], "accept-edits");
    assert!(json_b.get("permission_mode").is_none());

    // Switch to ReadOnly: current != initial
    session_b
        .set_permission_mode(PermissionMode::ReadOnly)
        .unwrap();
    session_b.wait_saved().await.unwrap();

    let json_b2: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record_b_path).unwrap()).unwrap();
    assert_eq!(json_b2["permission_mode"], "read-only");

    // Switch back to AcceptEdits: current == initial
    session_b
        .set_permission_mode(PermissionMode::AcceptEdits)
        .unwrap();
    session_b.wait_saved().await.unwrap();

    let json_b3: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record_b_path).unwrap()).unwrap();
    assert!(json_b3.get("permission_mode").is_none());

    host.close(&session_b.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test]
async fn explicit_ask_normalized_to_none_on_session_creation() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("store");

    let mut def = base_definition("https://example.test", dir.path());
    def.permission_mode = Some(PermissionMode::Ask);

    let host = SessionHost::new(Some(storage.clone()));
    let prepared = PreparedCreate::persistent(def, Context::default());
    let id = prepared.conversation_id().clone();
    let session = host.create(prepared).await.unwrap();

    assert_eq!(session.definition().permission_mode, None);
    assert_eq!(
        session.definition().initial_permission_mode(),
        PermissionMode::Ask
    );

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    let record_path = storage
        .join("sessions")
        .join(format!("{}.json", id.as_str()));
    let record_str = std::fs::read_to_string(&record_path).unwrap();
    assert!(
        !record_str.contains("permission_mode"),
        "saved record must omit permission_mode key"
    );

    let reopened = host.open(id).await.unwrap();
    assert_eq!(reopened.definition().permission_mode, None);
    assert_eq!(
        reopened.definition().initial_permission_mode(),
        PermissionMode::Ask
    );
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test]
async fn wm17_reopen_permission_mode_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("store");
    let host = SessionHost::new(Some(storage.clone()));

    // Case 1: Switched to accept-edits -> reopens as accept-edits
    let def1 = base_definition("https://example.test", dir.path());
    let p1 = PreparedCreate::persistent(def1, Context::default());
    let id1 = p1.conversation_id().clone();
    let s1 = host.create(p1).await.unwrap();
    s1.set_permission_mode(PermissionMode::AcceptEdits).unwrap();
    s1.wait_saved().await.unwrap();
    host.close(&s1.target(), ClosePolicy::Save).await.unwrap();

    let r1 = host.open(id1).await.unwrap();
    assert_eq!(r1.permission_mode(), Some((PermissionMode::AcceptEdits, 0)));
    host.close(&r1.target(), ClosePolicy::Save).await.unwrap();

    // Case 2: Initial is ask, switched to full-access -> reopens as ask
    let def2 = base_definition("https://example.test", dir.path());
    let p2 = PreparedCreate::persistent(def2, Context::default());
    let id2 = p2.conversation_id().clone();
    let s2 = host.create(p2).await.unwrap();
    s2.set_permission_mode(PermissionMode::FullAccess).unwrap();
    s2.wait_saved().await.unwrap();
    host.close(&s2.target(), ClosePolicy::Save).await.unwrap();

    let r2 = host.open(id2).await.unwrap();
    assert_eq!(r2.permission_mode(), Some((PermissionMode::Ask, 0)));
    host.close(&r2.target(), ClosePolicy::Save).await.unwrap();

    // Case 3: Initial is full-access -> reopens as full-access
    let mut def3 = base_definition("https://example.test", dir.path());
    def3.permission_mode = Some(PermissionMode::FullAccess);
    let p3 = PreparedCreate::persistent(def3, Context::default());
    let id3 = p3.conversation_id().clone();
    let s3 = host.create(p3).await.unwrap();
    assert_eq!(s3.permission_mode(), Some((PermissionMode::FullAccess, 0)));
    host.close(&s3.target(), ClosePolicy::Save).await.unwrap();

    let r3 = host.open(id3).await.unwrap();
    assert_eq!(r3.permission_mode(), Some((PermissionMode::FullAccess, 0)));
    host.close(&r3.target(), ClosePolicy::Save).await.unwrap();
}

// Saving scenarios

#[tokio::test]
async fn saving_scenario_1_idle_switch_triggers_save_context_unchanged_revision_incremented() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("store");
    let host = SessionHost::new(Some(storage));

    let def = base_definition("https://example.test", dir.path());
    let initial_context = Context::default();
    let p = PreparedCreate::persistent(def, initial_context.clone());
    let s = host.create(p).await.unwrap();
    let initial_status = s.wait_saved().await.unwrap();
    let initial_rev = match initial_status {
        SaveStatus::Saved { revision, .. } => revision,
        _ => panic!("must be saved"),
    };

    let before_blocks = s.read_materials().unwrap().context.blocks().to_vec();

    // Idle switch: triggers save, revision + 1
    s.set_permission_mode(PermissionMode::AcceptEdits).unwrap();
    let after_status = s.wait_saved().await.unwrap();
    let after_rev = match after_status {
        SaveStatus::Saved { revision, .. } => revision,
        _ => panic!("must be saved"),
    };
    assert_eq!(after_rev, initial_rev + 1);

    // Context unchanged
    let after_blocks = s.read_materials().unwrap().context.blocks().to_vec();
    assert_eq!(before_blocks, after_blocks);

    host.close(&s.target(), ClosePolicy::Save).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saving_scenario_2_switch_during_active_work_saved_after_work() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("store");

    let replies = vec![
        call_write("work_file.txt", "initial content"),
        Reply::Answer,
    ];
    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(Some(storage.clone()));
    let def = base_definition(&fixture.endpoint, dir.path());
    let p = PreparedCreate::persistent(def, Context::default());
    let id = p.conversation_id().clone();
    let s = host.create(p).await.unwrap();

    let mut rx = s.connect_approvals();
    let s_clone = s.clone();

    let work = submit(&s, "active work");

    // Receive approval request while work is active
    let req = rx.recv().await.expect("request received");
    // Switch multiple times during active work
    s_clone
        .set_permission_mode(PermissionMode::ReadOnly)
        .unwrap();
    s_clone
        .set_permission_mode(PermissionMode::AcceptEdits)
        .unwrap();

    // Now reply to allow
    req.reply.send(ApprovalDecision::Allow).unwrap();

    let obs = finish(&s, &work).await;
    assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Succeeded));

    // The single save after work contains the latest mode
    let record_path = storage
        .join("sessions")
        .join(format!("{}.json", id.as_str()));
    let json_val: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(json_val["permission_mode"], "accept-edits");

    host.close(&s.target(), ClosePolicy::Save).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn saving_scenario_3_switch_with_pending_failed_commit_retries_original_then_saves_new_mode()
{
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("store");
    let host = SessionHost::new(Some(storage.clone()));

    let def = base_definition("https://example.test", dir.path());
    let p = PreparedCreate::persistent(def, Context::default());
    let id = p.conversation_id().clone();
    let s = host.create(p).await.unwrap();
    s.wait_saved().await.unwrap();

    // Inject save failure on the writer
    s.set_writer_fault_hook(Some(Arc::new(|stage| {
        if stage == CommitStage::Write {
            FaultAction::Fail(std::io::Error::other("injected storage error"))
        } else {
            FaultAction::Continue
        }
    })));

    // Trigger save via edit
    let _ = s.edit(
        s.read_materials().unwrap().version,
        vec![crate::application::text_input("mutated")],
    );

    // Wait until save is blocked
    crate::application::tests::support::until(|| {
        matches!(s.status().save, SaveStatus::SaveBlocked { .. })
    })
    .await;

    // Pending commit is fixed with original Ask mode (permission_mode is None)
    let pending_commit_id = s.pending_commit_id().unwrap();
    assert_eq!(s.pending_commit_permission_mode(), Some(None));

    // Switch mode while save is blocked
    s.set_permission_mode(PermissionMode::AcceptEdits).unwrap();

    // Pending commit is UNTOUCHED
    assert_eq!(s.pending_commit_id().unwrap(), pending_commit_id);
    assert_eq!(s.pending_commit_permission_mode(), Some(None));

    // Clear fault hook and retry
    s.set_writer_fault_hook(None);

    let retry_status = s.retry_save().await.unwrap();
    assert!(matches!(retry_status, SaveStatus::Saved { .. }));

    // Verify disk record now contains AcceptEdits
    let record_path = storage
        .join("sessions")
        .join(format!("{}.json", id.as_str()));
    let json_val: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record_path).unwrap()).unwrap();
    assert_eq!(json_val["permission_mode"], "accept-edits");

    host.close(&s.target(), ClosePolicy::Save).await.unwrap();
}

#[tokio::test]
async fn saving_scenario_4_close_save_guarantees_latest_mode_saved() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("store");
    let host = SessionHost::new(Some(storage.clone()));

    let def = base_definition("https://example.test", dir.path());
    let p = PreparedCreate::persistent(def, Context::default());
    let id = p.conversation_id().clone();
    let s = host.create(p).await.unwrap();
    s.wait_saved().await.unwrap();

    // Switch mode and close with Save immediately
    s.set_permission_mode(PermissionMode::AcceptEdits).unwrap();
    let outcome = host.close(&s.target(), ClosePolicy::Save).await.unwrap();
    assert!(matches!(outcome.save, SaveStatus::Saved { .. }));

    // Reopen confirms latest mode is saved
    let reopened = host.open(id).await.unwrap();
    assert_eq!(
        reopened.permission_mode(),
        Some((PermissionMode::AcceptEdits, 0))
    );
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test]
async fn saving_scenario_5_transient_session_switch_does_not_save() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("store");
    let host = SessionHost::new(Some(storage.clone()));

    let def = base_definition("https://example.test", dir.path());
    let p = PreparedCreate::transient(def, Context::default());
    let s = host.create(p).await.unwrap();

    s.set_permission_mode(PermissionMode::AcceptEdits).unwrap();
    let status = s.wait_saved().await.unwrap();
    assert!(matches!(status, SaveStatus::NotTracked));

    // No sessions dir or file created
    let sessions_dir = storage.join("sessions");
    assert!(!sessions_dir.exists() || std::fs::read_dir(sessions_dir).unwrap().next().is_none());

    host.close(&s.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm03_reopen_requires_read_before_edit() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("store");
    let ws_dir = dir.path().join("workspace");
    std::fs::create_dir_all(&ws_dir).unwrap();
    let file_path = ws_dir.join("test.txt");
    std::fs::write(&file_path, "alpha beta\n").unwrap();

    let replies = vec![
        // Round 1: read_file
        call_read("test.txt"),
        Reply::Answer,
        // Round 2 (in reopened session): edit_file without reading
        call_edit("test.txt", "alpha", "gamma"),
        Reply::Answer,
        // Round 3 (in reopened session): re-read file
        call_read("test.txt"),
        Reply::Answer,
        // Round 4 (in reopened session): edit_file after re-reading
        call_edit("test.txt", "alpha", "gamma"),
        Reply::Answer,
    ];

    let fixture = Fixture::new("openai-chat", replies, None);
    let mut def = base_definition(&fixture.endpoint, &ws_dir);
    def.permission_mode = Some(PermissionMode::AcceptEdits);

    let host = SessionHost::new(Some(storage.clone()));
    let prepared = PreparedCreate::persistent(def.clone(), Context::default());
    let id = prepared.conversation_id().clone();
    let session = host.create(prepared).await.unwrap();

    // Round 1: read_file succeeds
    let work1 = submit(&session, "round 1 read");
    let obs1 = finish(&session, &work1).await;
    assert_eq!(
        obs1.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );

    // Close session
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // Reopen session
    let reopened = host.open(id.clone()).await.unwrap();

    // Round 2: edit_file without prior read in reopened session fails precheck
    let work2 = submit(&reopened, "round 2 edit");
    let obs2 = finish(&reopened, &work2).await;
    assert_eq!(obs2.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    let err_msg = tool_result_error(&reopened, obs2.tools.calls[0].call_block_id);
    assert!(err_msg.contains("must be read with read_file before editing"));
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "alpha beta\n");

    // Round 3: read_file to re-observe file
    let work3 = submit(&reopened, "round 3 re-read");
    let obs3 = finish(&reopened, &work3).await;
    assert_eq!(
        obs3.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );

    // Round 4: edit_file now succeeds after re-reading
    let work4 = submit(&reopened, "round 4 edit after read");
    let obs4 = finish(&reopened, &work4).await;
    assert_eq!(
        obs4.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "gamma beta\n");

    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm01_wm02_precheck_negative_cases_and_directory_creation_through_chain() {
    let dir = tempfile::tempdir().unwrap();
    let ws_dir = dir.path().join("workspace");
    std::fs::create_dir_all(&ws_dir).unwrap();

    let target_file = ws_dir.join("target.txt");
    std::fs::write(&target_file, "alpha\nbeta\nalpha\none two three four\n").unwrap();
    let original_bytes = std::fs::read(&target_file).unwrap();

    let link_file = ws_dir.join("link.txt");
    std::os::unix::fs::symlink(&target_file, &link_file).unwrap();

    let fifo_file = ws_dir.join("fifo.txt");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo_file)
        .status()
        .unwrap();
    assert!(status.success());

    let replies = vec![
        // Round 1: read_file target.txt to observe it for edit_file tests
        call_read("target.txt"),
        Reply::Answer,
        // Negative test 1: write_file absolute path
        call_write("/tmp/abs_path.txt", "evil"),
        Reply::Answer,
        // Negative test 2: write_file parent .. path
        call_write("../outside.txt", "evil"),
        Reply::Answer,
        // Negative test 3: write_file target is symlink
        call_write("link.txt", "evil"),
        Reply::Answer,
        // Negative test 4: write_file target is FIFO
        call_write("fifo.txt", "evil"),
        Reply::Answer,
        // Negative test 5: edit_file not found
        call_edit("target.txt", "nonexistent", "new"),
        Reply::Answer,
        // Negative test 6: edit_file matches multiple places
        call_edit("target.txt", "alpha", "new"),
        Reply::Answer,
        // Negative test 7: edit_file overlapping segments
        Reply::Call(
            "edit_file".into(),
            json!({
                "path": "target.txt",
                "edits": [
                    {"old_text": "one two", "new_text": "1 2"},
                    {"old_text": "two three", "new_text": "2 3"}
                ]
            }),
        ),
        Reply::Answer,
        // Negative test 8: edit_file empty old_text
        Reply::Call(
            "edit_file".into(),
            json!({
                "path": "target.txt",
                "edits": [{"old_text": "", "new_text": "something"}]
            }),
        ),
        Reply::Answer,
        // Directory creation 1: write_file a/b/c.txt, denied
        call_write("a/b/c.txt", "nested content\n"),
        Reply::Answer,
        // Directory creation 2: write_file a/b/c.txt, allowed
        call_write("a/b/c.txt", "nested content\n"),
        Reply::Answer,
    ];

    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let mut def = base_definition(&fixture.endpoint, &ws_dir);
    def.retained_work_capacity = 32;
    let session = host
        .create(PreparedCreate::transient(def, Context::default()))
        .await
        .unwrap();

    let mut rx = session.connect_approvals();

    // Round 1: observe target.txt
    let w0 = submit(&session, "observe");
    let o0 = finish(&session, &w0).await;
    assert_eq!(o0.tools.calls[0].status, Some(ToolResultStatus::Succeeded));

    // Test 1: absolute path
    let w1 = submit(&session, "abs write");
    let o1 = finish(&session, &w1).await;
    assert_eq!(o1.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    assert!(rx.try_recv().is_err(), "no approval request for abs path");

    // Test 2: parent ..
    let w2 = submit(&session, "parent write");
    let o2 = finish(&session, &w2).await;
    assert_eq!(o2.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    assert!(rx.try_recv().is_err(), "no approval request for parent ..");

    // Test 3: symlink
    let w3 = submit(&session, "symlink write");
    let o3 = finish(&session, &w3).await;
    assert_eq!(o3.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    assert!(rx.try_recv().is_err(), "no approval request for symlink");

    // Test 4: fifo
    let w4 = submit(&session, "fifo write");
    let o4 = finish(&session, &w4).await;
    assert_eq!(o4.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    assert!(rx.try_recv().is_err(), "no approval request for fifo");

    // Test 5: edit not found
    let w5 = submit(&session, "edit not found");
    let o5 = finish(&session, &w5).await;
    assert_eq!(o5.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    let err5 = tool_result_error(&session, o5.tools.calls[0].call_block_id);
    assert!(err5.contains("edits[0]"), "must indicate segment 0: {err5}");
    assert!(
        rx.try_recv().is_err(),
        "no approval request for edit not found"
    );

    // Test 6: edit multiple matches
    let w6 = submit(&session, "edit multi");
    let o6 = finish(&session, &w6).await;
    assert_eq!(o6.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    let err6 = tool_result_error(&session, o6.tools.calls[0].call_block_id);
    assert!(err6.contains("edits[0]"), "must indicate segment 0: {err6}");
    assert!(
        rx.try_recv().is_err(),
        "no approval request for multi match"
    );

    // Test 7: edit overlapping
    let w7 = submit(&session, "edit overlap");
    let o7 = finish(&session, &w7).await;
    assert_eq!(o7.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    let err7 = tool_result_error(&session, o7.tools.calls[0].call_block_id);
    assert!(err7.contains("edits[1]"), "must indicate segment 1: {err7}");
    assert!(rx.try_recv().is_err(), "no approval request for overlap");

    // Test 8: edit empty old_text
    let w8 = submit(&session, "edit empty");
    let o8 = finish(&session, &w8).await;
    assert_eq!(o8.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    let err8 = tool_result_error(&session, o8.tools.calls[0].call_block_id);
    assert!(err8.contains("edits[0]"), "must indicate segment 0: {err8}");
    assert!(
        rx.try_recv().is_err(),
        "no approval request for empty old_text"
    );

    // Verify target file bytes remain completely unchanged
    assert_eq!(std::fs::read(&target_file).unwrap(), original_bytes);

    // Directory creation Part A: Deny
    assert!(!ws_dir.join("a").exists());
    let w9 = submit(&session, "create a/b/c.txt deny");
    let req9 = rx
        .recv()
        .await
        .expect("must request approval for directory write");
    req9.reply
        .send(ApprovalDecision::Deny {
            reason: Some("declined".into()),
        })
        .unwrap();
    let o9 = finish(&session, &w9).await;
    assert_eq!(o9.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    assert!(
        !ws_dir.join("a").exists(),
        "directory a must not be created when denied"
    );

    // Directory creation Part B: Allow
    let w10 = submit(&session, "create a/b/c.txt allow");
    let req10 = rx
        .recv()
        .await
        .expect("must request approval for directory write again");
    req10.reply.send(ApprovalDecision::Allow).unwrap();
    let o10 = finish(&session, &w10).await;
    assert_eq!(o10.tools.calls[0].status, Some(ToolResultStatus::Succeeded));
    assert!(
        ws_dir.join("a/b/c.txt").exists(),
        "file a/b/c.txt must exist"
    );
    assert_eq!(
        std::fs::read_to_string(ws_dir.join("a/b/c.txt")).unwrap(),
        "nested content\n"
    );
}

#[test]
fn wm17_decide_20_cells_exhaustive() {
    use crate::permission::{PermissionMode, PermissionVerdict, ToolPermissionClass, decide};

    // ReadOnly
    assert_eq!(
        decide(PermissionMode::ReadOnly, ToolPermissionClass::KnownReadOnly),
        PermissionVerdict::Execute
    );
    assert_eq!(
        decide(PermissionMode::ReadOnly, ToolPermissionClass::BuiltinWrite),
        PermissionVerdict::Reject
    );
    assert_eq!(
        decide(
            PermissionMode::ReadOnly,
            ToolPermissionClass::BuiltinCommand
        ),
        PermissionVerdict::Reject
    );
    assert_eq!(
        decide(PermissionMode::ReadOnly, ToolPermissionClass::McpPrompt),
        PermissionVerdict::Reject
    );
    assert_eq!(
        decide(PermissionMode::ReadOnly, ToolPermissionClass::McpApprove),
        PermissionVerdict::Reject
    );

    // Ask
    assert_eq!(
        decide(PermissionMode::Ask, ToolPermissionClass::KnownReadOnly),
        PermissionVerdict::Execute
    );
    assert_eq!(
        decide(PermissionMode::Ask, ToolPermissionClass::BuiltinWrite),
        PermissionVerdict::Ask
    );
    assert_eq!(
        decide(PermissionMode::Ask, ToolPermissionClass::BuiltinCommand),
        PermissionVerdict::Ask
    );
    assert_eq!(
        decide(PermissionMode::Ask, ToolPermissionClass::McpPrompt),
        PermissionVerdict::Ask
    );
    assert_eq!(
        decide(PermissionMode::Ask, ToolPermissionClass::McpApprove),
        PermissionVerdict::Execute
    );

    // AcceptEdits
    assert_eq!(
        decide(
            PermissionMode::AcceptEdits,
            ToolPermissionClass::KnownReadOnly
        ),
        PermissionVerdict::Execute
    );
    assert_eq!(
        decide(
            PermissionMode::AcceptEdits,
            ToolPermissionClass::BuiltinWrite
        ),
        PermissionVerdict::Execute
    );
    assert_eq!(
        decide(
            PermissionMode::AcceptEdits,
            ToolPermissionClass::BuiltinCommand
        ),
        PermissionVerdict::Ask
    );
    assert_eq!(
        decide(PermissionMode::AcceptEdits, ToolPermissionClass::McpPrompt),
        PermissionVerdict::Ask
    );
    assert_eq!(
        decide(PermissionMode::AcceptEdits, ToolPermissionClass::McpApprove),
        PermissionVerdict::Execute
    );

    // FullAccess
    assert_eq!(
        decide(
            PermissionMode::FullAccess,
            ToolPermissionClass::KnownReadOnly
        ),
        PermissionVerdict::Execute
    );
    assert_eq!(
        decide(
            PermissionMode::FullAccess,
            ToolPermissionClass::BuiltinWrite
        ),
        PermissionVerdict::Execute
    );
    assert_eq!(
        decide(
            PermissionMode::FullAccess,
            ToolPermissionClass::BuiltinCommand
        ),
        PermissionVerdict::Execute
    );
    assert_eq!(
        decide(PermissionMode::FullAccess, ToolPermissionClass::McpPrompt),
        PermissionVerdict::Execute
    );
    assert_eq!(
        decide(PermissionMode::FullAccess, ToolPermissionClass::McpApprove),
        PermissionVerdict::Execute
    );
}

#[tokio::test]
async fn wm17_mode_refusal_before_precheck() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![call_edit("never_read.txt", "a", "b"), Reply::Answer];
    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let mut def = base_definition(&fixture.endpoint, root.path());
    def.permission_mode = Some(PermissionMode::ReadOnly);
    let session = host
        .create(PreparedCreate::transient(def, Context::default()))
        .await
        .unwrap();

    let work = submit(&session, "try edit without read in read-only");
    let obs = finish(&session, &work).await;

    assert_eq!(obs.tools.calls.len(), 1);
    assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    assert_eq!(
        obs.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::ModeRefused
    );
    let err = tool_result_error(&session, obs.tools.calls[0].call_block_id);
    assert_eq!(
        err,
        "the current permission mode 'read-only' does not allow this write"
    );
    assert!(!err.contains("read before edit"));
}

#[tokio::test]
async fn wm17_no_confirmation_route_before_precheck() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![call_edit("never_read.txt", "a", "b"), Reply::Answer];
    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let mut def = base_definition(&fixture.endpoint, root.path());
    def.permission_mode = Some(PermissionMode::Ask);
    let session = host
        .create(PreparedCreate::transient(def, Context::default()))
        .await
        .unwrap();
    // Do NOT connect approvals!

    let work = submit(&session, "try edit in ask without route");
    let obs = finish(&session, &work).await;

    assert_eq!(obs.tools.calls.len(), 1);
    assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    assert_eq!(
        obs.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::NoConfirmationRoute
    );
    let err = tool_result_error(&session, obs.tools.calls[0].call_block_id);
    assert_eq!(err, "no confirmation route available");
    assert!(!err.contains("read before edit"));
}

#[tokio::test]
async fn wm17_mode_switch_between_gate_and_confirmation() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![call_write("between.txt", "test"), Reply::Answer];
    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let mut def = base_definition(&fixture.endpoint, root.path());
    def.permission_mode = Some(PermissionMode::Ask);
    let session = host
        .create(PreparedCreate::transient(def, Context::default()))
        .await
        .unwrap();

    struct SwitchModeProcessor {
        session: ManagedSession,
    }
    #[async_trait::async_trait]
    impl causa::kernel::ToolBatchProcessor for SwitchModeProcessor {
        async fn process(
            &self,
            _batch: &mut causa::kernel::ToolBatch,
            _context: &causa::kernel::ProcessorContext<'_>,
        ) -> Result<(), causa::kernel::ProcessorError> {
            self.session
                .set_permission_mode(PermissionMode::ReadOnly)
                .unwrap();
            Ok(())
        }
    }

    let _rx = session.connect_approvals();

    session.add_test_intermediate_processor(Arc::new(SwitchModeProcessor {
        session: session.clone(),
    }));

    let work = submit(&session, "write that gets mode switched mid-chain");
    let obs = finish(&session, &work).await;

    assert_eq!(obs.tools.calls.len(), 1);
    assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    let decision = obs.tools.calls[0].permission_decision.as_ref().unwrap();
    assert_eq!(decision.kind, PermissionDecisionKind::ModeRefused);
    assert_eq!(decision.mode, PermissionMode::ReadOnly);
    assert_eq!(decision.revision, 1);
    let err = tool_result_error(&session, obs.tools.calls[0].call_block_id);
    assert_eq!(
        err,
        "the current permission mode 'read-only' does not allow this write"
    );
    assert!(!root.path().join("between.txt").exists());
}

fn command_definition(endpoint: &str, root: &Path) -> SessionDefinition {
    let mut definition = base_definition(endpoint, root);
    definition.tools.push(WorkspaceTool::RunCommand);
    definition.command = Some(crate::definition::CommandSettings::default());
    definition
}

#[cfg(target_os = "macos")]
fn call_command(arguments: serde_json::Value) -> ScriptedToolCall {
    ScriptedToolCall {
        name: "run_command".into(),
        arguments,
    }
}

#[cfg(target_os = "macos")]
fn command_result(
    session: &ManagedSession,
    observation: &WorkObservation,
    index: usize,
) -> serde_json::Value {
    let payload = session
        .read_materials()
        .unwrap()
        .context
        .blocks()
        .iter()
        .find_map(|block| match block.content() {
            BlockContent::ToolResult(payload)
                if payload.call_block_id == observation.tools.calls[index].call_block_id =>
            {
                Some(payload.clone())
            }
            _ => None,
        })
        .expect("command result");
    payload.output.content
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_full_access_stays_sandboxed() {
    let root = tempfile::tempdir().unwrap();
    let outside = root
        .path()
        .parent()
        .unwrap()
        .join(format!("tarz-command-outside-{}", uuid::Uuid::now_v7()));
    let script = format!(
        "printf inside > inside.txt; printf outside > '{}'",
        outside.display()
    );
    let replies = vec![
        Reply::Calls(vec![ScriptedToolCall {
            name: "run_command".into(),
            arguments: json!({ "command": script }),
        }]),
        Reply::Answer,
    ];
    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let session = host
        .create(PreparedCreate::transient(
            command_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .unwrap();
    session
        .set_permission_mode(PermissionMode::FullAccess)
        .unwrap();
    let work = submit(&session, "sandboxed command");
    let observation = finish(&session, &work).await;
    assert_eq!(observation.tools.calls.len(), 1);
    assert_eq!(
        observation.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(
        observation.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::DirectExecution
    );
    let content = command_result(&session, &observation, 0);
    assert_eq!(content["exit_code"], 1);
    assert!(root.path().join("inside.txt").exists());
    assert!(!outside.exists());
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_ask_allows_and_denies_with_the_command_subject() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![
        Reply::Calls(vec![call_command(
            json!({"command": "printf allow > allowed.txt"}),
        )]),
        Reply::Answer,
        Reply::Calls(vec![call_command(
            json!({"command": "printf deny > denied.txt"}),
        )]),
        Reply::Answer,
    ];
    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let session = host
        .create(PreparedCreate::transient(
            command_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .unwrap();
    let mut rx = session.connect_approvals();
    let approvals = tokio::spawn(async move {
        let allow = rx.recv().await.expect("allow request");
        let ApprovalSubject::Command {
            command,
            workdir,
            timeout_secs,
            network,
        } = &allow.subject
        else {
            panic!("expected Command subject")
        };
        assert_eq!(command, "printf allow > allowed.txt");
        assert_eq!(workdir, ".");
        assert_eq!(*timeout_secs, 120);
        assert_eq!(*network, crate::definition::CommandNetwork::None);
        allow.reply.send(ApprovalDecision::Allow).unwrap();

        let deny = rx.recv().await.expect("deny request");
        let ApprovalSubject::Command { command, .. } = &deny.subject else {
            panic!("expected Command subject")
        };
        assert_eq!(command, "printf deny > denied.txt");
        deny.reply
            .send(ApprovalDecision::Deny {
                reason: Some("user policy".into()),
            })
            .unwrap();
    });

    let allow_work = submit(&session, "allow command");
    let allow_obs = finish(&session, &allow_work).await;
    let deny_work = submit(&session, "deny command");
    let deny_obs = finish(&session, &deny_work).await;
    approvals.await.unwrap();

    assert_eq!(
        allow_obs.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(
        allow_obs.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::AskedAndAllowed
    );
    assert!(root.path().join("allowed.txt").exists());
    assert_eq!(
        deny_obs.tools.calls[0].status,
        Some(ToolResultStatus::Rejected)
    );
    assert_eq!(
        deny_obs.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::AskedAndDenied
    );
    assert!(!root.path().join("denied.txt").exists());
    assert!(
        command_result(&session, &deny_obs, 0)
            .to_string()
            .contains("user declined this command")
    );
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_read_only_refuses_before_asking() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![
        Reply::Calls(vec![call_command(json!({"command": "printf no > no.txt"}))]),
        Reply::Answer,
    ];
    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let session = host
        .create(PreparedCreate::transient(
            command_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .unwrap();
    session
        .set_permission_mode(PermissionMode::ReadOnly)
        .unwrap();
    let work = submit(&session, "read-only command");
    let observation = finish(&session, &work).await;
    assert_eq!(
        observation.tools.calls[0].status,
        Some(ToolResultStatus::Rejected)
    );
    assert_eq!(
        observation.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::ModeRefused
    );
    assert!(!root.path().join("no.txt").exists());
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_invalid_workdir_and_missing_route_are_rejected_without_prompt() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![
        Reply::Calls(vec![call_command(json!({
            "command": "printf no > no.txt",
            "workdir": "../"
        }))]),
        Reply::Answer,
        Reply::Calls(vec![call_command(json!({"command": "printf no > no.txt"}))]),
        Reply::Answer,
    ];
    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let session = host
        .create(PreparedCreate::transient(
            command_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .unwrap();
    let mut rx = session.connect_approvals();
    let invalid_work = submit(&session, "invalid workdir");
    let invalid_obs = finish(&session, &invalid_work).await;
    assert_eq!(
        invalid_obs.tools.calls[0].status,
        Some(ToolResultStatus::Rejected)
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(250), rx.recv())
            .await
            .is_err(),
        "invalid workdir must not prompt"
    );
    drop(rx);

    // A valid command in Ask mode still needs a connected route.
    let no_route_work = submit(&session, "no route");
    let no_route_obs = finish(&session, &no_route_work).await;
    assert_eq!(
        no_route_obs.tools.calls[0].status,
        Some(ToolResultStatus::Rejected)
    );
    assert_eq!(
        no_route_obs.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::NoConfirmationRoute
    );
    assert!(!root.path().join("no.txt").exists());
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_timeout_and_cancel_are_reported_and_stop_work() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![
        Reply::Calls(vec![call_command(json!({
            "command": "sleep 5; printf done > timeout.txt",
            "timeout_secs": 1
        }))]),
        Reply::Answer,
        Reply::Calls(vec![call_command(json!({
            "command": "printf started > started.txt; sleep 30; printf done > cancelled.txt"
        }))]),
        Reply::Answer,
    ];
    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let session = host
        .create(PreparedCreate::transient(
            command_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .unwrap();
    session
        .set_permission_mode(PermissionMode::FullAccess)
        .unwrap();

    let timeout_work = submit(&session, "timeout command");
    let timeout_obs = finish(&session, &timeout_work).await;
    assert_eq!(
        timeout_obs.tools.calls[0].status,
        Some(ToolResultStatus::TimedOut)
    );
    assert_eq!(
        command_result(&session, &timeout_obs, 0)["may_have_partially_executed"],
        true
    );
    assert!(!root.path().join("timeout.txt").exists());

    let cancel_work = submit(&session, "cancel command");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !root.path().join("started.txt").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "command did not start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    session.cancel(&cancel_work).unwrap();
    let cancel_obs = session
        .wait(&cancel_work, Duration::from_secs(5))
        .await
        .unwrap();
    // Causa's batch stops when the turn is cancelled and settles every
    // started call as UnknownOutcome; the command's Drop guard still kills
    // the process tree. This is the integration boundary, not a Cancelled
    // result produced by the tool.
    assert_eq!(
        cancel_obs.tools.calls[0].status,
        Some(ToolResultStatus::UnknownOutcome)
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!root.path().join("cancelled.txt").exists());

    let inspection = session.inspect_repair(Some(&cancel_work)).unwrap();
    let original = inspection.original.expect("retained command batch");
    assert!(original.write_warnings_count >= 1);
    assert_eq!(original.command_warnings_count, 1);
    assert!(original.results.iter().any(|result| {
        result
            .write_warning
            .as_ref()
            .is_some_and(|warning| warning.kind == crate::application::RepairWarningKind::Command)
    }));
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_command_rejection_is_not_warned_by_repair() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![
        Reply::Calls(vec![
            call_command(json!({
                "command": "printf started > started.txt; sleep 30; printf done > first.txt"
            })),
            call_command(json!({
                "command": "printf queued > queued.txt",
                "timeout_secs": 1
            })),
        ]),
        Reply::Answer,
    ];
    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let mut definition = command_definition(&fixture.endpoint, root.path());
    // The running command's own timeout is bounded below the work deadline.
    // Keep that bound well above the queued command's one second so the first
    // command is still running when the second rejects and work is cancelled.
    definition.work_timeout_secs = Some(30);
    let session = host
        .create(PreparedCreate::transient(definition, Context::default()))
        .await
        .unwrap();
    session
        .set_permission_mode(PermissionMode::FullAccess)
        .unwrap();

    let work = submit(&session, "queued command in a batch");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !root.path().join("started.txt").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the first command did not start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // While the first command holds the session's command permit, the second
    // must return Rejected after its own one-second deadline, not wait forever.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let observed = session.observe(&work).unwrap();
        if observed
            .tools
            .calls
            .get(1)
            .and_then(|call| call.status.clone())
            == Some(ToolResultStatus::Rejected)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the queued command did not reject while the first command was running"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    session.cancel(&work).unwrap();
    let observation = finish(&session, &work).await;

    assert_eq!(observation.tools.calls.len(), 2);
    assert_eq!(
        observation.tools.calls[0].status,
        Some(ToolResultStatus::UnknownOutcome)
    );
    assert_eq!(
        observation.tools.calls[1].status,
        Some(ToolResultStatus::Rejected)
    );
    assert!(!root.path().join("queued.txt").exists());
    assert!(!root.path().join("first.txt").exists());

    let inspection = session.inspect_repair(Some(&work)).unwrap();
    let original = inspection.original.expect("retained command batch");
    assert_eq!(original.completed_count, 2);
    let rejected = original
        .results
        .iter()
        .find(|result| result.status == ToolResultStatus::Rejected)
        .expect("queued command result");
    let unknown = original
        .results
        .iter()
        .find(|result| result.status == ToolResultStatus::UnknownOutcome)
        .expect("interrupted command result");
    assert_eq!(
        rejected.call_block_id,
        observation.tools.calls[1].call_block_id
    );
    assert_eq!(
        unknown.call_block_id,
        observation.tools.calls[0].call_block_id
    );
    assert!(
        rejected.write_warning.is_none(),
        "a command that never started must not warn about execution"
    );
    let warning = unknown.write_warning.as_ref().expect("command warning");
    assert_eq!(warning.kind, crate::application::RepairWarningKind::Command);
    assert_eq!(original.command_warnings_count, 1);
    assert_eq!(original.write_warnings_count, 1);

    let plan = session
        .plan_repair(crate::application::RepairMode::AdoptOriginal(work.clone()))
        .unwrap();
    let summary = plan.summary();
    assert_eq!(summary.command_warnings_count, 1);
    assert_eq!(summary.write_warnings_count, 1);
    assert_eq!(summary.write_warnings.len(), 1);
    assert_eq!(
        summary.write_warnings[0].call_block_id,
        unknown.call_block_id
    );
    assert_eq!(
        summary.write_warnings[0].kind,
        crate::application::RepairWarningKind::Command
    );
    assert!(
        summary.write_warnings[0]
            .advice()
            .contains("may have executed"),
        "only the interrupted command may be reported as possibly executed"
    );
    assert!(
        !summary
            .write_warnings
            .iter()
            .any(|warning| warning.call_block_id == rejected.call_block_id),
        "the rejected queued command must not appear in the repair warnings"
    );

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_tool_creation_fails_on_unsupported_platform() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![Reply::Answer];
    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let error = host
        .create(PreparedCreate::transient(
            command_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .expect_err("run_command must fail closed on unsupported platforms");
    assert!(error.to_string().contains("command"));
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn command_settings_survive_save_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("store");
    let host = SessionHost::new(Some(storage));
    let mut definition = command_definition("https://example.test", dir.path());
    definition.command = Some(crate::definition::CommandSettings {
        network: crate::definition::CommandNetwork::Open,
        default_timeout_secs: 30,
        max_timeout_secs: 60,
    });
    let prepared = PreparedCreate::persistent(definition.clone(), Context::default());
    let id = prepared.conversation_id().clone();
    let session = host.create(prepared).await.unwrap();
    session.wait_saved().await.unwrap();
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    let reopened = host.open(id).await.unwrap();
    let saved = reopened.definition();
    assert_eq!(saved.command, definition.command);
    assert_eq!(saved.tools, definition.tools);
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_accept_edits_still_asks() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![
        Reply::Calls(vec![call_command(
            json!({"command": "printf yes > accepted.txt"}),
        )]),
        Reply::Answer,
    ];
    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let session = host
        .create(PreparedCreate::transient(
            command_definition(&fixture.endpoint, root.path()),
            Context::default(),
        ))
        .await
        .unwrap();
    session
        .set_permission_mode(PermissionMode::AcceptEdits)
        .unwrap();
    let mut rx = session.connect_approvals();
    let approval = tokio::spawn(async move {
        let request = rx.recv().await.expect("command approval");
        assert!(matches!(request.subject, ApprovalSubject::Command { .. }));
        request.reply.send(ApprovalDecision::Allow).unwrap();
    });
    let work = submit(&session, "accept-edits command");
    let observation = finish(&session, &work).await;
    approval.await.unwrap();
    assert_eq!(
        observation.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(
        observation.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::AskedAndAllowed
    );
    assert!(root.path().join("accepted.txt").exists());
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

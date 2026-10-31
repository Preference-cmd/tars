//! Integration tests for cancellation and repair of write and edit tools.

use super::support::*;
use super::workspace_http as wire;
use crate::application::{PreparedCreate, RepairMode, RepairWarningKind, SessionHost};
use crate::definition::{CredentialRef, Provider, WorkspaceTool};
use crate::permission::{ApprovalDecision, PermissionMode};
use crate::session::{SubmitRequest, WorkState};
use crate::tools::TestGate;
use causa::kernel::{Context, ToolResultStatus};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wm08_write_cancelled_before_rename_unknown_outcome_old_content_and_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("file.txt"), "old content\n").unwrap();

    let fixture = wire::Fixture::new(
        "openai-chat",
        vec![
            // Round 1: read_file so file is observed
            wire::Reply::Read("file.txt"),
            wire::Reply::Answer,
            // Round 2: write_file to overwrite
            wire::Reply::Call(
                "write_file".into(),
                serde_json::json!({"path": "file.txt", "content": "new content\n"}),
            ),
            // Round 3 (after repair): edit_file based on actual old content
            wire::Reply::Call(
                "edit_file".into(),
                serde_json::json!({
                    "path": "file.txt",
                    "edits": [{"old_text": "old content\n", "new_text": "final content\n"}]
                }),
            ),
            wire::Reply::Answer,
        ],
        None,
    );
    fixture.release();

    let host = SessionHost::new(Some(dir.path().join("store")));
    let mut def = definition();
    def.provider = Provider::OpenAiChat;
    def.endpoint = Some(fixture.endpoint.clone());
    def.credential = CredentialRef::Environment("PATH".into());
    def.workspace_root = Some(std::fs::canonicalize(&workspace).unwrap());
    def.tools = vec![
        WorkspaceTool::ReadFile,
        WorkspaceTool::WriteFile,
        WorkspaceTool::EditFile,
    ];
    def.max_model_rounds = 4;
    def.max_tool_calls = 4;
    def.permission_mode = Some(PermissionMode::FullAccess);

    let session = host
        .create(PreparedCreate::persistent(def, Context::new()))
        .await
        .unwrap();

    let read_work = session
        .submit(SubmitRequest {
            request_key: "r1".into(),
            blocks: vec![text_input("read file")],
        })
        .unwrap()
        .work;
    finished(&session, &read_work).await;
    session.wait_saved().await.unwrap();

    // Round 2: write_file with gate pausing before rename
    let gate = TestGate::new(false);
    session
        .workspace_tools()
        .unwrap()
        .set_write_gate(Some(gate.clone()));

    let original = session
        .submit(SubmitRequest {
            request_key: "r2".into(),
            blocks: vec![text_input("do write")],
        })
        .unwrap()
        .work;

    gate.reached.notified().await;
    session.cancel(&original).unwrap();
    gate.release();

    finished(&session, &original).await;
    session.wait_saved().await.unwrap();

    let observation = session.observe(&original).unwrap();
    let WorkState::Finished(result) = observation.state else {
        panic!("work not finished");
    };
    assert!(matches!(
        result.as_ref(),
        crate::session::WorkResult::Run(causa::runtime::TurnResult::Interrupted { .. })
    ));

    // File content remains old content because rename did not happen.
    assert_eq!(
        std::fs::read_to_string(workspace.join("file.txt")).unwrap(),
        "old content\n"
    );

    for entry in std::fs::read_dir(&workspace).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().to_string();
        assert!(
            !name.starts_with(".tars-write-"),
            "temporary write file was not cleaned up: {name}"
        );
    }

    let inspection = session.inspect_repair(Some(&original)).unwrap();
    let original_batch = inspection.original.as_ref().unwrap();
    assert_eq!(original_batch.write_warnings_count, 1);
    assert_eq!(original_batch.results.len(), 1);
    assert_eq!(
        original_batch.results[0].status,
        ToolResultStatus::UnknownOutcome
    );
    let warning = original_batch.results[0].write_warning.as_ref().unwrap();
    assert_eq!(warning.tool_name, "write_file");
    assert_eq!(warning.path, "file.txt");

    let plan = session
        .plan_repair(RepairMode::AdoptOriginal(original.clone()))
        .unwrap();
    let summary = plan.summary();
    assert_eq!(summary.adopted_count, 1);
    assert_eq!(summary.write_warnings_count, 1);
    assert_eq!(summary.write_warnings[0].tool_name, "write_file");
    assert_eq!(summary.write_warnings[0].path, "file.txt");

    session.apply_repair(&plan).unwrap();
    session.wait_saved().await.unwrap();

    session.workspace_tools().unwrap().set_write_gate(None);

    let next = session
        .submit(SubmitRequest {
            request_key: "r3".into(),
            blocks: vec![text_input("do edit")],
        })
        .unwrap()
        .work;

    finished(&session, &next).await;
    session.wait_saved().await.unwrap();

    let next_obs = session.observe(&next).unwrap();
    let WorkState::Finished(next_res) = next_obs.state else {
        panic!("next turn not finished");
    };
    assert!(matches!(
        next_res.as_ref(),
        crate::session::WorkResult::Run(causa::runtime::TurnResult::Completed { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(workspace.join("file.txt")).unwrap(),
        "final content\n"
    );

    let context = session.read_materials().unwrap().context;
    let mut edit_call_id = None;
    let mut edit_result_status = None;
    let mut read_file_after_third_input = false;
    let mut seen_third_input = false;

    for block in context.blocks() {
        match block.content() {
            BlockContent::Message { parts, .. } => {
                for part in parts {
                    if let causa::kernel::ContentPart::Text(t) = part
                        && t.0 == "do edit"
                    {
                        seen_third_input = true;
                    }
                }
            }
            BlockContent::ToolCall(call) => {
                if seen_third_input && call.tool_name == "read_file" {
                    read_file_after_third_input = true;
                }
                if call.tool_name == "edit_file" {
                    edit_call_id = Some(block.id());
                }
            }
            BlockContent::ToolResult(result) if Some(result.call_block_id) == edit_call_id => {
                edit_result_status = Some(result.status.clone());
            }
            _ => {}
        }
    }
    assert!(
        !read_file_after_third_input,
        "third turn must not issue read_file before edit"
    );
    assert_eq!(
        edit_result_status,
        Some(ToolResultStatus::Succeeded),
        "edit_file in third turn must succeed without prior read"
    );

    let requests = fixture.requests.lock().unwrap();
    assert!(requests.len() >= 5);
    let messages = requests[3]["messages"].as_array().expect("messages array");
    let read_calls_count = messages
        .iter()
        .filter_map(|m| m.get("tool_calls")?.as_array())
        .flatten()
        .filter(|c| c["function"]["name"] == "read_file")
        .count();
    assert_eq!(
        read_calls_count, 1,
        "only round 1 issued read_file; no new read_file before round 3 model request"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wm08_write_cancelled_after_rename_unknown_outcome_new_content() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("file.txt"), "old content\n").unwrap();

    let fixture = wire::Fixture::new(
        "openai-chat",
        vec![
            // Round 1: read_file so file is observed
            wire::Reply::Read("file.txt"),
            wire::Reply::Answer,
            // Round 2: write_file to overwrite
            wire::Reply::Call(
                "write_file".into(),
                serde_json::json!({"path": "file.txt", "content": "new content\n"}),
            ),
            // Round 3 (after repair): edit_file based on new content
            wire::Reply::Call(
                "edit_file".into(),
                serde_json::json!({
                    "path": "file.txt",
                    "edits": [{"old_text": "new content\n", "new_text": "final content\n"}]
                }),
            ),
            wire::Reply::Answer,
        ],
        None,
    );
    fixture.release();

    let host = SessionHost::new(Some(dir.path().join("store")));
    let mut def = definition();
    def.provider = Provider::OpenAiChat;
    def.endpoint = Some(fixture.endpoint.clone());
    def.credential = CredentialRef::Environment("PATH".into());
    def.workspace_root = Some(std::fs::canonicalize(&workspace).unwrap());
    def.tools = vec![
        WorkspaceTool::ReadFile,
        WorkspaceTool::WriteFile,
        WorkspaceTool::EditFile,
    ];
    def.max_model_rounds = 4;
    def.max_tool_calls = 4;
    def.permission_mode = Some(PermissionMode::FullAccess);

    let session = host
        .create(PreparedCreate::persistent(def, Context::new()))
        .await
        .unwrap();

    let read_work = session
        .submit(SubmitRequest {
            request_key: "r1".into(),
            blocks: vec![text_input("read file")],
        })
        .unwrap()
        .work;
    finished(&session, &read_work).await;
    session.wait_saved().await.unwrap();

    // Round 2: write_file with gate pausing after rename
    let gate = TestGate::new(true);
    session
        .workspace_tools()
        .unwrap()
        .set_write_gate(Some(gate.clone()));

    let original = session
        .submit(SubmitRequest {
            request_key: "r2".into(),
            blocks: vec![text_input("do write")],
        })
        .unwrap()
        .work;

    gate.reached.notified().await;
    session.cancel(&original).unwrap();
    gate.release();

    finished(&session, &original).await;
    session.wait_saved().await.unwrap();

    let observation = session.observe(&original).unwrap();
    let WorkState::Finished(result) = observation.state else {
        panic!("work not finished");
    };
    assert!(matches!(
        result.as_ref(),
        crate::session::WorkResult::Run(causa::runtime::TurnResult::Interrupted { .. })
    ));

    // File content is new content because rename completed before interruption.
    assert_eq!(
        std::fs::read_to_string(workspace.join("file.txt")).unwrap(),
        "new content\n"
    );

    let inspection = session.inspect_repair(Some(&original)).unwrap();
    let original_batch = inspection.original.as_ref().unwrap();
    assert_eq!(original_batch.write_warnings_count, 1);
    assert_eq!(original_batch.results.len(), 1);
    assert_eq!(
        original_batch.results[0].status,
        ToolResultStatus::UnknownOutcome
    );
    let warning = original_batch.results[0].write_warning.as_ref().unwrap();
    assert_eq!(warning.tool_name, "write_file");
    assert_eq!(warning.path, "file.txt");

    let plan = session
        .plan_repair(RepairMode::AdoptOriginal(original.clone()))
        .unwrap();
    let summary = plan.summary();
    assert_eq!(summary.adopted_count, 1);
    assert_eq!(summary.write_warnings_count, 1);
    assert_eq!(summary.write_warnings[0].tool_name, "write_file");
    assert_eq!(summary.write_warnings[0].path, "file.txt");

    session.apply_repair(&plan).unwrap();
    session.wait_saved().await.unwrap();

    session.workspace_tools().unwrap().set_write_gate(None);

    let next = session
        .submit(SubmitRequest {
            request_key: "r3".into(),
            blocks: vec![text_input("do edit")],
        })
        .unwrap()
        .work;

    finished(&session, &next).await;
    session.wait_saved().await.unwrap();

    let next_obs = session.observe(&next).unwrap();
    let WorkState::Finished(next_res) = next_obs.state else {
        panic!("next turn not finished");
    };
    assert!(matches!(
        next_res.as_ref(),
        crate::session::WorkResult::Run(causa::runtime::TurnResult::Completed { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(workspace.join("file.txt")).unwrap(),
        "final content\n"
    );

    let context = session.read_materials().unwrap().context;
    let mut edit_call_id = None;
    let mut edit_result_status = None;
    let mut read_file_after_third_input = false;
    let mut seen_third_input = false;

    for block in context.blocks() {
        match block.content() {
            BlockContent::Message { parts, .. } => {
                for part in parts {
                    if let causa::kernel::ContentPart::Text(t) = part
                        && t.0 == "do edit"
                    {
                        seen_third_input = true;
                    }
                }
            }
            BlockContent::ToolCall(call) => {
                if seen_third_input && call.tool_name == "read_file" {
                    read_file_after_third_input = true;
                }
                if call.tool_name == "edit_file" {
                    edit_call_id = Some(block.id());
                }
            }
            BlockContent::ToolResult(result) if Some(result.call_block_id) == edit_call_id => {
                edit_result_status = Some(result.status.clone());
            }
            _ => {}
        }
    }
    assert!(
        !read_file_after_third_input,
        "third turn must not issue read_file before edit"
    );
    assert_eq!(
        edit_result_status,
        Some(ToolResultStatus::Succeeded),
        "edit_file in third turn must succeed without prior read"
    );

    let requests = fixture.requests.lock().unwrap();
    assert!(requests.len() >= 5);
    let messages = requests[3]["messages"].as_array().expect("messages array");
    let read_calls_count = messages
        .iter()
        .filter_map(|m| m.get("tool_calls")?.as_array())
        .flatten()
        .filter(|c| c["function"]["name"] == "read_file")
        .count();
    assert_eq!(
        read_calls_count, 1,
        "only round 1 issued read_file; no new read_file before round 3 model request"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wm08_remove_unpaired_shows_write_warning_for_interrupted_write() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("file.txt"), "old content\n").unwrap();

    let fixture = wire::Fixture::new(
        "openai-chat",
        vec![
            wire::Reply::Read("file.txt"),
            wire::Reply::Answer,
            wire::Reply::Call(
                "write_file".into(),
                serde_json::json!({"path": "file.txt", "content": "new content\n"}),
            ),
        ],
        None,
    );
    fixture.release();

    let host = SessionHost::new(Some(dir.path().join("store")));
    let mut def = definition();
    def.provider = Provider::OpenAiChat;
    def.endpoint = Some(fixture.endpoint.clone());
    def.credential = CredentialRef::Environment("PATH".into());
    def.workspace_root = Some(std::fs::canonicalize(&workspace).unwrap());
    def.tools = vec![WorkspaceTool::ReadFile, WorkspaceTool::WriteFile];
    def.max_model_rounds = 4;
    def.max_tool_calls = 4;
    def.permission_mode = Some(PermissionMode::FullAccess);

    let session = host
        .create(PreparedCreate::persistent(def, Context::new()))
        .await
        .unwrap();

    let read_work = session
        .submit(SubmitRequest {
            request_key: "r1".into(),
            blocks: vec![text_input("read file")],
        })
        .unwrap()
        .work;
    finished(&session, &read_work).await;
    session.wait_saved().await.unwrap();

    let gate = TestGate::new(false);
    session
        .workspace_tools()
        .unwrap()
        .set_write_gate(Some(gate.clone()));

    let original = session
        .submit(SubmitRequest {
            request_key: "r2".into(),
            blocks: vec![text_input("do write")],
        })
        .unwrap()
        .work;

    gate.reached.notified().await;
    session.cancel(&original).unwrap();
    gate.release();

    finished(&session, &original).await;
    session.wait_saved().await.unwrap();

    let plan = session.plan_repair(RepairMode::RemoveUnpaired).unwrap();
    let summary = plan.summary();
    assert_eq!(summary.removed_count, 1);
    assert_eq!(summary.write_warnings_count, 1);
    assert_eq!(summary.write_warnings[0].tool_name, "write_file");
    assert_eq!(summary.write_warnings[0].path, "file.txt");
    assert_eq!(summary.write_warnings[0].kind, RepairWarningKind::FileWrite);
    assert_eq!(summary.external_warnings_count, 0);

    session.apply_repair(&plan).unwrap();
    session.wait_saved().await.unwrap();

    let inspection = session.inspect_repair(None).unwrap();
    assert!(inspection.diagnosis.is_ready());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wm09_read_and_write_cancelled_during_approval_wait_neither_started_adopts_none_no_warning()
{
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("file.txt"), "initial text\n").unwrap();

    let fixture = wire::Fixture::new(
        "openai-chat",
        vec![wire::Reply::Calls(vec![
            wire::ScriptedToolCall {
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "file.txt"}),
            },
            wire::ScriptedToolCall {
                name: "write_file".into(),
                arguments: serde_json::json!({"path": "created.txt", "content": "created text\n"}),
            },
        ])],
        None,
    );
    fixture.release();

    let host = SessionHost::new(Some(dir.path().join("store")));
    let mut def = definition();
    def.provider = Provider::OpenAiChat;
    def.endpoint = Some(fixture.endpoint.clone());
    def.credential = CredentialRef::Environment("PATH".into());
    def.workspace_root = Some(std::fs::canonicalize(&workspace).unwrap());
    def.tools = vec![WorkspaceTool::ReadFile, WorkspaceTool::WriteFile];
    def.max_model_rounds = 4;
    def.max_tool_calls = 4;
    def.permission_mode = Some(PermissionMode::Ask);

    let session = host
        .create(PreparedCreate::persistent(def, Context::new()))
        .await
        .unwrap();

    let mut approvals = session.connect_approvals();

    let original = session
        .submit(SubmitRequest {
            request_key: "original".into(),
            blocks: vec![text_input("read then write")],
        })
        .unwrap()
        .work;

    // Approval request arrives for write_file (created.txt).
    let _req = tokio::time::timeout(std::time::Duration::from_secs(5), approvals.recv())
        .await
        .expect("approval timeout")
        .expect("approval channel closed");

    // Cancel while write_file is waiting for approval.
    session.cancel(&original).unwrap();

    finished(&session, &original).await;
    session.wait_saved().await.unwrap();

    let inspection = session.inspect_repair(Some(&original)).unwrap();
    let original_batch = inspection.original.as_ref().unwrap();
    // PermissionProcessor runs in the before phase. Cancelling during approval wait
    // stops the batch before tool dispatch begins, so neither tool is executed.
    // Both declarations remain pending; upon adoption, neither has a completed result,
    // so both unstarted declarations are removed without write warnings.
    assert_eq!(original_batch.completed_count, 0);
    assert_eq!(original_batch.pending_count, 2);
    assert_eq!(original_batch.write_warnings_count, 0);

    let plan = session
        .plan_repair(RepairMode::AdoptOriginal(original.clone()))
        .unwrap();
    let summary = plan.summary();
    assert_eq!(summary.adopted_count, 0);
    assert_eq!(summary.removed_count, 2);
    assert_eq!(summary.write_warnings_count, 0);

    session.apply_repair(&plan).unwrap();
    session.wait_saved().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wm09_rejected_write_and_cancelled_write_during_approval_adopts_rejected_no_warning() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();

    let fixture = wire::Fixture::new(
        "openai-chat",
        vec![wire::Reply::Calls(vec![
            wire::ScriptedToolCall {
                name: "write_file".into(),
                arguments: serde_json::json!({"path": "first.txt", "content": "one\n"}),
            },
            wire::ScriptedToolCall {
                name: "write_file".into(),
                arguments: serde_json::json!({"path": "second.txt", "content": "two\n"}),
            },
        ])],
        None,
    );
    fixture.release();

    let host = SessionHost::new(Some(dir.path().join("store")));
    let mut def = definition();
    def.provider = Provider::OpenAiChat;
    def.endpoint = Some(fixture.endpoint.clone());
    def.credential = CredentialRef::Environment("PATH".into());
    def.workspace_root = Some(std::fs::canonicalize(&workspace).unwrap());
    def.tools = vec![WorkspaceTool::WriteFile];
    def.max_model_rounds = 4;
    def.max_tool_calls = 4;
    def.permission_mode = Some(PermissionMode::Ask);

    let session = host
        .create(PreparedCreate::persistent(def, Context::new()))
        .await
        .unwrap();

    let mut approvals = session.connect_approvals();

    let original = session
        .submit(SubmitRequest {
            request_key: "original".into(),
            blocks: vec![text_input("write twice")],
        })
        .unwrap()
        .work;

    let req1 = tokio::time::timeout(std::time::Duration::from_secs(5), approvals.recv())
        .await
        .expect("first approval timeout")
        .expect("approval channel closed");
    let _ = req1.reply.send(ApprovalDecision::Deny { reason: None });

    let _req2 = tokio::time::timeout(std::time::Duration::from_secs(5), approvals.recv())
        .await
        .expect("second approval timeout")
        .expect("approval channel closed");
    session.cancel(&original).unwrap();

    finished(&session, &original).await;
    session.wait_saved().await.unwrap();

    let inspection = session.inspect_repair(Some(&original)).unwrap();
    let original_batch = inspection.original.as_ref().unwrap();
    assert_eq!(original_batch.completed_count, 1);
    assert_eq!(original_batch.results[0].status, ToolResultStatus::Rejected);
    assert_eq!(original_batch.pending_count, 1);
    assert_eq!(original_batch.write_warnings_count, 0);

    let plan = session
        .plan_repair(RepairMode::AdoptOriginal(original.clone()))
        .unwrap();
    let summary = plan.summary();
    assert_eq!(summary.adopted_count, 1);
    assert_eq!(summary.removed_count, 1);
    assert_eq!(summary.write_warnings_count, 0);

    session.apply_repair(&plan).unwrap();
    session.wait_saved().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wm09_read_and_write_cancelled_before_rename_adopts_both_warns_write_only() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("existing.txt"), "existing content\n").unwrap();

    let fixture = wire::Fixture::new(
        "openai-chat",
        vec![
            wire::Reply::Calls(vec![
                wire::ScriptedToolCall {
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "existing.txt"}),
                },
                wire::ScriptedToolCall {
                    name: "write_file".into(),
                    arguments: serde_json::json!({"path": "target.txt", "content": "written content\n"}),
                },
            ]),
            wire::Reply::Answer,
        ],
        None,
    );
    fixture.release();

    let host = SessionHost::new(Some(dir.path().join("store")));
    let mut def = definition();
    def.provider = Provider::OpenAiChat;
    def.endpoint = Some(fixture.endpoint.clone());
    def.credential = CredentialRef::Environment("PATH".into());
    def.workspace_root = Some(std::fs::canonicalize(&workspace).unwrap());
    def.tools = vec![WorkspaceTool::ReadFile, WorkspaceTool::WriteFile];
    def.max_model_rounds = 4;
    def.max_tool_calls = 4;
    def.permission_mode = Some(PermissionMode::AcceptEdits);
    // Limit concurrency to 1 so read_file finishes and is recorded in the batch
    // before write_file reaches the test gate.
    def.tool_limits.max_concurrent_operations = 1;

    let session = host
        .create(PreparedCreate::persistent(def, Context::new()))
        .await
        .unwrap();

    let tools = session.workspace_tools().unwrap();
    let write_gate = TestGate::new(false); // gate stops before rename
    tools.set_write_gate(Some(write_gate.clone()));

    let mut progress = session.subscribe_progress();

    let original = session
        .submit(SubmitRequest {
            request_key: "batch_rw".into(),
            blocks: vec![text_input("read and write")],
        })
        .unwrap()
        .work;

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
                // A lagged receiver skipped older updates; keep waiting instead
                // of proceeding without seeing the read return.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    panic!("progress channel closed before read_file returned")
                }
            }
        }
    })
    .await
    .expect("read_file returned timeout");

    tokio::time::timeout(Duration::from_secs(5), write_gate.reached.notified())
        .await
        .expect("write_gate reached timeout");

    session.cancel(&original).unwrap();
    write_gate.release();

    finished(&session, &original).await;
    session.wait_saved().await.unwrap();

    let inspection = session.inspect_repair(Some(&original)).unwrap();
    let original_batch = inspection.original.as_ref().unwrap();
    assert_eq!(original_batch.completed_count, 2);
    assert_eq!(original_batch.pending_count, 0);
    assert_eq!(original_batch.write_warnings_count, 1);

    let read_result = original_batch
        .results
        .iter()
        .find(|r| r.write_warning.is_none())
        .expect("read_file preview result");
    assert_eq!(read_result.status, ToolResultStatus::Succeeded);

    let write_result = original_batch
        .results
        .iter()
        .find(|r| r.write_warning.is_some())
        .expect("write_file preview result");
    assert_eq!(write_result.status, ToolResultStatus::UnknownOutcome);
    let warning = write_result.write_warning.as_ref().unwrap();
    assert_eq!(warning.tool_name, "write_file");
    assert_eq!(warning.path, "target.txt");

    let plan = session
        .plan_repair(RepairMode::AdoptOriginal(original.clone()))
        .unwrap();
    let summary = plan.summary();
    assert_eq!(summary.adopted_count, 2);
    assert_eq!(summary.removed_count, 0);
    assert_eq!(summary.write_warnings_count, 1);
    assert_eq!(summary.write_warnings.len(), 1);
    assert_eq!(summary.write_warnings[0].tool_name, "write_file");
    assert_eq!(summary.write_warnings[0].path, "target.txt");
    assert_eq!(summary.write_warnings[0].kind, RepairWarningKind::FileWrite);
    assert_eq!(summary.external_warnings_count, 0);

    session.apply_repair(&plan).unwrap();
    session.wait_saved().await.unwrap();

    let next = session
        .submit(SubmitRequest {
            request_key: "r2".into(),
            blocks: vec![text_input("continue after repair")],
        })
        .unwrap()
        .work;

    finished(&session, &next).await;
    session.wait_saved().await.unwrap();

    let next_obs = session.observe(&next).unwrap();
    let WorkState::Finished(next_res) = next_obs.state else {
        panic!("next turn not finished");
    };
    assert!(matches!(
        next_res.as_ref(),
        crate::session::WorkResult::Run(causa::runtime::TurnResult::Completed { .. })
    ));
}

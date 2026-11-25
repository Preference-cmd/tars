use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use causa::extension::McpToolAnnotations;
use causa::kernel::{
    BlockContent, Context, ProcessorContext, ProcessorError, ToolBatch, ToolBatchProcessor,
    ToolResultStatus,
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
        temperature: None,
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
        context_window: None,
        auto_compact: true,
        compact_threshold: 70,
        prompt_cache: true,
        prelude: Default::default(),
        code_execution: None,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_modes_refuse_writes_into_version_control_metadata() {
    for mode in [PermissionMode::AcceptEdits, PermissionMode::FullAccess] {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join(".git")).unwrap();
        std::fs::write(root.path().join(".git/config"), "[core]\n").unwrap();

        // Read first so the write is refused for the metadata path itself,
        // not for skipping read-before-write.
        let replies = vec![
            call_read(".git/config"),
            Reply::Calls(vec![
                ScriptedToolCall {
                    name: "write_file".into(),
                    arguments: json!({
                        "path": ".git/config",
                        "content": "[core]\n\tfsmonitor = touch pwned\n",
                    }),
                },
                ScriptedToolCall {
                    name: "write_file".into(),
                    arguments: json!({"path": ".gitignore", "content": "target/\n"}),
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

        let work = submit(&session, "vcs metadata write");
        let obs = finish(&session, &work).await;
        assert_eq!(obs.tools.calls.len(), 3);
        assert_eq!(
            obs.tools.calls[0].status,
            Some(ToolResultStatus::Succeeded),
            "{mode}"
        );
        assert_eq!(
            obs.tools.calls[1].status,
            Some(ToolResultStatus::Rejected),
            "{mode}"
        );
        assert_eq!(
            obs.tools.calls[2].status,
            Some(ToolResultStatus::Succeeded),
            "{mode}"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join(".git/config")).unwrap(),
            "[core]\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join(".gitignore")).unwrap(),
            "target/\n"
        );

        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_access_refuses_writes_into_dot_tars_and_in_workspace_store() {
    // 1. Direct child store (`root/data` -> `root/data/sessions`) + `.tars` + `.TARS` + normal file.
    {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("data");
        std::fs::create_dir_all(root.path().join(".tars/skills/a")).unwrap();
        std::fs::write(
            root.path().join(".tars/skills/a/SKILL.md"),
            "---\nname: a\ndescription: d\n---\nbody\n",
        )
        .unwrap();
        std::fs::write(root.path().join("normal.txt"), "before\n").unwrap();
        std::fs::create_dir_all(store.join("sessions")).unwrap();
        std::fs::write(store.join("sessions/existing.json"), "{\"v\":1}\n").unwrap();

        let replies = vec![
            Reply::Calls(vec![
                ScriptedToolCall {
                    name: "read_file".into(),
                    arguments: json!({"path": ".tars/skills/a/SKILL.md"}),
                },
                ScriptedToolCall {
                    name: "read_file".into(),
                    arguments: json!({"path": "data/sessions/existing.json"}),
                },
                ScriptedToolCall {
                    name: "read_file".into(),
                    arguments: json!({"path": "normal.txt"}),
                },
            ]),
            Reply::Calls(vec![
                ScriptedToolCall {
                    name: "write_file".into(),
                    arguments: json!({"path": ".tars", "content": "pwn"}),
                },
                ScriptedToolCall {
                    name: "write_file".into(),
                    arguments: json!({"path": ".tars/x", "content": "pwn"}),
                },
                ScriptedToolCall {
                    name: "edit_file".into(),
                    arguments: json!({
                        "path": ".tars/skills/a/SKILL.md",
                        "edits": [{"old_text": "body", "new_text": "pwned"}]
                    }),
                },
                ScriptedToolCall {
                    name: "write_file".into(),
                    arguments: json!({"path": ".TARS/x", "content": "pwn"}),
                },
                ScriptedToolCall {
                    name: "write_file".into(),
                    arguments: json!({"path": "data/sessions/forged.json", "content": "{}"}),
                },
                ScriptedToolCall {
                    name: "edit_file".into(),
                    arguments: json!({
                        "path": "data/sessions/existing.json",
                        "edits": [{"old_text": "\"v\":1", "new_text": "\"v\":2"}]
                    }),
                },
                ScriptedToolCall {
                    name: "edit_file".into(),
                    arguments: json!({
                        "path": "normal.txt",
                        "edits": [{"old_text": "before", "new_text": "after"}]
                    }),
                },
            ]),
            Reply::Answer,
        ];

        let fixture = Fixture::new("openai-chat", replies, None);
        let host = SessionHost::new(Some(store.clone()));
        let mut def = base_definition(&fixture.endpoint, root.path());
        def.max_tool_calls = 16;
        def.permission_mode = Some(PermissionMode::FullAccess);
        let session = host
            .create(PreparedCreate::persistent(def, Context::default()))
            .await
            .unwrap();

        let work = submit(&session, "full access protected paths");
        let obs = finish(&session, &work).await;
        assert_eq!(obs.tools.calls.len(), 10);
        for idx in 0..=2 {
            assert_eq!(
                obs.tools.calls[idx].status,
                Some(ToolResultStatus::Succeeded)
            );
        }
        for idx in 3..=8 {
            assert_eq!(
                obs.tools.calls[idx].status,
                Some(ToolResultStatus::Rejected),
                "call {idx} ({}) must be rejected",
                obs.tools.calls[idx].tool_name
            );
        }
        assert_eq!(obs.tools.calls[9].status, Some(ToolResultStatus::Succeeded));
        assert!(!root.path().join(".tars/x").exists());
        assert!(!store.join("sessions/forged.json").exists());
        assert_eq!(
            std::fs::read_to_string(store.join("sessions/existing.json")).unwrap(),
            "{\"v\":1}\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("normal.txt")).unwrap(),
            "after\n"
        );
        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }

    // 2. Nested in-workspace store (`root/nested/data` -> `root/nested/data/sessions`)
    // blocks store writes while leaving `nested/other.txt` writable.
    {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("nested/data");
        let replies = vec![
            Reply::Calls(vec![
                ScriptedToolCall {
                    name: "write_file".into(),
                    arguments: json!({"path": "nested/data/sessions/forged.json", "content": "{}"}),
                },
                ScriptedToolCall {
                    name: "write_file".into(),
                    arguments: json!({"path": "nested/other.txt", "content": "ok\n"}),
                },
            ]),
            Reply::Answer,
        ];
        let fixture = Fixture::new("openai-chat", replies, None);
        let host = SessionHost::new(Some(store.clone()));
        let mut def = base_definition(&fixture.endpoint, root.path());
        def.permission_mode = Some(PermissionMode::FullAccess);
        let session = host
            .create(PreparedCreate::persistent(def, Context::default()))
            .await
            .unwrap();
        let work = submit(&session, "nested store");
        let obs = finish(&session, &work).await;
        assert_eq!(obs.tools.calls.len(), 2);
        assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Rejected));
        assert_eq!(obs.tools.calls[1].status, Some(ToolResultStatus::Succeeded));
        assert!(!store.join("sessions/forged.json").exists());
        assert_eq!(
            std::fs::read_to_string(root.path().join("nested/other.txt")).unwrap(),
            "ok\n"
        );
        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }

    // 3. Store directory equal to workspace root (`dir/sessions` == `workspace_root`)
    // refuses all writes.
    {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("sessions");
        std::fs::create_dir_all(&root).unwrap();
        let replies = vec![call_write("ordinary.txt", "blocked\n"), Reply::Answer];
        let fixture = Fixture::new("openai-chat", replies, None);
        let host = SessionHost::new(Some(base.path().to_path_buf()));
        let mut def = base_definition(&fixture.endpoint, &root);
        def.permission_mode = Some(PermissionMode::FullAccess);
        let session = host
            .create(PreparedCreate::persistent(def, Context::default()))
            .await
            .unwrap();
        let work = submit(&session, "store equals root");
        let obs = finish(&session, &work).await;
        assert_eq!(obs.tools.calls.len(), 1);
        assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Rejected));
        assert!(!root.join("ordinary.txt").exists());
        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }

    // 4. External store leaves normal workspace writes unaffected.
    {
        let root = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let replies = vec![call_write("ordinary.txt", "allowed\n"), Reply::Answer];
        let fixture = Fixture::new("openai-chat", replies, None);
        let host = SessionHost::new(Some(store.path().to_path_buf()));
        let mut def = base_definition(&fixture.endpoint, root.path());
        def.permission_mode = Some(PermissionMode::FullAccess);
        let session = host
            .create(PreparedCreate::persistent(def, Context::default()))
            .await
            .unwrap();
        let work = submit(&session, "external store");
        let obs = finish(&session, &work).await;
        assert_eq!(obs.tools.calls.len(), 1);
        assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Succeeded));
        assert_eq!(
            std::fs::read_to_string(root.path().join("ordinary.txt")).unwrap(),
            "allowed\n"
        );
        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm05_core_allow_deny_and_batch_ordering() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![
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

    let approval_task1 = tokio::spawn(async move {
        let req1 = rx.recv().await.expect("req1");
        let ApprovalSubject::FileWrite { path, .. } = &req1.subject else {
            panic!("expected FileWrite")
        };
        assert_eq!(path, "file1.txt");
        req1.reply.send(ApprovalDecision::Allow).unwrap();

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

    let d0 = obs1.tools.calls[0].permission_decision.as_ref().unwrap();
    assert_eq!(d0.mode, PermissionMode::Ask);
    assert_eq!(d0.kind, PermissionDecisionKind::AskedAndAllowed);

    let d1 = obs1.tools.calls[1].permission_decision.as_ref().unwrap();
    assert_eq!(d1.mode, PermissionMode::Ask);
    assert_eq!(d1.kind, PermissionDecisionKind::AskedAndDenied);

    {
        let requests = fixture.requests.lock().unwrap();
        assert!(requests.len() >= 2);
        let req1_str = requests[1].to_string();
        assert!(
            req1_str.contains("user declined this write: policy disallows file2"),
            "model request payload must contain refusal reason: {req1_str}"
        );
    }

    let approval_task2 = tokio::spawn(async move {
        let req = rx.recv().await.expect("req for same.txt");
        let ApprovalSubject::FileWrite { path, .. } = &req.subject else {
            panic!("expected FileWrite")
        };
        assert_eq!(path, "same.txt");
        req.reply.send(ApprovalDecision::Allow).unwrap();
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

    // A precheck rejection records no permission decision.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wm07_call_timeout_does_not_abort_approval_wait() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![call_write("timeout_test.txt", "success"), Reply::Answer];

    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let mut def = base_definition(&fixture.endpoint, root.path());
    def.tool_limits.call_timeout_secs = 1;

    let session = host
        .create(PreparedCreate::transient(def, Context::default()))
        .await
        .unwrap();

    let mut rx = session.connect_approvals();
    let approval_task = tokio::spawn(async move {
        let req = rx.recv().await.expect("approval request");
        // Exceed the one-second call timeout before answering.
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
    def.work_timeout_secs = Some(1);

    let session = host
        .create(PreparedCreate::persistent(def, Context::default()))
        .await
        .unwrap();

    let mut rx = session.connect_approvals();
    let approval_task = tokio::spawn(async move {
        let _req = rx.recv().await.expect("approval request");
        // Exceed the one-second work timeout without answering.
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
        drop(req.reply);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_mode_switch_wm17() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![
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
        // Switch mid-batch, before replying.
        let (mode, rev) = session_clone
            .set_permission_mode(PermissionMode::AcceptEdits)
            .unwrap();
        assert_eq!(mode, PermissionMode::AcceptEdits);
        assert_eq!(rev, 1);
        req.reply.send(ApprovalDecision::Allow).unwrap();
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

struct VerifyingProcessor {
    seen_mode: Mutex<Option<PermissionMode>>,
    seen_revision: Mutex<Option<u64>>,
    seen_metadata: Mutex<Vec<(String, bool, bool, bool)>>,
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
        let mut records = Vec::new();
        for entry in batch.calls() {
            let tool_name = entry.call().input.tool_name.clone();
            let has_managed = ctx
                .tool_metadata::<Arc<dyn crate::tools::ManagedTool>>(&tool_name)
                .is_some();
            let has_mcp_annotations = ctx
                .tool_metadata::<McpToolAnnotations>(&tool_name)
                .is_some();
            let has_attachments = !entry.call().attachments().is_empty();
            records.push((tool_name, has_managed, has_mcp_annotations, has_attachments));
        }
        *self.seen_metadata.lock().unwrap() = records;
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
        seen_metadata: Mutex::new(Vec::new()),
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

    // The mode and revision read by the test processor must match every decision.
    for call in &obs.tools.calls {
        let decision = call
            .permission_decision
            .as_ref()
            .expect("decision must be recorded for each call");
        assert_eq!(decision.mode, seen_mode.unwrap());
        assert_eq!(decision.revision, seen_rev.unwrap());
        assert_eq!(decision.revision, 2); // equal to switch count
    }

    let metadata = verifier.seen_metadata.lock().unwrap().clone();
    assert_eq!(metadata.len(), 5);

    for (expected_name, expected_attachments, record) in [
        ("read_file", false, &metadata[0]),
        ("list_directory", false, &metadata[1]),
        ("search_text", false, &metadata[2]),
        ("write_file", true, &metadata[3]),
        ("edit_file", true, &metadata[4]),
    ] {
        assert_eq!(record.0, expected_name);
        assert!(record.1, "{expected_name} must expose ManagedTool metadata");
        assert!(
            !record.2,
            "{expected_name} must not carry MCP tool annotations"
        );
        assert_eq!(
            record.3, expected_attachments,
            "{expected_name} call attachment presence mismatch"
        );
    }

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unprepared_preview_in_ask_mode_records_unprepared_decision() {
    use crate::tools::{ManagedTool, ToolCallIntent};
    use causa::kernel::{
        CallControl, InvocationId, RoundId, ToolCallContext, ToolCallPayload, ToolDefinition,
        ToolDescription, TurnId,
    };
    use tokio_util::sync::CancellationToken;

    struct NoPreviewTool(crate::policy::ToolPermissionClass);
    #[async_trait::async_trait]
    impl ManagedTool for NoPreviewTool {
        fn classify(&self, _arguments: &serde_json::Value) -> ToolCallIntent {
            ToolCallIntent {
                class: self.0,
                command_subject: None,
            }
        }
    }

    for (class, expected_kind) in [
        (
            crate::policy::ToolPermissionClass::BuiltinWrite,
            PermissionDecisionKind::Unprepared,
        ),
        (
            crate::policy::ToolPermissionClass::CommandOutsideSandbox,
            PermissionDecisionKind::UnpreparedOutsideSandbox,
        ),
    ] {
        let call_id = causa::kernel::BlockId::new(uuid::Uuid::now_v7());
        let call = ToolCallContext::new(call_id, ToolCallPayload::new("mock_tool", json!({})));
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

        let perm_hub = Arc::new(crate::permission::PermissionHub::new(PermissionMode::Ask));
        let _rx = perm_hub.connect_approvals();
        let processor =
            crate::permission::ToolPolicyProcessor::new(work, reporter.clone(), perm_hub);

        let mut batch = ToolBatch::new(vec![call.clone()]).unwrap();
        let declaration_order = [call.call_block_id];
        let control = CallControl::new(CancellationToken::new(), None);
        let managed: Arc<dyn ManagedTool> = Arc::new(NoPreviewTool(class));
        let bound_tools = [ToolDescription::new(ToolDefinition::new(
            "mock_tool",
            "mock",
            json!({"type": "object"}),
        ))
        .with_meta(managed)];
        let ctx = ProcessorContext::new(&turn_id, RoundId(0), &declaration_order, &control)
            .with_tools(&bound_tools);

        processor.process(&mut batch, &ctx).await.unwrap();

        let snap = reporter.snapshot();
        assert_eq!(snap.calls.len(), 1);
        let decision = snap.calls[0].permission_decision.as_ref().unwrap();
        assert_eq!(decision.mode, PermissionMode::Ask);
        assert_eq!(decision.kind, expected_kind);

        let (_, result) = batch.results().first().unwrap().result().unwrap();
        assert_eq!(result.status, ToolResultStatus::Rejected);
        assert_eq!(
            result.output.content["error"],
            "tool call could not produce an approval preview"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_without_permissions_behavior() {
    let dir = tempfile::tempdir().unwrap();
    let host = SessionHost::new(None);
    host.set_test_assemble(Some(Arc::new(|definition, id, context| {
        Ok(crate::session::Session::new(
            id,
            context,
            move |_work, _control| async {
                panic!("not called");
            },
            definition.session_config(),
        ))
    })));
    let def = base_definition("http://127.0.0.1:0", dir.path());
    let session = host
        .create(PreparedCreate::transient(def, Context::default()))
        .await
        .unwrap();

    assert_eq!(session.permission_mode(), None);

    let res = session.set_permission_mode(PermissionMode::ReadOnly);
    assert!(matches!(res, Err(ApplicationError::PermissionUnavailable)));

    let mut rx = session.connect_approvals();
    assert!(rx.recv().await.is_none());

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
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

#[tokio::test]
async fn minimal_record_fixture_reopen_preserves_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("store");
    let legacy_dir = dir.path().join("legacy");
    std::fs::create_dir_all(&legacy_dir).unwrap();

    let id = uuid::Uuid::now_v7().to_string();
    let commit = uuid::Uuid::now_v7().to_string();
    let record_path = legacy_dir.join(format!("{id}.json"));
    let raw_json = format!(
        r#"{{"format":"tars-session-v2.4","conversation_id":"{id}","revision":1,"last_commit_id":"{commit}","updated_at_ms":1000,"definition":{{"provider":"openai-chat","model":"legacy-model","endpoint":null,"credential":{{"environment":"PATH"}},"work_timeout_secs":null,"retained_work_capacity":16,"max_model_rounds":8,"max_tool_calls":0,"workspace_root":null,"tools":[],"tool_limits":{{"max_concurrent_operations":4,"call_timeout_secs":10,"max_output_bytes":65536,"max_directory_entries":200,"max_file_scan_bytes":2097152,"max_search_files":1000,"max_search_depth":32,"max_search_scan_bytes":8388608,"max_search_matches":200}}}},"context":{{"blocks":[]}}}}"#
    );
    std::fs::write(&record_path, raw_json.as_bytes()).unwrap();
    let original_bytes = std::fs::read(&record_path).unwrap();

    let host = SessionHost::new(Some(storage.clone()));
    host.import_legacy_record(&record_path).await.unwrap();
    let before_open = crate::application::tests::support::saved_record(&storage, &id);
    let session = host.open(id.clone().into()).await.unwrap();

    assert!(session.definition().tools.is_empty());
    assert_eq!(session.definition().write_limits, None);
    assert_eq!(session.definition().permission_mode, None);
    assert_eq!(
        session.definition().initial_permission_mode(),
        PermissionMode::Ask
    );
    assert_eq!(session.permission_mode(), Some((PermissionMode::Ask, 0)));
    assert_eq!(
        session.session_usage(),
        Some(crate::usage::SessionUsage::default())
    );

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    assert_eq!(std::fs::read(&record_path).unwrap(), original_bytes);
    let after_close = crate::application::tests::support::saved_record(&storage, &id);
    assert_eq!(after_close, before_open);
}

#[tokio::test]
async fn field_writing_rules_and_omissions() {
    let dir = tempfile::tempdir().unwrap();
    let storage = dir.path().join("store");

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

    let json_a = crate::application::tests::support::saved_record(&storage, id_a.as_str());
    assert!(json_a.get("permission_mode").is_none());
    assert!(json_a["definition"].get("permission_mode").is_none());
    assert!(json_a["definition"].get("write_limits").is_none());

    let mut def_b = base_definition("https://example.test", dir.path());
    def_b.permission_mode = Some(PermissionMode::AcceptEdits);

    let prepared_b = PreparedCreate::persistent(def_b, Context::default());
    let id_b = prepared_b.conversation_id().clone();
    let session_b = host.create(prepared_b).await.unwrap();
    session_b.wait_saved().await.unwrap();

    let json_b = crate::application::tests::support::saved_record(&storage, id_b.as_str());
    assert_eq!(json_b["definition"]["permission_mode"], "accept-edits");
    assert!(json_b.get("permission_mode").is_none());

    session_b
        .set_permission_mode(PermissionMode::ReadOnly)
        .unwrap();
    session_b.wait_saved().await.unwrap();

    let json_b2 = crate::application::tests::support::saved_record(&storage, id_b.as_str());
    assert_eq!(json_b2["permission_mode"], "read-only");

    session_b
        .set_permission_mode(PermissionMode::AcceptEdits)
        .unwrap();
    session_b.wait_saved().await.unwrap();

    let json_b3 = crate::application::tests::support::saved_record(&storage, id_b.as_str());
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

    let record_str = serde_json::to_string(&crate::application::tests::support::saved_record(
        &storage,
        id.as_str(),
    ))
    .unwrap();
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

    s.set_permission_mode(PermissionMode::AcceptEdits).unwrap();
    let after_status = s.wait_saved().await.unwrap();
    let after_rev = match after_status {
        SaveStatus::Saved { revision, .. } => revision,
        _ => panic!("must be saved"),
    };
    assert_eq!(after_rev, initial_rev + 1);

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

    let req = rx.recv().await.expect("request received");
    s_clone
        .set_permission_mode(PermissionMode::ReadOnly)
        .unwrap();
    s_clone
        .set_permission_mode(PermissionMode::AcceptEdits)
        .unwrap();

    req.reply.send(ApprovalDecision::Allow).unwrap();

    let obs = finish(&s, &work).await;
    assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Succeeded));

    let json_val = crate::application::tests::support::saved_record(&storage, id.as_str());
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

    s.set_writer_fault_hook(Some(Arc::new(|stage| {
        if stage == CommitStage::Write {
            FaultAction::Fail(std::io::Error::other("injected storage error"))
        } else {
            FaultAction::Continue
        }
    })));

    let _ = s.edit(
        s.read_materials().unwrap().version,
        vec![crate::application::text_input("mutated")],
    );

    crate::application::tests::support::until(|| {
        matches!(s.status().save, SaveStatus::SaveBlocked { .. })
    })
    .await;

    // The pending commit keeps the original Ask mode, stored as None.
    let pending_commit_id = s.pending_commit_id().unwrap();
    assert_eq!(s.pending_commit_permission_mode(), Some(None));

    s.set_permission_mode(PermissionMode::AcceptEdits).unwrap();

    assert_eq!(s.pending_commit_id().unwrap(), pending_commit_id);
    assert_eq!(s.pending_commit_permission_mode(), Some(None));

    s.set_writer_fault_hook(None);

    let retry_status = s.retry_save().await.unwrap();
    assert!(matches!(retry_status, SaveStatus::Saved { .. }));

    let json_val = crate::application::tests::support::saved_record(&storage, id.as_str());
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

    s.set_permission_mode(PermissionMode::AcceptEdits).unwrap();
    let outcome = host.close(&s.target(), ClosePolicy::Save).await.unwrap();
    assert!(matches!(outcome.save, SaveStatus::Saved { .. }));

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
        call_read("test.txt"),
        Reply::Answer,
        call_edit("test.txt", "alpha", "gamma"),
        Reply::Answer,
        call_read("test.txt"),
        Reply::Answer,
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

    let work1 = submit(&session, "round 1 read");
    let obs1 = finish(&session, &work1).await;
    assert_eq!(
        obs1.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    let reopened = host.open(id.clone()).await.unwrap();

    let work2 = submit(&reopened, "round 2 edit");
    let obs2 = finish(&reopened, &work2).await;
    assert_eq!(obs2.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    let err_msg = tool_result_error(&reopened, obs2.tools.calls[0].call_block_id);
    assert!(err_msg.contains("must be read with read_file before editing"));
    assert_eq!(std::fs::read_to_string(&file_path).unwrap(), "alpha beta\n");

    let work3 = submit(&reopened, "round 3 re-read");
    let obs3 = finish(&reopened, &work3).await;
    assert_eq!(
        obs3.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );

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
        call_read("target.txt"),
        Reply::Answer,
        call_write("/tmp/abs_path.txt", "evil"),
        Reply::Answer,
        call_write("../outside.txt", "evil"),
        Reply::Answer,
        call_write("link.txt", "evil"),
        Reply::Answer,
        call_write("fifo.txt", "evil"),
        Reply::Answer,
        call_edit("target.txt", "nonexistent", "new"),
        Reply::Answer,
        call_edit("target.txt", "alpha", "new"),
        Reply::Answer,
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
        Reply::Call(
            "edit_file".into(),
            json!({
                "path": "target.txt",
                "edits": [{"old_text": "", "new_text": "something"}]
            }),
        ),
        Reply::Answer,
        call_write("a/b/c.txt", "nested content\n"),
        Reply::Answer,
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

    let w0 = submit(&session, "observe");
    let o0 = finish(&session, &w0).await;
    assert_eq!(o0.tools.calls[0].status, Some(ToolResultStatus::Succeeded));

    let w1 = submit(&session, "abs write");
    let o1 = finish(&session, &w1).await;
    assert_eq!(o1.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    assert!(rx.try_recv().is_err(), "no approval request for abs path");

    let w2 = submit(&session, "parent write");
    let o2 = finish(&session, &w2).await;
    assert_eq!(o2.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    assert!(rx.try_recv().is_err(), "no approval request for parent ..");

    let w3 = submit(&session, "symlink write");
    let o3 = finish(&session, &w3).await;
    assert_eq!(o3.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    assert!(rx.try_recv().is_err(), "no approval request for symlink");

    let w4 = submit(&session, "fifo write");
    let o4 = finish(&session, &w4).await;
    assert_eq!(o4.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    assert!(rx.try_recv().is_err(), "no approval request for fifo");

    let w5 = submit(&session, "edit not found");
    let o5 = finish(&session, &w5).await;
    assert_eq!(o5.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    let err5 = tool_result_error(&session, o5.tools.calls[0].call_block_id);
    assert!(err5.contains("edits[0]"), "must indicate segment 0: {err5}");
    assert!(
        rx.try_recv().is_err(),
        "no approval request for edit not found"
    );

    let w6 = submit(&session, "edit multi");
    let o6 = finish(&session, &w6).await;
    assert_eq!(o6.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    let err6 = tool_result_error(&session, o6.tools.calls[0].call_block_id);
    assert!(err6.contains("edits[0]"), "must indicate segment 0: {err6}");
    assert!(
        rx.try_recv().is_err(),
        "no approval request for multi match"
    );

    let w7 = submit(&session, "edit overlap");
    let o7 = finish(&session, &w7).await;
    assert_eq!(o7.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    let err7 = tool_result_error(&session, o7.tools.calls[0].call_block_id);
    assert!(err7.contains("edits[1]"), "must indicate segment 1: {err7}");
    assert!(rx.try_recv().is_err(), "no approval request for overlap");

    let w8 = submit(&session, "edit empty");
    let o8 = finish(&session, &w8).await;
    assert_eq!(o8.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    let err8 = tool_result_error(&session, o8.tools.calls[0].call_block_id);
    assert!(err8.contains("edits[0]"), "must indicate segment 0: {err8}");
    assert!(
        rx.try_recv().is_err(),
        "no approval request for empty old_text"
    );

    assert_eq!(std::fs::read(&target_file).unwrap(), original_bytes);

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
fn wm17_decide_24_cells_exhaustive() {
    use crate::permission::{PermissionMode, PermissionVerdict, ToolPermissionClass, decide};

    use PermissionMode::{AcceptEdits, Ask, FullAccess, ReadOnly};
    use PermissionVerdict::{Ask as AskUser, Execute, Reject};
    use ToolPermissionClass::{
        BuiltinCommand, BuiltinWrite, CommandOutsideSandbox, KnownReadOnly, McpApprove, McpPrompt,
    };

    let cells = [
        (ReadOnly, KnownReadOnly, Execute),
        (ReadOnly, BuiltinWrite, Reject),
        (ReadOnly, BuiltinCommand, Reject),
        (ReadOnly, CommandOutsideSandbox, Reject),
        (ReadOnly, McpPrompt, Reject),
        (ReadOnly, McpApprove, Reject),
        (Ask, KnownReadOnly, Execute),
        (Ask, BuiltinWrite, AskUser),
        (Ask, BuiltinCommand, AskUser),
        (Ask, CommandOutsideSandbox, AskUser),
        (Ask, McpPrompt, AskUser),
        (Ask, McpApprove, Execute),
        (AcceptEdits, KnownReadOnly, Execute),
        (AcceptEdits, BuiltinWrite, Execute),
        (AcceptEdits, BuiltinCommand, Execute),
        (AcceptEdits, CommandOutsideSandbox, AskUser),
        (AcceptEdits, McpPrompt, AskUser),
        (AcceptEdits, McpApprove, Execute),
        (FullAccess, KnownReadOnly, Execute),
        (FullAccess, BuiltinWrite, Execute),
        (FullAccess, BuiltinCommand, Execute),
        (FullAccess, CommandOutsideSandbox, Execute),
        (FullAccess, McpPrompt, Execute),
        (FullAccess, McpApprove, Execute),
    ];
    for (mode, class, expected) in cells {
        assert_eq!(
            decide(mode, class),
            expected,
            "mode {mode:?}, class {class:?}"
        );
    }
}

#[test]
fn malformed_run_command_arguments_take_the_outside_class() {
    use crate::permission::ToolPermissionClass;
    use crate::tools::{ToolResources, WorkspaceTools};
    use crate::workspace::Workspace;

    let dir = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(&dir.path().canonicalize().unwrap()).unwrap();
    let shared = WorkspaceTools::new(workspace, Default::default(), ToolResources::new(4));
    let turn_id = causa::kernel::TurnId::new("classify");
    let reporter = crate::session::ToolReporter::new(
        WorkRef {
            conversation_id: "classify".into(),
            turn_id,
        },
        tokio::sync::broadcast::channel(8).0,
    );
    let (_, managed) = shared
        .bind_managed_for_test(&[WorkspaceTool::RunCommand], reporter)
        .pop()
        .unwrap();

    assert_eq!(
        managed.classify(&json!("not an object")).class,
        ToolPermissionClass::CommandOutsideSandbox
    );
    assert_eq!(
        managed.classify(&json!({"outside_sandbox": null})).class,
        ToolPermissionClass::CommandOutsideSandbox
    );
    assert_eq!(
        managed.classify(&json!({"command": "true"})).class,
        ToolPermissionClass::BuiltinCommand
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

    let _rx = session.connect_approvals();

    let session_clone = session.clone();
    session.set_test_after_prepare_hook(Arc::new(move || {
        session_clone
            .set_permission_mode(PermissionMode::ReadOnly)
            .unwrap();
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
        .join(format!("tars-command-outside-{}", uuid::Uuid::now_v7()));
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
            target,
            ..
        } = &allow.subject
        else {
            panic!("expected Command subject")
        };
        assert_eq!(command, "printf allow > allowed.txt");
        assert_eq!(workdir, ".");
        assert_eq!(*timeout_secs, 120);
        assert_eq!(
            *target,
            crate::permission::CommandTarget::Sandboxed {
                network: crate::definition::CommandNetwork::None
            }
        );
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
    // Causa settles every started call as UnknownOutcome when the turn is
    // cancelled; the command's Drop guard still kills the process tree. This
    // is the integration boundary, not a Cancelled result from the tool.
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
    // The running command's own timeout is bounded below the work deadline;
    // keep that bound well above the queued command's one second so the first
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
        .err()
        .expect("run_command must fail closed on unsupported platforms");
    let reason = error.to_string();
    assert!(
        reason.contains("requires a platform sandbox backend"),
        "{reason}"
    );
    assert!(
        reason.contains(std::env::consts::OS),
        "the error must name the platform: {reason}"
    );
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
fn call_command_outside(command: &str, justification: &str) -> ScriptedToolCall {
    call_command(json!({
        "command": command,
        "outside_sandbox": true,
        "justification": justification
    }))
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_outside_sandbox_full_access_runs_and_clears_environment() {
    let root = tempfile::tempdir().unwrap();
    let outside = root
        .path()
        .parent()
        .unwrap()
        .join(format!("tars-outside-{}.txt", uuid::Uuid::now_v7()));
    let script = format!("{{ printf 'outside\\n'; env; }} > '{}'", outside.display());
    let replies = vec![
        Reply::Calls(vec![call_command_outside(
            &script,
            "the sandbox blocks a path outside the workspace",
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
        .set_permission_mode(PermissionMode::FullAccess)
        .unwrap();
    let work = submit(&session, "outside command");
    let observation = finish(&session, &work).await;
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
    assert_eq!(content["exit_code"], 0);
    assert_eq!(content["outside_sandbox"], true);
    let written = std::fs::read_to_string(&outside).unwrap();
    assert!(written.starts_with("outside\n"));
    assert!(!written.contains("CARGO_"), "{written}");
    assert!(!written.contains("SSH_AUTH_SOCK"), "{written}");
    assert!(written.contains("TMPDIR="), "{written}");
    std::fs::remove_file(&outside).unwrap();
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_outside_sandbox_ask_allows_and_denies_with_the_subject() {
    let root = tempfile::tempdir().unwrap();
    let parent = root.path().parent().unwrap();
    let allowed = parent.join(format!("tars-outside-allow-{}", uuid::Uuid::now_v7()));
    let denied = parent.join(format!("tars-outside-deny-{}", uuid::Uuid::now_v7()));
    let replies = vec![
        Reply::Calls(vec![call_command_outside(
            &format!("printf allow > '{}'", allowed.display()),
            "needs to write outside the workspace",
        )]),
        Reply::Answer,
        Reply::Calls(vec![call_command_outside(
            &format!("printf deny > '{}'", denied.display()),
            "needs to write outside the workspace",
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
        for allow in [true, false] {
            let request = rx.recv().await.expect("outside command request");
            let ApprovalSubject::Command {
                target, command, ..
            } = &request.subject
            else {
                panic!("expected Command subject")
            };
            assert!(matches!(
                target,
                crate::permission::CommandTarget::Outside { justification }
                    if justification == "needs to write outside the workspace"
            ));
            assert!(command.contains("printf"));
            request
                .reply
                .send(if allow {
                    ApprovalDecision::Allow
                } else {
                    ApprovalDecision::Deny {
                        reason: Some("user policy".into()),
                    }
                })
                .unwrap();
        }
    });
    let allow_work = submit(&session, "allow outside command");
    let allow_obs = finish(&session, &allow_work).await;
    let deny_work = submit(&session, "deny outside command");
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
    assert_eq!(
        command_result(&session, &allow_obs, 0)["outside_sandbox"],
        true
    );
    assert!(allowed.exists());
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
    assert!(
        tool_result_error(&session, deny_obs.tools.calls[0].call_block_id)
            .contains("outside the sandbox")
    );
    assert!(!denied.exists());
    std::fs::remove_file(&allowed).unwrap();
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_outside_sandbox_accept_edits_asks_and_allows_with_a_route() {
    let root = tempfile::tempdir().unwrap();
    let outside = root
        .path()
        .parent()
        .unwrap()
        .join(format!("tars-outside-accept-{}", uuid::Uuid::now_v7()));
    let replies = vec![
        Reply::Calls(vec![call_command_outside(
            &format!("printf allow > '{}'", outside.display()),
            "needs to write outside the workspace",
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
        let request = rx.recv().await.expect("outside command approval");
        let ApprovalSubject::Command { target, .. } = &request.subject else {
            panic!("expected Command subject")
        };
        assert!(matches!(
            target,
            crate::permission::CommandTarget::Outside { .. }
        ));
        request.reply.send(ApprovalDecision::Allow).unwrap();
    });
    let work = submit(&session, "accept-edits outside command");
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
    assert_eq!(
        command_result(&session, &observation, 0)["outside_sandbox"],
        true
    );
    assert!(outside.exists());
    std::fs::remove_file(&outside).unwrap();
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_outside_sandbox_read_only_rejects_without_prompt() {
    let root = tempfile::tempdir().unwrap();
    let outside = root
        .path()
        .parent()
        .unwrap()
        .join(format!("tars-outside-readonly-{}", uuid::Uuid::now_v7()));
    let replies = vec![
        Reply::Calls(vec![call_command_outside(
            &format!("printf x > '{}'", outside.display()),
            "needs to write outside the workspace",
        )]),
        Reply::Answer,
    ];
    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let mut definition = command_definition(&fixture.endpoint, root.path());
    definition.permission_mode = Some(PermissionMode::ReadOnly);
    let session = host
        .create(PreparedCreate::transient(definition, Context::default()))
        .await
        .unwrap();
    let work = submit(&session, "read-only outside command");
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
    assert!(
        tool_result_error(&session, observation.tools.calls[0].call_block_id)
            .contains("outside the sandbox")
    );
    assert!(!outside.exists());
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_outside_sandbox_requires_a_valid_justification() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![
        Reply::Calls(vec![call_command(json!({
            "command": "printf x > missing.txt",
            "outside_sandbox": true
        }))]),
        Reply::Answer,
        Reply::Calls(vec![call_command(json!({
            "command": "printf x > malformed.txt",
            "outside_sandbox": "yes",
            "justification": "reason"
        }))]),
        Reply::Answer,
        Reply::Calls(vec![call_command(json!({
            "command": "printf x > null.txt",
            "outside_sandbox": null,
            "justification": "reason"
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
    let missing_work = submit(&session, "missing justification");
    let missing_obs = finish(&session, &missing_work).await;
    let malformed_work = submit(&session, "malformed outside flag");
    let malformed_obs = finish(&session, &malformed_work).await;
    let null_work = submit(&session, "null outside flag");
    let null_obs = finish(&session, &null_work).await;
    assert_eq!(
        missing_obs.tools.calls[0].status,
        Some(ToolResultStatus::Rejected)
    );
    assert!(
        tool_result_error(&session, missing_obs.tools.calls[0].call_block_id)
            .contains("justification")
    );
    for (label, observation, text) in [
        (
            "malformed",
            &malformed_obs,
            "outside_sandbox must be a boolean",
        ),
        ("null", &null_obs, "outside_sandbox must be a boolean"),
    ] {
        assert!(
            !observation.tools.calls.is_empty(),
            "{label} produced no tool call"
        );
        assert_eq!(
            observation.tools.calls[0].status,
            Some(ToolResultStatus::Rejected)
        );
        assert!(
            tool_result_error(&session, observation.tools.calls[0].call_block_id).contains(text)
        );
    }
    assert!(!root.path().join("missing.txt").exists());
    assert!(!root.path().join("malformed.txt").exists());
    assert!(!root.path().join("null.txt").exists());
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_outside_sandbox_accept_edits_without_a_route_rejects_only_the_outside_call() {
    let root = tempfile::tempdir().unwrap();
    let outside = root
        .path()
        .parent()
        .unwrap()
        .join(format!("tars-outside-batch-{}", uuid::Uuid::now_v7()));
    let replies = vec![
        Reply::Calls(vec![
            call_command_outside(
                &format!("printf x > '{}'", outside.display()),
                "needs to write outside the workspace",
            ),
            call_command(json!({"command": "printf inside > inside.txt"})),
        ]),
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
    let work = submit(&session, "accept-edits outside batch");
    let observation = finish(&session, &work).await;
    assert_eq!(observation.tools.calls.len(), 2);
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
        PermissionDecisionKind::NoConfirmationRoute
    );
    assert_eq!(
        observation.tools.calls[1].status,
        Some(ToolResultStatus::Succeeded)
    );
    assert_eq!(
        observation.tools.calls[1]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::DirectExecution
    );
    assert!(!outside.exists());
    assert!(root.path().join("inside.txt").exists());
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_accept_edits_runs_sandboxed_commands() {
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
    let work = submit(&session, "accept-edits command");
    let observation = finish(&session, &work).await;
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
    assert!(root.path().join("accepted.txt").exists());
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_outside_flag_in_accept_edits_is_gated_as_outside() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![
        Reply::Calls(vec![call_command(json!({
            "command": "printf x > bad.txt",
            "outside_sandbox": "yes",
            "justification": "reason"
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
        .set_permission_mode(PermissionMode::AcceptEdits)
        .unwrap();
    let work = submit(&session, "malformed outside flag");
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
        PermissionDecisionKind::NoConfirmationRoute
    );
    assert!(!root.path().join("bad.txt").exists());
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_accept_edits_runs_sandboxed_commands_with_the_open_setting() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![
        Reply::Calls(vec![call_command(
            json!({"command": "printf yes > accepted-open.txt"}),
        )]),
        Reply::Answer,
    ];
    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let mut definition = command_definition(&fixture.endpoint, root.path());
    definition.command = Some(crate::definition::CommandSettings {
        network: crate::definition::CommandNetwork::Open,
        ..crate::definition::CommandSettings::default()
    });
    let session = host
        .create(PreparedCreate::transient(definition, Context::default()))
        .await
        .unwrap();
    session
        .set_permission_mode(PermissionMode::AcceptEdits)
        .unwrap();
    let work = submit(&session, "accept-edits command with open network");
    let observation = finish(&session, &work).await;
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
    assert!(root.path().join("accepted-open.txt").exists());
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_cancel_repair_adopts_and_never_reruns() {
    command_cancel_repair_case(false).await;
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outside_command_cancel_repair_warns_and_never_reruns() {
    command_cancel_repair_case(true).await;
}

#[cfg(target_os = "macos")]
async fn command_cancel_repair_case(outside: bool) {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let marker = root
        .path()
        .parent()
        .unwrap()
        .join(format!("tars-outside-repair-{}", uuid::Uuid::now_v7()));
    let marker_write = if outside {
        format!("printf outside > '{}';", marker.display())
    } else {
        String::new()
    };
    let command = format!(
        "printf x >> runs.txt; {marker_write} printf started > started.txt; sleep 30; printf done > done.txt"
    );
    let arguments = if outside {
        json!({
            "command": &command,
            "outside_sandbox": true,
            "justification": "needs to run outside the workspace"
        })
    } else {
        json!({ "command": &command })
    };
    let replies = vec![
        Reply::Calls(vec![call_command(arguments)]),
        // The only model reply after the interrupted turn belongs to the new
        // input; it must not ask for another command.
        Reply::Answer,
    ];
    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(Some(storage.path().join("store")));
    let mut definition = command_definition(&fixture.endpoint, root.path());
    // Keep the command's bounded timeout above the cancellation window.
    definition.work_timeout_secs = Some(30);
    let session = host
        .create(PreparedCreate::persistent(definition, Context::default()))
        .await
        .unwrap();
    session.wait_saved().await.unwrap();
    session
        .set_permission_mode(PermissionMode::FullAccess)
        .unwrap();
    session.wait_saved().await.unwrap();

    let work = submit(&session, "start a long command");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !root.path().join("started.txt").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the command did not start"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    session.cancel(&work).unwrap();
    let observation = finish(&session, &work).await;
    assert_eq!(observation.tools.calls.len(), 1);
    assert_eq!(
        observation.tools.calls[0].status,
        Some(ToolResultStatus::UnknownOutcome)
    );
    let original_id = observation.tools.calls[0].call_block_id;

    let inspection = session.inspect_repair(Some(&work)).unwrap();
    let original = inspection.original.expect("retained command batch");
    assert_eq!(original.command_warnings_count, 1);
    let warning = original.results[0]
        .write_warning
        .as_ref()
        .expect("command warning");
    assert_eq!(warning.kind, crate::application::RepairWarningKind::Command);
    let plan = session
        .plan_repair(crate::application::RepairMode::AdoptOriginal(work.clone()))
        .unwrap();
    let summary = plan.summary();
    assert_eq!(summary.command_warnings_count, 1);
    assert_eq!(summary.write_warnings_count, 1);
    assert_eq!(summary.write_warnings[0].call_block_id, original_id);
    assert_eq!(
        summary.write_warnings[0].kind,
        crate::application::RepairWarningKind::Command
    );
    assert!(
        summary.write_warnings[0]
            .advice()
            .contains("may have executed"),
        "the repair material must warn that the command may have executed"
    );
    session.apply_repair(&plan).unwrap();
    session.wait_saved().await.unwrap();

    let next = submit(&session, "continue after repair");
    let next_observation = finish(&session, &next).await;
    assert!(
        next_observation.tools.calls.is_empty(),
        "the command must not rerun during continuation"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("runs.txt")).unwrap(),
        "x",
        "the command ran more than once"
    );

    {
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "the fixture served two work turns");
        let second = requests[1].to_string();
        assert!(
            second.contains("material_repair"),
            "the repaired material did not reach the model: {second}"
        );
        assert!(
            second.contains("no acceptable result was observed"),
            "the model did not see the unknown command result: {second}"
        );
    }

    let context = session.read_materials().unwrap().context;
    let adopted = context
        .blocks()
        .iter()
        .find_map(|block| match block.content() {
            BlockContent::ToolResult(payload) if payload.call_block_id == original_id => {
                Some(payload.status.clone())
            }
            _ => None,
        });
    assert_eq!(
        adopted,
        Some(ToolResultStatus::UnknownOutcome),
        "the adopted original call must stay an unknown outcome"
    );
    assert!(!root.path().join("done.txt").exists(), "the command reran");
    assert!(root.path().join("started.txt").exists());
    if outside {
        assert!(
            marker.exists(),
            "the interrupted command did not run outside the sandbox"
        );
        std::fs::remove_file(&marker).unwrap();
    } else {
        assert!(!marker.exists(), "a sandboxed command wrote outside");
    }
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rc03_orchestrator_evaluates_modes_rules_and_routes_consistently() {
    use crate::definition::{CommandSettings, WriteToolLimits};
    use crate::permission::rules::{
        CommandRule, CommandRuleSet, RuleDecision, RuleScope, RuleSource,
    };
    use crate::permission::{PermissionHub, ToolPolicyProcessor};
    use crate::tools::{CommandRuntime, ToolResources, WorkspaceTools};
    use crate::workspace::Workspace;
    use causa::kernel::{
        CallControl, InvocationId, ProcessorContext, RoundId, ToolBatch, ToolCallContext,
        ToolCallPayload, ToolDescription, TurnId,
    };
    use tars_sandbox::{BackendId, BackendReport, Capability, Limitation, Sandbox};
    use tokio_util::sync::CancellationToken;

    let rules = CommandRuleSet::with_layers(
        vec![CommandRule {
            prefix: vec!["rm".into()],
            decision: RuleDecision::Deny,
            outside_sandbox: false,
        }],
        vec![
            CommandRule {
                prefix: vec!["cargo".into(), "test".into()],
                decision: RuleDecision::Allow,
                outside_sandbox: false,
            },
            CommandRule {
                prefix: vec!["git".into(), "push".into()],
                decision: RuleDecision::Allow,
                outside_sandbox: true,
            },
            CommandRule {
                prefix: vec!["curl".into()],
                decision: RuleDecision::Ask,
                outside_sandbox: false,
            },
        ],
        vec![CommandRule {
            prefix: vec!["pwd".into()],
            decision: RuleDecision::Allow,
            outside_sandbox: false,
        }],
    );

    let make_call = |outside: bool, command: &str| {
        let args = if outside {
            json!({
                "command": command,
                "outside_sandbox": true,
                "justification": "needed outside",
            })
        } else {
            json!({ "command": command })
        };
        ToolCallContext::new(
            causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
            ToolCallPayload::new("run_command", args),
        )
    };

    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(&root.path().canonicalize().unwrap()).unwrap();
    let report = BackendReport::new(
        BackendId::RECORDING,
        Capability::Full,
        Capability::Full,
        Capability::Full,
        Capability::Full,
        Capability::Partial(Limitation::ProcessGroupEscape),
    );
    let sandbox = Sandbox::open_with(Box::new(
        tars_sandbox::test_support::RecordingBackend::available(report),
    ))
    .unwrap();
    let cmd_runtime = CommandRuntime::bind_with_sandbox(
        root.path(),
        CommandSettings::default(),
        Ok(sandbox),
        None,
    )
    .unwrap();
    let ws_tools = WorkspaceTools::with_command(
        workspace,
        WorkspaceToolLimits::default(),
        WriteToolLimits::default(),
        ToolResources::new(4),
        CommandSettings::default(),
        Some(cmd_runtime),
    );

    for mode in [
        PermissionMode::ReadOnly,
        PermissionMode::Ask,
        PermissionMode::AcceptEdits,
        PermissionMode::FullAccess,
    ] {
        for (command, outside, expected_kind, expected_rule) in [
            (
                "rm -rf x",
                false,
                PermissionDecisionKind::RuleDenied,
                Some(RuleSource {
                    scope: RuleScope::User,
                    prefix: vec!["rm".into()],
                }),
            ),
            (
                "rm -rf x",
                true,
                PermissionDecisionKind::RuleDenied,
                Some(RuleSource {
                    scope: RuleScope::User,
                    prefix: vec!["rm".into()],
                }),
            ),
            (
                "cargo test -p x",
                false,
                match mode {
                    PermissionMode::ReadOnly => PermissionDecisionKind::ModeRefused,
                    PermissionMode::Ask => PermissionDecisionKind::RuleAllowed,
                    PermissionMode::AcceptEdits | PermissionMode::FullAccess => {
                        PermissionDecisionKind::DirectExecution
                    }
                },
                match mode {
                    PermissionMode::Ask => Some(RuleSource {
                        scope: RuleScope::Workspace,
                        prefix: vec!["cargo".into(), "test".into()],
                    }),
                    _ => None,
                },
            ),
            (
                "cargo test -p x",
                true,
                match mode {
                    PermissionMode::ReadOnly => PermissionDecisionKind::ModeRefused,
                    PermissionMode::Ask | PermissionMode::AcceptEdits => {
                        PermissionDecisionKind::NoConfirmationRoute
                    }
                    PermissionMode::FullAccess => PermissionDecisionKind::DirectExecution,
                },
                None,
            ),
            (
                "git push origin main",
                true,
                match mode {
                    PermissionMode::ReadOnly => PermissionDecisionKind::ModeRefused,
                    PermissionMode::Ask | PermissionMode::AcceptEdits => {
                        PermissionDecisionKind::RuleAllowed
                    }
                    PermissionMode::FullAccess => PermissionDecisionKind::DirectExecution,
                },
                match mode {
                    PermissionMode::Ask | PermissionMode::AcceptEdits => Some(RuleSource {
                        scope: RuleScope::Workspace,
                        prefix: vec!["git".into(), "push".into()],
                    }),
                    _ => None,
                },
            ),
            (
                "curl https://example.com",
                false,
                match mode {
                    PermissionMode::ReadOnly => PermissionDecisionKind::ModeRefused,
                    PermissionMode::Ask
                    | PermissionMode::AcceptEdits
                    | PermissionMode::FullAccess => PermissionDecisionKind::NoConfirmationRoute,
                },
                match mode {
                    PermissionMode::ReadOnly => None,
                    _ => Some(RuleSource {
                        scope: RuleScope::Workspace,
                        prefix: vec!["curl".into()],
                    }),
                },
            ),
        ] {
            let perm_hub = Arc::new(PermissionHub::new(mode));
            perm_hub.set_rules(rules.clone());
            let turn_id = TurnId::new(uuid::Uuid::now_v7().to_string());
            let work = WorkRef {
                conversation_id: "test".into(),
                turn_id: turn_id.clone(),
            };
            let inv = InvocationId {
                turn_id: turn_id.clone(),
                round_id: RoundId(0),
            };
            let call = make_call(outside, command);
            let decl = [call.call_block_id];
            let control = CallControl::new(CancellationToken::new(), None);
            let reporter = crate::session::ToolReporter::new(
                work.clone(),
                tokio::sync::broadcast::channel(16).0,
            );
            let mut batch = ToolBatch::new(vec![call.clone()]).unwrap();
            reporter.batch_ready(&inv, &batch);
            let tool = ws_tools
                .bind_for_test(&[WorkspaceTool::RunCommand], reporter.clone())
                .pop()
                .unwrap();
            let bound_desc =
                [ToolDescription::new(tool.definition()).with_metadata(tool.metadata())];
            let ctx = ProcessorContext::new(&turn_id, RoundId(0), &decl, &control)
                .with_tools(&bound_desc);
            let processor = ToolPolicyProcessor::new(work, reporter.clone(), perm_hub);
            processor.process(&mut batch, &ctx).await.unwrap();

            let proc_decision = reporter.snapshot().calls[0]
                .permission_decision
                .clone()
                .unwrap();
            assert_eq!(
                proc_decision.kind, expected_kind,
                "mode={mode:?}, cmd={command:?}, outside={outside}"
            );
            assert_eq!(
                proc_decision.rule, expected_rule,
                "mode={mode:?}, cmd={command:?}, outside={outside}"
            );

            if matches!(
                expected_kind,
                PermissionDecisionKind::RuleDenied
                    | PermissionDecisionKind::ModeRefused
                    | PermissionDecisionKind::NoConfirmationRoute
            ) {
                let resolved_entry = &batch.results()[0];
                assert!(resolved_entry.call().attachments().is_empty());
                let (_, proc_res) = resolved_entry.result().unwrap();
                assert_eq!(proc_res.status, ToolResultStatus::Rejected);
                if expected_kind == PermissionDecisionKind::RuleDenied {
                    let err = proc_res.output.content["error"].as_str().unwrap();
                    assert!(err.contains("user permission rule"));
                    assert!(!err.contains("cargo"));
                    assert!(!err.contains("git"));
                }
            } else {
                assert!(batch.results().is_empty());
                let pending_call = batch.calls()[0].call();
                assert!(!pending_call.attachments().is_empty());
                let out = tool.execute(pending_call, &control, None).await;
                assert_ne!(
                    out.output.content["error"],
                    "outside-sandbox execution was not granted"
                );
            }
        }
    }

    // With confirmation route in FullAccess, `ask` rule asks and attaches RuleSource.
    for approved in [true, false] {
        let perm_hub = Arc::new(PermissionHub::new(PermissionMode::FullAccess));
        perm_hub.set_rules(rules.clone());
        let mut rx = perm_hub.connect_approvals();
        let responder = tokio::spawn(async move {
            let req = rx.recv().await.unwrap();
            assert!(matches!(req.subject, ApprovalSubject::Command { .. }));
            let _ = req.reply.send(if approved {
                ApprovalDecision::Allow
            } else {
                ApprovalDecision::Deny {
                    reason: Some("nope".into()),
                }
            });
        });

        let turn_id = TurnId::new(uuid::Uuid::now_v7().to_string());
        let work = WorkRef {
            conversation_id: "test".into(),
            turn_id: turn_id.clone(),
        };
        let inv = InvocationId {
            turn_id: turn_id.clone(),
            round_id: RoundId(0),
        };
        let call = make_call(false, "curl https://example.com");
        let decl = [call.call_block_id];
        let control = CallControl::new(CancellationToken::new(), None);
        let reporter =
            crate::session::ToolReporter::new(work.clone(), tokio::sync::broadcast::channel(16).0);
        let mut batch = ToolBatch::new(vec![call.clone()]).unwrap();
        reporter.batch_ready(&inv, &batch);
        let tool = ws_tools
            .bind_for_test(&[WorkspaceTool::RunCommand], reporter.clone())
            .pop()
            .unwrap();
        let bound_desc = [ToolDescription::new(tool.definition()).with_metadata(tool.metadata())];
        let ctx =
            ProcessorContext::new(&turn_id, RoundId(0), &decl, &control).with_tools(&bound_desc);
        ToolPolicyProcessor::new(work, reporter.clone(), perm_hub.clone())
            .process(&mut batch, &ctx)
            .await
            .unwrap();
        responder.await.unwrap();

        let decision = reporter.snapshot().calls[0]
            .permission_decision
            .clone()
            .unwrap();
        assert_eq!(
            decision.kind,
            if approved {
                PermissionDecisionKind::AskedAndAllowed
            } else {
                PermissionDecisionKind::AskedAndDenied
            }
        );
        assert_eq!(
            decision.rule,
            Some(RuleSource {
                scope: RuleScope::Workspace,
                prefix: vec!["curl".into()],
            })
        );
        if approved {
            let pending_call = batch.calls()[0].call();
            assert!(!pending_call.attachments().is_empty());
            let _ = tool.execute(pending_call, &control, None).await;
        } else {
            assert!(batch.results()[0].call().attachments().is_empty());
        }
    }
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rc03_end_to_end_command_rules_execution_and_grants() {
    use crate::permission::rules::{
        CommandRule, CommandRuleSet, RuleDecision, RuleScope, RuleSource,
    };

    let root = tempfile::tempdir().unwrap();
    let outside_allowed = root
        .path()
        .parent()
        .unwrap()
        .join(format!("tars-rule-outside-ok-{}", uuid::Uuid::now_v7()));
    let outside_blocked = root
        .path()
        .parent()
        .unwrap()
        .join(format!("tars-rule-outside-no-{}", uuid::Uuid::now_v7()));
    let _ = std::fs::remove_file(&outside_allowed);
    let _ = std::fs::remove_dir_all(&outside_blocked);

    let rules = CommandRuleSet::with_layers(
        vec![CommandRule {
            prefix: vec!["mkdir".into(), "denied_dir".into()],
            decision: RuleDecision::Deny,
            outside_sandbox: false,
        }],
        vec![
            CommandRule {
                prefix: vec!["mkdir".into(), "asked_dir".into()],
                decision: RuleDecision::Ask,
                outside_sandbox: false,
            },
            CommandRule {
                prefix: vec!["mkdir".into()],
                decision: RuleDecision::Allow,
                outside_sandbox: false,
            },
            CommandRule {
                prefix: vec!["touch".into()],
                decision: RuleDecision::Allow,
                outside_sandbox: true,
            },
        ],
        vec![],
    );

    let replies = vec![
        // 1. ReadOnly + allow rule -> still ModeRefused
        Reply::Calls(vec![call_command(json!({
            "command": "mkdir readonly_dir"
        }))]),
        Reply::Answer,
        // 2. FullAccess + deny rule -> RuleDenied
        Reply::Calls(vec![call_command(json!({
            "command": "mkdir denied_dir"
        }))]),
        Reply::Answer,
        // 3. FullAccess + ask rule with no route -> NoConfirmationRoute, no process spawned
        Reply::Calls(vec![call_command(json!({
            "command": "mkdir asked_dir"
        }))]),
        Reply::Answer,
        // 4. Ask + allow rule -> RuleAllowed without prompt
        Reply::Calls(vec![call_command(json!({
            "command": "mkdir allowed_dir"
        }))]),
        Reply::Answer,
        // 5. AcceptEdits + outside_sandbox with allow(outside=false) vs allow(outside=true)
        Reply::Calls(vec![
            call_command_outside(
                &format!("mkdir '{}'", outside_blocked.display()),
                "rule lacks outside_sandbox",
            ),
            call_command_outside(
                &format!("touch '{}'", outside_allowed.display()),
                "rule grants outside_sandbox",
            ),
        ]),
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
    session.set_command_rules(rules);

    // 1. ReadOnly
    session
        .set_permission_mode(PermissionMode::ReadOnly)
        .unwrap();
    let obs1 = finish(&session, &submit(&session, "turn 1")).await;
    assert_eq!(obs1.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    assert_eq!(
        obs1.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::ModeRefused
    );
    assert!(!root.path().join("readonly_dir").exists());

    // 2. FullAccess + deny
    session
        .set_permission_mode(PermissionMode::FullAccess)
        .unwrap();
    let obs2 = finish(&session, &submit(&session, "turn 2")).await;
    assert_eq!(obs2.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    let d2 = obs2.tools.calls[0].permission_decision.as_ref().unwrap();
    assert_eq!(d2.kind, PermissionDecisionKind::RuleDenied);
    assert_eq!(
        d2.rule,
        Some(RuleSource {
            scope: RuleScope::User,
            prefix: vec!["mkdir".into(), "denied_dir".into()],
        })
    );
    assert!(!root.path().join("denied_dir").exists());

    // 3. FullAccess + ask without confirmation route
    let obs3 = finish(&session, &submit(&session, "turn 3")).await;
    assert_eq!(obs3.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    let d3 = obs3.tools.calls[0].permission_decision.as_ref().unwrap();
    assert_eq!(d3.kind, PermissionDecisionKind::NoConfirmationRoute);
    assert_eq!(
        d3.rule,
        Some(RuleSource {
            scope: RuleScope::Workspace,
            prefix: vec!["mkdir".into(), "asked_dir".into()],
        })
    );
    assert!(!root.path().join("asked_dir").exists());

    // 4. Ask + allow rule executes directly without route
    session.set_permission_mode(PermissionMode::Ask).unwrap();
    let obs4 = finish(&session, &submit(&session, "turn 4")).await;
    assert_eq!(
        obs4.tools.calls[0].status,
        Some(ToolResultStatus::Succeeded)
    );
    let d4 = obs4.tools.calls[0].permission_decision.as_ref().unwrap();
    assert_eq!(d4.kind, PermissionDecisionKind::RuleAllowed);
    assert_eq!(
        d4.rule,
        Some(RuleSource {
            scope: RuleScope::Workspace,
            prefix: vec!["mkdir".into()],
        })
    );
    assert!(root.path().join("allowed_dir").is_dir());

    // 5. AcceptEdits + outside_sandbox
    session
        .set_permission_mode(PermissionMode::AcceptEdits)
        .unwrap();
    let obs5 = finish(&session, &submit(&session, "turn 5")).await;
    assert_eq!(obs5.tools.calls.len(), 2);
    assert_eq!(obs5.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    assert_eq!(
        obs5.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::NoConfirmationRoute
    );
    assert!(!outside_blocked.exists());
    assert_eq!(
        obs5.tools.calls[1].status,
        Some(ToolResultStatus::Succeeded)
    );
    let d5 = obs5.tools.calls[1].permission_decision.as_ref().unwrap();
    assert_eq!(d5.kind, PermissionDecisionKind::RuleAllowed);
    assert_eq!(
        d5.rule,
        Some(RuleSource {
            scope: RuleScope::Workspace,
            prefix: vec!["touch".into()],
        })
    );
    assert!(outside_allowed.exists());
    let _ = std::fs::remove_file(&outside_allowed);

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tl04_t7_unselected_builtin_tools_match_unknown_tools_with_unavailable_decision() {
    for mode in [
        PermissionMode::ReadOnly,
        PermissionMode::Ask,
        PermissionMode::AcceptEdits,
        PermissionMode::FullAccess,
    ] {
        let root = tempfile::tempdir().unwrap();
        let replies = vec![
            Reply::Calls(vec![
                ScriptedToolCall {
                    name: "write_file".into(),
                    arguments: json!({"path": "unselected.txt", "content": "nope"}),
                },
                ScriptedToolCall {
                    name: "run_command".into(),
                    arguments: json!({"command": "echo nope"}),
                },
                ScriptedToolCall {
                    name: "nonexistent_tool".into(),
                    arguments: json!({"x": 1}),
                },
            ]),
            Reply::Answer,
        ];

        let fixture = Fixture::new("openai-chat", replies, None);
        let host = SessionHost::new(None);
        let mut def = base_definition(&fixture.endpoint, root.path());
        // Only enable read_file; write_file and run_command are NOT selected.
        def.tools = vec![WorkspaceTool::ReadFile];
        def.permission_mode = Some(mode);
        let session = host
            .create(PreparedCreate::transient(def, Context::default()))
            .await
            .unwrap();
        let mut rx = session.connect_approvals();

        let work = submit(&session, "call unselected builtins and unknown tool");
        let obs = finish(&session, &work).await;
        assert_eq!(obs.tools.calls.len(), 3, "{mode}");

        for (idx, expected_name) in ["write_file", "run_command", "nonexistent_tool"]
            .iter()
            .enumerate()
        {
            let call = &obs.tools.calls[idx];
            assert_eq!(call.status, Some(ToolResultStatus::Rejected), "{mode}");
            let decision = call
                .permission_decision
                .as_ref()
                .expect("unavailable call must record decision");
            assert_eq!(decision.mode, mode);
            assert_eq!(decision.kind, PermissionDecisionKind::Unavailable);
            assert_eq!(
                tool_result_error(&session, call.call_block_id),
                format!("tool '{expected_name}' is not available")
            );
        }
        assert!(
            rx.try_recv().is_err(),
            "unavailable tools must never trigger approval prompts"
        );
        assert!(!root.path().join("unselected.txt").exists());

        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }
}

#[test]
fn tl05_mcp_trust_boundary_enforces_frozen_approval_over_hints() {
    use crate::definition::McpApproval;
    use crate::mcp::{McpToolAdapter, McpToolInfo, classify_mcp_tool};
    use crate::permission::ToolPermissionClass;
    use crate::tools::{ApprovalPreview, ManagedTool, PreparedCall};

    let mut read_only_ann = McpToolAnnotations::default();
    read_only_ann.read_only = Some(true);
    read_only_ann.destructive = Some(false);
    // Frozen `Prompt` (`ask`) must still require confirmation even when server claims readOnlyHint: true.
    assert_eq!(
        classify_mcp_tool(McpApproval::Prompt, Some(&read_only_ann)),
        ToolPermissionClass::McpPrompt
    );
    // Unannotated tool (absent annotations or empty annotations) under `Writes` defaults to `McpPrompt` (effectful).
    assert_eq!(
        classify_mcp_tool(McpApproval::Writes, None),
        ToolPermissionClass::McpPrompt
    );
    assert_eq!(
        classify_mcp_tool(McpApproval::Writes, Some(&McpToolAnnotations::default())),
        ToolPermissionClass::McpPrompt
    );
    // `Writes` + `readOnlyHint: true` + `destructiveHint: true` remains `McpPrompt`.
    let mut destructive_ann = McpToolAnnotations::default();
    destructive_ann.read_only = Some(true);
    destructive_ann.destructive = Some(true);
    assert_eq!(
        classify_mcp_tool(McpApproval::Writes, Some(&destructive_ann)),
        ToolPermissionClass::McpPrompt
    );
    // Only `Writes` + `readOnlyHint: true` without `destructiveHint: true` relaxes to `KnownReadOnly`.
    assert_eq!(
        classify_mcp_tool(McpApproval::Writes, Some(&read_only_ann)),
        ToolPermissionClass::KnownReadOnly
    );

    let adapter = McpToolAdapter::new(McpToolInfo {
        server_id: "srv".into(),
        raw_tool_name: "lookup".into(),
        category: classify_mcp_tool(McpApproval::Prompt, Some(&read_only_ann)),
    });
    assert_eq!(
        adapter.classify(&json!({"q": "x"})).class,
        ToolPermissionClass::McpPrompt
    );
    let call = causa::kernel::ToolCallContext::new(
        causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
        causa::kernel::ToolCallPayload::new("mcp_srv_lookup", json!({"q": "x"})),
    );
    assert_eq!(
        adapter.approval_preview(&call, &PreparedCall::empty()),
        Some(ApprovalPreview::McpCall {
            server: "srv".into(),
            tool: "lookup".into(),
            arguments: r#"{"q":"x"}"#.into(),
        })
    );
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tl06_run_command_decodes_and_checks_exactly_once_per_execution() {
    let root = tempfile::tempdir().unwrap();
    let replies = vec![
        Reply::Calls(vec![call_command(
            json!({"command": "printf ok > once.txt"}),
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
    let approver = tokio::spawn(async move {
        let req = rx.recv().await.expect("command approval");
        req.reply.send(ApprovalDecision::Allow).unwrap();
    });

    let work = submit(&session, "single decode and check");
    let obs = finish(&session, &work).await;
    approver.await.unwrap();

    assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Succeeded));
    assert_eq!(
        std::fs::read_to_string(root.path().join("once.txt")).unwrap(),
        "ok"
    );

    let ws_tools = session.workspace_tools().unwrap();
    assert_eq!(ws_tools.command_decode_count(), 1);
    assert_eq!(ws_tools.command().unwrap().check_count(), 1);

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tl07_t5_staging_clears_across_rejections_cancels_and_execution() {
    // 1. Mode refused (ReadOnly), 2. User declined (Ask + Deny),
    // 3. No confirmation route (Ask without receiver), 4. Mode switched after prepare,
    // 5. Allowed execution clears staged entries afterwards.
    for case in [
        "mode_refused",
        "user_declined",
        "no_route",
        "mode_switched",
        "allowed",
    ] {
        let root = tempfile::tempdir().unwrap();
        let replies = vec![call_write("t5.txt", "data"), Reply::Answer];
        let fixture = Fixture::new("openai-chat", replies, None);
        let host = SessionHost::new(None);
        let mut def = base_definition(&fixture.endpoint, root.path());
        def.permission_mode = Some(if case == "mode_refused" {
            PermissionMode::ReadOnly
        } else {
            PermissionMode::Ask
        });
        let session = host
            .create(PreparedCreate::transient(def, Context::default()))
            .await
            .unwrap();

        let mut _rx_keep = None;
        let _rx_guard = match case {
            "user_declined" => {
                let mut rx = session.connect_approvals();
                Some(tokio::spawn(async move {
                    let req = rx.recv().await.unwrap();
                    req.reply
                        .send(ApprovalDecision::Deny {
                            reason: Some("no".into()),
                        })
                        .unwrap();
                }))
            }
            "allowed" => {
                let mut rx = session.connect_approvals();
                Some(tokio::spawn(async move {
                    let req = rx.recv().await.unwrap();
                    req.reply.send(ApprovalDecision::Allow).unwrap();
                }))
            }
            "mode_switched" => {
                _rx_keep = Some(session.connect_approvals());
                let session_clone = session.clone();
                session.set_test_after_prepare_hook(Arc::new(move || {
                    session_clone
                        .set_permission_mode(PermissionMode::ReadOnly)
                        .unwrap();
                }));
                None
            }
            _ => None,
        };

        let work = submit(&session, case);
        let obs = finish(&session, &work).await;
        if let Some(handle) = _rx_guard {
            let _ = handle.await;
        }

        let ws_tools = session.workspace_tools().unwrap();
        if case == "allowed" {
            assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Succeeded));
            assert!(root.path().join("t5.txt").exists());
        } else {
            assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Rejected));
            assert!(!root.path().join("t5.txt").exists());
            assert_eq!(
                obs.tools.calls[0]
                    .permission_decision
                    .as_ref()
                    .unwrap()
                    .kind,
                match case {
                    "mode_refused" | "mode_switched" => PermissionDecisionKind::ModeRefused,
                    "user_declined" => PermissionDecisionKind::AskedAndDenied,
                    "no_route" => PermissionDecisionKind::NoConfirmationRoute,
                    _ => unreachable!(),
                },
                "case={case}"
            );
            assert_eq!(
                ws_tools.metrics().started,
                if matches!(case, "mode_switched" | "user_declined") {
                    1
                } else {
                    0
                },
                "case={case}"
            );
        }

        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }

    // 6. Cancel during approval wait on 1st call prevents 2nd call disk precheck and clears staging,
    //    AND approving 1st call then cancelling during 2nd call's approval wait rolls back 1st call's staged entry.
    for cancel_on_first in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let replies = vec![
            Reply::Calls(vec![
                ScriptedToolCall {
                    name: "write_file".into(),
                    arguments: json!({"path": "first.txt", "content": "1"}),
                },
                ScriptedToolCall {
                    name: "write_file".into(),
                    arguments: json!({"path": "second.txt", "content": "2"}),
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
        let target_entered = Arc::new(tokio::sync::Notify::new());
        let notify = target_entered.clone();
        let approval_task = tokio::spawn(async move {
            let req1 = rx.recv().await.expect("first approval");
            if cancel_on_first {
                notify.notify_one();
                tokio::time::sleep(Duration::from_secs(5)).await;
            } else {
                req1.reply.send(ApprovalDecision::Allow).unwrap();
                let _req2 = rx.recv().await.expect("second approval");
                notify.notify_one();
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });

        let work = submit(&session, "cancel during batch");
        target_entered.notified().await;
        session.cancel(&work).unwrap();
        let _obs = session.wait(&work, Duration::from_secs(5)).await.unwrap();
        approval_task.abort();
        session.wait_saved().await.unwrap();

        let ws_tools = session.workspace_tools().unwrap();
        assert!(!root.path().join("first.txt").exists());
        assert!(!root.path().join("second.txt").exists());
        if cancel_on_first {
            // Only the 1st write performed disk prepare; the 2nd never ran prepare.
            assert_eq!(ws_tools.metrics().started, 1);
        } else {
            assert_eq!(ws_tools.metrics().started, 2);
        }

        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tl07_t5_rule_denied_command_leaves_staging_empty_and_skips_prepare() {
    use crate::permission::rules::{CommandRule, CommandRuleSet, RuleDecision};

    let root = tempfile::tempdir().unwrap();
    let replies = vec![
        Reply::Calls(vec![call_command(json!({"command": "echo forbidden"}))]),
        Reply::Answer,
    ];
    let fixture = Fixture::new("openai-chat", replies, None);
    let host = SessionHost::new(None);
    let mut def = command_definition(&fixture.endpoint, root.path());
    def.permission_mode = Some(PermissionMode::FullAccess);
    let session = host
        .create(PreparedCreate::transient(def, Context::default()))
        .await
        .unwrap();
    session.set_command_rules(CommandRuleSet::with_layers(
        vec![CommandRule {
            prefix: vec!["echo".into()],
            decision: RuleDecision::Deny,
            outside_sandbox: false,
        }],
        vec![],
        vec![],
    ));

    let work = submit(&session, "rule denied");
    let obs = finish(&session, &work).await;
    assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Rejected));
    assert_eq!(
        obs.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::RuleDenied
    );
    let ws_tools = session.workspace_tools().unwrap();
    assert_eq!(ws_tools.command_decode_count(), 0);
    assert_eq!(ws_tools.command().unwrap().check_count(), 0);

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tl08_allow_for_session_applies_to_subsequent_calls_in_same_batch() {
    use crate::permission::rules::CommandRuleSet;

    let root = tempfile::tempdir().unwrap();
    let replies = vec![
        Reply::Calls(vec![
            call_command(json!({"command": "echo first a"})),
            call_command(json!({"command": "echo first b"})),
        ]),
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
    session.set_command_rules(CommandRuleSet::new());

    let mut rx = session.connect_approvals();
    let approver = tokio::spawn(async move {
        let req = rx.recv().await.expect("first approval");
        req.reply.send(ApprovalDecision::AllowForSession).unwrap();
        let second = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(
            second.is_err(),
            "AllowForSession on call 1 must immediately allow matching call 2 in same batch"
        );
    });

    let work = submit(&session, "same batch session rule");
    let obs = finish(&session, &work).await;
    approver.await.unwrap();

    assert_eq!(obs.tools.calls.len(), 2);
    assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Succeeded));
    assert_eq!(obs.tools.calls[1].status, Some(ToolResultStatus::Succeeded));
    assert_eq!(
        obs.tools.calls[0]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::AskedAndAllowed
    );
    assert_eq!(
        obs.tools.calls[1]
            .permission_decision
            .as_ref()
            .unwrap()
            .kind,
        PermissionDecisionKind::RuleAllowed
    );

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tl10_call_attachments_clear_on_cancel_and_reject_in_batch() {
    use crate::definition::WriteToolLimits;
    use crate::permission::{PermissionHub, ToolPolicyProcessor};
    use crate::tools::{ToolResources, WorkspaceTools};
    use crate::workspace::Workspace;
    use causa::kernel::{
        BlockId, CallControl, InvocationId, ProcessorContext, RoundId, ToolBatch, ToolCallContext,
        ToolCallPayload, ToolDescription, TurnId,
    };
    use tokio_util::sync::CancellationToken;

    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(&root.path().canonicalize().unwrap()).unwrap();
    let ws_tools = WorkspaceTools::with_write_limits(
        workspace,
        WorkspaceToolLimits::default(),
        WriteToolLimits::default(),
        ToolResources::new(4),
    );

    let turn_id = TurnId::new(uuid::Uuid::now_v7().to_string());
    let work = WorkRef {
        conversation_id: "tl10".into(),
        turn_id: turn_id.clone(),
    };
    let inv = InvocationId {
        turn_id: turn_id.clone(),
        round_id: RoundId(0),
    };
    let call1 = ToolCallContext::new(
        BlockId::new(uuid::Uuid::now_v7()),
        ToolCallPayload::new("write_file", json!({"path": "one.txt", "content": "1"})),
    );
    let call2 = ToolCallContext::new(
        BlockId::new(uuid::Uuid::now_v7()),
        ToolCallPayload::new("write_file", json!({"path": "two.txt", "content": "2"})),
    );
    let decl = [call1.call_block_id, call2.call_block_id];
    let reporter =
        crate::session::ToolReporter::new(work.clone(), tokio::sync::broadcast::channel(16).0);
    let mut batch = ToolBatch::new(vec![call1.clone(), call2.clone()]).unwrap();
    reporter.batch_ready(&inv, &batch);

    let tool = ws_tools
        .bind_for_test(&[WorkspaceTool::WriteFile], reporter.clone())
        .pop()
        .unwrap();
    let bound_desc = [ToolDescription::new(tool.definition()).with_metadata(tool.metadata())];

    let perm_hub = Arc::new(PermissionHub::new(PermissionMode::Ask));
    let mut rx = perm_hub.connect_approvals();
    let cancel_token = CancellationToken::new();
    let cancel_for_task = cancel_token.clone();
    let approver = tokio::spawn(async move {
        let req1 = rx.recv().await.expect("first approval");
        req1.reply.send(ApprovalDecision::Allow).unwrap();
        let _req2 = rx.recv().await.expect("second approval");
        cancel_for_task.cancel();
    });

    let control = CallControl::new(cancel_token, None);
    let ctx = ProcessorContext::new(&turn_id, RoundId(0), &decl, &control).with_tools(&bound_desc);
    ToolPolicyProcessor::new(work, reporter, perm_hub)
        .process(&mut batch, &ctx)
        .await
        .unwrap();
    approver.await.unwrap();

    // Approving call 1 attached PreparedWrite, then cancelling while waiting on call 2
    // must clear attachments on every pending entry so nothing leaks in the batch.
    assert_eq!(batch.calls().len(), 2);
    for entry in batch.calls() {
        assert!(
            entry.call().attachments().is_empty(),
            "cancelled batch must leave zero pending call attachments"
        );
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn find_files_executes_without_approval_across_all_modes_and_chains_with_read_file() {
    for mode in [
        PermissionMode::ReadOnly,
        PermissionMode::Ask,
        PermissionMode::AcceptEdits,
        PermissionMode::FullAccess,
    ] {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(
            root.path().join("src/target.rs"),
            "pub const MARKER: &str = \"find_then_read_ok\";\n",
        )
        .unwrap();

        let replies = vec![
            Reply::Call(
                "find_files".into(),
                json!({ "path": ".", "pattern": "*.rs" }),
            ),
            Reply::Call("read_file".into(), json!({ "path": "src/target.rs" })),
            Reply::Answer,
        ];
        let fixture = Fixture::new("openai-chat", replies, None);
        let host = SessionHost::new(None);
        let mut def = base_definition(&fixture.endpoint, root.path());
        def.tools = vec![WorkspaceTool::FindFiles, WorkspaceTool::ReadFile];
        let session = host
            .create(PreparedCreate::transient(def, Context::default()))
            .await
            .unwrap();
        session.set_permission_mode(mode).unwrap();

        let work = submit(&session, "find and read");
        let obs = finish(&session, &work).await;
        assert_eq!(obs.tools.calls.len(), 2);
        assert_eq!(obs.tools.calls[0].tool_name, "find_files");
        assert_eq!(obs.tools.calls[0].status, Some(ToolResultStatus::Succeeded));
        assert_eq!(obs.tools.calls[1].tool_name, "read_file");
        assert_eq!(obs.tools.calls[1].status, Some(ToolResultStatus::Succeeded));

        let context = session.read_materials().unwrap().context;
        let find_res = context
            .blocks()
            .iter()
            .find_map(|b| match b.content() {
                BlockContent::ToolResult(p)
                    if p.call_block_id == obs.tools.calls[0].call_block_id =>
                {
                    Some(&p.output.content)
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(find_res["files"], json!(["src/target.rs"]));

        let read_res = context
            .blocks()
            .iter()
            .find_map(|b| match b.content() {
                BlockContent::ToolResult(p)
                    if p.call_block_id == obs.tools.calls[1].call_block_id =>
                {
                    Some(&p.output.content)
                }
                _ => None,
            })
            .unwrap();
        assert!(
            read_res["text"]
                .as_str()
                .unwrap()
                .contains("find_then_read_ok")
        );

        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }
}

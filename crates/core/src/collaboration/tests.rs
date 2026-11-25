use super::*;
use crate::application::{ClosePolicy, PreparedCreate, SessionHost, text_input};
use crate::definition::{
    CodeExecutionSettings, CredentialRef, Provider, SessionDefinition, WorkspaceTool,
    WorkspaceToolLimits,
};
use crate::events::OperationStatus;
use crate::permission::PermissionMode;
use crate::policy::ToolPermissionClass;
use crate::session::{SubmitRequest, WorkState};
use crate::tools::ManagedTool;
use causa::kernel::{
    BlockContent, CallControl, CancellationToken, ModelGateway, ModelInvokeError, ModelItem,
    ModelOutput, ModelRequest, ModelResponse, ModelStopReason, ToolCallContext, ToolCallDraft,
    ToolCallPayload, ToolResultStatus, current_tools,
};
use serde_json::json;
use std::collections::VecDeque;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

#[test]
fn only_four_collaboration_tools_exist_and_schemas_forbid_unknown_fields() {
    let defs = collaboration_tool_definitions();
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            SPAWN_AGENT_TOOL_NAME,
            MESSAGE_AGENT_TOOL_NAME,
            READ_AGENT_TOOL_NAME,
            CANCEL_AGENT_TOOL_NAME,
        ]
    );
    assert!(!names.contains(&"wait_agent"));
    assert!(!names.contains(&"agent_catalog"));
    assert!(!names.contains(&"agent_list"));

    for def in &defs {
        assert_eq!(
            def.parameters["additionalProperties"],
            json!(false),
            "{} must forbid unknown fields",
            def.name
        );
    }
    assert_eq!(
        defs[0].parameters["properties"]["model"]["additionalProperties"],
        json!(false)
    );
}

#[test]
fn collaboration_tools_are_marked_read_only_managed_and_reject_spoofed_fields() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let host = SessionHost::new(None);
        let tools = collaboration_tools(CollaborationContext::for_entry(
            std::sync::Weak::new(),
            "entry-id".into(),
        ));
        assert_eq!(tools.len(), 4);

        for tool in &tools {
            let meta = tool.metadata();
            let managed = meta
                .get::<Arc<dyn ManagedTool>>()
                .expect("managed tool metadata");
            assert_eq!(
                managed.classify(&json!({})).class,
                ToolPermissionClass::KnownReadOnly
            );
        }

        let control = CallControl::new(CancellationToken::new(), None);
        let dummy_ctx = |name: &str, args: serde_json::Value| {
            ToolCallContext::new(
                causa::runtime::new_block_id(),
                ToolCallPayload::new(name, args),
            )
        };

        // Unknown / spoofed caller fields rejected before touching coordinator
        let bad_spawn = tools[0]
            .execute(
                &dummy_ctx(
                    SPAWN_AGENT_TOOL_NAME,
                    json!({
                        "agent": "explore",
                        "prompt": "hi",
                        "caller_agent_id": "spoof",
                    }),
                ),
                &control,
                None,
            )
            .await;
        assert_eq!(bad_spawn.status, ToolResultStatus::Rejected);

        let bad_msg = tools[1]
            .execute(
                &dummy_ctx(
                    MESSAGE_AGENT_TOOL_NAME,
                    json!({
                        "agent_id": "a1",
                        "prompt": "hi",
                        "caller_operation_id": "spoof",
                    }),
                ),
                &control,
                None,
            )
            .await;
        assert_eq!(bad_msg.status, ToolResultStatus::Rejected);

        let bad_read_limit = tools[2]
            .execute(
                &dummy_ctx(
                    READ_AGENT_TOOL_NAME,
                    json!({
                        "operation_id": "op1",
                        "limit_bytes": 0,
                    }),
                ),
                &control,
                None,
            )
            .await;
        assert_eq!(bad_read_limit.status, ToolResultStatus::Rejected);

        let bad_cancel = tools[3]
            .execute(
                &dummy_ctx(
                    CANCEL_AGENT_TOOL_NAME,
                    json!({
                        "operation_id": "op1",
                        "source_block_id": "spoof",
                    }),
                ),
                &control,
                None,
            )
            .await;
        assert_eq!(bad_cancel.status, ToolResultStatus::Rejected);

        drop(host);
    });
}

fn ws_definition(ws_root: &Path, tools: Vec<WorkspaceTool>, ce: bool) -> SessionDefinition {
    SessionDefinition {
        provider: Provider::OpenAiChat,
        model: "parent-model".into(),
        endpoint: None,
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(30),
        retained_work_capacity: 16,
        max_model_rounds: 6,
        max_tool_calls: 16,
        max_tokens: None,
        reasoning_effort: None,
        temperature: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: Some(ws_root.canonicalize().unwrap()),
        tools,
        tool_limits: WorkspaceToolLimits::default(),
        write_limits: None,
        permission_mode: Some(PermissionMode::Ask),
        command: None,
        mcp_servers: Vec::new(),
        context_window: None,
        auto_compact: true,
        compact_threshold: 70,
        prompt_cache: true,
        prelude: Default::default(),
        code_execution: ce.then(CodeExecutionSettings::default),
    }
}

struct SeenChildRequest {
    model: String,
    tools: Vec<String>,
    texts: Vec<String>,
}

struct ScriptedAgentGateway {
    main_steps: Mutex<VecDeque<ModelOutput>>,
    seen_main_requests: Arc<Mutex<Vec<Vec<String>>>>,
    seen_child_requests: Arc<Mutex<Vec<SeenChildRequest>>>,
    pause_child: Option<(Arc<Notify>, Arc<Notify>)>,
}

impl ScriptedAgentGateway {
    fn new() -> Self {
        Self {
            main_steps: Mutex::new(VecDeque::new()),
            seen_main_requests: Arc::new(Mutex::new(Vec::new())),
            seen_child_requests: Arc::new(Mutex::new(Vec::new())),
            pause_child: None,
        }
    }

    fn push_main_tool(&self, name: &str, args: serde_json::Value) {
        self.main_steps.lock().unwrap().push_back(ModelOutput::new(
            ModelResponse {
                items: vec![ModelItem::tool_call(ToolCallDraft::new(name, args))],
            },
            ModelStopReason::ToolUse,
        ));
    }

    fn push_main_answer(&self, text: &str) {
        self.main_steps.lock().unwrap().push_back(ModelOutput::new(
            ModelResponse {
                items: vec![ModelItem::text(text)],
            },
            ModelStopReason::EndTurn,
        ));
    }
}

#[async_trait::async_trait]
impl ModelGateway for ScriptedAgentGateway {
    async fn invoke(
        &self,
        request: &ModelRequest,
        control: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        let texts: Vec<String> = request
            .frame
            .blocks
            .iter()
            .filter_map(|b| match b.content() {
                BlockContent::Message { parts, .. } => Some(
                    parts
                        .iter()
                        .filter_map(|p| match p {
                            causa::kernel::ContentPart::Text(t) => Some(t.0.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                ),
                BlockContent::ToolResult(res) => Some(format!(
                    "TOOL_RESULT({:?}):{}",
                    res.status, res.output.content
                )),
                _ => None,
            })
            .collect();

        let is_main = texts
            .iter()
            .any(|t| t.contains("You are running inside the TARS agent host."));
        if is_main {
            self.seen_main_requests.lock().unwrap().push(texts);
            let output = self
                .main_steps
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    ModelOutput::new(
                        ModelResponse {
                            items: vec![ModelItem::text("done")],
                        },
                        ModelStopReason::EndTurn,
                    )
                });
            Ok(output)
        } else {
            let tool_names: Vec<String> = current_tools(&request.frame.blocks)
                .into_iter()
                .map(|t| t.name)
                .collect();
            self.seen_child_requests
                .lock()
                .unwrap()
                .push(SeenChildRequest {
                    model: request.model.0.as_str().to_owned(),
                    tools: tool_names,
                    texts,
                });
            if let Some((entered, release)) = &self.pause_child {
                entered.notify_one();
                tokio::select! {
                    _ = release.notified() => {}
                    _ = control.cancellation_token().cancelled() => {
                        return Err(ModelInvokeError::new(
                            causa::kernel::ModelInvokeErrorKind::Cancelled,
                            "cancelled",
                        ));
                    }
                }
            }
            Ok(ModelOutput::new(
                ModelResponse {
                    items: vec![ModelItem::text("child completed")],
                },
                ModelStopReason::EndTurn,
            ))
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn workspace_agents_trust_gate_toctou_and_frozen_preset_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let ws_agents = ws.join(".tars/agents");
    let store = dir.path().join("store");
    fs::create_dir_all(&ws_agents).unwrap();

    fs::write(
        ws_agents.join("custom.md"),
        concat!(
            "---\n",
            "name: custom\n",
            "description: Initial trusted custom agent\n",
            "tools: [read_file]\n",
            "default_interaction: reusable\n",
            "---\n",
            "Initial frozen role prompt\n",
        ),
    )
    .unwrap();

    let gw = Arc::new(ScriptedAgentGateway::new());
    // Turn 1 (untrusted): try spawning `custom` -> rejected by host, and `<available_agents>` omits `custom`!
    gw.push_main_tool(
        "spawn_agent",
        json!({ "agent": "custom", "prompt": "do work" }),
    );
    gw.push_main_answer("blocked untrusted");
    // Turn 2 (after trust_workspace_agents): `custom` succeeds using open-time snapshot!
    gw.push_main_tool(
        "spawn_agent",
        json!({ "agent": "custom", "prompt": "run trusted" }),
    );
    gw.push_main_answer("ran trusted");

    let host = SessionHost::new(Some(store.clone()));
    host.set_test_gateway(Some(gw.clone()));

    let def = ws_definition(
        &ws,
        vec![WorkspaceTool::ReadFile, WorkspaceTool::WriteFile],
        false,
    );
    let session = host
        .create(PreparedCreate::prepare_persistent(def, None).unwrap())
        .await
        .unwrap();
    let conv_id = session.target().conversation_id().clone();

    assert!(!session.workspace_agents_trusted());
    assert_eq!(
        session.open_report().workspace_agents.untrusted_agents,
        vec!["custom".to_owned()]
    );
    assert!(session.agent_registry_snapshot().get("custom").is_none());

    let w1 = session
        .submit(SubmitRequest {
            request_key: "t1".into(),
            blocks: vec![text_input("try untrusted")],
        })
        .unwrap()
        .work;
    let obs1 = session.wait(&w1, Duration::from_secs(5)).await.unwrap();
    assert!(matches!(obs1.state, WorkState::Finished(_)));
    session.wait_saved().await.unwrap();

    // Turn 1 saw `<available_agents>` WITHOUT `custom` and received Rejected tool result.
    {
        let reqs = gw.seen_main_requests.lock().unwrap();
        let req0_joined = reqs[0].join("\n");
        assert!(req0_joined.contains("<available_agents>"));
        assert!(!req0_joined.contains("<name>custom</name>"));
        let req1_joined = reqs[1].join("\n");
        assert!(req1_joined.contains("TOOL_RESULT(Rejected)"));
    }

    // TOCTOU attack: mutate `.tars/agents/custom.md` on disk AFTER open to escalate tools & change role prompt.
    fs::write(
        ws_agents.join("custom.md"),
        concat!(
            "---\n",
            "name: custom\n",
            "description: Tampered post-open agent\n",
            "tools: [read_file, write_file]\n",
            "---\n",
            "Tampered role prompt\n",
        ),
    )
    .unwrap();

    // Trusting activates ONLY the open-time snapshot, NOT the tampered disk file!
    assert_eq!(session.trust_workspace_agents().await.unwrap(), 1);
    assert!(session.workspace_agents_trusted());
    let active_custom = session
        .agent_registry_snapshot()
        .get("custom")
        .unwrap()
        .clone();
    assert_eq!(active_custom.description, "Initial trusted custom agent");
    assert_eq!(
        active_custom.role_prompt.as_deref(),
        Some("Initial frozen role prompt")
    );
    assert_eq!(
        active_custom.tools,
        Some(vec![crate::definition::BuiltinTool::Workspace(
            WorkspaceTool::ReadFile
        )])
    );

    // Turn 2: `<available_agents updated="true">` emits `custom`, and child gets ONLY `read_file` & initial role prompt!
    let w2 = session
        .submit(SubmitRequest {
            request_key: "t2".into(),
            blocks: vec![text_input("run trusted")],
        })
        .unwrap()
        .work;
    let obs2 = session.wait(&w2, Duration::from_secs(5)).await.unwrap();
    assert!(matches!(obs2.state, WorkState::Finished(_)));
    session.wait_saved().await.unwrap();

    {
        let child_reqs = gw.seen_child_requests.lock().unwrap();
        assert_eq!(child_reqs.len(), 1);
        let child_req = &child_reqs[0];
        let _ = &child_req.model;
        assert!(child_req.tools.contains(&"read_file".to_owned()));
        assert!(!child_req.tools.contains(&"write_file".to_owned()));
        assert!(
            child_req
                .texts
                .iter()
                .any(|t| t.contains("Initial frozen role prompt"))
        );
        assert!(
            !child_req
                .texts
                .iter()
                .any(|t| t.contains("Tampered role prompt"))
        );
    }

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
    drop(session);

    // Reopen detects that disk `.tars/agents/custom.md` changed vs trusted manifest -> untrusted again!
    let host2 = SessionHost::new(Some(store));
    host2.set_test_gateway(Some(gw));
    let reopened = host2.open(conv_id).await.unwrap();
    assert!(!reopened.workspace_agents_trusted());
    assert_eq!(
        reopened.open_report().workspace_agents.untrusted_agents,
        vec!["custom".to_owned()]
    );
    host2
        .close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn explore_narrows_subset_and_custom_escalations_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let cfg_agents = dir.path().join("cfg/agents");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&cfg_agents).unwrap();

    // Custom agent requesting write_file (parent only has read_file) and another requesting run_code (parent has ce=false)
    fs::write(
        cfg_agents.join("writer.md"),
        "---\nname: writer\ndescription: wants write\ntools: [write_file]\n---\n",
    )
    .unwrap();
    fs::write(
        cfg_agents.join("coder.md"),
        "---\nname: coder\ndescription: wants run_code\ntools: [read_file, run_code]\n---\n",
    )
    .unwrap();

    let gw = Arc::new(ScriptedAgentGateway::new());
    gw.push_main_tool(
        "spawn_agent",
        json!({ "agent": "explore", "prompt": "inspect" }),
    );
    gw.push_main_tool(
        "spawn_agent",
        json!({ "agent": "writer", "prompt": "write" }),
    );
    gw.push_main_tool("spawn_agent", json!({ "agent": "coder", "prompt": "code" }));
    gw.push_main_answer("finished checks");

    let host = SessionHost::new(None);
    host.set_test_gateway(Some(gw.clone()));

    let mut def = ws_definition(&ws, vec![WorkspaceTool::ReadFile], false);
    def.prelude.agent_dirs = vec![cfg_agents.canonicalize().unwrap()];

    let session = host
        .create(PreparedCreate::prepare_transient(def, None).unwrap())
        .await
        .unwrap();
    let work = session
        .submit(SubmitRequest {
            request_key: "t1".into(),
            blocks: vec![text_input("check")],
        })
        .unwrap()
        .work;
    let obs = session.wait(&work, Duration::from_secs(5)).await.unwrap();
    assert!(matches!(obs.state, WorkState::Finished(_)));

    let main_reqs = gw.seen_main_requests.lock().unwrap();
    // Request 1 has explore's Succeeded ToolResult; Requests 2 and 3 have Rejected for writer and coder!
    assert!(
        main_reqs[1]
            .iter()
            .any(|t| t.contains("TOOL_RESULT(Succeeded)"))
    );
    assert!(
        main_reqs[2]
            .iter()
            .any(|t| t.contains("TOOL_RESULT(Rejected)"))
    );
    assert!(
        main_reqs[3]
            .iter()
            .any(|t| t.contains("TOOL_RESULT(Rejected)"))
    );

    let child_reqs = gw.seen_child_requests.lock().unwrap();
    assert_eq!(child_reqs.len(), 1);
    assert!(child_reqs[0].tools.contains(&"read_file".to_owned()));
    assert!(!child_reqs[0].tools.contains(&"find_files".to_owned()));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn explore_inherits_find_files_when_parent_enables_it() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();

    let gw = Arc::new(ScriptedAgentGateway::new());
    gw.push_main_tool(
        "spawn_agent",
        json!({ "agent": "explore", "prompt": "discover files" }),
    );
    gw.push_main_answer("done");

    let host = SessionHost::new(None);
    host.set_test_gateway(Some(gw.clone()));

    let def = ws_definition(
        &ws,
        vec![
            WorkspaceTool::ReadFile,
            WorkspaceTool::FindFiles,
            WorkspaceTool::WriteFile,
        ],
        false,
    );
    let session = host
        .create(PreparedCreate::prepare_transient(def, None).unwrap())
        .await
        .unwrap();
    let work = session
        .submit(SubmitRequest {
            request_key: "t1".into(),
            blocks: vec![text_input("explore")],
        })
        .unwrap()
        .work;
    let obs = session.wait(&work, Duration::from_secs(5)).await.unwrap();
    assert!(matches!(obs.state, WorkState::Finished(_)));

    let child_reqs = gw.seen_child_requests.lock().unwrap();
    assert_eq!(child_reqs.len(), 1);
    assert!(child_reqs[0].tools.contains(&"read_file".to_owned()));
    assert!(child_reqs[0].tools.contains(&"find_files".to_owned()));
    assert!(!child_reqs[0].tools.contains(&"write_file".to_owned()));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn sync_spawn_cancellation_propagates_to_child_and_leaves_no_zombie() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir_all(&ws).unwrap();

    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut gw = ScriptedAgentGateway::new();
    gw.pause_child = Some((entered.clone(), release.clone()));
    gw.push_main_tool(
        "spawn_agent",
        json!({ "agent": "general", "prompt": "slow work" }),
    );
    let gw = Arc::new(gw);

    let host = SessionHost::new(None);
    host.set_test_gateway(Some(gw));
    let def = ws_definition(&ws, vec![WorkspaceTool::ReadFile], false);
    let session = host
        .create(PreparedCreate::prepare_transient(def, None).unwrap())
        .await
        .unwrap();

    let work = session
        .submit(SubmitRequest {
            request_key: "t-cancel".into(),
            blocks: vec![text_input("spawn and cancel")],
        })
        .unwrap()
        .work;

    entered.notified().await;
    session.cancel(&work).unwrap();
    let obs = session.wait(&work, Duration::from_secs(5)).await.unwrap();
    assert!(!matches!(obs.state, WorkState::Running));

    // Verify child agent exists and has zero active operation (settled as Cancelled).
    let agents = session.list_agents();
    assert_eq!(agents.len(), 2);
    let child = agents.iter().find(|a| a.name == "general").unwrap();
    assert_eq!(child.active_operation_id, None);
    let _ = OperationStatus::Cancelled;
}

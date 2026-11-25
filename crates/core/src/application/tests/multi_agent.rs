use super::support::*;
use crate::agent::{AgentMaterialVersion, AgentRepairPlan};
use crate::application::{
    CoordinatorConfig, CoordinatorError, MessageAgentRequest, RepairMode, SpawnAgentOutcome,
    SpawnAgentRequest,
};
use crate::definition::{ChildNarrowingRequest, WorkspaceTool};
use crate::events::{
    AgentInteraction, EventPriority, EventSourceKind, MAX_READ_AGENT_PAGE_BYTES, NewInboxEvent,
    OnParentComplete, OperationExecutionMode, OperationStatus, SessionEvent,
    TASK_NOTIFICATION_SOURCE, WakePolicy,
};
use crate::permission::{ApprovalDecision, ApprovalSubject};
use crate::policy::PermissionMode;
use crate::store::{AdmitOperationRequest, NewAgentRow, OutboxCommitRequest};
use causa::kernel::{
    BlockContent, BlockMeta, CallControl, ModelGateway, ModelInvokeError, ModelInvokeErrorKind,
    ModelItem, ModelOutput, ModelRequest, ModelResponse, ModelStopReason, ToolCallDraft,
    ToolCallPayload,
};
use causa::runtime::new_block_id;
use std::collections::VecDeque;
use tokio::sync::Barrier;

struct ScriptedStep {
    entered: Option<Arc<Notify>>,
    release: Option<Arc<Notify>>,
    response_text: Option<String>,
    tool_calls: Vec<ToolCallDraft>,
    capture_blocks: Option<Arc<Mutex<Vec<ContextBlock>>>>,
    wait_for_cancel: bool,
}

impl ScriptedStep {
    fn text(text: impl Into<String>) -> Self {
        Self {
            entered: None,
            release: None,
            response_text: Some(text.into()),
            tool_calls: Vec::new(),
            capture_blocks: None,
            wait_for_cancel: false,
        }
    }

    fn tool(name: impl Into<String>, args: serde_json::Value) -> Self {
        Self {
            entered: None,
            release: None,
            response_text: None,
            tool_calls: vec![ToolCallDraft::new(name, args)],
            capture_blocks: None,
            wait_for_cancel: false,
        }
    }
}

struct ScriptedGateway {
    calls: Arc<AtomicUsize>,
    steps: Mutex<VecDeque<ScriptedStep>>,
    default_text: String,
    captured_requests: Arc<Mutex<Vec<Vec<ContextBlock>>>>,
}

impl ScriptedGateway {
    fn new(steps: Vec<ScriptedStep>, default_text: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            calls: Arc::new(AtomicUsize::new(0)),
            steps: Mutex::new(steps.into()),
            default_text: default_text.into(),
            captured_requests: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn simple(text: impl Into<String>) -> Arc<Self> {
        Self::new(Vec::new(), text)
    }
}

#[async_trait::async_trait]
impl ModelGateway for ScriptedGateway {
    async fn invoke(
        &self,
        request: &ModelRequest,
        control: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let blocks = request.frame.blocks.clone();
        self.captured_requests.lock().unwrap().push(blocks.clone());
        let step = self.steps.lock().unwrap().pop_front();
        if let Some(step) = step {
            if let Some(capture) = &step.capture_blocks {
                *capture.lock().unwrap() = blocks;
            }
            if let Some(entered) = &step.entered {
                entered.notify_one();
            }
            if step.wait_for_cancel {
                control.cancellation_token().cancelled().await;
                return Err(ModelInvokeError::new(
                    ModelInvokeErrorKind::Cancelled,
                    "cancelled",
                ));
            }
            if let Some(release) = &step.release {
                tokio::select! {
                    _ = release.notified() => {}
                    _ = control.cancellation_token().cancelled() => {
                        return Err(ModelInvokeError::new(
                            ModelInvokeErrorKind::Cancelled,
                            "cancelled",
                        ));
                    }
                }
            }
            if !step.tool_calls.is_empty() {
                return Ok(ModelOutput::new(
                    ModelResponse {
                        items: step
                            .tool_calls
                            .into_iter()
                            .map(ModelItem::tool_call)
                            .collect(),
                    },
                    ModelStopReason::ToolUse,
                ));
            }
            return Ok(ModelOutput::new(
                ModelResponse {
                    items: vec![ModelItem::text(
                        step.response_text
                            .unwrap_or_else(|| self.default_text.clone()),
                    )],
                },
                ModelStopReason::EndTurn,
            ));
        }
        Ok(ModelOutput::new(
            ModelResponse {
                items: vec![ModelItem::text(self.default_text.clone())],
            },
            ModelStopReason::EndTurn,
        ))
    }
}

struct BarrierGateway {
    barrier: Arc<Barrier>,
    entered: Arc<Notify>,
    release: Option<Arc<Notify>>,
    text: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ModelGateway for BarrierGateway {
    async fn invoke(
        &self,
        _: &ModelRequest,
        control: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        tokio::select! {
            _ = self.barrier.wait() => {}
            _ = control.cancellation_token().cancelled() => {
                return Err(ModelInvokeError::new(
                    ModelInvokeErrorKind::Cancelled,
                    "cancelled",
                ));
            }
        }
        if let Some(release) = &self.release {
            tokio::select! {
                _ = release.notified() => {}
                _ = control.cancellation_token().cancelled() => {
                    return Err(ModelInvokeError::new(
                        ModelInvokeErrorKind::Cancelled,
                        "cancelled",
                    ));
                }
            }
        }
        Ok(ModelOutput::new(
            ModelResponse {
                items: vec![ModelItem::text(self.text.clone())],
            },
            ModelStopReason::EndTurn,
        ))
    }
}

fn spawn_req(caller_agent_id: &str, name: &str, prompt: &str) -> SpawnAgentRequest {
    SpawnAgentRequest {
        caller_agent_id: caller_agent_id.to_owned(),
        caller_operation_id: None,
        source_block_id: None,
        name: name.to_owned(),
        preset_name: Some("general".to_owned()),
        role_prompt: Some(format!("role for {name}")),
        prompt: prompt.to_owned(),
        narrowing: ChildNarrowingRequest::default(),
        interaction: AgentInteraction::SingleWork,
        execution_mode: OperationExecutionMode::Background,
        on_parent_complete: OnParentComplete::Continue,
        timeout_secs: None,
        max_rounds: None,
        coalesce_key: None,
    }
}

fn msg_req(caller_agent_id: &str, target_agent_id: &str, prompt: &str) -> MessageAgentRequest {
    MessageAgentRequest {
        caller_agent_id: caller_agent_id.to_owned(),
        caller_operation_id: None,
        source_block_id: None,
        target_agent_id: target_agent_id.to_owned(),
        prompt: prompt.to_owned(),
        execution_mode: OperationExecutionMode::Sync,
        on_parent_complete: OnParentComplete::Continue,
        timeout_secs: None,
        max_rounds: None,
        coalesce_key: None,
    }
}

fn entry_id(session: &ManagedSession) -> String {
    session.target().conversation_id().as_str().to_owned()
}

fn extract_block_text(block: &ContextBlock) -> String {
    match block.content() {
        BlockContent::Message { parts, .. } => parts
            .iter()
            .filter_map(|p| match p {
                causa::kernel::ContentPart::Text(t) => Some(t.0.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// 1. Concurrent multi-agent execution (AC11)
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_agents_b_and_c_run_in_parallel_and_b_commits_while_c_runs() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let barrier = Arc::new(Barrier::new(2));
    let c_release = Arc::new(Notify::new());
    let b_entered = Arc::new(Notify::new());
    let c_entered = Arc::new(Notify::new());

    let main_gw = ScriptedGateway::simple("main ok");
    let b_gw = Arc::new(BarrierGateway {
        barrier: barrier.clone(),
        entered: b_entered.clone(),
        release: None,
        text: "B result".into(),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let c_gw = Arc::new(BarrierGateway {
        barrier: barrier.clone(),
        entered: c_entered.clone(),
        release: Some(c_release.clone()),
        text: "C result".into(),
        calls: Arc::new(AtomicUsize::new(0)),
    });

    let b_gw_clone = b_gw.clone();
    let c_gw_clone = c_gw.clone();
    host.set_test_gateway(Some(main_gw));
    host.set_test_agent_gateway(Some(Arc::new(move |name, _| match name {
        "agent-b" => Some(b_gw_clone.clone()),
        "agent-c" => Some(c_gw_clone.clone()),
        _ => None,
    })));

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let root_id = entry_id(&session);

    let SpawnAgentOutcome::Background(b_rec) = session
        .spawn_agent(spawn_req(&root_id, "agent-b", "work b"))
        .await
        .unwrap()
    else {
        panic!("expected background receipt");
    };
    let SpawnAgentOutcome::Background(c_rec) = session
        .spawn_agent(spawn_req(&root_id, "agent-c", "work c"))
        .await
        .unwrap()
    else {
        panic!("expected background receipt");
    };

    // Both B and C reach the shared Barrier simultaneously -> true parallel execution!
    tokio::time::timeout(Duration::from_secs(5), b_entered.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), c_entered.notified())
        .await
        .unwrap();

    // B passes the barrier and commits while C is still held on c_release.
    until(|| {
        session
            .agent_snapshot(&b_rec.agent_id)
            .is_some_and(|s| s.active_operation_id.is_none() && s.material_revision >= 1)
    })
    .await;

    let b_read = session
        .read_agent(&b_rec.operation_id, 0, 1024)
        .await
        .unwrap();
    assert_eq!(b_read.status, OperationStatus::Completed);
    assert_eq!(b_read.page.content, "B result");

    // C is still running!
    let c_snap = session.agent_snapshot(&c_rec.agent_id).unwrap();
    assert_eq!(
        c_snap.active_operation_id.as_deref(),
        Some(c_rec.operation_id.as_str())
    );
    assert_eq!(session.active_finite_background_tasks(), 1);

    // Release C and wait for all background tasks to settle.
    c_release.notify_one();
    session.wait_background_tasks_settled().await;

    let c_read = session
        .read_agent(&c_rec.operation_id, 0, 1024)
        .await
        .unwrap();
    assert_eq!(c_read.status, OperationStatus::Completed);
    assert_eq!(c_read.page.content, "C result");

    // Wait for any triggered idle wakeup on root to finish before closing.
    until(|| session.status().active_work.is_none()).await;
    session.wait_saved().await.unwrap();

    let agents = session.list_agents();
    assert_eq!(agents.len(), 3);

    let target = session.target();
    host.close(&target, ClosePolicy::Save).await.unwrap();

    // Reopen and verify both B and C archived outputs remain readable.
    let reopened = host.open(target.conversation_id().clone()).await.unwrap();
    assert_eq!(
        reopened
            .read_agent(&b_rec.operation_id, 0, 1024)
            .await
            .unwrap()
            .page
            .content,
        "B result"
    );
    assert_eq!(
        reopened
            .read_agent(&c_rec.operation_id, 0, 1024)
            .await
            .unwrap()
            .page
            .content,
        "C result"
    );
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 2. Sync vs Background unified path & parallel sync fan-out (AC02)
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sync_and_background_share_path_with_parallel_sync_and_suppress_wake() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let barrier = Arc::new(Barrier::new(2));
    let s1_entered = Arc::new(Notify::new());
    let s2_entered = Arc::new(Notify::new());

    let main_gw = ScriptedGateway::simple("main ok");
    let s1_gw = Arc::new(BarrierGateway {
        barrier: barrier.clone(),
        entered: s1_entered,
        release: None,
        text: "sync-1 ok".into(),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let s2_gw = Arc::new(BarrierGateway {
        barrier,
        entered: s2_entered,
        release: None,
        text: "sync-2 ok".into(),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let bg_gw = ScriptedGateway::simple("bg ok");

    host.set_test_gateway(Some(main_gw.clone()));
    host.set_test_agent_gateway(Some(Arc::new(move |name, _| match name {
        "sync-1" => Some(s1_gw.clone()),
        "sync-2" => Some(s2_gw.clone()),
        "bg-1" => Some(bg_gw.clone()),
        _ => None,
    })));

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let root_id = entry_id(&session);

    let mut r1 = spawn_req(&root_id, "sync-1", "p1");
    r1.execution_mode = OperationExecutionMode::Sync;
    let mut r2 = spawn_req(&root_id, "sync-2", "p2");
    r2.execution_mode = OperationExecutionMode::Sync;

    let (out1, out2) = tokio::join!(session.spawn_agent(r1), session.spawn_agent(r2));
    let SpawnAgentOutcome::Completed(c1) = out1.unwrap() else {
        panic!("expected sync completed");
    };
    let SpawnAgentOutcome::Completed(c2) = out2.unwrap() else {
        panic!("expected sync completed");
    };
    assert_eq!(c1.status, OperationStatus::Completed);
    assert_eq!(c1.result_text.as_deref(), Some("sync-1 ok"));
    assert_eq!(c2.status, OperationStatus::Completed);
    assert_eq!(c2.result_text.as_deref(), Some("sync-2 ok"));

    // Sync operations must NOT emit inbox events or wake the parent.
    let store = host.inner.store.as_ref().unwrap();
    assert!(
        store
            .list_pending_inbox_events(&root_id, &root_id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(main_gw.calls.load(Ordering::SeqCst), 0);

    // Background returns immediately and wakes parent via agent_events.
    let SpawnAgentOutcome::Background(bg_rec) = session
        .spawn_agent(spawn_req(&root_id, "bg-1", "p3"))
        .await
        .unwrap()
    else {
        panic!("expected background");
    };
    assert!(bg_rec.status.is_active());
    until(|| main_gw.calls.load(Ordering::SeqCst) == 1).await;
    until(|| session.status().active_work.is_none()).await;
    session.wait_saved().await.unwrap();

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 3. Deduplication & Reusable / SingleWork / Isolation gates (AC03 / AC06)
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dedup_precedes_capacity_and_reusable_isolation_gates_enforced() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let hold_entered = Arc::new(Notify::new());
    let hold_release = Arc::new(Notify::new());

    let hold_gw = ScriptedGateway::new(
        vec![ScriptedStep {
            entered: Some(hold_entered.clone()),
            release: Some(hold_release.clone()),
            response_text: Some("held done".into()),
            tool_calls: Vec::new(),
            capture_blocks: None,
            wait_for_cancel: false,
        }],
        "next done",
    );
    let simple_gw = ScriptedGateway::simple("worker ok");

    host.set_test_gateway(Some(ScriptedGateway::simple("main ok")));
    host.set_test_agent_gateway(Some(Arc::new(move |name, _| match name {
        "reusable-held" => Some(hold_gw.clone()),
        _ => Some(simple_gw.clone()),
    })));

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    session.set_coordinator_config(CoordinatorConfig {
        max_active_ops_per_agent: 1,
        max_active_child_ops_per_session: 1,
        max_delegation_depth: 1,
        ..CoordinatorConfig::default()
    });
    let root_id = entry_id(&session);

    // Spawn a Reusable child that stays busy.
    let mut r_held = spawn_req(&root_id, "reusable-held", "first");
    r_held.interaction = AgentInteraction::Reusable;
    r_held.source_block_id = Some("blk-held-1".into());
    let SpawnAgentOutcome::Background(held_rec) =
        session.spawn_agent(r_held.clone()).await.unwrap()
    else {
        panic!("expected background");
    };
    hold_entered.notified().await;

    // Slot is full (1/1) AND agent is busy, yet same (parent_op=None, source_block_id) replays!
    let SpawnAgentOutcome::Background(replay_rec) =
        session.spawn_agent(r_held.clone()).await.unwrap()
    else {
        panic!("expected replayed background");
    };
    assert!(replay_rec.replayed);
    assert_eq!(replay_rec.operation_id, held_rec.operation_id);

    // Same source_block_id with altered parameters -> DedupConflict!
    let mut r_conflict = r_held.clone();
    r_conflict.prompt = "tampered".into();
    assert!(matches!(
        session.spawn_agent(r_conflict).await,
        Err(CoordinatorError::DedupConflict { existing_operation_id }) if existing_operation_id == held_rec.operation_id
    ));

    // Different source_block_id while slot is full -> AgentConcurrencyLimit!
    let mut r_other = spawn_req(&root_id, "other", "p");
    r_other.source_block_id = Some("blk-other".into());
    assert!(matches!(
        session.spawn_agent(r_other).await,
        Err(CoordinatorError::AgentConcurrencyLimit { limit: 1 })
    ));

    // Concurrent message_agent to busy reusable agent -> AgentConcurrencyLimit or AgentBusy.
    // Raise concurrency limit to 4 so we hit AgentBusy directly.
    session.set_coordinator_config(CoordinatorConfig {
        max_active_ops_per_agent: 4,
        max_active_child_ops_per_session: 4,
        max_delegation_depth: 1,
        ..CoordinatorConfig::default()
    });
    assert!(matches!(
        session
            .message_agent(msg_req(&root_id, &held_rec.agent_id, "while busy"))
            .await,
        Err(CoordinatorError::AgentBusy { .. })
    ));

    // Release held agent and let it settle.
    hold_release.notify_one();
    session.wait_background_tasks_settled().await;
    until(|| session.status().active_work.is_none()).await;

    // Sequential message_agent to Reusable agent succeeds!
    let mut m_seq = msg_req(&root_id, &held_rec.agent_id, "follow-up");
    m_seq.source_block_id = Some("blk-msg-1".into());
    let SpawnAgentOutcome::Completed(m_out) = session.message_agent(m_seq.clone()).await.unwrap()
    else {
        panic!("expected completed");
    };
    assert_eq!(m_out.status, OperationStatus::Completed);
    assert_eq!(m_out.result_text.as_deref(), Some("next done"));

    // SingleWork agent rejects message_agent after its confirmed work, while replay still wins.
    let mut r_single = spawn_req(&root_id, "single-w", "only once");
    r_single.execution_mode = OperationExecutionMode::Sync;
    r_single.source_block_id = Some("blk-single".into());
    let SpawnAgentOutcome::Completed(s_out) = session.spawn_agent(r_single.clone()).await.unwrap()
    else {
        panic!("expected completed");
    };
    assert!(matches!(
        session
            .message_agent(msg_req(&root_id, &s_out.agent_id, "second"))
            .await,
        Err(CoordinatorError::NotReusable(_))
    ));
    let SpawnAgentOutcome::Completed(s_replay) = session.spawn_agent(r_single).await.unwrap()
    else {
        panic!("expected sync replay");
    };
    assert_eq!(s_replay.operation_id, s_out.operation_id);

    // Self-call & depth exceeded & capability widening via message_agent.
    assert!(matches!(
        session
            .message_agent(msg_req(&root_id, &root_id, "self"))
            .await,
        Err(CoordinatorError::SelfDelegation)
    ));
    // Spawn a narrowed read-only child and have it try to message_agent the wider reusable child -> CapabilityMismatch!
    // First allow depth=2 so depth doesn't reject before capability check, and test depth=1 rejection first.
    let mut narrow_caller_req = spawn_req(&root_id, "narrow-caller", "go");
    narrow_caller_req.execution_mode = OperationExecutionMode::Sync;
    narrow_caller_req.narrowing.permission_mode = Some(PermissionMode::ReadOnly);
    let SpawnAgentOutcome::Completed(narrow_out) =
        session.spawn_agent(narrow_caller_req).await.unwrap()
    else {
        panic!("expected completed");
    };
    // With max_delegation_depth=1, depth 2 is rejected!
    let mut depth2_msg = msg_req(&narrow_out.agent_id, &held_rec.agent_id, "escalate");
    depth2_msg.caller_operation_id = Some(narrow_out.operation_id.clone());
    assert!(matches!(
        session.message_agent(depth2_msg.clone()).await,
        Err(CoordinatorError::DepthExceeded {
            depth: 2,
            max_depth: 1
        })
    ));

    // Raise depth to 2 -> capability widening is now rejected with CapabilityMismatch!
    session.set_coordinator_config(CoordinatorConfig {
        max_delegation_depth: 2,
        ..CoordinatorConfig::default()
    });
    assert!(matches!(
        session.message_agent(depth2_msg).await,
        Err(CoordinatorError::CapabilityMismatch(_))
    ));

    // Put reusable-held into NeedsRepair via edit_agent_materials with unpaired ToolCall -> message_agent rejects without mutating Context!
    let snap = session.read_agent_materials(&held_rec.agent_id).unwrap();
    let before_len = snap.context.blocks().len();
    let mut blocks = snap.context.blocks().to_vec();
    blocks.push(ContextBlock::new(
        new_block_id(),
        BlockContent::ToolCall(ToolCallPayload::new(
            "read_file",
            serde_json::json!({"path":"a.txt"}),
        )),
        BlockMeta::default(),
    ));
    session.edit_agent_materials(snap.version, blocks).unwrap();
    session.wait_saved().await.unwrap();

    assert!(matches!(
        session
            .message_agent(msg_req(&root_id, &held_rec.agent_id, "should fail"))
            .await,
        Err(CoordinatorError::AgentNeedsRepair(_))
    ));
    // Verify input was NOT appended to target Context.
    let after_snap = session.read_agent_materials(&held_rec.agent_id).unwrap();
    assert_eq!(after_snap.context.blocks().len(), before_len + 1);

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 4. Mid-turn injection & crash-safe `delivered` commit (AC05)
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mid_turn_injection_and_transactional_delivered_commit() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let ws_root = std::fs::canonicalize(ws.path()).unwrap();
    std::fs::write(ws_root.join("hello.txt"), "hi").unwrap();
    let host = SessionHost::new(Some(root.path().into()));

    let round1_entered = Arc::new(Notify::new());
    let round1_release = Arc::new(Notify::new());
    let round2_captured = Arc::new(Mutex::new(Vec::<ContextBlock>::new()));

    let main_gw = ScriptedGateway::new(
        vec![
            ScriptedStep {
                entered: Some(round1_entered.clone()),
                release: Some(round1_release.clone()),
                response_text: None,
                tool_calls: vec![ToolCallDraft::new(
                    "list_directory",
                    serde_json::json!({ "path": "." }),
                )],
                capture_blocks: None,
                wait_for_cancel: false,
            },
            ScriptedStep {
                entered: None,
                release: None,
                response_text: Some("parent final".into()),
                tool_calls: Vec::new(),
                capture_blocks: Some(round2_captured.clone()),
                wait_for_cancel: false,
            },
        ],
        "fallback",
    );
    let child_gw = ScriptedGateway::simple("child finished mid-turn");

    host.set_test_gateway(Some(main_gw));
    host.set_test_agent_gateway(Some(Arc::new(move |name, _| {
        if name == "bg-child" {
            Some(child_gw.clone())
        } else {
            None
        }
    })));

    let mut def = definition();
    def.workspace_root = Some(ws_root);
    def.tools = vec![WorkspaceTool::ListDirectory];
    def.max_model_rounds = 4;
    def.max_tool_calls = 4;

    let session = host
        .create(PreparedCreate::persistent(def, Context::new()))
        .await
        .unwrap();
    let root_id = entry_id(&session);

    // Start parent turn; pause in Round 1 ModelGateway::invoke.
    let receipt = session
        .submit(SubmitRequest {
            request_key: "parent-turn".into(),
            blocks: vec![super::super::text_input("start parent")],
        })
        .unwrap();
    round1_entered.notified().await;

    // While parent is in Round 1, spawn background child and wait for it to finish & commit outbox.
    let SpawnAgentOutcome::Background(child_rec) = session
        .spawn_agent(spawn_req(&root_id, "bg-child", "do bg"))
        .await
        .unwrap()
    else {
        panic!("expected background");
    };
    session.wait_background_tasks_settled().await;

    // Event must be `pending` in SQLite before parent commits!
    let store = host.inner.store.as_ref().unwrap();
    assert_eq!(
        store
            .list_pending_inbox_events(&root_id, &root_id)
            .unwrap()
            .len(),
        1
    );

    // Gate parent's save at CommitStage::Write so we can prove event stays `pending` until commit!
    let save_gate = Gate::new();
    session.set_writer_fault_hook(Some(save_gate.hook(CommitStage::Write)));

    // Release Round 1 -> tool executes -> Round 2 CompactionPreparer injects `<task-notification>`.
    round1_release.notify_one();
    finished(&session, &receipt.work).await;
    save_gate.entered().await;

    // Verify Round 2 model saw `<task-notification>` with source == TASK_NOTIFICATION_SOURCE!
    let r2_blocks = round2_captured.lock().unwrap().clone();
    let notif_block = r2_blocks
        .iter()
        .find(|b| b.meta().source.as_deref() == Some(TASK_NOTIFICATION_SOURCE))
        .expect("mid-turn task-notification injected before round 2");
    assert!(extract_block_text(notif_block).contains("child finished mid-turn"));
    assert!(extract_block_text(notif_block).contains(&child_rec.operation_id));

    // STILL `pending` in SQLite while save is gated before COMMIT!
    assert_eq!(
        store
            .list_pending_inbox_events(&root_id, &root_id)
            .unwrap()
            .len(),
        1
    );

    // Release save -> COMMIT runs -> event atomically becomes `delivered`!
    save_gate.release();
    session.wait_saved().await.unwrap();
    assert!(
        store
            .list_pending_inbox_events(&root_id, &root_id)
            .unwrap()
            .is_empty()
    );

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 5. 7-Guard Idle Reactive Wakeup + 0ms/50ms Debounce + coalesce_key + max_auto_wakeups
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_wakeup_debounce_coalesce_and_max_auto_wakeups_limit() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let main_gw = ScriptedGateway::simple("parent woke");
    let sib2_entered = Arc::new(Notify::new());
    let sib2_release = Arc::new(Notify::new());

    let sib1_gw = ScriptedGateway::simple("sib1-done");
    let sib2_gw = ScriptedGateway::new(
        vec![ScriptedStep {
            entered: Some(sib2_entered.clone()),
            release: Some(sib2_release.clone()),
            response_text: Some("sib2-done".into()),
            tool_calls: Vec::new(),
            capture_blocks: None,
            wait_for_cancel: false,
        }],
        "sib2-done",
    );
    let quick_gw = ScriptedGateway::simple("quick-done");

    host.set_test_gateway(Some(main_gw.clone()));
    host.set_test_agent_gateway(Some(Arc::new(move |name, _| match name {
        "sib1" => Some(sib1_gw.clone()),
        "sib2" => Some(sib2_gw.clone()),
        _ => Some(quick_gw.clone()),
    })));

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    // Large debounce (4000ms) proves single child (0 siblings) wakes IMMEDIATELY at 0ms!
    session.set_coordinator_config(CoordinatorConfig {
        max_auto_wakeups: 2,
        sibling_debounce: Duration::from_secs(4),
        ..CoordinatorConfig::default()
    });
    let root_id = entry_id(&session);

    // A) Single child finishes with 0 active siblings -> 0ms immediate wakeup (no 4s wait).
    tokio::time::timeout(Duration::from_secs(2), async {
        session
            .spawn_agent(spawn_req(&root_id, "single", "p"))
            .await
            .unwrap();
        until(|| main_gw.calls.load(Ordering::SeqCst) == 1).await;
    })
    .await
    .expect("0ms immediate wakeup should not wait for sibling_debounce");
    until(|| session.status().active_work.is_none()).await;
    session.wait_saved().await.unwrap();

    // B) Two siblings sharing coalesce_key finish within debounce window -> 1 wakeup + older dropped!
    session.set_coordinator_config(CoordinatorConfig {
        max_auto_wakeups: 2,
        sibling_debounce: Duration::from_millis(250),
        ..CoordinatorConfig::default()
    });
    let mut r_s1 = spawn_req(&root_id, "sib1", "p1");
    r_s1.coalesce_key = Some("shared-key".into());
    let mut r_s2 = spawn_req(&root_id, "sib2", "p2");
    r_s2.coalesce_key = Some("shared-key".into());

    session.spawn_agent(r_s1).await.unwrap();
    session.spawn_agent(r_s2).await.unwrap();
    sib2_entered.notified().await;
    // Let sib1 finish first, wait until sib1 is done while sib2 is still active, then release sib2 shortly after.
    until(|| session.active_finite_background_tasks() == 1).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    sib2_release.notify_one();
    session.wait_background_tasks_settled().await;
    until(|| main_gw.calls.load(Ordering::SeqCst) == 2).await;
    until(|| session.status().active_work.is_none()).await;
    session.wait_saved().await.unwrap();

    // Both siblings coalesced into ONE wakeup (calls == 2, not 3), and only sib2 survived!
    let last_req = main_gw
        .captured_requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    let last_text: String = last_req
        .iter()
        .map(extract_block_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(last_text.contains("sib2-done"));
    assert!(!last_text.contains("sib1-done"));

    // C) Exceeding max_auto_wakeups (already at 2/2) downgrades 3rd wakeup and emits AutoWakeupLimitReached!
    let mut events_rx = session.subscribe_events();
    session
        .spawn_agent(spawn_req(&root_id, "over-budget", "p3"))
        .await
        .unwrap();
    session.wait_background_tasks_settled().await;

    let mut saw_limit_event = false;
    tokio::time::timeout(Duration::from_secs(2), async {
        while let Ok(ev) = events_rx.recv().await {
            if matches!(
                *ev,
                SessionEvent::AutoWakeupLimitReached {
                    max_auto_wakeups: 2,
                    ..
                }
            ) {
                saw_limit_event = true;
                break;
            }
        }
    })
    .await
    .expect("AutoWakeupLimitReached event");
    assert!(saw_limit_event);

    // Parent was NOT auto-woken (calls still 2), and event in SQLite was downgraded to deliver_on_next_turn!
    assert_eq!(main_gw.calls.load(Ordering::SeqCst), 2);
    let store = host.inner.store.as_ref().unwrap();
    let pending = store.list_pending_inbox_events(&root_id, &root_id).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].wake_policy, WakePolicy::DeliverOnNextTurn);

    // Explicit user submit resets auto_wakeups_in_cycle to 0 and delivers the downgraded notification!
    let user_work = session
        .submit(SubmitRequest {
            request_key: "reset-cycle".into(),
            blocks: vec![super::super::text_input("user resumes")],
        })
        .unwrap();
    finished(&session, &user_work.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(main_gw.calls.load(Ordering::SeqCst), 3);
    assert!(
        store
            .list_pending_inbox_events(&root_id, &root_id)
            .unwrap()
            .is_empty()
    );

    // Now a 4th child CAN auto-wake again because budget was reset!
    session
        .spawn_agent(spawn_req(&root_id, "after-reset", "p4"))
        .await
        .unwrap();
    until(|| main_gw.calls.load(Ordering::SeqCst) == 4).await;
    until(|| session.status().active_work.is_none()).await;
    session.wait_saved().await.unwrap();

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 6. Reopen Safety, Stale Op Reconciliation, Missing Credentials, UTF-8 Pagination
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reopen_reconciles_stale_ops_downgrades_wakeups_isolates_missing_creds_and_pages_utf8() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let main_gw = ScriptedGateway::simple("main ok");
    let multibyte_text = "你好世界🚀".repeat(5_000); // > 64 KiB multi-byte UTF-8
    let b_gw = ScriptedGateway::new(
        vec![
            ScriptedStep::text(multibyte_text.clone()),
            ScriptedStep::text("B2 short"),
        ],
        "B2 short",
    );
    let c_gw = ScriptedGateway::simple("C ok");

    host.set_test_gateway(Some(main_gw.clone()));
    let b_gw_c = b_gw.clone();
    let c_gw_c = c_gw.clone();
    host.set_test_agent_gateway(Some(Arc::new(move |name, _| match name {
        "agent-b" => Some(b_gw_c.clone()),
        "agent-c" => Some(c_gw_c.clone()),
        _ => None,
    })));

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let root_id = entry_id(&session);

    // Run B1 (long UTF-8) and B2 (modifies B's Context) on Reusable Agent B.
    let mut r_b1 = spawn_req(&root_id, "agent-b", "b1");
    r_b1.interaction = AgentInteraction::Reusable;
    r_b1.execution_mode = OperationExecutionMode::Sync;
    let SpawnAgentOutcome::Completed(b1_out) = session.spawn_agent(r_b1).await.unwrap() else {
        panic!("expected completed");
    };
    let SpawnAgentOutcome::Completed(_b2_out) = session
        .message_agent(msg_req(&root_id, &b1_out.agent_id, "b2"))
        .await
        .unwrap()
    else {
        panic!("expected completed");
    };

    // Spawn Agent C too so we verify C still runs on reopen when B has missing credentials.
    let mut r_c = spawn_req(&root_id, "agent-c", "c1");
    r_c.interaction = AgentInteraction::Reusable;
    r_c.execution_mode = OperationExecutionMode::Sync;
    let SpawnAgentOutcome::Completed(c1_out) = session.spawn_agent(r_c).await.unwrap() else {
        panic!("expected completed");
    };

    let target = session.target();
    host.close(&target, ClosePolicy::Save).await.unwrap();

    // Inject stale `running` operation AND a `wake_if_idle` event into SQLite while closed!
    let store = host.inner.store.as_ref().unwrap();
    let stale_op_id = uuid::Uuid::now_v7().to_string();
    let stale_agent_id = uuid::Uuid::now_v7().to_string();
    {
        let writer = store.acquire_writer(&root_id).unwrap();
        let mut bad_cred_def = definition();
        bad_cred_def.credential =
            crate::definition::CredentialRef::Environment("TARS_MISSING_CRED_XYZ_999".into());
        let admit = writer
            .admit_operation(&AdmitOperationRequest {
                commit_id: uuid::Uuid::now_v7().to_string(),
                operation_id: stale_op_id.clone(),
                target_agent_id: stale_agent_id.clone(),
                parent_operation_id: None,
                parent_agent_id: Some(root_id.clone()),
                source_block_id: None,
                dedup_fingerprint: None,
                execution_mode: OperationExecutionMode::Background,
                on_parent_complete: OnParentComplete::Continue,
                depth: 1,
                input_json: "{}".into(),
                created_at_ms: 1000,
                new_agent: Some(NewAgentRow {
                    agent_id: stale_agent_id.clone(),
                    name: "stale-worker".into(),
                    preset_name: None,
                    interaction: AgentInteraction::SingleWork,
                    definition_json: serde_json::to_string(&bad_cred_def).unwrap(),
                    context_json: serde_json::to_string(&Context::new()).unwrap(),
                    history_index_json: "[]".into(),
                    usage_json: None,
                    created_at_ms: 1000,
                    updated_at_ms: 1000,
                }),
            })
            .unwrap();
        let rev = match admit {
            crate::store::AdmitOperationOutcome::Admitted {
                session_revision, ..
            } => session_revision,
            _ => unreachable!(),
        };
        writer
            .transition_operation_running(&stale_op_id, 1100)
            .unwrap();
        writer
            .commit_outbox(&OutboxCommitRequest {
                commit_id: uuid::Uuid::now_v7().to_string(),
                expected_session_revision: rev,
                updated_at_ms: 1200,
                agent_update: None,
                operation_update: None,
                new_events: vec![NewInboxEvent {
                    target_agent_id: root_id.clone(),
                    priority: EventPriority::Normal,
                    source_kind: EventSourceKind::AgentOperation,
                    source_id: stale_op_id.clone(),
                    coalesce_key: None,
                    wake_policy: WakePolicy::WakeIfIdle,
                    payload_json: "{\"summary\":\"stale\"}".into(),
                    created_at_ms: 1200,
                }],
                delivered_event_ids: Vec::new(),
                dropped_event_ids: Vec::new(),
            })
            .unwrap();
    }

    // Reopen on a host WITHOUT gateway override for `agent-b` (simulate missing credentials for B,
    // while `main` and `agent-c` keep their test gateways).
    let host2 = SessionHost::new(Some(root.path().into()));
    let main_gw2 = ScriptedGateway::simple("main reopened");
    let c_gw2 = ScriptedGateway::simple("C reopened");
    host2.set_test_agent_gateway(Some(Arc::new({
        let main_gw2 = main_gw2.clone();
        let c_gw2 = c_gw2.clone();
        move |name, _| match name {
            "main" => Some(main_gw2.clone()),
            "agent-c" => Some(c_gw2.clone()),
            _ => None, // agent-b falls back to definition_gateway -> CredentialUnavailable!
        }
    })));
    // Use `agent_gateway_override` for "main" and "agent-c", leaving `gateway_override` as `None`
    // so "agent-b" hits real `definition_gateway` -> `CredentialUnavailable`.
    host2.set_test_gateway(None);

    let reopened = host2.open(target.conversation_id().clone()).await.unwrap();

    // 1. Reopen MUST NOT auto-execute any model call!
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(main_gw2.calls.load(Ordering::SeqCst), 0);

    // 2. Stale `running` operation was reconciled to `interrupted`!
    let stale_read = reopened.read_agent(&stale_op_id, 0, 1024).await.unwrap();
    assert_eq!(stale_read.status, OperationStatus::Interrupted);

    // 3. Pending `wake_if_idle` event was downgraded to `deliver_on_next_turn`!
    let store2 = host2.inner.store.as_ref().unwrap();
    let pending_after = store2
        .list_pending_inbox_events(&root_id, &root_id)
        .unwrap();
    assert_eq!(pending_after.len(), 1);
    assert_eq!(pending_after[0].wake_policy, WakePolicy::DeliverOnNextTurn);

    // 4. B1's complete archived output is still readable in UTF-8 safe pages (<= 64 KiB) even after B2 and missing creds!
    let mut collected = String::new();
    let mut offset = 0usize;
    loop {
        let page = reopened
            .read_agent(&b1_out.operation_id, offset, usize::MAX)
            .await
            .unwrap();
        assert_eq!(page.status, OperationStatus::Completed);
        assert!(page.page.content.len() <= MAX_READ_AGENT_PAGE_BYTES);
        collected.push_str(&page.page.content);
        if !page.page.has_more {
            break;
        }
        assert!(page.page.next_offset_bytes > offset);
        offset = page.page.next_offset_bytes;
    }
    assert_eq!(collected, multibyte_text);

    // 5. Missing credentials on Agent B fails B's execution without blocking Agent C or root!
    let SpawnAgentOutcome::Completed(b_fail) = reopened
        .message_agent(msg_req(&root_id, &b1_out.agent_id, "should fail cred"))
        .await
        .unwrap()
    else {
        panic!("expected sync outcome");
    };
    assert_eq!(b_fail.status, OperationStatus::Failed);
    assert!(
        !reopened
            .agent_snapshot(&b1_out.agent_id)
            .unwrap()
            .executable
    );

    let SpawnAgentOutcome::Completed(c_ok) = reopened
        .message_agent(msg_req(&root_id, &c1_out.agent_id, "c runs fine"))
        .await
        .unwrap()
    else {
        panic!("expected completed");
    };
    assert_eq!(c_ok.status, OperationStatus::Completed);
    assert_eq!(c_ok.result_text.as_deref(), Some("C reopened"));

    host2
        .close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 7. Cancellation Cascade, on_parent_complete, Close Cascade, Future Drop
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_cascade_on_parent_complete_close_cascade_and_future_drop() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let c_entered = Arc::new(Notify::new());
    let cancel_child_entered = Arc::new(Notify::new());
    let continue_entered = Arc::new(Notify::new());
    let continue_release = Arc::new(Notify::new());
    let parent_entered = Arc::new(Notify::new());
    let parent_release = Arc::new(Notify::new());

    let main_gw = ScriptedGateway::new(
        vec![ScriptedStep {
            entered: Some(parent_entered.clone()),
            release: Some(parent_release.clone()),
            response_text: Some("root turn done".into()),
            tool_calls: Vec::new(),
            capture_blocks: None,
            wait_for_cancel: false,
        }],
        "main ok",
    );
    let a_gw = ScriptedGateway::new(
        vec![ScriptedStep {
            entered: None,
            release: Some(Arc::new(Notify::new())),
            response_text: None,
            tool_calls: Vec::new(),
            capture_blocks: None,
            wait_for_cancel: true,
        }],
        "a",
    );
    let b_gw = ScriptedGateway::simple("B completed");
    let c_gw = ScriptedGateway::new(
        vec![ScriptedStep {
            entered: Some(c_entered.clone()),
            release: None,
            response_text: None,
            tool_calls: Vec::new(),
            capture_blocks: None,
            wait_for_cancel: true,
        }],
        "c",
    );
    let cancel_child_gw = ScriptedGateway::new(
        vec![ScriptedStep {
            entered: Some(cancel_child_entered.clone()),
            release: None,
            response_text: None,
            tool_calls: Vec::new(),
            capture_blocks: None,
            wait_for_cancel: true,
        }],
        "cancel-child",
    );
    let continue_gw = ScriptedGateway::new(
        vec![ScriptedStep {
            entered: Some(continue_entered.clone()),
            release: Some(continue_release.clone()),
            response_text: Some("continue child done".into()),
            tool_calls: Vec::new(),
            capture_blocks: None,
            wait_for_cancel: false,
        }],
        "continue",
    );

    host.set_test_gateway(Some(main_gw.clone()));
    host.set_test_agent_gateway(Some(Arc::new(move |name, _| match name {
        "node-a" => Some(a_gw.clone()),
        "node-b" => Some(b_gw.clone()),
        "node-c" => Some(c_gw.clone()),
        "cancel-on-complete" => Some(cancel_child_gw.clone()),
        "continue-on-complete" => Some(continue_gw.clone()),
        _ => None,
    })));

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    session.set_coordinator_config(CoordinatorConfig {
        max_delegation_depth: 4,
        max_active_ops_per_agent: 8,
        max_active_child_ops_per_session: 8,
        ..CoordinatorConfig::default()
    });
    let root_id = entry_id(&session);

    // Part A: A -> B(completed) -> C(running). Cancelling A cascades through terminal B to cancel C!
    let SpawnAgentOutcome::Background(a_rec) = session
        .spawn_agent(spawn_req(&root_id, "node-a", "a"))
        .await
        .unwrap()
    else {
        panic!("expected background");
    };
    let mut r_b = spawn_req(&a_rec.agent_id, "node-b", "b");
    r_b.caller_operation_id = Some(a_rec.operation_id.clone());
    r_b.execution_mode = OperationExecutionMode::Sync;
    let SpawnAgentOutcome::Completed(b_out) = session.spawn_agent(r_b).await.unwrap() else {
        panic!("expected completed");
    };
    assert_eq!(b_out.status, OperationStatus::Completed);

    let mut r_c = spawn_req(&b_out.agent_id, "node-c", "c");
    r_c.caller_operation_id = Some(b_out.operation_id.clone());
    let SpawnAgentOutcome::Background(c_rec) = session.spawn_agent(r_c).await.unwrap() else {
        panic!("expected background");
    };
    c_entered.notified().await;

    let a_cancel_out = session.cancel_agent(&a_rec.operation_id).await.unwrap();
    assert_eq!(a_cancel_out.status, OperationStatus::Cancelled);

    // B's already-won Completed terminal state MUST be preserved; C MUST be Cancelled!
    assert_eq!(
        session
            .read_agent(&b_out.operation_id, 0, 1024)
            .await
            .unwrap()
            .status,
        OperationStatus::Completed
    );
    until(|| {
        session
            .agent_snapshot(&c_rec.agent_id)
            .is_some_and(|s| s.active_operation_id.is_none())
    })
    .await;
    assert_eq!(
        session
            .read_agent(&c_rec.operation_id, 0, 1024)
            .await
            .unwrap()
            .status,
        OperationStatus::Cancelled
    );

    // Part B: Root turn normal completion cancels `OnParentComplete::Cancel` child while letting `Continue` finish!
    let root_work = session
        .submit(SubmitRequest {
            request_key: "root-turn".into(),
            blocks: vec![super::super::text_input("run children")],
        })
        .unwrap();
    parent_entered.notified().await;

    let mut r_cancel_child = spawn_req(&root_id, "cancel-on-complete", "cc");
    r_cancel_child.on_parent_complete = OnParentComplete::Cancel;
    let SpawnAgentOutcome::Background(cc_rec) = session.spawn_agent(r_cancel_child).await.unwrap()
    else {
        panic!("expected background");
    };
    cancel_child_entered.notified().await;

    let mut r_cont_child = spawn_req(&root_id, "continue-on-complete", "cont");
    r_cont_child.on_parent_complete = OnParentComplete::Continue;
    let SpawnAgentOutcome::Background(cont_rec) = session.spawn_agent(r_cont_child).await.unwrap()
    else {
        panic!("expected background");
    };
    continue_entered.notified().await;

    parent_release.notify_one();
    finished(&session, &root_work.work).await;
    session.wait_saved().await.unwrap();

    // `OnParentComplete::Cancel` child was cancelled; `Continue` child is STILL running!
    until(|| {
        session
            .agent_snapshot(&cc_rec.agent_id)
            .is_some_and(|s| s.active_operation_id.is_none())
    })
    .await;
    assert_eq!(
        session
            .read_agent(&cc_rec.operation_id, 0, 1024)
            .await
            .unwrap()
            .status,
        OperationStatus::Cancelled
    );
    assert!(
        session
            .agent_snapshot(&cont_rec.agent_id)
            .unwrap()
            .active_operation_id
            .is_some()
    );

    // Part C: Dropping caller's `Sync` future does NOT orphan supervised task!
    let mut r_drop = spawn_req(&root_id, "dropped-sync-child", "dropped-sync");
    r_drop.execution_mode = OperationExecutionMode::Sync;
    {
        let mut fut = Box::pin(session.spawn_agent(r_drop));
        let _ = futures_util::poll!(&mut fut);
    }
    // Wait until the supervised admission completes and appears on the session before close.
    until(|| {
        session
            .list_agents()
            .iter()
            .any(|a| a.name == "dropped-sync-child")
    })
    .await;
    // Part D: SessionHost::close cancels ALL tasks including `Continue` & dropped-sync children,
    // settles them before releasing OS lock, and leaves zero non-terminal rows in SQLite.
    let target = session.target();
    let conv_id = target.conversation_id().as_str().to_owned();
    host.close(&target, ClosePolicy::Save).await.unwrap();

    // Verify directly in SQLite that the dropped-sync operation was supervised to terminal settlement.
    let store = host.inner.store.as_ref().unwrap();
    let ops = store.list_operations(&conv_id).unwrap();
    let dropped_sync_agent = store
        .list_agents(&conv_id)
        .unwrap()
        .into_iter()
        .find(|a| a.name == "dropped-sync-child")
        .expect("dropped-sync child persisted");
    let dropped_sync_op = ops
        .iter()
        .find(|op| op.target_agent_id == dropped_sync_agent.agent_id)
        .expect("dropped-sync operation persisted");
    assert!(
        dropped_sync_op.status.is_terminal(),
        "dropped-sync op must settle terminally, got {:?}",
        dropped_sync_op.status
    );
    assert!(
        ops.iter().all(|op| op.status.is_terminal()),
        "no operation may remain creating/running after close"
    );

    // Reopen immediately proves OS lock was released cleanly after all child tasks settled.
    let reopened = host.open(target.conversation_id().clone()).await.unwrap();
    assert_eq!(
        reopened
            .read_agent(&cont_rec.operation_id, 0, 1024)
            .await
            .unwrap()
            .status,
        OperationStatus::Cancelled
    );
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 8. SaveBlocked Isolation (AC11)
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn save_blocked_on_b_blocks_new_admissions_but_allows_c_to_settle_and_read_cancel_close() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let c_entered = Arc::new(Notify::new());
    let c_release = Arc::new(Notify::new());
    let b_entered = Arc::new(Notify::new());
    let b_release = Arc::new(Notify::new());

    let b_gw = ScriptedGateway::new(
        vec![ScriptedStep {
            entered: Some(b_entered.clone()),
            release: Some(b_release.clone()),
            response_text: Some("B ok".into()),
            tool_calls: Vec::new(),
            capture_blocks: None,
            wait_for_cancel: false,
        }],
        "B ok",
    );
    let c_gw = ScriptedGateway::new(
        vec![ScriptedStep {
            entered: Some(c_entered.clone()),
            release: Some(c_release.clone()),
            response_text: Some("C ok after B blocked".into()),
            tool_calls: Vec::new(),
            capture_blocks: None,
            wait_for_cancel: false,
        }],
        "C ok",
    );

    host.set_test_gateway(Some(ScriptedGateway::simple("main ok")));
    host.set_test_agent_gateway(Some(Arc::new(move |name, _| match name {
        "agent-b" => Some(b_gw.clone()),
        "agent-c" => Some(c_gw.clone()),
        _ => None,
    })));

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let root_id = entry_id(&session);

    // Start C and hold it running.
    let SpawnAgentOutcome::Background(c_rec) = session
        .spawn_agent(spawn_req(&root_id, "agent-c", "c"))
        .await
        .unwrap()
    else {
        panic!("expected background");
    };
    c_entered.notified().await;

    // Start B and hold it in model execution so admit_operation commits before we inject the outbox fault.
    let SpawnAgentOutcome::Background(_b_rec) = session
        .spawn_agent(spawn_req(&root_id, "agent-b", "b"))
        .await
        .unwrap()
    else {
        panic!("expected background");
    };
    b_entered.notified().await;

    // Inject Write failure on next commit (which will be B's commit_outbox).
    let fail_once = Arc::new(AtomicUsize::new(0));
    let fail_once_c = fail_once.clone();
    session.set_writer_fault_hook(Some(Arc::new(move |stage| {
        if stage == CommitStage::Write && fail_once_c.fetch_add(1, Ordering::SeqCst) == 0 {
            FaultAction::Fail(std::io::Error::other("injected outbox write error"))
        } else {
            FaultAction::Continue
        }
    })));
    b_release.notify_one();

    // Wait until session enters SaveBlocked from B's commit failure.
    until(|| matches!(session.status().save, SaveStatus::SaveBlocked { .. })).await;

    // New admissions must be rejected with SaveBlocked.
    assert!(matches!(
        session
            .spawn_agent(spawn_req(&root_id, "agent-d", "d"))
            .await,
        Err(CoordinatorError::Application(
            ApplicationError::SaveBlocked(_)
        ))
    ));

    // Meanwhile already-running C still finishes and is readable via read_agent!
    c_release.notify_one();
    session.wait_background_tasks_settled().await;
    let c_read = session
        .read_agent(&c_rec.operation_id, 0, 1024)
        .await
        .unwrap();
    assert_eq!(c_read.status, OperationStatus::Completed);
    assert_eq!(c_read.page.content, "C ok after B blocked");

    // retry_save recovers both B and C to SQLite!
    session.set_writer_fault_hook(None);
    session.retry_save().await.unwrap();

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 9. Cross-Agent Repair & Edit Isolation (AC09)
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_agent_repair_and_edit_isolation() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    host.set_test_gateway(Some(ScriptedGateway::simple("main ok")));
    host.set_test_agent_gateway(Some(Arc::new(|_, _| {
        Some(ScriptedGateway::simple("child ok"))
    })));

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let root_id = entry_id(&session);

    let SpawnAgentOutcome::Completed(b_out) = session
        .spawn_agent({
            let mut r = spawn_req(&root_id, "agent-b", "b");
            r.interaction = AgentInteraction::Reusable;
            r.execution_mode = OperationExecutionMode::Sync;
            r
        })
        .await
        .unwrap()
    else {
        panic!("expected completed");
    };

    let b_snap = session.read_agent_materials(&b_out.agent_id).unwrap();
    let a_snap = session.read_agent_materials(&root_id).unwrap();

    // Put B into a repairable state (unpaired ToolCall).
    let mut b_blocks = b_snap.context.blocks().to_vec();
    b_blocks.push(ContextBlock::new(
        new_block_id(),
        BlockContent::ToolCall(ToolCallPayload::new(
            "read_file",
            serde_json::json!({"path": "x.txt"}),
        )),
        BlockMeta::default(),
    ));
    let b_v1 = session
        .edit_agent_materials(b_snap.version.clone(), b_blocks)
        .unwrap();
    session.wait_saved().await.unwrap();

    // Stale generation edit rejected without mutating.
    assert!(matches!(
        session.edit_agent_materials(b_snap.version, vec![]),
        Err(CoordinatorError::Application(
            ApplicationError::StaleMaterial
        ))
    ));

    // Cross-instance version rejected.
    let mut wrong_target_version = AgentMaterialVersion {
        target: a_snap.version.target.clone(),
        agent_id: b_out.agent_id.clone(),
        generation: b_v1.generation,
    };
    let inspection_b = session.inspect_agent_repair(&b_out.agent_id, None).unwrap();
    assert_eq!(inspection_b.diagnosis.unpaired_count, 1);

    let plan_b: AgentRepairPlan = session
        .prepare_agent_repair(&b_out.agent_id, RepairMode::RemoveUnpaired)
        .unwrap();
    assert_eq!(plan_b.base().agent_id, b_out.agent_id);

    // Mutate B first so plan_b becomes stale -> rejected!
    let mut b_blocks2 = session
        .read_agent_materials(&b_out.agent_id)
        .unwrap()
        .context
        .blocks()
        .to_vec();
    b_blocks2.push(super::super::text_input("extra"));
    session
        .edit_agent_materials(b_v1.clone(), b_blocks2)
        .unwrap();
    session.wait_saved().await.unwrap();
    assert!(matches!(
        session.apply_agent_repair(plan_b),
        Err(CoordinatorError::Application(
            ApplicationError::StaleMaterial
        ))
    ));

    // Prepare fresh plan for B and apply -> succeeds!
    let fresh_plan_b = session
        .prepare_agent_repair(&b_out.agent_id, RepairMode::RemoveUnpaired)
        .unwrap();
    let repaired_ver = session.apply_agent_repair(fresh_plan_b).unwrap();
    session.wait_saved().await.unwrap();
    assert_eq!(
        session
            .inspect_agent_repair(&b_out.agent_id, None)
            .unwrap()
            .diagnosis
            .unpaired_count,
        0
    );

    // Entry agent A remains untouched and ready.
    assert_eq!(
        session
            .inspect_agent_repair(&root_id, None)
            .unwrap()
            .diagnosis
            .unpaired_count,
        0
    );

    // Stale instance version against reopened session rejected.
    let target = session.target();
    host.close(&target, ClosePolicy::Save).await.unwrap();
    let reopened = host.open(target.conversation_id().clone()).await.unwrap();
    wrong_target_version.generation = repaired_ver.generation;
    assert!(matches!(
        reopened.edit_agent_materials(wrong_target_version, vec![]),
        Err(CoordinatorError::Application(
            ApplicationError::StaleInstance
        ))
    ));
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 10. ApprovalsHub Multi-Agent Origin Tagging & Cancellation Abort (AC08)
// ---------------------------------------------------------------------------
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn child_approval_carries_agent_origin_and_aborts_cleanly_on_cancel() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let ws_root = std::fs::canonicalize(ws.path()).unwrap();
    let host = SessionHost::new(Some(root.path().into()));

    let child_gw = ScriptedGateway::new(
        vec![
            ScriptedStep::tool(
                "write_file",
                serde_json::json!({ "path": "out.txt", "content": "hello" }),
            ),
            ScriptedStep::text("wrote file"),
        ],
        "done",
    );
    let cancel_child_gw = ScriptedGateway::new(
        vec![ScriptedStep::tool(
            "write_file",
            serde_json::json!({ "path": "never.txt", "content": "x" }),
        )],
        "done",
    );

    host.set_test_gateway(Some(ScriptedGateway::simple("main ok")));
    host.set_test_agent_gateway(Some(Arc::new(move |name, _| match name {
        "writer-approve" => Some(child_gw.clone()),
        "writer-cancel" => Some(cancel_child_gw.clone()),
        _ => None,
    })));

    let mut def = definition();
    def.workspace_root = Some(ws_root.clone());
    def.tools = vec![WorkspaceTool::WriteFile];
    def.permission_mode = Some(PermissionMode::Ask);
    def.max_model_rounds = 4;
    def.max_tool_calls = 4;

    let session = host
        .create(PreparedCreate::persistent(def, Context::new()))
        .await
        .unwrap();
    let root_id = entry_id(&session);
    let mut approvals_rx = session.connect_approvals();

    // A) Child requests approval -> tagged with child's (agent_id, agent_name, operation_id), approve -> succeeds!
    let SpawnAgentOutcome::Background(approve_rec) = session
        .spawn_agent(spawn_req(&root_id, "writer-approve", "write it"))
        .await
        .unwrap()
    else {
        panic!("expected background");
    };

    let req1 = tokio::time::timeout(Duration::from_secs(5), approvals_rx.recv())
        .await
        .unwrap()
        .expect("approval request");
    assert_eq!(req1.agent_id, approve_rec.agent_id);
    assert_eq!(req1.agent_name, "writer-approve");
    assert_eq!(req1.operation_id, approve_rec.operation_id);
    assert!(matches!(req1.subject, ApprovalSubject::FileWrite { .. }));
    req1.reply.send(ApprovalDecision::Allow).unwrap();

    session.wait_background_tasks_settled().await;
    assert_eq!(
        session
            .read_agent(&approve_rec.operation_id, 0, 1024)
            .await
            .unwrap()
            .status,
        OperationStatus::Completed
    );
    assert_eq!(
        std::fs::read_to_string(ws_root.join("out.txt")).unwrap(),
        "hello"
    );

    // B) Cancel child while approval is pending -> aborts approval & settles Cancelled without writing file!
    until(|| session.status().active_work.is_none()).await;
    let SpawnAgentOutcome::Background(cancel_rec) = session
        .spawn_agent(spawn_req(&root_id, "writer-cancel", "write never"))
        .await
        .unwrap()
    else {
        panic!("expected background");
    };

    let req2 = tokio::time::timeout(Duration::from_secs(5), approvals_rx.recv())
        .await
        .unwrap()
        .expect("second approval request");
    assert_eq!(req2.agent_id, cancel_rec.agent_id);
    assert_eq!(req2.agent_name, "writer-cancel");
    assert_eq!(req2.operation_id, cancel_rec.operation_id);

    let cancel_out = session
        .cancel_agent(&cancel_rec.operation_id)
        .await
        .unwrap();
    assert_eq!(cancel_out.status, OperationStatus::Cancelled);
    // Reply channel is now closed because ToolPolicyProcessor aborted cleanly on cancellation!
    assert!(req2.reply.send(ApprovalDecision::Allow).is_err());
    assert!(!ws_root.join("never.txt").exists());

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 11. SaveBlocked Dual-Order Recovery (Root-First AND Outbox-First)
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn save_blocked_recovery_both_root_first_and_outbox_first() {
    // --- Order A: Root saving::save fails first at Write, then child settles (`PendingCommit`). ---
    {
        let root = tempfile::tempdir().unwrap();
        let host = SessionHost::new(Some(root.path().into()));
        let child_entered = Arc::new(Notify::new());
        let child_release = Arc::new(Notify::new());

        let child_gw = ScriptedGateway::new(
            vec![ScriptedStep {
                entered: Some(child_entered.clone()),
                release: Some(child_release.clone()),
                response_text: Some("order-a child ok".into()),
                tool_calls: Vec::new(),
                capture_blocks: None,
                wait_for_cancel: false,
            }],
            "fallback",
        );
        host.set_test_gateway(Some(ScriptedGateway::simple("order-a root ok")));
        host.set_test_agent_gateway(Some(Arc::new(move |name, _| match name {
            "order-a-child" => Some(child_gw.clone()),
            _ => None,
        })));

        let session = host
            .create(PreparedCreate::persistent(definition(), Context::new()))
            .await
            .unwrap();
        let root_id = entry_id(&session);

        // Start child in background and pause inside model invocation after `admit_operation`.
        let SpawnAgentOutcome::Background(child_rec) = session
            .spawn_agent(spawn_req(&root_id, "order-a-child", "work-a"))
            .await
            .unwrap()
        else {
            panic!("expected background");
        };
        child_entered.notified().await;

        // Fail the NEXT root Write commit once.
        let root_fail_once = Arc::new(AtomicUsize::new(0));
        let root_fail_c = root_fail_once.clone();
        session.set_writer_fault_hook(Some(Arc::new(move |stage| {
            if stage == CommitStage::Write && root_fail_c.fetch_add(1, Ordering::SeqCst) == 0 {
                FaultAction::Fail(std::io::Error::other("injected root write error"))
            } else {
                FaultAction::Continue
            }
        })));

        let w = session.submit(request("root-a-1")).unwrap();
        finished(&session, &w.work).await;
        until(|| matches!(session.status().save, SaveStatus::SaveBlocked { .. })).await;

        // Now release child while root retains `WriterState::pending` (`PendingWriterKind::RootCommit`).
        child_release.notify_one();
        until(|| {
            session
                .agent_snapshot(&child_rec.agent_id)
                .is_some_and(|s| s.active_operation_id.is_none())
        })
        .await;

        // Clear fault and call retry_save(): must resolve root commit FIRST, then deferred child outbox!
        session.set_writer_fault_hook(None);
        let save_status = session.retry_save().await.unwrap();
        assert!(matches!(save_status, SaveStatus::Saved { .. }));

        let target = session.target();
        host.close(&target, ClosePolicy::Save).await.unwrap();

        let reopened = host.open(target.conversation_id().clone()).await.unwrap();
        let child_read = reopened
            .read_agent(&child_rec.operation_id, 0, 1024)
            .await
            .unwrap();
        assert_eq!(child_read.status, OperationStatus::Completed);
        assert_eq!(child_read.page.content, "order-a child ok");
        assert!(
            reopened
                .read_agent_materials(&root_id)
                .unwrap()
                .context
                .blocks()
                .iter()
                .any(|b| format!("{:?}", b.content()).contains("order-a root ok"))
        );
        host.close(&reopened.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }

    // --- Order B: Child commit_outbox fails first at Write, then root turn finishes (`PendingCommit`). ---
    {
        let root = tempfile::tempdir().unwrap();
        let host = SessionHost::new(Some(root.path().into()));
        let root_entered = Arc::new(Notify::new());
        let root_release = Arc::new(Notify::new());
        let child_entered = Arc::new(Notify::new());
        let child_release = Arc::new(Notify::new());

        let main_gw = ScriptedGateway::new(
            vec![ScriptedStep {
                entered: Some(root_entered.clone()),
                release: Some(root_release.clone()),
                response_text: Some("order-b root ok".into()),
                tool_calls: Vec::new(),
                capture_blocks: None,
                wait_for_cancel: false,
            }],
            "fallback",
        );
        let child_gw = ScriptedGateway::new(
            vec![ScriptedStep {
                entered: Some(child_entered.clone()),
                release: Some(child_release.clone()),
                response_text: Some("order-b child ok".into()),
                tool_calls: Vec::new(),
                capture_blocks: None,
                wait_for_cancel: false,
            }],
            "fallback",
        );
        host.set_test_gateway(Some(main_gw));
        host.set_test_agent_gateway(Some(Arc::new(move |name, _| match name {
            "order-b-child" => Some(child_gw.clone()),
            _ => None,
        })));

        let session = host
            .create(PreparedCreate::persistent(definition(), Context::new()))
            .await
            .unwrap();
        let root_id = entry_id(&session);

        // Submit root turn FIRST while session is healthy, and pause inside model invocation.
        let w = session.submit(request("root-b-1")).unwrap();
        root_entered.notified().await;

        let SpawnAgentOutcome::Background(child_rec) = session
            .spawn_agent(spawn_req(&root_id, "order-b-child", "work-b"))
            .await
            .unwrap()
        else {
            panic!("expected background");
        };
        child_entered.notified().await;

        let outbox_fail_once = Arc::new(AtomicUsize::new(0));
        let outbox_fail_c = outbox_fail_once.clone();
        session.set_writer_fault_hook(Some(Arc::new(move |stage| {
            if stage == CommitStage::Write && outbox_fail_c.fetch_add(1, Ordering::SeqCst) == 0 {
                FaultAction::Fail(std::io::Error::other("injected outbox write error"))
            } else {
                FaultAction::Continue
            }
        })));

        // Child fails `commit_outbox` at Write -> `WriterState::pending_outbox` is active.
        child_release.notify_one();
        until(|| matches!(session.status().save, SaveStatus::SaveBlocked { .. })).await;

        // Now release in-flight root turn -> `saving::save` fails with `PendingCommit`
        // (`pending_attempted` must reset to `false` so retry refreshes revision after outbox!).
        root_release.notify_one();
        finished(&session, &w.work).await;
        until(|| {
            session.pending_commit_id().is_some()
                && matches!(session.status().save, SaveStatus::SaveBlocked { .. })
        })
        .await;

        // Clear fault and call retry_save(): must clear child outbox FIRST, then refresh & save root!
        session.set_writer_fault_hook(None);
        let save_status = session.retry_save().await.unwrap();
        assert!(matches!(save_status, SaveStatus::Saved { .. }));

        let target = session.target();
        host.close(&target, ClosePolicy::Save).await.unwrap();

        let reopened = host.open(target.conversation_id().clone()).await.unwrap();
        let child_read = reopened
            .read_agent(&child_rec.operation_id, 0, 1024)
            .await
            .unwrap();
        assert_eq!(child_read.status, OperationStatus::Completed);
        assert_eq!(child_read.page.content, "order-b child ok");
        assert!(
            reopened
                .read_agent_materials(&root_id)
                .unwrap()
                .context
                .blocks()
                .iter()
                .any(|b| format!("{:?}", b.content()).contains("order-b root ok"))
        );
        host.close(&reopened.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}

// ---------------------------------------------------------------------------
// 12. Concurrent `message_agent` to Same Idle `Reusable` Agent
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_message_agent_to_idle_reusable_never_orphans_creating() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let msg_entered = Arc::new(Notify::new());
    let msg_release = Arc::new(Notify::new());

    let child_gw = ScriptedGateway::new(
        vec![
            ScriptedStep::text("initial ok"),
            ScriptedStep {
                entered: Some(msg_entered.clone()),
                release: Some(msg_release.clone()),
                response_text: Some("followup ok".into()),
                tool_calls: Vec::new(),
                capture_blocks: None,
                wait_for_cancel: false,
            },
        ],
        "extra",
    );
    host.set_test_gateway(Some(ScriptedGateway::simple("main ok")));
    host.set_test_agent_gateway(Some(Arc::new(move |name, _| match name {
        "reusable-worker" => Some(child_gw.clone()),
        _ => None,
    })));

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let root_id = entry_id(&session);

    let mut spawn = spawn_req(&root_id, "reusable-worker", "boot");
    spawn.interaction = AgentInteraction::Reusable;
    spawn.execution_mode = OperationExecutionMode::Sync;
    let SpawnAgentOutcome::Completed(boot_out) = session.spawn_agent(spawn).await.unwrap() else {
        panic!("expected completed");
    };
    assert_eq!(boot_out.status, OperationStatus::Completed);
    let target_agent_id = boot_out.agent_id;

    let m1 = MessageAgentRequest {
        caller_agent_id: root_id.clone(),
        caller_operation_id: None,
        source_block_id: None,
        target_agent_id: target_agent_id.clone(),
        prompt: "first concurrent".into(),
        execution_mode: OperationExecutionMode::Background,
        on_parent_complete: OnParentComplete::Continue,
        timeout_secs: None,
        max_rounds: None,
        coalesce_key: None,
    };
    let m2 = MessageAgentRequest {
        prompt: "second concurrent".into(),
        ..m1.clone()
    };

    let (r1, r2) = tokio::join!(session.message_agent(m1), session.message_agent(m2));
    let (winner, loser) = match (r1, r2) {
        (Ok(SpawnAgentOutcome::Background(rec)), Err(err))
        | (Err(err), Ok(SpawnAgentOutcome::Background(rec))) => (rec, err),
        other => panic!("expected exactly one winner and one AgentBusy error, got {other:?}"),
    };
    assert!(
        matches!(loser, CoordinatorError::AgentBusy { .. }),
        "expected AgentBusy, got {loser:?}"
    );

    msg_entered.notified().await;
    msg_release.notify_one();
    session.wait_background_tasks_settled().await;
    assert_eq!(
        session
            .read_agent(&winner.operation_id, 0, 1024)
            .await
            .unwrap()
            .status,
        OperationStatus::Completed
    );

    let target = session.target();
    let conv_id = target.conversation_id().as_str().to_owned();
    host.close(&target, ClosePolicy::Save).await.unwrap();

    // SQLite must contain ONLY the 2 admitted operations (boot + winner), ZERO orphaned `creating` rows!
    let store = host.inner.store.as_ref().unwrap();
    let ops = store.list_operations(&conv_id).unwrap();
    assert_eq!(ops.len(), 2, "must not orphan failed concurrent admission");
    assert!(ops.iter().all(|o| o.status == OperationStatus::Completed));
}

// ---------------------------------------------------------------------------
// 13. `message_agent` Enforces `max_rounds` & Rejects `timeout_secs > target`
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn message_agent_enforces_max_rounds_and_rejects_timeout_widening() {
    let root = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let ws_root = std::fs::canonicalize(ws.path()).unwrap();
    std::fs::write(ws_root.join("note.txt"), "hello").unwrap();

    let host = SessionHost::new(Some(root.path().into()));
    let child_gw = ScriptedGateway::new(
        vec![
            ScriptedStep::text("boot ok"),
            // Message turn tries 2 rounds (tool + text), but `max_rounds = Some(1)` stops after round 1!
            ScriptedStep::tool("read_file", serde_json::json!({ "path": "note.txt" })),
            ScriptedStep::text("should never reach round 2"),
        ],
        "fallback",
    );
    let child_calls = child_gw.calls.clone();

    host.set_test_gateway(Some(ScriptedGateway::simple("main ok")));
    host.set_test_agent_gateway(Some(Arc::new(move |name, _| match name {
        "bounded-child" => Some(child_gw.clone()),
        _ => None,
    })));

    let mut def = definition();
    def.workspace_root = Some(ws_root);
    def.tools = vec![WorkspaceTool::ReadFile];
    def.permission_mode = Some(PermissionMode::ReadOnly);
    def.work_timeout_secs = None;
    def.max_model_rounds = 6;
    def.max_tool_calls = 6;

    let session = host
        .create(PreparedCreate::persistent(def, Context::new()))
        .await
        .unwrap();
    let root_id = entry_id(&session);

    let mut spawn = spawn_req(&root_id, "bounded-child", "boot");
    spawn.interaction = AgentInteraction::Reusable;
    spawn.execution_mode = OperationExecutionMode::Sync;
    spawn.narrowing = ChildNarrowingRequest {
        work_timeout_secs: Some(5),
        max_model_rounds: Some(4),
        ..Default::default()
    };
    let SpawnAgentOutcome::Completed(boot_out) = session.spawn_agent(spawn).await.unwrap() else {
        panic!("expected completed");
    };
    assert_eq!(boot_out.status, OperationStatus::Completed);
    assert_eq!(child_calls.load(Ordering::SeqCst), 1);

    // 1) timeout_secs (10) > target.work_timeout_secs (5) must fail even though root has None!
    let widen_req = MessageAgentRequest {
        caller_agent_id: root_id.clone(),
        caller_operation_id: None,
        source_block_id: None,
        target_agent_id: boot_out.agent_id.clone(),
        prompt: "too long".into(),
        execution_mode: OperationExecutionMode::Sync,
        on_parent_complete: OnParentComplete::Continue,
        timeout_secs: Some(10),
        max_rounds: None,
        coalesce_key: None,
    };
    assert!(matches!(
        session.message_agent(widen_req).await,
        Err(CoordinatorError::CapabilityMismatch(_))
    ));

    // 2) max_rounds = Some(1) limits the child turn to 1 model round (interrupted before round 2!).
    let bounded_req = MessageAgentRequest {
        caller_agent_id: root_id,
        caller_operation_id: None,
        source_block_id: None,
        target_agent_id: boot_out.agent_id,
        prompt: "read then stop".into(),
        execution_mode: OperationExecutionMode::Sync,
        on_parent_complete: OnParentComplete::Continue,
        timeout_secs: Some(5),
        max_rounds: Some(1),
        coalesce_key: None,
    };
    let SpawnAgentOutcome::Completed(msg_out) = session.message_agent(bounded_req).await.unwrap()
    else {
        panic!("expected completed");
    };
    assert_eq!(msg_out.status, OperationStatus::Failed);
    // Boot took 1 call, bounded message took ONLY 1 call (never reached round 2).
    assert_eq!(child_calls.load(Ordering::SeqCst), 2);

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 14. Transient Session Without `context_window` Receives Mid-Turn Injection
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_without_context_window_receives_mid_turn_inbox_injection() {
    let ws = tempfile::tempdir().unwrap();
    let ws_root = std::fs::canonicalize(ws.path()).unwrap();
    std::fs::write(ws_root.join("data.txt"), "payload").unwrap();

    let host = SessionHost::new(None);
    let round1_entered = Arc::new(Notify::new());
    let round1_release = Arc::new(Notify::new());
    let round2_captured = Arc::new(Mutex::new(Vec::new()));

    let main_gw = ScriptedGateway::new(
        vec![
            ScriptedStep {
                entered: Some(round1_entered.clone()),
                release: Some(round1_release.clone()),
                response_text: None,
                tool_calls: vec![ToolCallDraft::new(
                    "read_file",
                    serde_json::json!({ "path": "data.txt" }),
                )],
                capture_blocks: None,
                wait_for_cancel: false,
            },
            ScriptedStep {
                capture_blocks: Some(round2_captured.clone()),
                ..ScriptedStep::text("transient parent final")
            },
        ],
        "fallback",
    );
    let child_gw = ScriptedGateway::simple("transient child finished");

    host.set_test_gateway(Some(main_gw));
    host.set_test_agent_gateway(Some(Arc::new(move |name, _| match name {
        "transient-bg" => Some(child_gw.clone()),
        _ => None,
    })));

    let mut def = definition();
    def.context_window = None;
    def.workspace_root = Some(ws_root);
    def.tools = vec![WorkspaceTool::ReadFile];
    def.permission_mode = Some(PermissionMode::ReadOnly);
    def.max_model_rounds = 4;
    def.max_tool_calls = 4;

    let session = host
        .create(PreparedCreate::transient(def, Context::new()))
        .await
        .unwrap();
    let root_id = entry_id(&session);

    let root_work = session.submit(request("transient-turn")).unwrap();
    round1_entered.notified().await;

    // Complete background child while root is between round 1 and round 2!
    let SpawnAgentOutcome::Background(_) = session
        .spawn_agent(spawn_req(&root_id, "transient-bg", "bg"))
        .await
        .unwrap()
    else {
        panic!("expected background");
    };
    session.wait_background_tasks_settled().await;
    round1_release.notify_one();

    finished(&session, &root_work.work).await;
    let r2_text: String = round2_captured
        .lock()
        .unwrap()
        .iter()
        .map(extract_block_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        r2_text.contains("<task-notification") && r2_text.contains("transient child finished"),
        "InboxOnlyPreparer must inject <task-notification> when context_window is None, got: {r2_text}"
    );

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

// ---------------------------------------------------------------------------
// 15. `InboxDrainHandle::restore_drained` Preserves Events on Failed Submit
// ---------------------------------------------------------------------------
#[test]
fn inbox_drain_restore_on_failed_submit_preserves_events() {
    use crate::events::InboxDrainHandle;

    let handle = InboxDrainHandle::with_initial(
        vec![
            crate::events::InboxEvent {
                event_id: 1,
                session_id: "01900000-0000-7000-8000-000000000001".into(),
                target_agent_id: "01900000-0000-7000-8000-000000000002".into(),
                priority: EventPriority::Normal,
                source_kind: EventSourceKind::AgentOperation,
                source_id: "op-1".into(),
                coalesce_key: Some("k".into()),
                wake_policy: WakePolicy::WakeIfIdle,
                payload_json: "{\"msg\":\"old\"}".into(),
                state: crate::events::EventDeliveryState::Pending,
                claimed_op_id: None,
                created_at_ms: 100,
                delivered_at_ms: None,
            },
            crate::events::InboxEvent {
                event_id: 2,
                session_id: "01900000-0000-7000-8000-000000000001".into(),
                target_agent_id: "01900000-0000-7000-8000-000000000002".into(),
                priority: EventPriority::Normal,
                source_kind: EventSourceKind::AgentOperation,
                source_id: "op-2".into(),
                coalesce_key: Some("k".into()),
                wake_policy: WakePolicy::WakeIfIdle,
                payload_json: "{\"msg\":\"new\"}".into(),
                state: crate::events::EventDeliveryState::Pending,
                claimed_op_id: None,
                created_at_ms: 200,
                delivered_at_ms: None,
            },
        ],
        4096,
    );

    let drained = handle.drain_notification_detailed().unwrap();
    assert_eq!(handle.pending_count(), 0);
    assert_eq!(drained.consumed.delivered_ids, vec![2]);
    assert_eq!(drained.consumed.dropped_ids, vec![1]);

    // Simulated submit failure restores both events and clears consumed state.
    handle.restore_drained(&drained);
    assert_eq!(handle.pending_count(), 2);
    assert!(handle.consumed_snapshot().delivered_ids.is_empty());
    assert!(handle.consumed_snapshot().dropped_ids.is_empty());

    // Subsequent drain delivers them cleanly.
    let re_drained = handle.drain_notification_detailed().unwrap();
    assert_eq!(re_drained.consumed.delivered_ids, vec![2]);
    assert_eq!(re_drained.consumed.dropped_ids, vec![1]);
    assert_eq!(handle.pending_count(), 0);
}

// ---------------------------------------------------------------------------
// 16. `wait_settled`, `cancel_all_active`, Root Wakeup Events, and Lock-Free Reads
// ---------------------------------------------------------------------------
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_settled_returns_none_immediately_when_idle() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    host.set_test_gateway(Some(ScriptedGateway::simple("idle")));
    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();

    let settled = tokio::time::timeout(Duration::from_secs(2), session.wait_settled())
        .await
        .expect("idle wait_settled must return immediately")
        .unwrap();
    assert_eq!(settled, None);

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_settled_tracks_root_reactive_wakeup_and_publishes_plane2_events() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let main_gw = ScriptedGateway::new(
        vec![
            ScriptedStep::text("initial done"),
            ScriptedStep::text("wakeup merged answer"),
        ],
        "fallback",
    );
    let bg_entered = Arc::new(Notify::new());
    let bg_release = Arc::new(Notify::new());
    let bg_gw = ScriptedGateway::new(
        vec![ScriptedStep {
            entered: Some(bg_entered.clone()),
            release: Some(bg_release.clone()),
            response_text: Some("bg payload".into()),
            tool_calls: Vec::new(),
            capture_blocks: None,
            wait_for_cancel: false,
        }],
        "bg fallback",
    );

    host.set_test_gateway(Some(main_gw.clone()));
    host.set_test_agent_gateway(Some(Arc::new(move |name, _| {
        if name == "worker" {
            Some(bg_gw.clone())
        } else {
            None
        }
    })));

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let root_id = entry_id(&session);
    let mut events = session.subscribe_events();

    let initial = session.submit(request("start")).unwrap();
    finished(&session, &initial.work).await;
    session.wait_saved().await.unwrap();

    // Verify wait_settled returns None when initial work already finished & saved.
    assert_eq!(session.wait_settled().await.unwrap(), None);

    // Spawn a background child task, wait until it is running, then wait_settled in parallel.
    let SpawnAgentOutcome::Background(bg_rec) = session
        .spawn_agent(spawn_req(&root_id, "worker", "run bg"))
        .await
        .unwrap()
    else {
        panic!("expected background");
    };
    tokio::time::timeout(Duration::from_secs(5), bg_entered.notified())
        .await
        .unwrap();

    let session_waiter = session.clone();
    let wait_task = tokio::spawn(async move { session_waiter.wait_settled().await });

    // Let background child complete, triggering root reactive wakeup.
    bg_release.notify_one();
    let settled_work = tokio::time::timeout(Duration::from_secs(5), wait_task)
        .await
        .expect("wait_settled should settle after reactive wakeup")
        .unwrap()
        .unwrap();
    let wakeup_work = settled_work.expect("must return reactive wakeup WorkRef");
    assert_ne!(wakeup_work, initial.work);

    let obs = session.observe(&wakeup_work).unwrap();
    let crate::session::WorkState::Finished(res) = obs.state else {
        panic!("expected finished wakeup work");
    };
    assert_eq!(crate::application::WorkOutcome::of(&res).kind, "completed");

    // Verify Plane 2 OperationStateChanged Running + Completed for root reactive wakeup!
    let mut saw_root_running = false;
    let mut saw_root_completed = false;
    while let Ok(ev) = events.try_recv() {
        if let SessionEvent::OperationStateChanged {
            operation_id,
            target_agent_id,
            parent_operation_id,
            status,
            ..
        } = &*ev
            && operation_id == &wakeup_work.turn_id.0
        {
            assert_eq!(target_agent_id, &root_id);
            assert_eq!(parent_operation_id, &None);
            match status {
                OperationStatus::Running => saw_root_running = true,
                OperationStatus::Completed => saw_root_completed = true,
                _ => {}
            }
        }
    }
    assert!(saw_root_running, "missing root wakeup Running event");
    assert!(saw_root_completed, "missing root wakeup Completed event");

    // Verify lock-free inspect, inspect_agent, and read_saved_history!
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    let ro_host = SessionHost::new(Some(root.path().into()));
    let conv_id = initial.work.conversation_id.clone();
    let inspection = ro_host.inspect(&conv_id).await.unwrap();
    assert_eq!(inspection.agents.len(), 2);
    assert!(
        inspection
            .agents
            .iter()
            .any(|a| a.is_entry && a.name == "main")
    );
    assert!(
        inspection
            .agents
            .iter()
            .any(|a| !a.is_entry && a.agent_id == bg_rec.agent_id && a.name == "worker")
    );

    let by_id = ro_host
        .inspect_agent(&conv_id, &bg_rec.agent_id)
        .await
        .unwrap();
    assert_eq!(by_id.agent_id, bg_rec.agent_id);
    assert_eq!(by_id.operations.len(), 1);
    assert_eq!(by_id.operations[0].status, OperationStatus::Completed);

    let by_name = ro_host.inspect_agent(&conv_id, "worker").await.unwrap();
    assert_eq!(by_name.agent_id, bg_rec.agent_id);

    let missing_err = ro_host.inspect_agent(&conv_id, "nope").await.unwrap_err();
    assert!(matches!(missing_err, ApplicationError::NotFound));

    let child_hist = ro_host
        .read_saved_history(&conv_id, Some(&bg_rec.agent_id), None)
        .await
        .unwrap();
    assert!(!child_hist.blocks.is_empty());
    let root_hist = ro_host
        .read_saved_history(&conv_id, None, None)
        .await
        .unwrap();
    assert!(root_hist.blocks.len() > child_hist.blocks.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_all_active_cancels_running_background_task_and_prevents_zombie_wakeup() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let main_calls = Arc::new(AtomicUsize::new(0));
    let main_gw = Arc::new(ScriptedGateway {
        calls: main_calls.clone(),
        steps: Mutex::new(VecDeque::from([ScriptedStep::text("root done")])),
        default_text: "unexpected wakeup".into(),
        captured_requests: Arc::new(Mutex::new(Vec::new())),
    });
    let bg_entered = Arc::new(Notify::new());
    let bg_gw = ScriptedGateway::new(
        vec![ScriptedStep {
            entered: Some(bg_entered.clone()),
            release: None,
            response_text: None,
            tool_calls: Vec::new(),
            capture_blocks: None,
            wait_for_cancel: true,
        }],
        "bg fallback",
    );

    host.set_test_gateway(Some(main_gw));
    host.set_test_agent_gateway(Some(Arc::new(move |name, _| {
        if name == "blocker" {
            Some(bg_gw.clone())
        } else {
            None
        }
    })));

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let root_id = entry_id(&session);

    let initial = session.submit(request("go")).unwrap();
    finished(&session, &initial.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(main_calls.load(Ordering::SeqCst), 1);

    let SpawnAgentOutcome::Background(bg_rec) = session
        .spawn_agent(spawn_req(&root_id, "blocker", "block forever"))
        .await
        .unwrap()
    else {
        panic!("expected background");
    };
    tokio::time::timeout(Duration::from_secs(5), bg_entered.notified())
        .await
        .unwrap();

    // Cancel all active operations and await convergence.
    session.cancel_all_active();
    let settled = tokio::time::timeout(Duration::from_secs(5), session.wait_settled())
        .await
        .expect("cancel_all_active + wait_settled must converge promptly")
        .unwrap();
    assert_eq!(settled, None);
    assert_eq!(session.active_finite_background_tasks(), 0);

    let read = session
        .read_agent(&bg_rec.operation_id, 0, 1024)
        .await
        .unwrap();
    assert_eq!(read.status, OperationStatus::Cancelled);
    // No zombie reactive wakeup occurred on root.
    assert_eq!(main_calls.load(Ordering::SeqCst), 1);

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inspect_agent_reports_ambiguous_duplicate_names() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    host.set_test_gateway(Some(ScriptedGateway::simple("ok")));
    host.set_test_agent_gateway(Some(Arc::new(|_, _| {
        Some(ScriptedGateway::simple("child ok"))
    })));

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let root_id = entry_id(&session);

    let SpawnAgentOutcome::Background(first) = session
        .spawn_agent(spawn_req(&root_id, "dup", "one"))
        .await
        .unwrap()
    else {
        panic!("expected background");
    };
    let SpawnAgentOutcome::Background(second) = session
        .spawn_agent(spawn_req(&root_id, "dup", "two"))
        .await
        .unwrap()
    else {
        panic!("expected background");
    };
    session.wait_settled().await.unwrap();
    let conv_id = session.target().conversation_id().clone();
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    let ro_host = SessionHost::new(Some(root.path().into()));
    let err = ro_host.inspect_agent(&conv_id, "dup").await.unwrap_err();
    match err {
        ApplicationError::AmbiguousAgent { name, candidates } => {
            assert_eq!(name, "dup");
            assert_eq!(candidates.len(), 2);
            assert!(candidates.contains(&first.agent_id));
            assert!(candidates.contains(&second.agent_id));
        }
        other => panic!("expected AmbiguousAgent, got {other:?}"),
    }
    // Exact UUID selection still succeeds.
    assert_eq!(
        ro_host
            .inspect_agent(&conv_id, &second.agent_id)
            .await
            .unwrap()
            .agent_id,
        second.agent_id
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wait_settled_after_catches_fast_completed_reactive_wakeup() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let main_calls = Arc::new(AtomicUsize::new(0));
    let main_gw = Arc::new(ScriptedGateway {
        calls: main_calls.clone(),
        steps: Mutex::new(VecDeque::from([
            ScriptedStep::text("initial ok"),
            ScriptedStep::text("fast wakeup ok"),
        ])),
        default_text: "fallback".into(),
        captured_requests: Arc::new(Mutex::new(Vec::new())),
    });
    host.set_test_gateway(Some(main_gw));
    host.set_test_agent_gateway(Some(Arc::new(|_, _| {
        Some(ScriptedGateway::simple("fast child ok"))
    })));

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let root_id = entry_id(&session);

    let initial = session.submit(request("go")).unwrap();
    finished(&session, &initial.work).await;
    session.wait_saved().await.unwrap();

    // Spawn child and let BOTH the child and the resulting root reactive wakeup
    // finish completely BEFORE calling `wait_settled_after(&initial.work)`.
    let SpawnAgentOutcome::Background(_) = session
        .spawn_agent(spawn_req(&root_id, "fast-worker", "quick"))
        .await
        .unwrap()
    else {
        panic!("expected background");
    };
    until(|| main_calls.load(Ordering::SeqCst) >= 2).await;
    session.wait_settled().await.unwrap();

    let after = tokio::time::timeout(
        Duration::from_secs(2),
        session.wait_settled_after(&initial.work),
    )
    .await
    .expect("wait_settled_after must return immediately once settled")
    .unwrap();
    let wakeup_work = after.expect("fast-completed wakeup must not be missed");
    assert_ne!(wakeup_work, initial.work);
    let obs = session.observe(&wakeup_work).unwrap();
    let crate::session::WorkState::Finished(res) = obs.state else {
        panic!("expected finished wakeup");
    };
    assert_eq!(crate::application::WorkOutcome::of(&res).kind, "completed");

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn low_retained_capacity_evicts_unread_auto_wakeup_turns_without_hanging() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let main_calls = Arc::new(AtomicUsize::new(0));
    let main_gw = Arc::new(ScriptedGateway {
        calls: main_calls.clone(),
        steps: Mutex::new(VecDeque::from([
            ScriptedStep::text("user turn"),
            ScriptedStep::text("wakeup 1"),
            ScriptedStep::text("wakeup 2"),
        ])),
        default_text: "fallback".into(),
        captured_requests: Arc::new(Mutex::new(Vec::new())),
    });
    host.set_test_gateway(Some(main_gw));
    host.set_test_agent_gateway(Some(Arc::new(|_, _| {
        Some(ScriptedGateway::simple("bg ok"))
    })));

    let mut def = definition();
    def.retained_work_capacity = 1;
    let session = host
        .create(PreparedCreate::persistent(def, Context::new()))
        .await
        .unwrap();
    let root_id = entry_id(&session);

    // User work #1 fills the single retained slot, then is read by `finished`.
    let initial = session.submit(request("start")).unwrap();
    finished(&session, &initial.work).await;
    session.wait_saved().await.unwrap();

    // First background child triggers auto-wakeup #1 (unread!).
    let SpawnAgentOutcome::Background(_) = session
        .spawn_agent(spawn_req(&root_id, "w1", "task 1"))
        .await
        .unwrap()
    else {
        panic!("expected background");
    };
    let first_wakeup = tokio::time::timeout(
        Duration::from_secs(5),
        session.wait_settled_after(&initial.work),
    )
    .await
    .expect("first auto-wakeup must settle")
    .unwrap()
    .expect("first wakeup WorkRef");

    // Deliberately DO NOT call `observe(&first_wakeup)`, leaving the single
    // retained slot occupied by an unread auto-wakeup work!
    let SpawnAgentOutcome::Background(_) = session
        .spawn_agent(spawn_req(&root_id, "w2", "task 2"))
        .await
        .unwrap()
    else {
        panic!("expected background");
    };
    let second_wakeup = tokio::time::timeout(
        Duration::from_secs(5),
        session.wait_settled_after(&first_wakeup),
    )
    .await
    .expect("second auto-wakeup must evict unread first auto-wakeup and settle")
    .unwrap()
    .expect("second wakeup WorkRef");
    assert_ne!(second_wakeup, first_wakeup);
    assert_eq!(main_calls.load(Ordering::SeqCst), 3);

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

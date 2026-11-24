pub(super) use super::super::*;
pub(super) use crate::definition::{CredentialRef, Provider, SessionDefinition};
pub(super) use crate::session::{Session, SubmitRequest, WorkResult, WorkState};
pub(super) use crate::store::{CommitStage, FaultAction, FaultHook};
pub(super) use causa::kernel::{BlockContent, BlockId, BlockMeta, Context, ContextBlock};
pub(super) use causa::runtime::{ToolExecutor, ToolExecutorOptions, TurnRunner};
pub(super) use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicUsize, Ordering},
};
pub(super) use std::time::Duration;
pub(super) use tokio::sync::Notify;

pub(super) fn definition() -> SessionDefinition {
    SessionDefinition {
        provider: Provider::OpenAiChat,
        model: "fixture".into(),
        endpoint: None,
        credential: CredentialRef::Environment("TARS_UNUSED_FIXTURE".into()),
        work_timeout_secs: None,
        retained_work_capacity: 8,
        max_model_rounds: 1,
        max_tool_calls: 0,
        max_tokens: None,
        reasoning_effort: None,
        temperature: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: None,
        tools: Vec::new(),
        tool_limits: Default::default(),
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
pub(super) fn request(key: &str) -> SubmitRequest {
    SubmitRequest {
        request_key: key.into(),
        blocks: vec![ContextBlock::new(
            BlockId::new(uuid::Uuid::now_v7()),
            BlockContent::Message {
                role: causa::kernel::Role::Assistant,
                parts: vec![],
            },
            BlockMeta::default(),
        )],
    }
}
pub(super) struct Gateway {
    pub(super) calls: Arc<AtomicUsize>,
    pub(super) entered: Option<Arc<Notify>>,
    pub(super) release: Option<Arc<Notify>>,
    pub(super) panic: bool,
}
#[async_trait::async_trait]
impl causa::kernel::ModelGateway for Gateway {
    async fn invoke(
        &self,
        _: &causa::kernel::ModelRequest,
        _: &causa::kernel::CallControl,
    ) -> Result<causa::kernel::ModelOutput, causa::kernel::ModelInvokeError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert!(!self.panic, "injected worker panic");
        if let Some(entered) = &self.entered {
            entered.notify_one();
        }
        if let Some(release) = &self.release {
            release.notified().await;
        }
        Ok(causa::kernel::ModelOutput::new(
            causa::kernel::ModelResponse {
                items: vec![causa::kernel::ModelItem::text("completed original")],
            },
            causa::kernel::ModelStopReason::EndTurn,
        ))
    }
}
pub(super) fn fixture(host: &SessionHost, gateway: Gateway) -> Arc<AtomicUsize> {
    let assemblies = Arc::new(AtomicUsize::new(0));
    let count = assemblies.clone();
    let runner = Arc::new(TurnRunner::new(
        Arc::new(gateway),
        Arc::new(ToolExecutor::new(vec![], ToolExecutorOptions::default()).unwrap()),
    ));
    *host.inner.assemble.lock().unwrap() = Some(Arc::new(move |definition, id, context| {
        count.fetch_add(1, Ordering::SeqCst);
        let runner = runner.clone();
        let options = definition.run_options();
        Ok(Session::new(
            id,
            context,
            move |_, _| {
                let runner = runner.clone();
                let options = options.clone();
                async move { Ok((runner, options)) }
            },
            definition.session_config(),
        ))
    }));
    assemblies
}
pub(super) fn normal_fixture(host: &SessionHost) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    fixture(
        host,
        Gateway {
            calls: calls.clone(),
            entered: None,
            release: None,
            panic: false,
        },
    );
    calls
}
pub(super) struct Gate {
    pub(super) entered: Notify,
    pub(super) released: Mutex<bool>,
    pub(super) release: Condvar,
}
impl Gate {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Notify::new(),
            released: Mutex::new(false),
            release: Condvar::new(),
        })
    }
    pub(super) fn hook(self: &Arc<Self>, stage: CommitStage) -> FaultHook {
        let gate = self.clone();
        let hits = AtomicUsize::new(0);
        Arc::new(move |current| {
            if current == stage && hits.fetch_add(1, Ordering::SeqCst) == 0 {
                gate.entered.notify_one();
                let mut released = gate.released.lock().unwrap();
                while !*released {
                    released = gate.release.wait(released).unwrap();
                }
            }
            FaultAction::Continue
        })
    }
    pub(super) async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .unwrap();
    }
    pub(super) fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}
pub(crate) async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("managed state transition timed out");
}
pub(super) async fn finished(session: &ManagedSession, work: &crate::session::WorkRef) {
    assert!(matches!(
        session
            .wait(work, Duration::from_secs(5))
            .await
            .unwrap()
            .state,
        WorkState::Finished(_)
    ));
}

pub(super) struct ToolGateway;
#[async_trait::async_trait]
impl causa::kernel::ModelGateway for ToolGateway {
    async fn invoke(
        &self,
        _: &causa::kernel::ModelRequest,
        _: &causa::kernel::CallControl,
    ) -> Result<causa::kernel::ModelOutput, causa::kernel::ModelInvokeError> {
        Ok(causa::kernel::ModelOutput::new(
            causa::kernel::ModelResponse {
                items: vec![causa::kernel::ModelItem::tool_call(
                    causa::kernel::ToolCallDraft::new("fixture", serde_json::json!({})),
                )],
            },
            causa::kernel::ModelStopReason::ToolUse,
        ))
    }
}
pub(super) struct HeldTool {
    pub(super) entered: Arc<Notify>,
    pub(super) release: Arc<Notify>,
    pub(super) stopped: Arc<AtomicUsize>,
}
#[async_trait::async_trait]
impl causa::kernel::Tool for HeldTool {
    fn definition(&self) -> causa::kernel::ToolDefinition {
        causa::kernel::ToolDefinition::new(
            "fixture",
            "fixture",
            serde_json::json!({"type":"object"}),
        )
    }
    async fn execute(
        &self,
        call: &causa::kernel::ToolCallContext,
        _: &causa::kernel::CallControl,
        _store: Option<&dyn causa::kernel::ArtifactStore>,
    ) -> causa::kernel::ToolResultPayload {
        struct Stopped(Arc<AtomicUsize>);
        impl Drop for Stopped {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let _stopped = Stopped(self.stopped.clone());
        self.entered.notify_one();
        self.release.notified().await;
        causa::kernel::ToolResultPayload::new(
            call.call_block_id,
            causa::kernel::ToolResultStatus::Succeeded,
            causa::kernel::ToolOutput::new(serde_json::json!({"actual":true})),
        )
    }
}
pub(super) fn custom_runner(host: &SessionHost, runner: Arc<TurnRunner>) {
    *host.inner.assemble.lock().unwrap() = Some(Arc::new(move |definition, id, context| {
        let runner = runner.clone();
        let options = definition.run_options();
        Ok(Session::new(
            id,
            context,
            move |_, _| {
                let runner = runner.clone();
                let options = options.clone();
                async move { Ok((runner, options)) }
            },
            definition.session_config(),
        ))
    }));
}

pub(super) struct CollisionBatch(pub(super) BlockId);
#[async_trait::async_trait]
impl causa::kernel::ToolBatchProcessor for CollisionBatch {
    async fn process(
        &self,
        batch: &mut causa::kernel::ToolBatch,
        _: &causa::kernel::ProcessorContext<'_>,
    ) -> Result<(), causa::kernel::ProcessorError> {
        let call = batch.calls()[0].call().clone();
        batch
            .resolve_at(0, self.0, {
                let mut result = causa::kernel::ToolResultPayload::new(
                    call.call_block_id,
                    causa::kernel::ToolResultStatus::Rejected,
                    causa::kernel::ToolOutput::new(serde_json::json!({"original":true})),
                );
                result.media = vec![causa::kernel::MediaRef::new(
                    "image/png",
                    "uncommitted original asset",
                )];
                result
            })
            .unwrap();
        Ok(())
    }
}

/// Definition used by the real assembly path with a scripted model.
pub(super) fn compacting_definition(context_window: Option<u64>) -> SessionDefinition {
    let mut definition = definition();
    // The real assembly resolves this environment variable; no request uses it.
    definition.credential = CredentialRef::Environment("PATH".into());
    definition.max_model_rounds = 4;
    definition.context_window = context_window;
    definition.auto_compact = true;
    definition.compact_threshold = 70;
    // Keep single-agent compaction/recall fixtures isolated from default multi-agent
    // tool declarations so small FIXTURE_WINDOW thresholds stay deterministic.
    definition.prelude.agents_enabled = false;
    definition
}

/// Window for compaction fixtures, in tokens.
///
/// Every persistent session declares `recall`, about 440 estimated tokens,
/// and compaction gives up when that fixed prefix is over half the trigger
/// line (70% of the window). The line therefore has to stay above twice the
/// declaration.
pub(super) const FIXTURE_WINDOW: u64 = 2_000;

/// Material an earlier work left behind: three large user messages and an
/// answer, about 1,400 estimated tokens.
///
/// A work's own input is protected from its compaction, so a fixture that
/// compacts needs material from before the work. With the `recall`
/// declaration it crosses the trigger line of [`FIXTURE_WINDOW`].
pub(super) fn earlier_conversation() -> Context {
    Context::from_blocks(vec![
        text_block(1_200),
        text_block(1_200),
        text_block(1_200),
        ContextBlock::text(
            BlockId::new(uuid::Uuid::now_v7()),
            causa::kernel::Role::Assistant,
            "earlier answer",
        ),
    ])
    .unwrap()
}

/// A short request for the work that compacts the earlier conversation.
pub(super) fn short_request() -> Vec<ContextBlock> {
    vec![ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        causa::kernel::Role::User,
        "continue the fixture task",
    )]
}

/// One user message whose size makes the byte-based estimator predictable.
pub(super) fn text_block(bytes: usize) -> ContextBlock {
    ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        causa::kernel::Role::User,
        "x".repeat(bytes),
    )
}

/// How the scripted model answers a summary request.
#[derive(Clone, Debug)]
pub(super) enum SummaryBehavior {
    /// Answer with this text.
    Text(String),
    /// Report a refusal.
    Fail,
    /// Answer with an empty string.
    Empty,
    /// Wait for release; a cancelled control is reported as cancelled.
    Block,
}

/// Scripted gateway distinguishing summary requests from ordinary rounds.
pub(super) struct CompactionGateway {
    pub(super) summary_calls: Arc<AtomicUsize>,
    pub(super) main_calls: Arc<AtomicUsize>,
    pub(super) summary_inputs: Mutex<Vec<Vec<ContextBlock>>>,
    pub(super) main_inputs: Mutex<Vec<Vec<ContextBlock>>>,
    pub(super) summary_behavior: Mutex<SummaryBehavior>,
    pub(super) main_overflow: std::sync::atomic::AtomicBool,
    /// Ordinary rounds report no usage, as some providers do.
    pub(super) main_without_usage: std::sync::atomic::AtomicBool,
    pub(super) entered: Arc<Notify>,
    pub(super) release: Arc<Notify>,
}

impl CompactionGateway {
    pub(super) fn new(behavior: SummaryBehavior) -> Arc<Self> {
        Arc::new(Self {
            summary_calls: Arc::new(AtomicUsize::new(0)),
            main_calls: Arc::new(AtomicUsize::new(0)),
            summary_inputs: Mutex::new(Vec::new()),
            main_inputs: Mutex::new(Vec::new()),
            summary_behavior: Mutex::new(behavior),
            main_overflow: std::sync::atomic::AtomicBool::new(false),
            main_without_usage: std::sync::atomic::AtomicBool::new(false),
            entered: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        })
    }

    pub(super) fn summary_text(&self) -> String {
        "Goal: finish the fixture task. Decisions: none. Progress: one step done. Next steps: report the result. Relevant files: none. Important context: fixture summary body."
            .to_owned()
    }
}

pub(super) fn is_summary_request(request: &causa::kernel::ModelRequest) -> bool {
    request.frame.blocks.iter().any(|block| {
        let causa::kernel::BlockContent::Message { parts, .. } = block.content() else {
            return false;
        };
        parts.iter().any(|part| {
            let causa::kernel::ContentPart::Text(text) = part else {
                return false;
            };
            text.0.contains("Prepare a continuation brief")
        })
    })
}

/// Input tokens the fixture reports for a conversation round. A brief
/// re-declares the session's tools (`recall` at least), so a compaction only
/// shrinks while this stays above that declaration's estimate; tests with a
/// 1,000-token window also need the reported size below the window.
pub(crate) const MAIN_INPUT_TOKENS: usize = 600;

#[async_trait::async_trait]
impl causa::kernel::ModelGateway for CompactionGateway {
    async fn invoke(
        &self,
        request: &causa::kernel::ModelRequest,
        control: &causa::kernel::CallControl,
    ) -> Result<causa::kernel::ModelOutput, causa::kernel::ModelInvokeError> {
        if is_summary_request(request) {
            self.summary_calls.fetch_add(1, Ordering::SeqCst);
            self.summary_inputs
                .lock()
                .unwrap()
                .push(request.frame.blocks.clone());
            let behavior = self.summary_behavior.lock().unwrap().clone();
            match behavior {
                SummaryBehavior::Text(text) => Ok(summary_output(&text)),
                SummaryBehavior::Fail => Err(causa::kernel::ModelInvokeError::new(
                    causa::kernel::ModelInvokeErrorKind::Permanent,
                    "summary refused",
                )),
                SummaryBehavior::Empty => Ok(summary_output("")),
                SummaryBehavior::Block => {
                    self.entered.notify_one();
                    tokio::select! {
                        biased;
                        _ = control.cancellation_token().cancelled() => {
                            Err(causa::kernel::ModelInvokeError::new(
                                causa::kernel::ModelInvokeErrorKind::Cancelled,
                                "summary cancelled",
                            ))
                        }
                        _ = self.release.notified() => Ok(summary_output(&self.summary_text())),
                    }
                }
            }
        } else {
            self.main_calls.fetch_add(1, Ordering::SeqCst);
            self.main_inputs
                .lock()
                .unwrap()
                .push(request.frame.blocks.clone());
            let overflow = self
                .main_overflow
                .swap(false, std::sync::atomic::Ordering::SeqCst);
            let (stop_reason, text) = if overflow {
                (
                    causa::kernel::ModelStopReason::ContextWindowExceeded,
                    "context window exceeded",
                )
            } else {
                (
                    causa::kernel::ModelStopReason::EndTurn,
                    "completed fixture work",
                )
            };
            let mut output = causa::kernel::ModelOutput::new(
                causa::kernel::ModelResponse {
                    items: vec![causa::kernel::ModelItem::text(text)],
                },
                stop_reason,
            );
            if !self.main_without_usage.load(Ordering::SeqCst) {
                output.usage = Some(causa::kernel::ModelUsage::new(MAIN_INPUT_TOKENS, 20));
            }
            Ok(output)
        }
    }
}

fn summary_output(text: &str) -> causa::kernel::ModelOutput {
    let mut output = causa::kernel::ModelOutput::new(
        causa::kernel::ModelResponse {
            items: vec![causa::kernel::ModelItem::text(text)],
        },
        causa::kernel::ModelStopReason::EndTurn,
    );
    output.usage = Some(causa::kernel::ModelUsage::new(400, 30));
    output
}

/// Read the saved record JSON of one conversation from SQLite `<root>/tars.db`.
pub(crate) fn saved_record(root: &std::path::Path, id: &str) -> serde_json::Value {
    let record = crate::store::SessionStore::new(root.into())
        .read_saved(id)
        .unwrap()
        .expect("saved session record in tars.db");
    serde_json::to_value(record).unwrap()
}

/// Open `<root>/tars.db` read-write for white-box test inspection/mutation.
pub(crate) fn open_db(root: &std::path::Path) -> rusqlite::Connection {
    crate::store::schema::open_read_write(&root.join("tars.db")).unwrap()
}

/// Toggle a SQLite trigger that rejects inserts into `history_records`.
pub(crate) fn set_history_write_blocked(root: &std::path::Path, blocked: bool) {
    let conn = open_db(root);
    if blocked {
        conn.execute_batch(
            "CREATE TRIGGER IF NOT EXISTS test_block_history_insert
             BEFORE INSERT ON history_records
             BEGIN
                 SELECT RAISE(ABORT, 'injected history write failure');
             END;",
        )
        .unwrap();
    } else {
        conn.execute_batch("DROP TRIGGER IF EXISTS test_block_history_insert;")
            .unwrap();
    }
}

/// Count `history_records` rows for `(session_id, session_id)`.
pub(crate) fn history_row_count(root: &std::path::Path, id: &str) -> usize {
    if !root.join("tars.db").exists() {
        return 0;
    }
    let conn = open_db(root);
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM history_records WHERE session_id = ?1 AND agent_id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .unwrap();
    usize::try_from(count).unwrap()
}

/// Insert an unreferenced stray row into `history_records`.
pub(crate) fn insert_stray_history_row(root: &std::path::Path, id: &str, stray_id: &str) {
    let conn = open_db(root);
    conn.execute(
        "INSERT INTO history_records (session_id, agent_id, record_id, seq, kind, summary_json, document_json)
         VALUES (?1, ?1, ?2, 99, 'summarize', '{}', '{}')",
        rusqlite::params![id, stray_id],
    )
    .unwrap();
}

/// Replace an existing `artifacts` row with an oversized zero blob (> 64 MiB).
pub(crate) fn set_artifact_oversized(root: &std::path::Path, id: &str, artifact_id: &str) {
    let conn = open_db(root);
    let oversized_len = (crate::compact::history::MAX_ARTIFACT_BYTES + 1) as i64;
    conn.execute(
        "INSERT OR REPLACE INTO artifacts (session_id, agent_id, artifact_id, content)
         VALUES (?1, ?1, ?2, zeroblob(?3))",
        rusqlite::params![id, artifact_id, oversized_len],
    )
    .unwrap();
}

/// Check whether a `history_records` row exists for `(session_id, record_id)`.
pub(crate) fn history_record_exists(root: &std::path::Path, id: &str, record_id: &str) -> bool {
    let conn = open_db(root);
    conn.query_row(
        "SELECT 1 FROM history_records WHERE session_id = ?1 AND agent_id = ?1 AND record_id = ?2",
        rusqlite::params![id, record_id],
        |_| Ok(()),
    )
    .is_ok()
}

/// Delete one `history_records` row by `(session_id, record_id)`.
pub(crate) fn delete_history_record_row(root: &std::path::Path, id: &str, record_id: &str) {
    let conn = open_db(root);
    conn.execute(
        "DELETE FROM history_records WHERE session_id = ?1 AND agent_id = ?1 AND record_id = ?2",
        rusqlite::params![id, record_id],
    )
    .unwrap();
}

/// Check whether a spilled `artifacts` row exists for `(session_id, artifact_id)`.
pub(crate) fn artifact_row_exists(root: &std::path::Path, id: &str, artifact_id: &str) -> bool {
    let conn = open_db(root);
    conn.query_row(
        "SELECT 1 FROM artifacts WHERE session_id = ?1 AND agent_id = ?1 AND artifact_id = ?2",
        rusqlite::params![id, artifact_id],
        |_| Ok(()),
    )
    .is_ok()
}

/// Delete one spilled `artifacts` row by `(session_id, artifact_id)`.
pub(crate) fn delete_artifact_row(root: &std::path::Path, id: &str, artifact_id: &str) {
    let conn = open_db(root);
    conn.execute(
        "DELETE FROM artifacts WHERE session_id = ?1 AND agent_id = ?1 AND artifact_id = ?2",
        rusqlite::params![id, artifact_id],
    )
    .unwrap();
}

/// Count spilled `artifacts` rows for `id`.
pub(crate) fn artifact_row_count(root: &std::path::Path, id: &str) -> usize {
    let conn = open_db(root);
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM artifacts WHERE session_id = ?1 AND agent_id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .unwrap();
    usize::try_from(count).unwrap()
}

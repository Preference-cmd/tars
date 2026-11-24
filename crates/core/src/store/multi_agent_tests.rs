use super::*;
use crate::definition::{CredentialRef, Provider, SessionDefinition};
use crate::events::{
    AgentInteraction, EventDeliveryState, EventPriority, EventSourceKind,
    MAX_OPERATION_INPUT_BYTES, MAX_READ_AGENT_PAGE_BYTES, NewInboxEvent, OnParentComplete,
    OperationExecutionMode, OperationStatus, TASK_NOTIFICATION_SOURCE, WakePolicy,
};
use causa::kernel::{BlockContent, BlockMeta, Context, ContextBlock, Role};
use causa::runtime::new_block_id;
use rusqlite::params;
use std::{io, sync::Arc};
use uuid::Uuid;

fn definition() -> SessionDefinition {
    SessionDefinition {
        provider: Provider::OpenAiChat,
        model: "fixture".into(),
        endpoint: None,
        credential: CredentialRef::Environment("TARS_TEST_UNUSED".into()),
        work_timeout_secs: None,
        retained_work_capacity: 16,
        max_model_rounds: 8,
        max_tool_calls: 0,
        max_tokens: None,
        reasoning_effort: None,
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

fn context_with_text(role: Role, text: &str) -> Context {
    Context::from_blocks(vec![ContextBlock::new(
        new_block_id(),
        BlockContent::text(role, text),
        BlockMeta::default(),
    )])
    .unwrap()
}

fn setup_session() -> (tempfile::TempDir, SessionStore, String, Writer) {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().into());
    let session_id = Uuid::now_v7().to_string();
    let writer = store.acquire_writer(&session_id).unwrap();
    let initial = PreparedCommit::new(
        None,
        SavedSessionRecord::new(
            session_id.clone(),
            Uuid::now_v7().to_string(),
            None,
            definition(),
            context_with_text(Role::User, "root prompt"),
            None,
            None,
            1_000,
        )
        .unwrap(),
    )
    .unwrap();
    writer.commit(&initial).unwrap();
    (dir, store, session_id, writer)
}

fn new_agent_row(agent_id: &str, name: &str, def: &SessionDefinition) -> NewAgentRow {
    NewAgentRow {
        agent_id: agent_id.to_owned(),
        name: name.to_owned(),
        preset_name: Some("explore".to_owned()),
        interaction: AgentInteraction::Reusable,
        definition_json: serde_json::to_string(def).unwrap(),
        context_json: serde_json::to_string(&Context::new()).unwrap(),
        history_index_json: "[]".to_owned(),
        usage_json: None,
        created_at_ms: 2_000,
        updated_at_ms: 2_000,
    }
}

fn fail_at(stage: CommitStage) -> FaultHook {
    Arc::new(move |at| {
        if at == stage {
            FaultAction::Fail(io::Error::other("injected failure"))
        } else {
            FaultAction::Continue
        }
    })
}

#[test]
fn admit_operation_creates_agent_and_enforces_source_block_dedup_including_null_parent() {
    let (_dir, store, session_id, writer) = setup_session();
    let child_id = Uuid::now_v7().to_string();
    let op_id = Uuid::now_v7().to_string();
    let def = definition();

    let req = AdmitOperationRequest {
        commit_id: Uuid::now_v7().to_string(),
        operation_id: op_id.clone(),
        target_agent_id: child_id.clone(),
        parent_operation_id: None,
        parent_agent_id: Some(session_id.clone()),
        source_block_id: Some("block-root-1".into()),
        dedup_fingerprint: Some("fp-alpha".into()),
        execution_mode: OperationExecutionMode::Background,
        on_parent_complete: OnParentComplete::Continue,
        depth: 1,
        input_json: "{\"prompt\":\"inspect\"}".into(),
        created_at_ms: 2_000,
        new_agent: Some(new_agent_row(&child_id, "explore-1", &def)),
    };

    let outcome = writer.admit_operation(&req).unwrap();
    let AdmitOperationOutcome::Admitted {
        operation,
        session_revision,
    } = outcome
    else {
        panic!("expected Admitted");
    };
    assert_eq!(session_revision, 2);
    assert_eq!(operation.operation_id, op_id);
    assert_eq!(operation.status, OperationStatus::Creating);
    assert_eq!(operation.depth, 1);

    // Same (session_id, parent_operation_id = NULL, source_block_id) + same fingerprint -> Replayed
    let replay_req = AdmitOperationRequest {
        commit_id: Uuid::now_v7().to_string(),
        operation_id: Uuid::now_v7().to_string(),
        new_agent: None,
        ..req.clone()
    };
    let replay = writer.admit_operation(&replay_req).unwrap();
    assert_eq!(replay, AdmitOperationOutcome::Replayed(operation.clone()));

    // Different fingerprint -> DedupConflict without mutating DB
    let conflict_req = AdmitOperationRequest {
        commit_id: Uuid::now_v7().to_string(),
        operation_id: Uuid::now_v7().to_string(),
        dedup_fingerprint: Some("fp-beta".into()),
        new_agent: None,
        ..req.clone()
    };
    let conflict = writer.admit_operation(&conflict_req).unwrap();
    assert_eq!(
        conflict,
        AdmitOperationOutcome::DedupConflict {
            existing: operation.clone()
        }
    );
    assert_eq!(store.list_operations(&session_id).unwrap().len(), 1);
    assert_eq!(store.list_agents(&session_id).unwrap().len(), 2);

    // Oversized input_json (> 128 KiB) rejected before DB mutation
    let oversized_req = AdmitOperationRequest {
        commit_id: Uuid::now_v7().to_string(),
        operation_id: Uuid::now_v7().to_string(),
        source_block_id: Some("block-root-2".into()),
        input_json: "x".repeat(MAX_OPERATION_INPUT_BYTES + 1),
        new_agent: None,
        ..req
    };
    assert!(matches!(
        writer.admit_operation(&oversized_req),
        Err(StoreError::RecordTooLarge {
            limit: MAX_OPERATION_INPUT_BYTES
        })
    ));
}

#[test]
fn transition_operation_running_succeeds_once_and_rejects_cancelled_creating() {
    let (_dir, store, session_id, writer) = setup_session();
    let child_id = Uuid::now_v7().to_string();
    let op1 = Uuid::now_v7().to_string();
    let op2 = Uuid::now_v7().to_string();
    let def = definition();

    writer
        .admit_operation(&AdmitOperationRequest {
            commit_id: Uuid::now_v7().to_string(),
            operation_id: op1.clone(),
            target_agent_id: child_id.clone(),
            parent_operation_id: None,
            parent_agent_id: Some(session_id.clone()),
            source_block_id: None,
            dedup_fingerprint: None,
            execution_mode: OperationExecutionMode::Background,
            on_parent_complete: OnParentComplete::Continue,
            depth: 1,
            input_json: "{}".into(),
            created_at_ms: 2_000,
            new_agent: Some(new_agent_row(&child_id, "explore-1", &def)),
        })
        .unwrap();

    assert!(writer.transition_operation_running(&op1, 2_100).unwrap());
    // Second transition from running -> running returns false
    assert!(!writer.transition_operation_running(&op1, 2_200).unwrap());

    // Admit second op and cancel it while still `creating`
    writer
        .admit_operation(&AdmitOperationRequest {
            commit_id: Uuid::now_v7().to_string(),
            operation_id: op2.clone(),
            target_agent_id: child_id.clone(),
            parent_operation_id: Some(op1.clone()),
            parent_agent_id: Some(child_id.clone()),
            source_block_id: None,
            dedup_fingerprint: None,
            execution_mode: OperationExecutionMode::Sync,
            on_parent_complete: OnParentComplete::Cancel,
            depth: 2,
            input_json: "{}".into(),
            created_at_ms: 2_300,
            new_agent: None,
        })
        .unwrap();

    writer
        .commit_outbox(&OutboxCommitRequest {
            commit_id: Uuid::now_v7().to_string(),
            expected_session_revision: 3,
            updated_at_ms: 2_400,
            agent_update: None,
            operation_update: Some(OperationTerminalUpdate {
                operation_id: op2.clone(),
                status: OperationStatus::Cancelled,
                outcome_json: None,
                result_text: None,
                finished_at_ms: 2_400,
            }),
            new_events: Vec::new(),
            delivered_event_ids: Vec::new(),
            dropped_event_ids: Vec::new(),
        })
        .unwrap();

    assert!(!writer.transition_operation_running(&op2, 2_500).unwrap());
    assert_eq!(
        store
            .read_operation(&session_id, &op2)
            .unwrap()
            .unwrap()
            .status,
        OperationStatus::Cancelled
    );
}

#[test]
fn commit_outbox_is_atomic_monotonic_and_rolls_back_on_fault_hook() {
    let (_dir, store, session_id, writer) = setup_session();
    let child_id = Uuid::now_v7().to_string();
    let op_id = Uuid::now_v7().to_string();
    let def = definition();

    writer
        .admit_operation(&AdmitOperationRequest {
            commit_id: Uuid::now_v7().to_string(),
            operation_id: op_id.clone(),
            target_agent_id: child_id.clone(),
            parent_operation_id: None,
            parent_agent_id: Some(session_id.clone()),
            source_block_id: None,
            dedup_fingerprint: None,
            execution_mode: OperationExecutionMode::Background,
            on_parent_complete: OnParentComplete::Continue,
            depth: 1,
            input_json: "{}".into(),
            created_at_ms: 2_000,
            new_agent: Some(new_agent_row(&child_id, "explore-1", &def)),
        })
        .unwrap();
    writer.transition_operation_running(&op_id, 2_100).unwrap();

    let updated_ctx_json =
        serde_json::to_string(&context_with_text(Role::Assistant, "done")).unwrap();
    let full_result = "测试🚀".repeat(20_000);

    let outbox_req = OutboxCommitRequest {
        commit_id: Uuid::now_v7().to_string(),
        expected_session_revision: 2,
        updated_at_ms: 3_000,
        agent_update: Some(AgentMaterialUpdate {
            agent_id: child_id.clone(),
            expected_material_revision: 0,
            context_json: updated_ctx_json.clone(),
            history_index_json: "[]".into(),
            usage_json: None,
        }),
        operation_update: Some(OperationTerminalUpdate {
            operation_id: op_id.clone(),
            status: OperationStatus::Completed,
            outcome_json: Some("{\"ok\":true}".into()),
            result_text: Some(full_result.clone()),
            finished_at_ms: 3_000,
        }),
        new_events: vec![
            NewInboxEvent {
                target_agent_id: session_id.clone(),
                priority: EventPriority::Normal,
                source_kind: EventSourceKind::AgentOperation,
                source_id: op_id.clone(),
                coalesce_key: Some(format!("op:{op_id}")),
                wake_policy: WakePolicy::WakeIfIdle,
                payload_json: "{\"summary\":\"v1\"}".into(),
                created_at_ms: 3_000,
            },
            NewInboxEvent {
                target_agent_id: session_id.clone(),
                priority: EventPriority::Normal,
                source_kind: EventSourceKind::AgentOperation,
                source_id: op_id.clone(),
                coalesce_key: Some(format!("op:{op_id}")),
                wake_policy: WakePolicy::WakeIfIdle,
                payload_json: "{\"summary\":\"v2\"}".into(),
                created_at_ms: 3_001,
            },
        ],
        delivered_event_ids: Vec::new(),
        dropped_event_ids: Vec::new(),
    };

    // Fault at Replace rolls back the entire transaction atomically
    writer.set_fault_hook(Some(fail_at(CommitStage::Replace)));
    assert!(matches!(
        writer.commit_outbox(&outbox_req),
        Err(StoreError::CommitIo {
            stage: CommitStage::Replace,
            ..
        })
    ));
    writer.set_fault_hook(None);

    // Nothing was partially written
    assert_eq!(
        store
            .read_agent_row(&session_id, &child_id)
            .unwrap()
            .unwrap()
            .material_revision,
        0
    );
    assert_eq!(
        store
            .read_operation(&session_id, &op_id)
            .unwrap()
            .unwrap()
            .status,
        OperationStatus::Running
    );
    assert!(
        store
            .list_pending_inbox_events(&session_id, &session_id)
            .unwrap()
            .is_empty()
    );

    // Real commit succeeds atomically
    let res = writer.commit_outbox(&outbox_req).unwrap();
    assert_eq!(res.session_revision, 3);
    assert_eq!(res.agent_material_revision, Some(1));
    assert!(res.operation_updated);
    assert_eq!(res.inserted_event_ids.len(), 2);

    // Lock-free WAL snapshot queries work while `writer` still holds the OS advisory lock!
    let agent_row = store
        .read_agent_row(&session_id, &child_id)
        .unwrap()
        .unwrap();
    assert_eq!(agent_row.material_revision, 1);
    assert_eq!(agent_row.context_json, updated_ctx_json);

    let mut combined = String::new();
    let mut offset = 0;
    let mut page_count = 0;
    loop {
        let page = store
            .read_agent_output_page(&session_id, &op_id, offset, MAX_READ_AGENT_PAGE_BYTES * 2)
            .unwrap()
            .unwrap();
        assert_eq!(page.operation.status, OperationStatus::Completed);
        assert!(page.page.content.len() <= MAX_READ_AGENT_PAGE_BYTES);
        combined.push_str(&page.page.content);
        page_count += 1;
        if !page.page.has_more {
            assert_eq!(page.page.next_offset_bytes, full_result.len());
            break;
        }
        assert!(page.page.next_offset_bytes > offset);
        offset = page.page.next_offset_bytes;
    }
    assert!(page_count > 1);
    assert_eq!(combined, full_result);

    // Late Cancelled commit does NOT overwrite terminal Completed, while persisting agent material
    let late_req = OutboxCommitRequest {
        commit_id: Uuid::now_v7().to_string(),
        expected_session_revision: 3,
        updated_at_ms: 4_000,
        agent_update: Some(AgentMaterialUpdate {
            agent_id: child_id.clone(),
            expected_material_revision: 1,
            context_json: serde_json::to_string(&context_with_text(Role::Assistant, "v2")).unwrap(),
            history_index_json: "[]".into(),
            usage_json: None,
        }),
        operation_update: Some(OperationTerminalUpdate {
            operation_id: op_id.clone(),
            status: OperationStatus::Cancelled,
            outcome_json: None,
            result_text: Some("overwritten".into()),
            finished_at_ms: 4_000,
        }),
        new_events: Vec::new(),
        delivered_event_ids: Vec::new(),
        dropped_event_ids: Vec::new(),
    };
    let late_res = writer.commit_outbox(&late_req).unwrap();
    assert_eq!(late_res.session_revision, 4);
    assert_eq!(late_res.agent_material_revision, Some(2));
    assert!(!late_res.operation_updated);

    // Parent agent commits its updated Context and atomically marks delivered/dropped inbox events
    let parent_consume_req = OutboxCommitRequest {
        commit_id: Uuid::now_v7().to_string(),
        expected_session_revision: 4,
        updated_at_ms: 4_100,
        agent_update: Some(AgentMaterialUpdate {
            agent_id: session_id.clone(),
            expected_material_revision: 1,
            context_json: serde_json::to_string(&context_with_text(Role::User, "after notif"))
                .unwrap(),
            history_index_json: "[]".into(),
            usage_json: None,
        }),
        operation_update: None,
        new_events: Vec::new(),
        delivered_event_ids: vec![res.inserted_event_ids[1]],
        dropped_event_ids: vec![res.inserted_event_ids[0]],
    };
    let parent_res = writer.commit_outbox(&parent_consume_req).unwrap();
    assert_eq!(parent_res.session_revision, 5);
    assert_eq!(parent_res.agent_material_revision, Some(2));

    let op_after = store.read_operation(&session_id, &op_id).unwrap().unwrap();
    assert_eq!(op_after.status, OperationStatus::Completed);
    assert_eq!(op_after.result_text.as_deref(), Some(full_result.as_str()));
    assert!(
        store
            .list_pending_inbox_events(&session_id, &session_id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn reopen_downgrade_and_cancel_wake_suppression_update_expected_rows() {
    let (_dir, store, session_id, writer) = setup_session();
    let other_agent_id = Uuid::now_v7().to_string();
    let op_id = Uuid::now_v7().to_string();
    let def = definition();

    writer
        .admit_operation(&AdmitOperationRequest {
            commit_id: Uuid::now_v7().to_string(),
            operation_id: op_id.clone(),
            target_agent_id: other_agent_id.clone(),
            parent_operation_id: None,
            parent_agent_id: Some(session_id.clone()),
            source_block_id: None,
            dedup_fingerprint: None,
            execution_mode: OperationExecutionMode::Background,
            on_parent_complete: OnParentComplete::Continue,
            depth: 1,
            input_json: "{}".into(),
            created_at_ms: 2_000,
            new_agent: Some(new_agent_row(&other_agent_id, "worker", &def)),
        })
        .unwrap();

    let res = writer
        .commit_outbox(&OutboxCommitRequest {
            commit_id: Uuid::now_v7().to_string(),
            expected_session_revision: 2,
            updated_at_ms: 2_500,
            agent_update: None,
            operation_update: None,
            new_events: vec![
                NewInboxEvent {
                    target_agent_id: session_id.clone(),
                    priority: EventPriority::Normal,
                    source_kind: EventSourceKind::AgentOperation,
                    source_id: op_id.clone(),
                    coalesce_key: None,
                    wake_policy: WakePolicy::WakeIfIdle,
                    payload_json: "{\"n\":1}".into(),
                    created_at_ms: 2_500,
                },
                NewInboxEvent {
                    target_agent_id: session_id.clone(),
                    priority: EventPriority::Normal,
                    source_kind: EventSourceKind::Timer,
                    source_id: "timer-a".into(),
                    coalesce_key: None,
                    wake_policy: WakePolicy::SuppressWake,
                    payload_json: "{\"n\":2}".into(),
                    created_at_ms: 2_501,
                },
                NewInboxEvent {
                    target_agent_id: other_agent_id.clone(),
                    priority: EventPriority::Urgent,
                    source_kind: EventSourceKind::UserSteer,
                    source_id: "steer-1".into(),
                    coalesce_key: None,
                    wake_policy: WakePolicy::WakeIfIdle,
                    payload_json: "{\"n\":3}".into(),
                    created_at_ms: 2_502,
                },
            ],
            delivered_event_ids: Vec::new(),
            dropped_event_ids: Vec::new(),
        })
        .unwrap();

    // Simulate a crashed `claimed` row on event 0
    writer
        .with_connection(|conn| {
            conn.execute(
                "UPDATE agent_events SET state = 'claimed', claimed_op_id = 'crashed-op' WHERE event_id = ?1",
                params![res.inserted_event_ids[0]],
            )?;
            Ok(())
        })
        .unwrap();

    let downgraded = writer.downgrade_pending_wakeups_on_reopen().unwrap();
    assert_eq!(downgraded, 2);

    let main_pending = store
        .list_pending_inbox_events(&session_id, &session_id)
        .unwrap();
    assert_eq!(main_pending.len(), 2);
    assert_eq!(main_pending[0].state, EventDeliveryState::Pending);
    assert_eq!(main_pending[0].claimed_op_id, None);
    assert_eq!(main_pending[0].wake_policy, WakePolicy::DeliverOnNextTurn);
    // SuppressWake remains untouched
    assert_eq!(main_pending[1].wake_policy, WakePolicy::SuppressWake);

    // Suppress by source and by target
    assert_eq!(
        writer
            .suppress_pending_wakeups_for_source(EventSourceKind::AgentOperation, &op_id)
            .unwrap(),
        1
    );
    assert_eq!(
        writer
            .suppress_pending_wakeups_for_target(&other_agent_id)
            .unwrap(),
        1
    );

    let main_after = store
        .list_pending_inbox_events(&session_id, &session_id)
        .unwrap();
    assert!(
        main_after
            .iter()
            .all(|e| e.wake_policy == WakePolicy::SuppressWake)
    );
    let worker_after = store
        .list_pending_inbox_events(&session_id, &other_agent_id)
        .unwrap();
    assert_eq!(worker_after[0].wake_policy, WakePolicy::SuppressWake);
}

#[test]
fn record_metadata_skips_task_notification_blocks_for_session_title() {
    let notif = ContextBlock::new(
        new_block_id(),
        BlockContent::text(
            Role::User,
            "<task-notification source=\"agent_operation\">internal</task-notification>",
        ),
        BlockMeta::from_source(TASK_NOTIFICATION_SOURCE),
    );
    let real_user = ContextBlock::new(
        new_block_id(),
        BlockContent::text(Role::User, "Implement P2a event bus"),
        BlockMeta::from_source("user"),
    );

    let notif_only = Context::from_blocks(vec![notif.clone()]).unwrap();
    let meta = RecordMetadata::unsaved(&Uuid::now_v7().to_string());
    let (title_only, _) = meta.next(&notif_only, 1_000);
    assert_eq!(title_only, None);

    let both = Context::from_blocks(vec![notif, real_user]).unwrap();
    let (title_both, _) = meta.next(&both, 1_000);
    assert_eq!(title_both.as_deref(), Some("Implement P2a event bus"));
}

#[test]
fn idempotent_retry_and_conflict_on_confirm_and_reply_faults_for_admit_and_outbox() {
    for stage in [CommitStage::Confirm, CommitStage::Reply] {
        let (_dir, store, session_id, writer) = setup_session();
        let child_id = Uuid::now_v7().to_string();
        let op_id = Uuid::now_v7().to_string();
        let def = definition();

        let admit_req = AdmitOperationRequest {
            commit_id: Uuid::now_v7().to_string(),
            operation_id: op_id.clone(),
            target_agent_id: child_id.clone(),
            parent_operation_id: None,
            parent_agent_id: Some(session_id.clone()),
            source_block_id: None,
            dedup_fingerprint: None,
            execution_mode: OperationExecutionMode::Background,
            on_parent_complete: OnParentComplete::Continue,
            depth: 1,
            input_json: "{\"task\":\"scan\"}".into(),
            created_at_ms: 2_000,
            new_agent: Some(new_agent_row(&child_id, "explore", &def)),
        };

        writer.set_fault_hook(Some(fail_at(stage)));
        assert!(matches!(
            writer.admit_operation(&admit_req),
            Err(StoreError::CommitIo { stage: s, .. }) if s == stage
        ));
        writer.set_fault_hook(None);

        // Same commit_id with mutated payload (even when source_block_id is None) -> CommitIdentityConflict
        let mut altered_admit = admit_req.clone();
        altered_admit.input_json = "{\"task\":\"different\"}".into();
        assert!(matches!(
            writer.admit_operation(&altered_admit),
            Err(StoreError::CommitIdentityConflict)
        ));

        // Same Writer idempotent retry succeeds without duplicating rows
        let retried = writer.admit_operation(&admit_req).unwrap();
        assert!(matches!(
            retried,
            AdmitOperationOutcome::Admitted {
                session_revision: 2,
                ..
            }
        ));

        // Fresh reopened Writer retrying same admit_req also succeeds idempotently, and rejects mutated payload
        drop(writer);
        let reopened = store.acquire_writer(&session_id).unwrap();
        assert!(matches!(
            reopened.admit_operation(&altered_admit),
            Err(StoreError::CommitIdentityConflict)
        ));
        assert!(matches!(
            reopened.admit_operation(&admit_req),
            Ok(AdmitOperationOutcome::Admitted {
                session_revision: 2,
                ..
            })
        ));

        // Now test commit_outbox at Confirm/Reply stage
        reopened
            .transition_operation_running(&op_id, 2_100)
            .unwrap();
        let outbox_req = OutboxCommitRequest {
            commit_id: Uuid::now_v7().to_string(),
            expected_session_revision: 2,
            updated_at_ms: 3_000,
            agent_update: Some(AgentMaterialUpdate {
                agent_id: child_id.clone(),
                expected_material_revision: 0,
                context_json: serde_json::to_string(&context_with_text(Role::Assistant, "ok"))
                    .unwrap(),
                history_index_json: "[]".into(),
                usage_json: None,
            }),
            operation_update: Some(OperationTerminalUpdate {
                operation_id: op_id.clone(),
                status: OperationStatus::Completed,
                outcome_json: Some("{\"done\":true}".into()),
                result_text: Some("finished".into()),
                finished_at_ms: 3_000,
            }),
            new_events: vec![NewInboxEvent {
                target_agent_id: session_id.clone(),
                priority: EventPriority::Normal,
                source_kind: EventSourceKind::AgentOperation,
                source_id: op_id.clone(),
                coalesce_key: None,
                wake_policy: WakePolicy::WakeIfIdle,
                payload_json: "{\"summary\":\"finished\"}".into(),
                created_at_ms: 3_000,
            }],
            delivered_event_ids: Vec::new(),
            dropped_event_ids: Vec::new(),
        };

        reopened.set_fault_hook(Some(fail_at(stage)));
        assert!(matches!(
            reopened.commit_outbox(&outbox_req),
            Err(StoreError::CommitIo { stage: s, .. }) if s == stage
        ));
        reopened.set_fault_hook(None);

        let mut altered_outbox = outbox_req.clone();
        altered_outbox
            .operation_update
            .as_mut()
            .unwrap()
            .result_text = Some("tampered".into());
        assert!(matches!(
            reopened.commit_outbox(&altered_outbox),
            Err(StoreError::CommitIdentityConflict)
        ));

        let same_writer_res = reopened.commit_outbox(&outbox_req).unwrap();
        assert_eq!(same_writer_res.session_revision, 3);
        assert_eq!(same_writer_res.agent_material_revision, Some(1));
        assert!(same_writer_res.operation_updated);
        assert_eq!(same_writer_res.inserted_event_ids.len(), 1);

        // Fresh reopened Writer retrying the same outbox_req also succeeds idempotently
        drop(reopened);
        let reopened2 = store.acquire_writer(&session_id).unwrap();
        assert!(matches!(
            reopened2.commit_outbox(&altered_outbox),
            Err(StoreError::CommitIdentityConflict)
        ));
        let reopened_res = reopened2.commit_outbox(&outbox_req).unwrap();
        assert_eq!(reopened_res, same_writer_res);
    }
}

#[test]
fn pending_commit_blocks_cross_mode_writes_until_resolved() {
    let (_dir, _store, session_id, writer) = setup_session();
    let child_id = Uuid::now_v7().to_string();
    let def = definition();

    let admit_req = AdmitOperationRequest {
        commit_id: Uuid::now_v7().to_string(),
        operation_id: Uuid::now_v7().to_string(),
        target_agent_id: child_id.clone(),
        parent_operation_id: None,
        parent_agent_id: Some(session_id.clone()),
        source_block_id: None,
        dedup_fingerprint: None,
        execution_mode: OperationExecutionMode::Background,
        on_parent_complete: OnParentComplete::Continue,
        depth: 1,
        input_json: "{}".into(),
        created_at_ms: 2_000,
        new_agent: Some(new_agent_row(&child_id, "explore", &def)),
    };

    writer.set_fault_hook(Some(fail_at(CommitStage::Write)));
    assert!(writer.admit_operation(&admit_req).is_err());
    writer.set_fault_hook(None);

    // Different admit_operation, commit_outbox, and legacy commit are all blocked with PendingCommit
    let other_admit = AdmitOperationRequest {
        commit_id: Uuid::now_v7().to_string(),
        operation_id: Uuid::now_v7().to_string(),
        target_agent_id: Uuid::now_v7().to_string(),
        new_agent: None,
        ..admit_req.clone()
    };
    assert!(matches!(
        writer.admit_operation(&other_admit),
        Err(StoreError::PendingCommit)
    ));
    assert!(matches!(
        writer.commit_outbox(&OutboxCommitRequest {
            commit_id: Uuid::now_v7().to_string(),
            expected_session_revision: 1,
            updated_at_ms: 2_100,
            agent_update: None,
            operation_update: None,
            new_events: Vec::new(),
            delivered_event_ids: Vec::new(),
            dropped_event_ids: Vec::new(),
        }),
        Err(StoreError::PendingCommit)
    ));
    let legacy_commit = PreparedCommit::new(
        Some(1),
        SavedSessionRecord::new(
            session_id,
            Uuid::now_v7().to_string(),
            Some(1),
            def,
            context_with_text(Role::User, "next"),
            None,
            None,
            2_100,
        )
        .unwrap(),
    )
    .unwrap();
    assert!(matches!(
        writer.commit(&legacy_commit),
        Err(StoreError::PendingCommit)
    ));

    // Retrying the original admit clears pending state and unblocks subsequent commits
    assert!(writer.admit_operation(&admit_req).is_ok());
}

#[test]
fn outbox_rejects_cross_agent_or_missing_event_ids() {
    let (_dir, store, session_id, writer) = setup_session();
    let child_id = Uuid::now_v7().to_string();
    let op_id = Uuid::now_v7().to_string();
    let def = definition();

    writer
        .admit_operation(&AdmitOperationRequest {
            commit_id: Uuid::now_v7().to_string(),
            operation_id: op_id.clone(),
            target_agent_id: child_id.clone(),
            parent_operation_id: None,
            parent_agent_id: Some(session_id.clone()),
            source_block_id: None,
            dedup_fingerprint: None,
            execution_mode: OperationExecutionMode::Background,
            on_parent_complete: OnParentComplete::Continue,
            depth: 1,
            input_json: "{}".into(),
            created_at_ms: 2_000,
            new_agent: Some(new_agent_row(&child_id, "worker", &def)),
        })
        .unwrap();

    // Insert an event targeting `session_id` (main agent)
    let res = writer
        .commit_outbox(&OutboxCommitRequest {
            commit_id: Uuid::now_v7().to_string(),
            expected_session_revision: 2,
            updated_at_ms: 2_500,
            agent_update: None,
            operation_update: None,
            new_events: vec![NewInboxEvent {
                target_agent_id: session_id.clone(),
                priority: EventPriority::Normal,
                source_kind: EventSourceKind::AgentOperation,
                source_id: op_id,
                coalesce_key: None,
                wake_policy: WakePolicy::WakeIfIdle,
                payload_json: "{\"n\":1}".into(),
                created_at_ms: 2_500,
            }],
            delivered_event_ids: Vec::new(),
            dropped_event_ids: Vec::new(),
        })
        .unwrap();
    let main_event_id = res.inserted_event_ids[0];

    // Child agent cannot mark main agent's event as delivered or dropped
    let cross_agent_delivered = writer.commit_outbox(&OutboxCommitRequest {
        commit_id: Uuid::now_v7().to_string(),
        expected_session_revision: 3,
        updated_at_ms: 3_000,
        agent_update: Some(AgentMaterialUpdate {
            agent_id: child_id.clone(),
            expected_material_revision: 0,
            context_json: serde_json::to_string(&Context::new()).unwrap(),
            history_index_json: "[]".into(),
            usage_json: None,
        }),
        operation_update: None,
        new_events: Vec::new(),
        delivered_event_ids: vec![main_event_id],
        dropped_event_ids: Vec::new(),
    });
    assert!(matches!(
        cross_agent_delivered,
        Err(StoreError::InvalidDatabase(_))
    ));

    let cross_agent_dropped = writer.commit_outbox(&OutboxCommitRequest {
        commit_id: Uuid::now_v7().to_string(),
        expected_session_revision: 3,
        updated_at_ms: 3_000,
        agent_update: Some(AgentMaterialUpdate {
            agent_id: child_id,
            expected_material_revision: 0,
            context_json: serde_json::to_string(&Context::new()).unwrap(),
            history_index_json: "[]".into(),
            usage_json: None,
        }),
        operation_update: None,
        new_events: Vec::new(),
        delivered_event_ids: Vec::new(),
        dropped_event_ids: vec![main_event_id],
    });
    assert!(matches!(
        cross_agent_dropped,
        Err(StoreError::InvalidDatabase(_))
    ));

    // Missing agent_update with delivered_event_ids is rejected
    let missing_target = writer.commit_outbox(&OutboxCommitRequest {
        commit_id: Uuid::now_v7().to_string(),
        expected_session_revision: 3,
        updated_at_ms: 3_000,
        agent_update: None,
        operation_update: None,
        new_events: Vec::new(),
        delivered_event_ids: vec![main_event_id],
        dropped_event_ids: Vec::new(),
    });
    assert!(matches!(
        missing_target,
        Err(StoreError::InvalidDatabase(_))
    ));

    // Nonexistent event_id is rejected
    let missing_event = writer.commit_outbox(&OutboxCommitRequest {
        commit_id: Uuid::now_v7().to_string(),
        expected_session_revision: 3,
        updated_at_ms: 3_000,
        agent_update: None,
        operation_update: None,
        new_events: Vec::new(),
        delivered_event_ids: Vec::new(),
        dropped_event_ids: vec![999_999],
    });
    assert!(matches!(missing_event, Err(StoreError::InvalidDatabase(_))));

    // Original event remains untouched as pending
    assert_eq!(
        store
            .list_pending_inbox_events(&session_id, &session_id)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn admit_rejects_cross_session_parent_and_invalid_agent_materials() {
    let (_dir, store, session_id, writer) = setup_session();
    let def = definition();

    // Create a second session in the same SQLite database with its own operation
    let other_session_id = Uuid::now_v7().to_string();
    let other_writer = store.acquire_writer(&other_session_id).unwrap();
    other_writer
        .commit(
            &PreparedCommit::new(
                None,
                SavedSessionRecord::new(
                    other_session_id.clone(),
                    Uuid::now_v7().to_string(),
                    None,
                    def.clone(),
                    context_with_text(Role::User, "other"),
                    None,
                    None,
                    1_000,
                )
                .unwrap(),
            )
            .unwrap(),
        )
        .unwrap();
    let other_op_id = Uuid::now_v7().to_string();
    let other_child_id = Uuid::now_v7().to_string();
    other_writer
        .admit_operation(&AdmitOperationRequest {
            commit_id: Uuid::now_v7().to_string(),
            operation_id: other_op_id.clone(),
            target_agent_id: other_child_id.clone(),
            parent_operation_id: None,
            parent_agent_id: Some(other_session_id),
            source_block_id: None,
            dedup_fingerprint: None,
            execution_mode: OperationExecutionMode::Background,
            on_parent_complete: OnParentComplete::Continue,
            depth: 1,
            input_json: "{}".into(),
            created_at_ms: 2_000,
            new_agent: Some(new_agent_row(&other_child_id, "other-worker", &def)),
        })
        .unwrap();

    // Cross-session parent_operation_id must be rejected
    let child_id = Uuid::now_v7().to_string();
    let cross_parent_op = writer.admit_operation(&AdmitOperationRequest {
        commit_id: Uuid::now_v7().to_string(),
        operation_id: Uuid::now_v7().to_string(),
        target_agent_id: child_id.clone(),
        parent_operation_id: Some(other_op_id),
        parent_agent_id: None,
        source_block_id: None,
        dedup_fingerprint: None,
        execution_mode: OperationExecutionMode::Background,
        on_parent_complete: OnParentComplete::Continue,
        depth: 2,
        input_json: "{}".into(),
        created_at_ms: 2_100,
        new_agent: Some(new_agent_row(&child_id, "child", &def)),
    });
    assert!(matches!(
        cross_parent_op,
        Err(StoreError::InvalidDatabase(_))
    ));

    // Missing / cross-session parent_agent_id must be rejected
    let missing_parent_agent = writer.admit_operation(&AdmitOperationRequest {
        commit_id: Uuid::now_v7().to_string(),
        operation_id: Uuid::now_v7().to_string(),
        target_agent_id: child_id.clone(),
        parent_operation_id: None,
        parent_agent_id: Some(Uuid::now_v7().to_string()),
        source_block_id: None,
        dedup_fingerprint: None,
        execution_mode: OperationExecutionMode::Background,
        on_parent_complete: OnParentComplete::Continue,
        depth: 1,
        input_json: "{}".into(),
        created_at_ms: 2_200,
        new_agent: Some(new_agent_row(&child_id, "child", &def)),
    });
    assert!(matches!(
        missing_parent_agent,
        Err(StoreError::InvalidDatabase(_))
    ));

    // Invalid definition_json rejected before write
    let mut bad_def_agent = new_agent_row(&child_id, "bad-def", &def);
    bad_def_agent.definition_json = "{}".into();
    let bad_def_res = writer.admit_operation(&AdmitOperationRequest {
        commit_id: Uuid::now_v7().to_string(),
        operation_id: Uuid::now_v7().to_string(),
        target_agent_id: child_id.clone(),
        parent_operation_id: None,
        parent_agent_id: Some(session_id.clone()),
        source_block_id: None,
        dedup_fingerprint: None,
        execution_mode: OperationExecutionMode::Background,
        on_parent_complete: OnParentComplete::Continue,
        depth: 1,
        input_json: "{}".into(),
        created_at_ms: 2_300,
        new_agent: Some(bad_def_agent),
    });
    assert!(matches!(bad_def_res, Err(StoreError::Json(_))));

    // Invalid context_json rejected before write
    let mut bad_ctx_agent = new_agent_row(&child_id, "bad-ctx", &def);
    bad_ctx_agent.context_json = "{\"blocks\":\"invalid\"}".into();
    let bad_ctx_res = writer.admit_operation(&AdmitOperationRequest {
        commit_id: Uuid::now_v7().to_string(),
        operation_id: Uuid::now_v7().to_string(),
        target_agent_id: child_id.clone(),
        parent_operation_id: None,
        parent_agent_id: Some(session_id.clone()),
        source_block_id: None,
        dedup_fingerprint: None,
        execution_mode: OperationExecutionMode::Background,
        on_parent_complete: OnParentComplete::Continue,
        depth: 1,
        input_json: "{}".into(),
        created_at_ms: 2_400,
        new_agent: Some(bad_ctx_agent),
    });
    assert!(matches!(bad_ctx_res, Err(StoreError::Json(_))));

    // Invalid AgentMaterialUpdate context_json rejected in commit_outbox
    let bad_outbox = writer.commit_outbox(&OutboxCommitRequest {
        commit_id: Uuid::now_v7().to_string(),
        expected_session_revision: 1,
        updated_at_ms: 2_500,
        agent_update: Some(AgentMaterialUpdate {
            agent_id: session_id,
            expected_material_revision: 1,
            context_json: "not-json".into(),
            history_index_json: "[]".into(),
            usage_json: None,
        }),
        operation_update: None,
        new_events: Vec::new(),
        delivered_event_ids: Vec::new(),
        dropped_event_ids: Vec::new(),
    });
    assert!(matches!(bad_outbox, Err(StoreError::Json(_))));
}

#[test]
fn reopened_writer_retries_late_terminal_outbox_with_same_or_newer_timestamp() {
    for (stage, late_ts) in [
        (CommitStage::Confirm, 3_000_u64),
        (CommitStage::Reply, 3_000_u64),
        (CommitStage::Confirm, 3_500_u64),
        (CommitStage::Reply, 3_500_u64),
    ] {
        let (_dir, store, session_id, writer) = setup_session();
        let child_id = Uuid::now_v7().to_string();
        let op_id = Uuid::now_v7().to_string();
        let def = definition();

        writer
            .admit_operation(&AdmitOperationRequest {
                commit_id: Uuid::now_v7().to_string(),
                operation_id: op_id.clone(),
                target_agent_id: child_id.clone(),
                parent_operation_id: None,
                parent_agent_id: Some(session_id.clone()),
                source_block_id: None,
                dedup_fingerprint: None,
                execution_mode: OperationExecutionMode::Background,
                on_parent_complete: OnParentComplete::Continue,
                depth: 1,
                input_json: "{}".into(),
                created_at_ms: 2_000,
                new_agent: Some(new_agent_row(&child_id, "worker", &def)),
            })
            .unwrap();
        writer.transition_operation_running(&op_id, 2_100).unwrap();

        // First commit settles operation as Completed at finished_at_ms = 3_000
        writer
            .commit_outbox(&OutboxCommitRequest {
                commit_id: Uuid::now_v7().to_string(),
                expected_session_revision: 2,
                updated_at_ms: 3_000,
                agent_update: None,
                operation_update: Some(OperationTerminalUpdate {
                    operation_id: op_id.clone(),
                    status: OperationStatus::Completed,
                    outcome_json: Some("{\"ok\":true}".into()),
                    result_text: Some("original result".into()),
                    finished_at_ms: 3_000,
                }),
                new_events: Vec::new(),
                delivered_event_ids: Vec::new(),
                dropped_event_ids: Vec::new(),
            })
            .unwrap();

        // Late Cancelled update with same or newer timestamp hits Confirm/Reply fault after commit
        let late_req = OutboxCommitRequest {
            commit_id: Uuid::now_v7().to_string(),
            expected_session_revision: 3,
            updated_at_ms: late_ts,
            agent_update: Some(AgentMaterialUpdate {
                agent_id: child_id.clone(),
                expected_material_revision: 0,
                context_json: serde_json::to_string(&context_with_text(
                    Role::Assistant,
                    "persisted material",
                ))
                .unwrap(),
                history_index_json: "[]".into(),
                usage_json: None,
            }),
            operation_update: Some(OperationTerminalUpdate {
                operation_id: op_id.clone(),
                status: OperationStatus::Cancelled,
                outcome_json: None,
                result_text: Some("late overwrite".into()),
                finished_at_ms: late_ts,
            }),
            new_events: Vec::new(),
            delivered_event_ids: Vec::new(),
            dropped_event_ids: Vec::new(),
        };

        writer.set_fault_hook(Some(fail_at(stage)));
        assert!(matches!(
            writer.commit_outbox(&late_req),
            Err(StoreError::CommitIo { stage: s, .. }) if s == stage
        ));
        drop(writer);

        // Fresh reopened Writer retries the exact same late_req idempotently
        let reopened = store.acquire_writer(&session_id).unwrap();
        let res = reopened.commit_outbox(&late_req).unwrap();
        assert_eq!(res.session_revision, 4);
        assert_eq!(res.agent_material_revision, Some(1));
        assert!(!res.operation_updated);

        let op_row = store.read_operation(&session_id, &op_id).unwrap().unwrap();
        assert_eq!(op_row.status, OperationStatus::Completed);
        assert_eq!(op_row.result_text.as_deref(), Some("original result"));
        assert_eq!(op_row.finished_at_ms, Some(3_000));
    }
}

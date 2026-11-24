use super::*;
use crate::definition::{CredentialRef, Provider, SessionDefinition};
use causa::kernel::{BlockContent, BlockMeta, ContentPart, Context, ContextBlock, MediaRef};
use causa::runtime::new_block_id;
use rusqlite::params;
use std::{
    fs, io,
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
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

fn context(text: &str) -> Context {
    Context::from_blocks(vec![ContextBlock::new(
        new_block_id(),
        BlockContent::text(causa::kernel::Role::Assistant, text),
        BlockMeta::default(),
    )])
    .unwrap()
}

fn op(id: &str, prior: Option<u64>, text: &str) -> PreparedCommit {
    PreparedCommit::new(
        prior,
        SavedSessionRecord::new(
            id.into(),
            Uuid::now_v7().to_string(),
            prior,
            definition(),
            context(text),
            None,
            None,
            1_000,
        )
        .unwrap(),
    )
    .unwrap()
}

fn setup() -> (tempfile::TempDir, SessionStore, String, Writer) {
    let folder = tempfile::tempdir().unwrap();
    let store = SessionStore::new(folder.path().into());
    let id = Uuid::now_v7().to_string();
    let writer = store.acquire_writer(&id).unwrap();
    (folder, store, id, writer)
}

fn failure(stage: CommitStage) -> FaultHook {
    Arc::new(move |at| {
        if at == stage {
            FaultAction::Fail(io::Error::other("injected"))
        } else {
            FaultAction::Continue
        }
    })
}

#[test]
fn commits_are_exact_idempotent_and_conflicts_preserve_record() {
    let (_folder, store, id, writer) = setup();
    let initial = op(&id, None, "initial");
    assert_eq!(writer.read_latest().unwrap(), None);
    assert_eq!(writer.commit(&initial).unwrap(), initial.record);
    assert_eq!(writer.commit(&initial).unwrap(), initial.record);
    assert_eq!(store.metrics().writes, 1);
    let mut changed = initial.clone();
    changed.record.context = context("changed");
    assert!(matches!(
        writer.commit(&changed),
        Err(StoreError::CommitIdentityConflict)
    ));
    let stale = op(&id, None, "would overwrite");
    assert!(matches!(
        writer.commit(&stale),
        Err(StoreError::Conflict { .. })
    ));
    let update = op(&id, Some(1), "update");
    writer.commit(&update).unwrap();
    assert_eq!(writer.read_latest().unwrap(), Some(update.record.clone()));
    assert!(matches!(
        writer.resolve_commit(&initial),
        Err(StoreError::ConfirmationUnknown)
    ));
    assert!(store.metrics().bytes_written > 0);
    assert!(store.metrics().snapshot_clones > 0);
    assert_eq!(store.metrics().serialization_count, 2);
}

#[test]
fn lost_reply_confirms_same_record_and_same_revision() {
    let (_folder, store, id, writer) = setup();
    let initial = op(&id, None, "saved");
    writer.set_fault_hook(Some(failure(CommitStage::Reply)));
    assert!(matches!(
        writer.commit(&initial),
        Err(StoreError::CommitIo {
            stage: CommitStage::Reply,
            ..
        })
    ));
    assert_eq!(
        writer.resolve_commit(&initial).unwrap(),
        CommitResolution::Applied(Box::new(initial.record.clone()))
    );
    writer.set_fault_hook(None);
    assert_eq!(writer.commit(&initial).unwrap().revision, 1);
    assert_eq!(store.metrics().writes, 1);
}

#[test]
fn prewrite_read_failure_retains_the_exact_operation_for_confirmation() {
    let (_folder, store, id, writer) = setup();
    let initial = op(&id, None, "old");
    writer.commit(&initial).unwrap();
    let update = op(&id, Some(1), "new");

    let conn = schema::open_read_write(store.db_path()).unwrap();
    let original_policy: String = conn
        .query_row(
            "SELECT policy_json FROM sessions WHERE session_id = ?1",
            params![&id],
            |row| row.get(0),
        )
        .unwrap();
    conn.execute(
        "UPDATE sessions SET policy_json = 'not-json' WHERE session_id = ?1",
        params![&id],
    )
    .unwrap();
    assert!(writer.commit(&update).is_err());

    conn.execute(
        "UPDATE sessions SET policy_json = ?1 WHERE session_id = ?2",
        params![original_policy, &id],
    )
    .unwrap();
    assert_eq!(
        writer.resolve_commit(&update).unwrap(),
        CommitResolution::NotApplied
    );
    let different = op(&id, Some(1), "different");
    assert!(matches!(
        writer.commit(&different),
        Err(StoreError::PendingCommit)
    ));
    assert_eq!(writer.commit(&update).unwrap().revision, 2);
    assert_eq!(store.metrics().writes, 2);
}

#[test]
fn failures_before_replace_leave_full_old_baseline_and_retry_original() {
    for stage in [
        CommitStage::Serialize,
        CommitStage::Write,
        CommitStage::Sync,
        CommitStage::Replace,
    ] {
        let (_folder, store, id, writer) = setup();
        let initial = op(&id, None, "old");
        writer.commit(&initial).unwrap();
        let update = op(&id, Some(1), "new");
        writer.set_fault_hook(Some(failure(stage)));
        assert!(matches!(
            writer.commit(&update),
            Err(StoreError::CommitIo { .. })
        ));
        assert_eq!(writer.read_latest().unwrap(), Some(initial.record.clone()));
        assert_eq!(
            writer.resolve_commit(&update).unwrap(),
            CommitResolution::NotApplied
        );
        let other = op(&id, Some(1), "different pending");
        assert!(matches!(
            writer.commit(&other),
            Err(StoreError::PendingCommit)
        ));
        assert_eq!(store.list().unwrap().len(), 1);
        writer.set_fault_hook(None);
        writer.commit(&update).unwrap();
    }
}

#[test]
fn advanced_or_changed_baseline_never_claims_not_applied() {
    let (_folder, store, id, writer) = setup();
    let initial = op(&id, None, "old");
    writer.commit(&initial).unwrap();
    let update = op(&id, Some(1), "new");
    writer.set_fault_hook(Some(failure(CommitStage::Sync)));
    writer.commit(&update).unwrap_err();
    let changed_ctx = serde_json::to_string(&context("other bytes same revision")).unwrap();
    let conn = schema::open_read_write(store.db_path()).unwrap();
    conn.execute(
        "UPDATE agents SET context_json = ?1 WHERE session_id = ?2 AND agent_id = ?2",
        params![changed_ctx, &id],
    )
    .unwrap();
    assert!(matches!(
        writer.resolve_commit(&update),
        Err(StoreError::Conflict { .. })
    ));
    drop(writer);
    let reopened = store.acquire_writer(&id).unwrap();
    reopened.read_latest().unwrap();
    assert!(matches!(
        reopened.resolve_commit(&update),
        Err(StoreError::ConfirmationUnknown)
    ));
}

#[test]
fn records_with_the_old_project_prefix_are_unsupported() {
    let (_folder, store, id, writer) = setup();
    let initial = op(&id, None, "old");
    writer.commit(&initial).unwrap();
    let conn = schema::open_read_write(store.db_path()).unwrap();
    conn.execute(
        r#"UPDATE sessions SET policy_json = '{"format":"tarz-session-v2"}' WHERE session_id = ?1"#,
        params![&id],
    )
    .unwrap();
    assert!(matches!(
        writer.read_latest(),
        Err(StoreError::UnsupportedFormat(format)) if format == "tarz-session-v2"
    ));
}

#[test]
fn bad_records_fail_individually_and_list_reports_active_lock() {
    let (_folder, store, id, writer) = setup();
    let initial = op(&id, None, "old");
    writer.commit(&initial).unwrap();

    let other_id = Uuid::now_v7().to_string();
    let other_writer = store.acquire_writer(&other_id).unwrap();
    other_writer
        .commit(&op(&other_id, None, "healthy session"))
        .unwrap();
    drop(other_writer);

    let conn = schema::open_read_write(store.db_path()).unwrap();
    let orig_policy: String = conn
        .query_row(
            "SELECT policy_json FROM sessions WHERE session_id = ?1",
            params![&id],
            |row| row.get(0),
        )
        .unwrap();
    let orig_commit: String = conn
        .query_row(
            "SELECT last_commit_id FROM sessions WHERE session_id = ?1",
            params![&id],
            |row| row.get(0),
        )
        .unwrap();
    let orig_context: String = conn
        .query_row(
            "SELECT context_json FROM agents WHERE session_id = ?1 AND agent_id = ?1",
            params![&id],
            |row| row.get(0),
        )
        .unwrap();

    for field in ["format", "last_commit_id", "revision", "blocks"] {
        // Restore baseline before each mutation.
        conn.execute(
            "UPDATE sessions SET policy_json = ?1, last_commit_id = ?2, revision = 1 WHERE session_id = ?3",
            params![orig_policy, orig_commit, &id],
        )
        .unwrap();
        conn.execute(
            "UPDATE agents SET context_json = ?1 WHERE session_id = ?2 AND agent_id = ?2",
            params![orig_context, &id],
        )
        .unwrap();

        match field {
            "format" => {
                conn.execute(
                    r#"UPDATE sessions SET policy_json = '{"format":"future-version"}' WHERE session_id = ?1"#,
                    params![&id],
                )
                .unwrap();
            }
            "last_commit_id" => {
                conn.execute(
                    "UPDATE sessions SET last_commit_id = '../escape' WHERE session_id = ?1",
                    params![&id],
                )
                .unwrap();
            }
            "revision" => {
                conn.execute(
                    "UPDATE sessions SET revision = 0 WHERE session_id = ?1",
                    params![&id],
                )
                .unwrap();
            }
            "blocks" => {
                let mut ctx_val: serde_json::Value = serde_json::from_str(&orig_context).unwrap();
                let block = ctx_val["blocks"][0].clone();
                ctx_val["blocks"] = serde_json::json!([block, block]);
                conn.execute(
                    "UPDATE agents SET context_json = ?1 WHERE session_id = ?2 AND agent_id = ?2",
                    params![serde_json::to_string(&ctx_val).unwrap(), &id],
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(writer.read_latest().is_err(), "{field}");
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 2);
        let bad = listed.iter().find(|s| s.conversation_id == id).unwrap();
        assert!(bad.record.is_err(), "{field}");
        assert_eq!(bad.active.as_ref().unwrap(), &true);
        let good = listed
            .iter()
            .find(|s| s.conversation_id == other_id)
            .unwrap();
        assert!(good.record.is_ok());
        assert_eq!(good.active.as_ref().unwrap(), &false);
    }

    conn.execute(
        "UPDATE sessions SET policy_json = ?1, last_commit_id = ?2, revision = 1 WHERE session_id = ?3",
        params![orig_policy, orig_commit, &id],
    )
    .unwrap();
    conn.execute(
        "UPDATE agents SET context_json = 'bad json' WHERE session_id = ?1 AND agent_id = ?1",
        params![&id],
    )
    .unwrap();
    assert!(matches!(writer.read_latest(), Err(StoreError::Json(_))));
    assert!(matches!(
        store.acquire_writer("../escape"),
        Err(StoreError::InvalidIdentity(_))
    ));

    let media_ctx = Context::from_blocks(vec![ContextBlock::new(
        new_block_id(),
        BlockContent::Message {
            role: causa::kernel::Role::Assistant,
            parts: vec![ContentPart::Media(MediaRef::new("image/png", "missing"))],
        },
        BlockMeta::default(),
    )])
    .unwrap();
    conn.execute(
        "UPDATE agents SET context_json = ?1 WHERE session_id = ?2 AND agent_id = ?2",
        params![serde_json::to_string(&media_ctx).unwrap(), &id],
    )
    .unwrap();
    assert!(matches!(
        writer.read_latest(),
        Err(StoreError::References(_))
    ));
}

#[test]
fn stable_lock_and_exclusive_ownership() {
    let (_folder, store, id, writer) = setup();
    let lock_path = store.locks_dir().join(format!("{id}.lock"));
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(&lock_path).unwrap().ino()
    };
    assert!(matches!(
        store.acquire_writer(&id),
        Err(StoreError::Occupied(_))
    ));
    drop(writer);
    let next = store.acquire_writer(&id).unwrap();
    next.commit(&op(&id, None, "saved")).unwrap();
    drop(next);
    assert!(lock_path.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(identity, fs::metadata(lock_path).unwrap().ino());
    }
}

#[test]
fn resource_limit_rejects_oversized_load_and_checked_revisions() {
    let (_folder, store, id, writer) = setup();
    writer.commit(&op(&id, None, "seed")).unwrap();
    let conn = schema::open_read_write(store.db_path()).unwrap();
    let oversized_ctx = "x".repeat(MAX_RECORD_BYTES + 1);
    conn.execute(
        "UPDATE agents SET context_json = ?1 WHERE session_id = ?2 AND agent_id = ?2",
        params![oversized_ctx, &id],
    )
    .unwrap();
    assert!(matches!(
        writer.read_latest(),
        Err(StoreError::RecordTooLarge { .. })
    ));
    assert!(
        SavedSessionRecord::new(
            id,
            Uuid::now_v7().to_string(),
            Some(u64::MAX),
            definition(),
            Context::new(),
            None,
            None,
            1_000,
        )
        .is_err()
    );
}

// Test executable is the actual other process. The parent waits on a marker
// emitted from the operation itself, then kills the child at a known stage.
#[test]
fn subprocess_helper() {
    let Ok(root) = std::env::var("TARS_STORE_CHILD_ROOT") else {
        return;
    };
    let id = std::env::var("TARS_STORE_CHILD_ID").unwrap();
    let mode = std::env::var("TARS_STORE_CHILD_MODE").unwrap();
    let store = SessionStore::new(root.clone().into());
    if mode == "contention" {
        assert!(matches!(
            store.acquire_writer(&id),
            Err(StoreError::Occupied(_))
        ));
        let other = Uuid::now_v7().to_string();
        let other_writer = store.acquire_writer(&other).unwrap();
        other_writer
            .commit(&op(&other, None, "concurrent other process"))
            .unwrap();
        return;
    }
    let writer = store.acquire_writer(&id).unwrap();
    let baseline = writer.read_latest().unwrap();
    let prior = baseline.as_ref().map(|r| r.revision);
    let stage = if mode == "before" {
        CommitStage::Replace
    } else {
        CommitStage::Confirm
    };
    writer.set_fault_hook(Some(Arc::new(move |at| {
        if at == stage {
            fs::write(std::path::Path::new(&root).join("barrier"), b"ready").unwrap();
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        FaultAction::Continue
    })));
    writer.commit(&op(&id, prior, "new child content")).unwrap();
}

fn child(folder: &tempfile::TempDir, id: &str, mode: &str) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "store::tests::subprocess_helper", "--nocapture"])
        .env("TARS_STORE_CHILD_ROOT", folder.path())
        .env("TARS_STORE_CHILD_ID", id)
        .env("TARS_STORE_CHILD_MODE", mode)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

#[test]
fn two_real_processes_contend_while_distinct_sessions_remain_available() {
    let (folder, store, id, _writer) = setup();
    assert!(
        child(&folder, &id, "contention")
            .wait_with_output()
            .unwrap()
            .status
            .success()
    );
    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert!(listed[0].record.is_ok());
}

#[test]
fn killed_processes_leave_complete_old_new_or_initial_absence() {
    for existing in [false, true] {
        for stage in ["before", "after"] {
            let (folder, store, id, writer) = setup();
            let old = op(&id, None, "old");
            if existing {
                writer.commit(&old).unwrap();
            }
            drop(writer);
            let mut process = child(&folder, &id, stage);
            let deadline = Instant::now() + Duration::from_secs(10);
            while !folder.path().join("barrier").exists() {
                assert!(Instant::now() < deadline, "child barrier timeout");
                if let Some(status) = process.try_wait().unwrap() {
                    panic!("child exited early: {status}");
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            // In-flight holder excludes another process/owner until confirmed dead.
            assert!(matches!(
                store.acquire_writer(&id),
                Err(StoreError::Occupied(_))
            ));
            process.kill().unwrap();
            process.wait().unwrap();
            let writer = store.acquire_writer(&id).unwrap();
            let restored = writer.read_latest().unwrap();
            if stage == "before" {
                assert_eq!(restored, existing.then_some(old.record));
            } else {
                let restored = restored.unwrap();
                assert_eq!(restored.revision, if existing { 2 } else { 1 });
                assert_child_text(&restored.context);
            }
        }
    }
}

fn assert_child_text(context: &Context) {
    assert_eq!(context.blocks().len(), 1);
    assert!(
        matches!(context.blocks()[0].content(), BlockContent::Message { parts, .. }
        if matches!(&parts[0], ContentPart::Text(text) if text.0 == "new child content"))
    );
}

#[test]
fn a_relative_root_resolves_to_an_absolute_directory() {
    let store = SessionStore::new(std::path::PathBuf::from("relative-store"));
    assert!(store.root().is_absolute());
    assert!(store.root().ends_with("relative-store"));
    assert!(store.db_path().ends_with("relative-store/tars.db"));
}

#[test]
fn a_relative_root_with_parent_components_is_normalized() {
    let store = SessionStore::new(std::path::PathBuf::from("missing/../data"));
    assert!(store.root().is_absolute());
    assert!(!store.root().to_string_lossy().contains(".."));
    assert!(store.root().ends_with("data"));
}

#[test]
fn oversized_save_stops_before_disk_write_and_store_hook_reaches_new_writer() {
    let (_folder, store, id, writer) = setup();
    let initial = op(&id, None, "small old record");
    writer.commit(&initial).unwrap();
    let oversized = op(&id, Some(1), &"x".repeat(MAX_RECORD_BYTES));
    assert!(matches!(
        writer.commit(&oversized),
        Err(StoreError::RecordTooLarge { .. })
    ));
    assert_eq!(writer.read_latest().unwrap(), Some(initial.record));
    assert_eq!(store.metrics().writes, 1);
    assert_eq!(
        writer.resolve_commit(&oversized).unwrap(),
        CommitResolution::NotApplied
    );
    drop(writer);
    store.set_fault_hook(Some(failure(CommitStage::Write)));
    let new_id = Uuid::now_v7().to_string();
    let writer = store.acquire_writer(&new_id).unwrap();
    assert!(matches!(
        writer.commit(&op(&new_id, None, "blocked initial")),
        Err(StoreError::CommitIo {
            stage: CommitStage::Write,
            ..
        })
    ));
}

#[test]
fn session_records_written_before_message_roles_load_and_save_in_the_current_form() {
    use causa::kernel::Role;
    let (_folder, store, id, writer) = setup();
    let old_record = op(&id, None, "assistant message");
    writer.commit(&old_record).unwrap();

    let mut ctx_val = serde_json::to_value(&old_record.record.context).unwrap();
    let parts = ctx_val["blocks"][0]["content"]["value"]["parts"].clone();
    ctx_val["blocks"][0]["content"] = serde_json::json!({
        "shape": "parts",
        "value": parts,
    });
    let conn = schema::open_read_write(store.db_path()).unwrap();
    conn.execute(
        "UPDATE agents SET context_json = ?1 WHERE session_id = ?2 AND agent_id = ?2",
        params![serde_json::to_string(&ctx_val).unwrap(), &id],
    )
    .unwrap();

    let loaded = writer.read_latest().unwrap().unwrap();
    assert!(matches!(
        loaded.context.blocks()[0].content(),
        BlockContent::Message {
            role: Role::Assistant,
            ..
        }
    ));
}

#[test]
fn duplicate_history_entries_in_record_are_rejected() {
    let record_id = uuid::Uuid::now_v7().to_string();
    let duplicate = vec![
        crate::compact::HistoryRecordEntry {
            record_id: record_id.clone(),
            created_at_ms: 1,
            trigger: crate::compact::HistoryTrigger::Auto,
            kind: crate::compact::RewriteKind::Summarize,
            removed_block_count: 1,
            inserted_block_ids: vec![new_block_id()],
        },
        crate::compact::HistoryRecordEntry {
            record_id,
            created_at_ms: 2,
            trigger: crate::compact::HistoryTrigger::Manual,
            kind: crate::compact::RewriteKind::Summarize,
            removed_block_count: 1,
            inserted_block_ids: vec![new_block_id()],
        },
    ];
    let record = super::SavedSessionRecord::new_with_compaction(
        uuid::Uuid::now_v7().to_string(),
        uuid::Uuid::now_v7().to_string(),
        None,
        definition(),
        Context::default(),
        None,
        None,
        None,
        duplicate,
        None,
        None,
        1_000,
    );
    assert!(record.is_err());
}

#[test]
fn test_be02_new_sessions_write_v2_4_and_opening_older_versions_fails_with_advice() {
    let (folder, store, id, writer) = setup();

    // 1. New sessions record tars-session-v2.4 in sessions.policy_json
    let initial = op(&id, None, "new session text");
    writer.commit(&initial).unwrap();
    let conn = schema::open_read_write(store.db_path()).unwrap();
    let policy_json: String = conn
        .query_row(
            "SELECT policy_json FROM sessions WHERE session_id = ?1",
            params![&id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(policy_json.contains("tars-session-v2.4"));

    // 2. Older session versions stored in SQLite fail with both version names & export advice
    for old_format in ["tars-session-v2.1", "tars-session-v2.3"] {
        let old_id = Uuid::now_v7().to_string();
        let old_writer = store.acquire_writer(&old_id).unwrap();
        old_writer
            .commit(&op(&old_id, None, "seed old format"))
            .unwrap();
        let bad_policy = serde_json::json!({ "format": old_format }).to_string();
        conn.execute(
            "UPDATE sessions SET policy_json = ?1 WHERE session_id = ?2",
            params![bad_policy, &old_id],
        )
        .unwrap();

        let read_err = old_writer.read_latest().unwrap_err();
        let err_msg = read_err.to_string();
        assert!(err_msg.contains(old_format), "{err_msg}");
        assert!(err_msg.contains("tars-session-v2.4"), "{err_msg}");
        assert!(
            err_msg.contains("tars session export-materials"),
            "{err_msg}"
        );

        let list = store.list().unwrap();
        let found = list
            .iter()
            .find(|item| item.conversation_id == old_id)
            .expect("old session in listing");
        assert!(found.record.is_err());
        let list_err = found.record.as_ref().unwrap_err().to_string();
        assert!(list_err.contains(old_format));
        assert!(list_err.contains("tars-session-v2.4"));
    }

    // 3. Reading v1 and v1.1 history records from SQLite fails with both version names
    let history_store = crate::compact::history::HistoryStore::new(folder.path(), &id, Vec::new());
    for old_hist_format in ["tars-history-v1", "tars-history-v1.1"] {
        let old_record_id = Uuid::now_v7().to_string();
        let old_history_payload = serde_json::json!({
            "format": old_hist_format,
            "conversation_id": id,
            "record_id": old_record_id,
            "created_at_ms": 12345678,
            "trigger": "auto",
            "kind": "summarize",
            "estimate_before": 1000,
            "estimate_after": 200,
            "replacements": []
        });
        conn.execute(
            "INSERT INTO history_records (session_id, agent_id, record_id, seq, kind, summary_json, document_json)
             VALUES (?1, ?1, ?2, 0, 'summarize', '{}', ?3)",
            params![&id, &old_record_id, old_history_payload.to_string()],
        )
        .unwrap();

        let hist_err = history_store.read(&old_record_id).unwrap_err();
        let hist_msg = hist_err.to_string();
        assert!(hist_msg.contains(old_hist_format), "{hist_msg}");
        assert!(hist_msg.contains("tars-history-v1.2"), "{hist_msg}");
        assert!(
            hist_msg.contains("tars session export-materials"),
            "{hist_msg}"
        );
    }
}

#[test]
fn title_derivation_skips_prelude_and_compaction_blocks() {
    use crate::prelude::{
        PRELUDE_BASE_SOURCE, PRELUDE_ENV_SOURCE, PRELUDE_WORKSPACE_INSTRUCTIONS_SOURCE,
    };
    use causa::kernel::Role;

    let prelude_only = Context::from_blocks(vec![
        ContextBlock::new(
            new_block_id(),
            BlockContent::text(Role::System, "host contract"),
            BlockMeta::from_source(PRELUDE_BASE_SOURCE),
        ),
        ContextBlock::new(
            new_block_id(),
            BlockContent::text(
                Role::User,
                "<workspace_instructions>\nIgnore me\n</workspace_instructions>",
            ),
            BlockMeta::from_source(PRELUDE_WORKSPACE_INSTRUCTIONS_SOURCE),
        ),
        ContextBlock::new(
            new_block_id(),
            BlockContent::text(Role::User, "<env>\nplatform: linux\n</env>"),
            BlockMeta::from_source(PRELUDE_ENV_SOURCE),
        ),
        ContextBlock::new(
            new_block_id(),
            BlockContent::text(Role::User, "compacted brief"),
            BlockMeta::from_source(crate::compact::SUMMARY_SOURCE),
        ),
        ContextBlock::new(
            new_block_id(),
            BlockContent::text(
                Role::User,
                "<skill_content name=\"alpha\" sha256=\"abc\">\nSkill body\n</skill_content>",
            ),
            BlockMeta::from_source(crate::skills::SKILL_SOURCE),
        ),
    ])
    .unwrap();

    let id = Uuid::now_v7().to_string();
    let initial_record = SavedSessionRecord::new(
        id.clone(),
        Uuid::now_v7().to_string(),
        None,
        definition(),
        prelude_only.clone(),
        None,
        None,
        1_000,
    )
    .unwrap();
    let meta_after_prelude = super::RecordMetadata::of(&initial_record);
    assert_eq!(meta_after_prelude.title, None);
    assert!(!meta_after_prelude.title_settled);

    let mut blocks = prelude_only.blocks().to_vec();
    blocks.push(ContextBlock::new(
        new_block_id(),
        BlockContent::text(
            Role::User,
            "<env updated=\"true\">\nmode: accept-edits\n</env>",
        ),
        BlockMeta::from_source("tars:prelude:update:env"),
    ));
    blocks.push(ContextBlock::new(
        new_block_id(),
        BlockContent::text(Role::User, "   Fix   the   login   bug  \nsecond line"),
        BlockMeta::from_source("user"),
    ));
    let with_user = Context::from_blocks(blocks).unwrap();
    let (derived_title, _) = meta_after_prelude.next(&with_user, 1_000);
    assert_eq!(derived_title.as_deref(), Some("Fix the login bug"));

    let saved_with_user = SavedSessionRecord::new(
        id,
        Uuid::now_v7().to_string(),
        Some(1),
        definition(),
        with_user,
        None,
        derived_title,
        2_000,
    )
    .unwrap();
    let settled = super::RecordMetadata::of(&saved_with_user);
    assert_eq!(settled.title.as_deref(), Some("Fix the login bug"));
    assert!(settled.title_settled);
}

#[test]
fn sqlite_schema_has_seven_tables_wal_pragmas_and_rejects_future_user_version() {
    let folder = tempfile::tempdir().unwrap();
    let store = SessionStore::new(folder.path().into());

    // Read-only operations on a fresh store must not create tars.db or locks/.
    assert!(store.list().unwrap().is_empty());
    let fresh_id = Uuid::now_v7().to_string();
    assert_eq!(store.read_saved(&fresh_id).unwrap(), None);
    assert!(!store.db_path().exists());
    assert!(!store.locks_dir().exists());

    // Acquiring a writer initializes tars.db with all 7 tables and WAL PRAGMAs.
    let writer = store.acquire_writer(&fresh_id).unwrap();
    assert!(store.db_path().exists());
    let conn = schema::open_read_write(store.db_path()).unwrap();

    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode;", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    let fk: i64 = conn
        .query_row("PRAGMA foreign_keys;", [], |row| row.get(0))
        .unwrap();
    assert_eq!(fk, 1);
    let user_version: u32 = conn
        .query_row("PRAGMA user_version;", [], |row| row.get(0))
        .unwrap();
    assert_eq!(user_version, 1);

    let tables: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                 ORDER BY name ASC",
            )
            .unwrap();
        stmt.query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(
        tables,
        vec![
            "agent_events",
            "agents",
            "artifacts",
            "history_records",
            "operations",
            "sessions",
            "workspace_trust",
        ]
    );

    // Future schema version (> 1) must fail closed on both read and write opens.
    drop(writer);
    conn.execute_batch("PRAGMA user_version = 99;").unwrap();
    drop(conn);

    assert!(matches!(
        store.list(),
        Err(StoreError::UnsupportedSchemaVersion {
            found: 99,
            supported: 1
        })
    ));
    assert!(matches!(
        store.read_saved(&fresh_id),
        Err(StoreError::UnsupportedSchemaVersion {
            found: 99,
            supported: 1
        })
    ));
    assert!(matches!(
        store.acquire_writer(&fresh_id),
        Err(StoreError::UnsupportedSchemaVersion {
            found: 99,
            supported: 1
        })
    ));
}

#[test]
fn wal_concurrent_reads_succeed_while_writer_holds_os_lock_and_commits() {
    let (_folder, store, id, writer) = setup();
    let first = op(&id, None, "rev1");
    writer.commit(&first).unwrap();

    // Writer still holds the OS lock; lock-free WAL readers see rev1 and active=true.
    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].record.as_ref().unwrap().revision, 1);
    assert_eq!(listed[0].active.as_ref().unwrap(), &true);

    // Commit rev2 while holding lock; reader sees rev2 without contending for OS lock.
    let second = op(&id, Some(1), "rev2");
    writer.commit(&second).unwrap();
    let read = store.read_saved(&id).unwrap().unwrap();
    assert_eq!(read.revision, 2);
}

#[test]
fn strict_history_store_enforces_foreign_key_against_missing_session() {
    let folder = tempfile::tempdir().unwrap();
    let store = SessionStore::new(folder.path().into());
    // Ensure database exists with schema v1, but no session/agent rows.
    let _ = schema::open_read_write(store.db_path()).unwrap();

    let missing_id = Uuid::now_v7().to_string();
    let strict =
        crate::compact::history::HistoryStore::new_strict(store.root(), &missing_id, Vec::new());
    assert!(matches!(
        strict.persist_spilled_artifact(b"orphan bytes"),
        Err(StoreError::Sqlite(_))
    ));
}

#[test]
fn legacy_v2_4_import_preserves_record_history_and_artifacts_without_modifying_source() {
    use crate::compact::HistoryTrigger;
    use crate::compact::history::{HISTORY_FORMAT, HistoryRecord, RangeReplacement, RewriteKind};
    use causa::kernel::{
        ArtifactKind, ArtifactRef, Role, ToolCallPayload, ToolOutput, ToolResultPayload,
        ToolResultStatus,
    };

    let folder = tempfile::tempdir().unwrap();
    let legacy_dir = folder.path().join("legacy");
    fs::create_dir_all(&legacy_dir).unwrap();

    let conv_id = Uuid::now_v7().to_string();
    let commit_id = Uuid::now_v7().to_string();
    let hist_id = Uuid::now_v7().to_string();
    let art_uuid = Uuid::now_v7().to_string();
    let art_ref_id = format!("artifact:{art_uuid}");

    let call_id = new_block_id();
    let res_id = new_block_id();
    let mut out = ToolOutput::new(serde_json::json!("preview"));
    out.artifact = Some(ArtifactRef::new(
        art_ref_id.clone(),
        14,
        ArtifactKind::FullOutput,
        true,
    ));
    let original_res_block = ContextBlock::new(
        res_id,
        BlockContent::ToolResult(ToolResultPayload::new(
            call_id,
            ToolResultStatus::Succeeded,
            out,
        )),
        BlockMeta::default(),
    );
    let original_call_block = ContextBlock::new(
        call_id,
        BlockContent::ToolCall(ToolCallPayload::new("read_file", serde_json::json!({}))),
        BlockMeta::default(),
    );
    let summary_block = ContextBlock::new(
        new_block_id(),
        BlockContent::text(Role::User, "brief summary"),
        BlockMeta::from_source(crate::compact::SUMMARY_SOURCE),
    );
    let current_user = ContextBlock::new(
        new_block_id(),
        BlockContent::text(Role::User, "follow-up"),
        BlockMeta::default(),
    );

    let history_doc = HistoryRecord {
        format: HISTORY_FORMAT.to_owned(),
        conversation_id: conv_id.clone(),
        record_id: hist_id.clone(),
        created_at_ms: 500,
        trigger: HistoryTrigger::Auto,
        kind: RewriteKind::Summarize,
        estimate_before: 1_000,
        estimate_after: 100,
        replacements: vec![RangeReplacement {
            range: 0..2,
            originals: vec![original_call_block, original_res_block],
            inserted_ids: vec![summary_block.id()],
        }],
    };
    let history_entry = history_doc.index_entry();

    let mut def = definition();
    def.permission_mode = Some(crate::policy::PermissionMode::AcceptEdits);
    let record = SavedSessionRecord::new_with_compaction(
        conv_id.clone(),
        commit_id,
        Some(2),
        def,
        Context::from_blocks(vec![summary_block, current_user]).unwrap(),
        Some(crate::policy::PermissionMode::AcceptEdits),
        Some(false),
        Some(80),
        vec![history_entry],
        None,
        Some("Legacy Title".into()),
        9_999,
    )
    .unwrap();

    let legacy_record_path = legacy_dir.join(format!("{conv_id}.json"));
    let legacy_history_dir = legacy_dir.join(format!("{conv_id}.history"));
    let legacy_artifacts_dir = legacy_dir.join(format!("{conv_id}.artifacts"));
    fs::create_dir_all(&legacy_history_dir).unwrap();
    fs::create_dir_all(&legacy_artifacts_dir).unwrap();
    let legacy_hist_path = legacy_history_dir.join(format!("{hist_id}.json"));
    let legacy_art_path = legacy_artifacts_dir.join(&art_uuid);

    let record_bytes = serde_json::to_vec(&record).unwrap();
    let hist_bytes = serde_json::to_vec(&history_doc).unwrap();
    let art_bytes = b"spilled payload";
    fs::write(&legacy_record_path, &record_bytes).unwrap();
    fs::write(&legacy_hist_path, &hist_bytes).unwrap();
    fs::write(&legacy_art_path, art_bytes).unwrap();

    let store = SessionStore::new(folder.path().join("sqlite-store"));
    let imported = store.import_legacy_v2_4(&legacy_record_path).unwrap();
    assert_eq!(imported.conversation_id, conv_id);
    assert_eq!(imported.revision, 3);
    assert_eq!(imported.block_count, 2);
    assert_eq!(imported.history_records, 1);
    assert_eq!(imported.artifacts, 1);

    // Source files remain byte-for-byte untouched.
    assert_eq!(fs::read(&legacy_record_path).unwrap(), record_bytes);
    assert_eq!(fs::read(&legacy_hist_path).unwrap(), hist_bytes);
    assert_eq!(fs::read(&legacy_art_path).unwrap(), art_bytes);

    // Duplicate import is refused cleanly.
    assert!(matches!(
        store.import_legacy_v2_4(&legacy_record_path),
        Err(StoreError::AlreadyExists(ref existing)) if existing == &conv_id
    ));

    // Verify imported record, history record, and spilled artifact in SQLite.
    let saved = store.read_saved(&conv_id).unwrap().unwrap();
    assert_eq!(saved, record);
    let history_store = crate::compact::history::HistoryStore::new_strict(
        store.root(),
        &conv_id,
        saved.history.clone(),
    );
    assert_eq!(history_store.read(&hist_id).unwrap(), history_doc);
    assert_eq!(
        history_store
            .read_spilled_artifact(&art_uuid, None)
            .unwrap(),
        art_bytes
    );
}

#[test]
fn open_read_write_fails_closed_when_user_version_is_zero_with_existing_tables() {
    let folder = tempfile::tempdir().unwrap();
    let db_path = folder.path().join("tars.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE preexisting_table (id INTEGER PRIMARY KEY);
             PRAGMA user_version = 0;",
        )
        .unwrap();
    }

    let err = super::schema::open_read_write(&db_path).unwrap_err();
    assert!(
        matches!(err, StoreError::InvalidDatabase(ref msg) if msg.contains("user_version 0")),
        "unexpected error: {err:?}"
    );
}

#[test]
fn list_isolates_column_storage_class_mismatch_to_the_corrupt_session() {
    let folder = tempfile::tempdir().unwrap();
    let store = SessionStore::new(folder.path().to_path_buf());
    let good_id = Uuid::now_v7().to_string();
    let corrupt_id = Uuid::now_v7().to_string();

    let good_writer = store.acquire_writer(&good_id).unwrap();
    let good_op = op(&good_id, None, "good session");
    let good_record = good_op.record.clone();
    good_writer.commit(&good_op).unwrap();
    drop(good_writer);

    let corrupt_writer = store.acquire_writer(&corrupt_id).unwrap();
    corrupt_writer
        .commit(&op(&corrupt_id, None, "corrupt session"))
        .unwrap();
    drop(corrupt_writer);

    // Inject a column storage-class mismatch (TEXT stored in INTEGER revision column).
    {
        let conn = rusqlite::Connection::open(store.db_path()).unwrap();
        conn.execute(
            "UPDATE sessions SET revision = 'corrupt-text' WHERE session_id = ?1",
            rusqlite::params![corrupt_id],
        )
        .unwrap();
    }

    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 2);

    let good = listed
        .iter()
        .find(|item| item.conversation_id == good_id)
        .unwrap();
    assert_eq!(good.record.as_ref().unwrap(), &good_record);

    let corrupt = listed
        .iter()
        .find(|item| item.conversation_id == corrupt_id)
        .unwrap();
    assert!(
        matches!(corrupt.record, Err(StoreError::Sqlite(_))),
        "expected sqlite error on corrupt row, got {:?}",
        corrupt.record
    );
}

#[test]
fn import_legacy_v2_4_fails_closed_on_malformed_artifact_id_in_history_originals() {
    use crate::compact::HistoryTrigger;
    use crate::compact::history::{HISTORY_FORMAT, HistoryRecord, RangeReplacement, RewriteKind};
    use causa::kernel::{
        ArtifactKind, ArtifactRef, Role, ToolCallPayload, ToolOutput, ToolResultPayload,
        ToolResultStatus,
    };

    let folder = tempfile::tempdir().unwrap();
    let legacy_dir = folder.path().join("legacy");
    fs::create_dir_all(&legacy_dir).unwrap();

    let conv_id = Uuid::now_v7().to_string();
    let hist_id = Uuid::now_v7().to_string();
    let commit_id = Uuid::now_v7().to_string();

    let call_id = new_block_id();
    let mut bad_out = ToolOutput::new(serde_json::json!("preview"));
    bad_out.artifact = Some(ArtifactRef::new(
        "artifact:not-a-uuid".to_owned(),
        14,
        ArtifactKind::FullOutput,
        true,
    ));
    let original_call_block = ContextBlock::new(
        call_id,
        BlockContent::ToolCall(ToolCallPayload::new("read_file", serde_json::json!({}))),
        BlockMeta::default(),
    );
    let bad_res_block = ContextBlock::new(
        new_block_id(),
        BlockContent::ToolResult(ToolResultPayload::new(
            call_id,
            ToolResultStatus::Succeeded,
            bad_out,
        )),
        BlockMeta::default(),
    );
    let summary_block = ContextBlock::new(
        new_block_id(),
        BlockContent::text(Role::User, "summary"),
        BlockMeta::from_source(crate::compact::SUMMARY_SOURCE),
    );
    let history_doc = HistoryRecord {
        format: HISTORY_FORMAT.to_owned(),
        conversation_id: conv_id.clone(),
        record_id: hist_id.clone(),
        created_at_ms: 1_000,
        trigger: HistoryTrigger::Auto,
        kind: RewriteKind::Summarize,
        estimate_before: 1_000,
        estimate_after: 100,
        replacements: vec![RangeReplacement {
            range: 0..2,
            originals: vec![original_call_block, bad_res_block],
            inserted_ids: vec![summary_block.id()],
        }],
    };

    let mut def = definition();
    def.permission_mode = Some(crate::policy::PermissionMode::Ask);
    let record = SavedSessionRecord::new_with_compaction(
        conv_id.clone(),
        commit_id,
        Some(1),
        def,
        Context::from_blocks(vec![summary_block]).unwrap(),
        Some(crate::policy::PermissionMode::Ask),
        None,
        None,
        vec![history_doc.index_entry()],
        None,
        None,
        2_000,
    )
    .unwrap();

    let legacy_record_path = legacy_dir.join(format!("{conv_id}.json"));
    let legacy_history_dir = legacy_dir.join(format!("{conv_id}.history"));
    fs::create_dir_all(&legacy_history_dir).unwrap();
    fs::write(&legacy_record_path, serde_json::to_vec(&record).unwrap()).unwrap();
    fs::write(
        legacy_history_dir.join(format!("{hist_id}.json")),
        serde_json::to_vec(&history_doc).unwrap(),
    )
    .unwrap();

    let store = SessionStore::new(folder.path().join("sqlite-store"));
    let err = store.import_legacy_v2_4(&legacy_record_path).unwrap_err();
    assert!(
        matches!(err, StoreError::InvalidHistory(_)),
        "expected InvalidHistory for malformed artifact id, got {err:?}"
    );
    assert!(!store.db_path().exists());
}

#[cfg(unix)]
#[test]
fn import_legacy_v2_4_rejects_symlinked_directories_and_child_files() {
    use crate::compact::HistoryTrigger;
    use crate::compact::history::{HISTORY_FORMAT, HistoryRecord, RangeReplacement, RewriteKind};
    use causa::kernel::{
        ArtifactKind, ArtifactRef, Role, ToolCallPayload, ToolOutput, ToolResultPayload,
        ToolResultStatus,
    };
    use std::os::unix::fs::symlink;

    let conv_id = Uuid::now_v7().to_string();
    let hist_id = Uuid::now_v7().to_string();
    let art_uuid = Uuid::now_v7().to_string();
    let commit_id = Uuid::now_v7().to_string();

    let call_id = new_block_id();
    let mut out = ToolOutput::new(serde_json::json!("preview"));
    out.artifact = Some(ArtifactRef::new(
        format!("artifact:{art_uuid}"),
        10,
        ArtifactKind::FullOutput,
        true,
    ));
    let spilled_block = ContextBlock::new(
        new_block_id(),
        BlockContent::ToolResult(ToolResultPayload::new(
            call_id,
            ToolResultStatus::Succeeded,
            out,
        )),
        BlockMeta::default(),
    );
    let call_block = ContextBlock::new(
        call_id,
        BlockContent::ToolCall(ToolCallPayload::new("read_file", serde_json::json!({}))),
        BlockMeta::default(),
    );
    let user_block = ContextBlock::new(
        new_block_id(),
        BlockContent::text(Role::User, "hi"),
        BlockMeta::default(),
    );
    let summary_block = ContextBlock::new(
        new_block_id(),
        BlockContent::text(Role::User, "after"),
        BlockMeta::from_source(crate::compact::SUMMARY_SOURCE),
    );
    let history_doc = HistoryRecord {
        format: HISTORY_FORMAT.to_owned(),
        conversation_id: conv_id.clone(),
        record_id: hist_id.clone(),
        created_at_ms: 1_000,
        trigger: HistoryTrigger::Auto,
        kind: RewriteKind::Summarize,
        estimate_before: 1_000,
        estimate_after: 100,
        replacements: vec![RangeReplacement {
            range: 0..1,
            originals: vec![user_block],
            inserted_ids: vec![summary_block.id()],
        }],
    };
    let mut def = definition();
    def.permission_mode = Some(crate::policy::PermissionMode::Ask);
    let record = SavedSessionRecord::new_with_compaction(
        conv_id.clone(),
        commit_id,
        Some(1),
        def,
        Context::from_blocks(vec![call_block, spilled_block, summary_block]).unwrap(),
        Some(crate::policy::PermissionMode::Ask),
        None,
        None,
        vec![history_doc.index_entry()],
        None,
        None,
        2_000,
    )
    .unwrap();

    let record_bytes = serde_json::to_vec(&record).unwrap();
    let hist_bytes = serde_json::to_vec(&history_doc).unwrap();
    let secret_art_bytes = b"HOST SECRET SHOULD NOT BE INGESTED";

    // External host directory outside the legacy folder.
    let outside = tempfile::tempdir().unwrap();
    let outside_hist_dir = outside.path().join("host-hist");
    let outside_art_dir = outside.path().join("host-art");
    fs::create_dir_all(&outside_hist_dir).unwrap();
    fs::create_dir_all(&outside_art_dir).unwrap();
    let outside_hist_file = outside_hist_dir.join(format!("{hist_id}.json"));
    let outside_art_file = outside_art_dir.join(&art_uuid);
    fs::write(&outside_hist_file, &hist_bytes).unwrap();
    fs::write(&outside_art_file, secret_art_bytes).unwrap();

    // Case 1: symlinked <conv_id>.history directory
    {
        let case = tempfile::tempdir().unwrap();
        let legacy_dir = case.path().join("legacy");
        fs::create_dir_all(&legacy_dir).unwrap();
        let record_path = legacy_dir.join(format!("{conv_id}.json"));
        fs::write(&record_path, &record_bytes).unwrap();
        symlink(
            &outside_hist_dir,
            legacy_dir.join(format!("{conv_id}.history")),
        )
        .unwrap();
        let art_dir = legacy_dir.join(format!("{conv_id}.artifacts"));
        fs::create_dir_all(&art_dir).unwrap();
        fs::write(art_dir.join(&art_uuid), b"ok").unwrap();

        let store = SessionStore::new(case.path().join("store"));
        let err = store.import_legacy_v2_4(&record_path).unwrap_err();
        assert!(matches!(err, StoreError::InvalidHistory(_)), "{err:?}");
        assert!(!store.db_path().exists());
    }

    // Case 2: symlinked <conv_id>.artifacts directory
    {
        let case = tempfile::tempdir().unwrap();
        let legacy_dir = case.path().join("legacy");
        fs::create_dir_all(&legacy_dir).unwrap();
        let record_path = legacy_dir.join(format!("{conv_id}.json"));
        fs::write(&record_path, &record_bytes).unwrap();
        let hist_dir = legacy_dir.join(format!("{conv_id}.history"));
        fs::create_dir_all(&hist_dir).unwrap();
        fs::write(hist_dir.join(format!("{hist_id}.json")), &hist_bytes).unwrap();
        symlink(
            &outside_art_dir,
            legacy_dir.join(format!("{conv_id}.artifacts")),
        )
        .unwrap();

        let store = SessionStore::new(case.path().join("store"));
        let err = store.import_legacy_v2_4(&record_path).unwrap_err();
        assert!(matches!(err, StoreError::InvalidHistory(_)), "{err:?}");
        assert!(!store.db_path().exists());
    }

    // Case 3: symlinked child history file
    {
        let case = tempfile::tempdir().unwrap();
        let legacy_dir = case.path().join("legacy");
        fs::create_dir_all(&legacy_dir).unwrap();
        let record_path = legacy_dir.join(format!("{conv_id}.json"));
        fs::write(&record_path, &record_bytes).unwrap();
        let hist_dir = legacy_dir.join(format!("{conv_id}.history"));
        fs::create_dir_all(&hist_dir).unwrap();
        symlink(&outside_hist_file, hist_dir.join(format!("{hist_id}.json"))).unwrap();
        let art_dir = legacy_dir.join(format!("{conv_id}.artifacts"));
        fs::create_dir_all(&art_dir).unwrap();
        fs::write(art_dir.join(&art_uuid), b"ok").unwrap();

        let store = SessionStore::new(case.path().join("store"));
        let err = store.import_legacy_v2_4(&record_path).unwrap_err();
        assert!(matches!(err, StoreError::InvalidHistory(_)), "{err:?}");
        assert!(!store.db_path().exists());
    }

    // Case 4: symlinked child artifact file
    {
        let case = tempfile::tempdir().unwrap();
        let legacy_dir = case.path().join("legacy");
        fs::create_dir_all(&legacy_dir).unwrap();
        let record_path = legacy_dir.join(format!("{conv_id}.json"));
        fs::write(&record_path, &record_bytes).unwrap();
        let hist_dir = legacy_dir.join(format!("{conv_id}.history"));
        fs::create_dir_all(&hist_dir).unwrap();
        fs::write(hist_dir.join(format!("{hist_id}.json")), &hist_bytes).unwrap();
        let art_dir = legacy_dir.join(format!("{conv_id}.artifacts"));
        fs::create_dir_all(&art_dir).unwrap();
        symlink(&outside_art_file, art_dir.join(&art_uuid)).unwrap();

        let store = SessionStore::new(case.path().join("store"));
        let err = store.import_legacy_v2_4(&record_path).unwrap_err();
        assert!(matches!(err, StoreError::InvalidHistory(_)), "{err:?}");
        assert!(!store.db_path().exists());
    }
}

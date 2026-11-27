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
fn sqlite_schema_has_eight_tables_wal_pragmas_and_rejects_future_user_version() {
    let folder = tempfile::tempdir().unwrap();
    let store = SessionStore::new(folder.path().into());

    // Read-only operations on a fresh store must not create tars.db or locks/.
    assert!(store.list().unwrap().is_empty());
    let fresh_id = Uuid::now_v7().to_string();
    assert_eq!(store.read_saved(&fresh_id).unwrap(), None);
    assert!(!store.db_path().exists());
    assert!(!store.locks_dir().exists());

    // Acquiring a writer initializes tars.db with all 8 tables and WAL PRAGMAs.
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
    assert_eq!(user_version, 2);

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
            "trusted_workspaces",
            "workspace_trust",
        ]
    );

    // Future schema version (> 2) must fail closed on both read and write opens.
    drop(writer);
    conn.execute_batch("PRAGMA user_version = 99;").unwrap();
    drop(conn);

    assert!(matches!(
        store.list(),
        Err(StoreError::UnsupportedSchemaVersion {
            found: 99,
            supported: 2
        })
    ));
    assert!(matches!(
        store.read_saved(&fresh_id),
        Err(StoreError::UnsupportedSchemaVersion {
            found: 99,
            supported: 2
        })
    ));
    assert!(matches!(
        store.acquire_writer(&fresh_id),
        Err(StoreError::UnsupportedSchemaVersion {
            found: 99,
            supported: 2
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
    // Ensure database exists with schema v2, but no session/agent rows.
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

#[test]
fn wf01_validate_session_title_rules_and_limits() {
    assert_eq!(validate_session_title(None).unwrap(), None);
    assert_eq!(
        validate_session_title(Some("  Hello World  ")).unwrap(),
        Some("Hello World".to_owned())
    );
    let max_ascii = "a".repeat(MAX_SESSION_TITLE_CHARS);
    assert_eq!(
        validate_session_title(Some(&max_ascii)).unwrap(),
        Some(max_ascii)
    );
    let max_unicode = "é".repeat(MAX_SESSION_TITLE_CHARS);
    assert_eq!(
        validate_session_title(Some(&max_unicode)).unwrap(),
        Some(max_unicode)
    );

    for bad in [
        "",
        "   ",
        "\t\n\r",
        "line1\nline2",
        "line1\rline2",
        "col1\tcol2",
        "esc\x1b[31m",
        "nul\x00byte",
        "del\x7fchar",
        "nel\u{0085}char",
        &"a".repeat(MAX_SESSION_TITLE_CHARS + 1),
        &"é".repeat(MAX_SESSION_TITLE_CHARS + 1),
    ] {
        assert!(
            matches!(
                validate_session_title(Some(bad)),
                Err(StoreError::InvalidTitle(_))
            ),
            "expected InvalidTitle for {bad:?}"
        );
    }
}

#[test]
fn wf01_store_rename_session_set_clear_and_counterexamples() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("store"));
    let missing_id = Uuid::now_v7().to_string();

    // Missing store must return NotFound without creating tars.db.
    assert!(matches!(
        store.rename_session(&missing_id, Some("Title")),
        Err(StoreError::NotFound(_))
    ));
    assert!(!store.db_path().exists());

    // Seed a session.
    let id = Uuid::now_v7().to_string();
    let writer = store.acquire_writer(&id).unwrap();
    let initial = op(&id, None, "initial");
    writer.commit(&initial).unwrap();
    drop(writer);

    // Nonexistent session in existing store returns NotFound without creating stray lock.
    assert!(matches!(
        store.rename_session(&missing_id, Some("Title")),
        Err(StoreError::NotFound(_))
    ));
    assert!(
        !store
            .locks_dir()
            .join(format!("{missing_id}.lock"))
            .exists()
    );

    // Invalid UUID and invalid titles fail without mutating DB.
    assert!(matches!(
        store.rename_session("not-a-uuid", Some("Title")),
        Err(StoreError::InvalidIdentity(_))
    ));
    for bad in ["   ", "bad\ntitle", "bad\x1btitle", &"x".repeat(161)] {
        assert!(matches!(
            store.rename_session(&id, Some(bad)),
            Err(StoreError::InvalidTitle(_))
        ));
    }
    let unchanged = store.read_saved(&id).unwrap().unwrap();
    assert_eq!(unchanged.revision, 1);
    assert_eq!(unchanged.title, None);

    // Valid rename sets title, bumps revision and updated_at_ms.
    let renamed = store.rename_session(&id, Some("  My Session  ")).unwrap();
    assert_eq!(renamed.title.as_deref(), Some("My Session"));
    assert_eq!(renamed.revision, 2);
    assert!(renamed.updated_at_ms >= unchanged.updated_at_ms);
    assert_eq!(store.read_saved(&id).unwrap().unwrap(), renamed);

    // Clear title sets None and bumps revision again.
    let cleared = store.rename_session(&id, None).unwrap();
    assert_eq!(cleared.title, None);
    assert_eq!(cleared.revision, 3);
    assert_eq!(store.read_saved(&id).unwrap().unwrap(), cleared);

    // Occupied lock fails closed without modifying DB.
    let held_writer = store.acquire_writer(&id).unwrap();
    assert!(matches!(
        store.rename_session(&id, Some("Blocked")),
        Err(StoreError::Occupied(_))
    ));
    drop(held_writer);
    let after_occupied = store.read_saved(&id).unwrap().unwrap();
    assert_eq!(after_occupied.revision, 3);
    assert_eq!(after_occupied.title, None);
}

#[test]
fn wf02_store_delete_session_cascades_nested_ops_and_preserves_workspace_trust() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("store"));
    let workspace = dir.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let missing_id = Uuid::now_v7().to_string();

    // Missing store returns NotFound without creating tars.db.
    assert!(matches!(
        store.delete_session(&missing_id),
        Err(StoreError::NotFound(_))
    ));
    assert!(!store.db_path().exists());

    // Create target session S1 with nested depth >= 2 operations, child agents,
    // inbox events, history_records, artifacts, and an unrelated session S2 + workspace_trust.
    let s1 = Uuid::now_v7().to_string();
    let s2 = Uuid::now_v7().to_string();
    let writer1 = store.acquire_writer(&s1).unwrap();
    writer1.commit(&op(&s1, None, "s1")).unwrap();

    // Seed depth 1 and depth 2 child operations + child agents.
    let child1_id = Uuid::now_v7().to_string();
    let child2_id = Uuid::now_v7().to_string();
    let op1_id = Uuid::now_v7().to_string();
    let op2_id = Uuid::now_v7().to_string();
    let def = definition();
    let new_agent = |aid: &str, name: &str| super::NewAgentRow {
        agent_id: aid.to_owned(),
        name: name.to_owned(),
        preset_name: Some("explore".into()),
        interaction: crate::events::AgentInteraction::Reusable,
        definition_json: serde_json::to_string(&def).unwrap(),
        context_json: serde_json::to_string(&Context::new()).unwrap(),
        history_index_json: "[]".into(),
        usage_json: None,
        created_at_ms: 2_000,
        updated_at_ms: 2_000,
    };
    writer1
        .admit_operation(&super::AdmitOperationRequest {
            commit_id: Uuid::now_v7().to_string(),
            operation_id: op1_id.clone(),
            target_agent_id: child1_id.clone(),
            parent_operation_id: None,
            parent_agent_id: Some(s1.clone()),
            source_block_id: Some(Uuid::now_v7().to_string()),
            dedup_fingerprint: Some("fp-1".into()),
            execution_mode: crate::events::OperationExecutionMode::Background,
            on_parent_complete: crate::events::OnParentComplete::Continue,
            depth: 1,
            input_json: "{\"p\":1}".into(),
            created_at_ms: 2_000,
            new_agent: Some(new_agent(&child1_id, "c1")),
        })
        .unwrap();
    writer1
        .admit_operation(&super::AdmitOperationRequest {
            commit_id: Uuid::now_v7().to_string(),
            operation_id: op2_id.clone(),
            target_agent_id: child2_id.clone(),
            parent_operation_id: Some(op1_id.clone()),
            parent_agent_id: Some(child1_id.clone()),
            source_block_id: Some(Uuid::now_v7().to_string()),
            dedup_fingerprint: Some("fp-2".into()),
            execution_mode: crate::events::OperationExecutionMode::Sync,
            on_parent_complete: crate::events::OnParentComplete::Cancel,
            depth: 2,
            input_json: "{\"p\":2}".into(),
            created_at_ms: 3_000,
            new_agent: Some(new_agent(&child2_id, "c2")),
        })
        .unwrap();

    // Seed inbox event, history record, and artifact on s1.
    let conn = schema::open_read_write(store.db_path()).unwrap();
    conn.execute(
        "INSERT INTO agent_events (
            session_id, target_agent_id, priority, source_kind, source_id,
            coalesce_key, wake_policy, payload_json, state, claimed_op_id, created_at_ms
         ) VALUES (?1, ?2, 'normal', 'timer', 't1', NULL, 'wake_if_idle', '{}', 'pending', NULL, 4000)",
        params![&s1, &s1],
    )
    .unwrap();
    drop(conn);
    let hist_store = crate::compact::history::HistoryStore::new_strict_for_agent(
        store.root(),
        &s1,
        &s1,
        Vec::new(),
    );
    let art_id = hist_store.persist_spilled_artifact(b"spilled s1").unwrap();
    let rec_id = Uuid::now_v7().to_string();
    let orig_block = ContextBlock::new(
        new_block_id(),
        BlockContent::text(causa::kernel::Role::User, "orig"),
        BlockMeta::default(),
    );
    hist_store
        .write(&crate::compact::HistoryRecord {
            format: crate::compact::history::HISTORY_FORMAT.into(),
            conversation_id: s1.clone(),
            record_id: rec_id.clone(),
            created_at_ms: 2_000,
            trigger: crate::compact::HistoryTrigger::Manual,
            kind: crate::compact::RewriteKind::Summarize,
            estimate_before: 100,
            estimate_after: 20,
            replacements: vec![crate::compact::RangeReplacement {
                range: 0..1,
                originals: vec![orig_block],
                inserted_ids: vec![new_block_id()],
            }],
        })
        .unwrap();

    // Seed workspace_trust and unrelated session S2.
    store
        .write_workspace_rules_trust(&workspace, "version = 1\n")
        .unwrap();
    store
        .write_workspace_agents_trust(&workspace, "explore.md=deadbeef\n")
        .unwrap();
    let writer2 = store.acquire_writer(&s2).unwrap();
    writer2.commit(&op(&s2, None, "s2")).unwrap();
    drop(writer2);

    // Occupied lock must fail closed.
    assert!(matches!(
        store.delete_session(&s1),
        Err(StoreError::Occupied(_))
    ));
    assert!(store.read_saved(&s1).unwrap().is_some());
    drop(writer1);

    // Invalid UUID and missing ID fail cleanly.
    assert!(matches!(
        store.delete_session("bad-uuid"),
        Err(StoreError::InvalidIdentity(_))
    ));
    assert!(matches!(
        store.delete_session(&missing_id),
        Err(StoreError::NotFound(_))
    ));

    // Delete S1: cascades all owned rows including depth-2 operation, removes lock, keeps S2 & trust.
    store.delete_session(&s1).unwrap();
    assert_eq!(store.read_saved(&s1).unwrap(), None);
    assert!(!store.locks_dir().join(format!("{s1}.lock")).exists());

    let conn = schema::open_read_only(store.db_path()).unwrap().unwrap();
    for table in [
        "sessions",
        "agents",
        "operations",
        "agent_events",
        "history_records",
        "artifacts",
    ] {
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE session_id = ?1"),
                params![&s1],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "expected 0 rows in {table} for deleted session");
    }
    assert!(store.read_saved(&s2).unwrap().is_some());
    assert_eq!(
        store
            .read_workspace_rules_trust(&workspace)
            .unwrap()
            .as_deref(),
        Some("version = 1\n")
    );
    assert_eq!(
        store
            .read_workspace_agents_trust(&workspace)
            .unwrap()
            .as_deref(),
        Some("explore.md=deadbeef\n")
    );
    let _ = art_id;
}

#[test]
fn wf03_store_fork_session_isolates_history_artifacts_and_downgrades_permissions() {
    use crate::policy::PermissionMode;
    use causa::kernel::{
        ArtifactKind, ArtifactRef, Role, ToolCallPayload, ToolOutput, ToolResultPayload,
        ToolResultStatus,
    };

    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("store"));
    let s1 = Uuid::now_v7().to_string();
    let s2 = Uuid::now_v7().to_string();

    // Missing store -> NotFound without creating tars.db.
    assert!(matches!(
        store.fork_session(&s1, &s2, None),
        Err(StoreError::NotFound(_))
    ));
    assert!(!store.db_path().exists());

    let writer1 = store.acquire_writer(&s1).unwrap();
    let initial = op(&s1, None, "seed");
    writer1.commit(&initial).unwrap();

    // Persist a spilled artifact and a history record referencing an original block.
    let hist_store = crate::compact::history::HistoryStore::new_strict_for_agent(
        store.root(),
        &s1,
        &s1,
        Vec::new(),
    );
    let spilled_id = hist_store
        .persist_spilled_artifact(b"spilled artifact body")
        .unwrap();

    let orig_call_id = new_block_id();
    let orig_call_block = ContextBlock::new(
        orig_call_id,
        BlockContent::ToolCall(ToolCallPayload::new(
            "read_file",
            serde_json::json!({"path":"a.txt"}),
        )),
        BlockMeta::default(),
    );
    let orig_res_id = new_block_id();
    let orig_res_block = ContextBlock::new(
        orig_res_id,
        BlockContent::ToolResult(ToolResultPayload::new(
            orig_call_id,
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::json!({"ok":true})),
        )),
        BlockMeta::default(),
    );
    let summary_block = ContextBlock::new(
        new_block_id(),
        BlockContent::text(Role::User, "summary"),
        BlockMeta::from_source(crate::compact::SUMMARY_SOURCE),
    );
    let rec_id = Uuid::now_v7().to_string();
    let history_doc = crate::compact::HistoryRecord {
        format: crate::compact::history::HISTORY_FORMAT.into(),
        conversation_id: s1.clone(),
        record_id: rec_id.clone(),
        created_at_ms: 2_000,
        trigger: crate::compact::HistoryTrigger::Manual,
        kind: crate::compact::RewriteKind::Summarize,
        estimate_before: 500,
        estimate_after: 100,
        replacements: vec![crate::compact::RangeReplacement {
            range: 0..2,
            originals: vec![orig_call_block, orig_res_block],
            inserted_ids: vec![summary_block.id()],
        }],
    };
    hist_store.write(&history_doc).unwrap();
    let hist_entry = history_doc.index_entry();

    // Context in S1 references both `<rec_id>/<orig_res_id>` and `artifact:<spilled_id>`.
    let ctx_call_id = new_block_id();
    let ctx_call2_id = new_block_id();
    let mut spilled_out = ToolOutput::new(serde_json::json!({"truncated":true}));
    spilled_out.artifact = Some(ArtifactRef::new(
        format!("artifact:{spilled_id}"),
        21,
        ArtifactKind::FullOutput,
        true,
    ));
    let mut rec_out = ToolOutput::new(serde_json::json!({"ref":true}));
    rec_out.artifact = Some(ArtifactRef::new(
        format!("{rec_id}/{}", orig_res_id.0),
        10,
        ArtifactKind::FullOutput,
        true,
    ));
    let s1_context = Context::from_blocks(vec![
        summary_block,
        ContextBlock::new(
            ctx_call_id,
            BlockContent::ToolCall(ToolCallPayload::new(
                "read_file",
                serde_json::json!({"path":"b.txt"}),
            )),
            BlockMeta::default(),
        ),
        ContextBlock::new(
            new_block_id(),
            BlockContent::ToolResult(ToolResultPayload::new(
                ctx_call_id,
                ToolResultStatus::Succeeded,
                spilled_out,
            )),
            BlockMeta::default(),
        ),
        ContextBlock::new(
            ctx_call2_id,
            BlockContent::ToolCall(ToolCallPayload::new("recall", serde_json::json!({}))),
            BlockMeta::default(),
        ),
        ContextBlock::new(
            new_block_id(),
            BlockContent::ToolResult(ToolResultPayload::new(
                ctx_call2_id,
                ToolResultStatus::Succeeded,
                rec_out,
            )),
            BlockMeta::default(),
        ),
    ])
    .unwrap();

    let mut def = definition();
    def.permission_mode = Some(PermissionMode::Ask);
    let mut usage = crate::usage::SessionUsage::default();
    usage.conversation.calls.succeeded = 1;
    usage.conversation.tokens.input_tokens = 10;
    usage.conversation.tokens.output_tokens = 5;
    usage.input_rounds = 1;
    let long_title = "T".repeat(MAX_SESSION_TITLE_CHARS);
    let rev2 = PreparedCommit::new(
        Some(1),
        SavedSessionRecord::new_with_compaction(
            s1.clone(),
            Uuid::now_v7().to_string(),
            Some(1),
            def,
            s1_context,
            Some(PermissionMode::FullAccess),
            Some(false),
            Some(55),
            vec![hist_entry],
            Some(usage),
            Some(long_title),
            3_000,
        )
        .unwrap(),
    )
    .unwrap();
    writer1.commit(&rev2).unwrap();

    // Seed a child agent, operation, and agent_event on S1 that must NOT be cloned.
    let child_id = Uuid::now_v7().to_string();
    let op_id = Uuid::now_v7().to_string();
    writer1
        .admit_operation(&super::AdmitOperationRequest {
            commit_id: Uuid::now_v7().to_string(),
            operation_id: op_id,
            target_agent_id: child_id.clone(),
            parent_operation_id: None,
            parent_agent_id: Some(s1.clone()),
            source_block_id: Some(Uuid::now_v7().to_string()),
            dedup_fingerprint: Some("fp".into()),
            execution_mode: crate::events::OperationExecutionMode::Background,
            on_parent_complete: crate::events::OnParentComplete::Continue,
            depth: 1,
            input_json: "{}".into(),
            created_at_ms: 4_000,
            new_agent: Some(super::NewAgentRow {
                agent_id: child_id,
                name: "child".into(),
                preset_name: None,
                interaction: crate::events::AgentInteraction::Reusable,
                definition_json: serde_json::to_string(&definition()).unwrap(),
                context_json: serde_json::to_string(&Context::new()).unwrap(),
                history_index_json: "[]".into(),
                usage_json: None,
                created_at_ms: 4_000,
                updated_at_ms: 4_000,
            }),
        })
        .unwrap();

    // Rejects when source is locked, self-fork, bad UUID, or bad title.
    assert!(matches!(
        store.fork_session(&s1, &s2, None),
        Err(StoreError::Occupied(_))
    ));
    drop(writer1);
    assert!(matches!(
        store.fork_session(&s1, &s1, None),
        Err(StoreError::InvalidIdentity(_))
    ));
    assert!(matches!(
        store.fork_session("bad", &s2, None),
        Err(StoreError::InvalidIdentity(_))
    ));
    assert!(matches!(
        store.fork_session(&s1, &s2, Some("bad\ntitle")),
        Err(StoreError::InvalidTitle(_))
    ));

    // Fork S1 -> S2 with derived title (truncated safely to 160 chars).
    let forked = store.fork_session(&s1, &s2, None).unwrap();
    assert_eq!(forked.conversation_id, s2);
    assert_eq!(forked.revision, 1);
    assert_eq!(forked.usage, None);
    assert_eq!(forked.permission_mode, Some(PermissionMode::Ask));
    assert_eq!(forked.auto_compact, Some(false));
    assert_eq!(forked.compact_threshold, Some(55));
    let forked_title = forked.title.as_deref().unwrap();
    assert!(forked_title.ends_with(" (fork)"));
    assert_eq!(forked_title.chars().count(), MAX_SESSION_TITLE_CHARS);

    // Target already exists -> AlreadyExists.
    assert!(matches!(
        store.fork_session(&s1, &s2, Some("Duplicate")),
        Err(StoreError::AlreadyExists(_))
    ));

    // Verify no child agents, operations, or agent_events were cloned to S2.
    assert_eq!(store.list_agents(&s2).unwrap().len(), 1);
    assert_eq!(store.list_operations(&s2).unwrap().len(), 0);
    assert_eq!(store.list_pending_inbox_events(&s2, &s2).unwrap().len(), 0);

    // Delete S1 completely, then verify S2's HistoryStore still validates and reads both history & artifacts!
    store.delete_session(&s1).unwrap();
    let s2_hist =
        crate::compact::history::HistoryStore::open_read_only_for_agent(store.root(), &s2, &s2);
    let read_doc = s2_hist.read(&rec_id).unwrap();
    assert_eq!(read_doc.conversation_id, s2);
    let s2_hist_indexed = crate::compact::history::HistoryStore::new_strict_for_agent(
        store.root(),
        &s2,
        &s2,
        forked.history.clone(),
    );
    let orig_block = s2_hist_indexed
        .read_original_block(&rec_id, orig_res_id)
        .unwrap();
    assert_eq!(orig_block.id(), orig_res_id);
    let art_bytes = s2_hist_indexed
        .read_spilled_artifact(&spilled_id, None)
        .unwrap();
    assert_eq!(art_bytes, b"spilled artifact body");

    // Verify a session created with initial FullAccess keeps FullAccess on fork.
    let s3 = Uuid::now_v7().to_string();
    let s4 = Uuid::now_v7().to_string();
    let w3 = store.acquire_writer(&s3).unwrap();
    let mut full_def = definition();
    full_def.permission_mode = Some(PermissionMode::FullAccess);
    w3.commit(
        &PreparedCommit::new(
            None,
            SavedSessionRecord::new(
                s3.clone(),
                Uuid::now_v7().to_string(),
                None,
                full_def,
                context("full"),
                Some(PermissionMode::FullAccess),
                None,
                1_000,
            )
            .unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    drop(w3);
    let forked_full = store.fork_session(&s3, &s4, Some("Explicit")).unwrap();
    assert_eq!(forked_full.title.as_deref(), Some("Explicit"));
    assert_eq!(forked_full.permission_mode, None);
    assert_eq!(
        forked_full.definition.initial_permission_mode(),
        PermissionMode::FullAccess
    );
}

fn sha256_hex_for_test(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[test]
fn schema_v1_migrates_in_place_to_v2_preserving_sessions_and_workspace_trust() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("store"));
    let workspace = dir.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let canon_ws = fs::canonicalize(&workspace).unwrap();

    // Seed a schema v1 database without trusted_workspaces.
    fs::create_dir_all(store.root()).unwrap();
    let session_id = Uuid::now_v7().to_string();
    let record = op(&session_id, None, "legacy v1 session").record;
    {
        let conn = rusqlite::Connection::open(store.db_path()).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                session_id        TEXT PRIMARY KEY,
                revision          INTEGER NOT NULL CHECK (revision >= 0),
                last_commit_id    TEXT NOT NULL,
                policy_json       TEXT NOT NULL,
                entry_agent_id    TEXT NOT NULL,
                title             TEXT,
                permission_mode   TEXT,
                auto_compact      INTEGER,
                compact_threshold INTEGER,
                usage_json        TEXT,
                created_at_ms     INTEGER NOT NULL,
                updated_at_ms     INTEGER NOT NULL
            );
            CREATE TABLE agents (
                session_id         TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                agent_id           TEXT NOT NULL,
                name               TEXT NOT NULL,
                preset_name        TEXT,
                interaction        TEXT NOT NULL CHECK (interaction IN ('single_work', 'reusable')),
                definition_json    TEXT NOT NULL,
                material_revision  INTEGER NOT NULL CHECK (material_revision >= 0),
                context_json       TEXT NOT NULL,
                history_index_json TEXT NOT NULL DEFAULT '[]',
                usage_json         TEXT,
                created_at_ms      INTEGER NOT NULL,
                updated_at_ms      INTEGER NOT NULL,
                PRIMARY KEY (session_id, agent_id)
            );
            CREATE TABLE operations (
                operation_id        TEXT PRIMARY KEY,
                session_id          TEXT NOT NULL,
                target_agent_id     TEXT NOT NULL,
                parent_operation_id TEXT REFERENCES operations(operation_id),
                parent_agent_id     TEXT,
                source_block_id     TEXT,
                dedup_fingerprint   TEXT,
                execution_mode      TEXT NOT NULL CHECK (execution_mode IN ('sync', 'background')),
                on_parent_complete  TEXT NOT NULL CHECK (on_parent_complete IN ('continue', 'cancel')),
                depth               INTEGER NOT NULL CHECK (depth >= 0),
                status              TEXT NOT NULL CHECK (status IN (
                                        'creating', 'running', 'completed', 'cancelled',
                                        'failed', 'deadline', 'faulted', 'interrupted'
                                    )),
                input_json          TEXT NOT NULL,
                outcome_json        TEXT,
                result_text         TEXT,
                result_bytes        INTEGER,
                created_at_ms       INTEGER NOT NULL,
                finished_at_ms      INTEGER,
                FOREIGN KEY (session_id, target_agent_id) REFERENCES agents(session_id, agent_id) ON DELETE CASCADE
            );
            CREATE TABLE agent_events (
                event_id        INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id      TEXT NOT NULL,
                target_agent_id TEXT NOT NULL,
                priority        TEXT NOT NULL CHECK (priority IN ('urgent', 'normal', 'low')),
                source_kind     TEXT NOT NULL CHECK (source_kind IN (
                                    'agent_operation', 'background_command', 'timer', 'user_steer'
                                )),
                source_id       TEXT NOT NULL,
                coalesce_key    TEXT,
                wake_policy     TEXT NOT NULL CHECK (wake_policy IN (
                                    'wake_if_idle', 'deliver_on_next_turn', 'suppress_wake'
                                )),
                payload_json    TEXT NOT NULL,
                state           TEXT NOT NULL CHECK (state IN ('pending', 'claimed', 'delivered', 'dropped')),
                claimed_op_id   TEXT,
                created_at_ms   INTEGER NOT NULL,
                delivered_at_ms INTEGER,
                FOREIGN KEY (session_id, target_agent_id) REFERENCES agents(session_id, agent_id) ON DELETE CASCADE
            );
            CREATE TABLE history_records (
                session_id    TEXT NOT NULL,
                agent_id      TEXT NOT NULL,
                record_id     TEXT NOT NULL,
                seq           INTEGER NOT NULL,
                kind          TEXT NOT NULL,
                summary_json  TEXT NOT NULL,
                document_json TEXT NOT NULL,
                PRIMARY KEY (session_id, agent_id, record_id),
                FOREIGN KEY (session_id, agent_id) REFERENCES agents(session_id, agent_id) ON DELETE CASCADE
            );
            CREATE TABLE artifacts (
                session_id  TEXT NOT NULL,
                agent_id    TEXT NOT NULL,
                artifact_id TEXT NOT NULL,
                content     BLOB NOT NULL,
                PRIMARY KEY (session_id, agent_id, artifact_id),
                FOREIGN KEY (session_id, agent_id) REFERENCES agents(session_id, agent_id) ON DELETE CASCADE
            );
            CREATE TABLE workspace_trust (
                workspace_hash TEXT NOT NULL,
                kind           TEXT NOT NULL CHECK (kind IN ('permissions_toml', 'workspace_agents')),
                workspace_root TEXT NOT NULL,
                content_sha256 TEXT NOT NULL,
                content        TEXT NOT NULL,
                trusted_at_ms  INTEGER NOT NULL,
                PRIMARY KEY (workspace_hash, kind)
            );
            PRAGMA user_version = 1;",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_id, revision, last_commit_id, policy_json, entry_agent_id,
                title, permission_mode, auto_compact, compact_threshold, usage_json,
                created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, 1, 70, NULL, ?6, ?7)",
            params![
                &session_id,
                1_i64,
                &record.last_commit_id,
                r#"{"format":"tars-session-v2.4"}"#,
                &session_id,
                1_000_i64,
                1_000_i64,
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agents (
                session_id, agent_id, name, preset_name, interaction,
                definition_json, material_revision, context_json, history_index_json,
                usage_json, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, 'main', NULL, 'reusable', ?3, 1, ?4, '[]', NULL, ?5, ?6)",
            params![
                &session_id,
                &session_id,
                serde_json::to_string(&record.definition).unwrap(),
                serde_json::to_string(&record.context).unwrap(),
                1_000_i64,
                1_000_i64,
            ],
        )
        .unwrap();
        let rules = "version = 1\n";
        conn.execute(
            "INSERT INTO workspace_trust (
                workspace_hash, kind, workspace_root, content_sha256, content, trusted_at_ms
             ) VALUES (?1, 'permissions_toml', ?2, ?3, ?4, 1000)",
            params![
                sha256_hex_for_test(canon_ws.as_os_str().as_encoded_bytes()),
                canon_ws.to_str().unwrap(),
                sha256_hex_for_test(rules.as_bytes()),
                rules,
            ],
        )
        .unwrap();
    }

    // Read-only operations accept v1 without upgrading or failing on missing trusted_workspaces.
    assert!(!store.is_workspace_folder_trusted(&workspace).unwrap());
    assert!(store.list_trusted_workspace_folders().unwrap().is_empty());
    assert!(!store.revoke_workspace_folder_trust(&workspace).unwrap());
    {
        let ro_conn = schema::open_read_only(store.db_path()).unwrap().unwrap();
        assert_eq!(schema::read_user_version(&ro_conn).unwrap(), 1);
        let has_table: i64 = ro_conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'trusted_workspaces'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_table, 0);
    }

    // First read-write open migrates v1 -> v2 idempotently while preserving existing data.
    let conn = schema::open_read_write(store.db_path()).unwrap();
    assert_eq!(schema::read_user_version(&conn).unwrap(), 2);
    drop(conn);
    let conn2 = schema::open_read_write(store.db_path()).unwrap();
    assert_eq!(schema::read_user_version(&conn2).unwrap(), 2);
    drop(conn2);

    assert!(store.read_saved(&session_id).unwrap().is_some());
    assert_eq!(
        store
            .read_workspace_rules_trust(&workspace)
            .unwrap()
            .as_deref(),
        Some("version = 1\n")
    );

    // Folder trust now works on the migrated v2 store.
    store.trust_workspace_folder(&workspace).unwrap();
    assert!(store.is_workspace_folder_trusted(&workspace).unwrap());
}

#[test]
fn folder_trust_missing_db_and_version_bounds_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("store"));
    let workspace = dir.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();

    // Missing tars.db: read/list/revoke must never create tars.db.
    assert!(!store.is_workspace_folder_trusted(&workspace).unwrap());
    assert!(store.list_trusted_workspace_folders().unwrap().is_empty());
    assert!(!store.revoke_workspace_folder_trust(&workspace).unwrap());
    assert!(!store.db_path().exists());

    // Initialize v2 DB, then bump user_version to 3 (> supported 2).
    store.trust_workspace_folder(&workspace).unwrap();
    let conn = schema::open_read_write(store.db_path()).unwrap();
    conn.execute_batch("PRAGMA user_version = 3;").unwrap();
    drop(conn);

    assert!(matches!(
        schema::open_read_only(store.db_path()),
        Err(StoreError::UnsupportedSchemaVersion {
            found: 3,
            supported: 2
        })
    ));
    assert!(matches!(
        schema::open_read_write(store.db_path()),
        Err(StoreError::UnsupportedSchemaVersion {
            found: 3,
            supported: 2
        })
    ));
    assert!(matches!(
        store.is_workspace_folder_trusted(&workspace),
        Err(StoreError::UnsupportedSchemaVersion {
            found: 3,
            supported: 2
        })
    ));
    assert!(matches!(
        store.trust_workspace_folder(&workspace),
        Err(StoreError::UnsupportedSchemaVersion {
            found: 3,
            supported: 2
        })
    ));
    assert!(matches!(
        store.list_trusted_workspace_folders(),
        Err(StoreError::UnsupportedSchemaVersion {
            found: 3,
            supported: 2
        })
    ));
    assert!(matches!(
        store.revoke_workspace_folder_trust(&workspace),
        Err(StoreError::UnsupportedSchemaVersion {
            found: 3,
            supported: 2
        })
    ));
}

#[test]
fn folder_trust_validation_tamper_resistance_and_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("store"));
    let ws_a = dir.path().join("ws-a");
    let ws_b = dir.path().join("ws-b");
    let missing_dir = dir.path().join("missing-dir");
    let file_path = dir.path().join("not-a-dir.txt");
    fs::create_dir_all(&ws_a).unwrap();
    fs::create_dir_all(&ws_b).unwrap();
    fs::write(&file_path, "hello").unwrap();

    // Non-existent dir returns Ok(false) without creating tars.db.
    assert!(!store.is_workspace_folder_trusted(&missing_dir).unwrap());
    assert!(!store.db_path().exists());

    // Non-directory trust request is rejected.
    let err = store.trust_workspace_folder(&file_path).unwrap_err();
    assert!(
        matches!(err, StoreError::Io(_)),
        "unexpected error: {err:?}"
    );

    // Trust ws_a: workspace_trust remains untouched and SessionStore::list() remains empty.
    store.trust_workspace_folder(&ws_a).unwrap();
    assert!(store.is_workspace_folder_trusted(&ws_a).unwrap());
    assert!(!store.is_workspace_folder_trusted(&ws_b).unwrap());
    assert_eq!(store.read_workspace_rules_trust(&ws_a).unwrap(), None);
    assert_eq!(store.read_workspace_agents_trust(&ws_a).unwrap(), None);
    assert!(store.list().unwrap().is_empty());

    // Tampered workspace_root in trusted_workspaces must fail closed.
    let canon_a = fs::canonicalize(&ws_a).unwrap();
    let canon_b = fs::canonicalize(&ws_b).unwrap();
    let hash_a = sha256_hex_for_test(canon_a.as_os_str().as_encoded_bytes());
    let conn = schema::open_read_write(store.db_path()).unwrap();
    conn.execute(
        "UPDATE trusted_workspaces SET workspace_root = ?1 WHERE workspace_hash = ?2",
        params![canon_b.to_str().unwrap(), hash_a],
    )
    .unwrap();
    drop(conn);
    assert!(!store.is_workspace_folder_trusted(&ws_a).unwrap());
}

#[cfg(unix)]
#[test]
fn folder_trust_canonicalizes_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("store"));
    let real_ws = dir.path().join("real-ws");
    let link_ws = dir.path().join("link-ws");
    fs::create_dir_all(&real_ws).unwrap();
    std::os::unix::fs::symlink(&real_ws, &link_ws).unwrap();

    store.trust_workspace_folder(&link_ws).unwrap();
    assert!(store.is_workspace_folder_trusted(&real_ws).unwrap());
    assert!(store.is_workspace_folder_trusted(&link_ws).unwrap());
    assert_eq!(store.list_trusted_workspace_folders().unwrap().len(), 1);
}

#[test]
fn folder_trust_list_ordering_and_delete_session_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("store"));
    let ws_alpha = dir.path().join("alpha");
    let ws_beta = dir.path().join("beta");
    let ws_gamma = dir.path().join("gamma");
    fs::create_dir_all(&ws_alpha).unwrap();
    fs::create_dir_all(&ws_beta).unwrap();
    fs::create_dir_all(&ws_gamma).unwrap();

    store.trust_workspace_folder(&ws_alpha).unwrap();
    store.trust_workspace_folder(&ws_beta).unwrap();
    store.trust_workspace_folder(&ws_gamma).unwrap();

    let canon_alpha = fs::canonicalize(&ws_alpha).unwrap();
    let canon_beta = fs::canonicalize(&ws_beta).unwrap();
    let canon_gamma = fs::canonicalize(&ws_gamma).unwrap();

    // Set explicit timestamps: gamma=2000 (newest), alpha=1000 & beta=1000 (tie broken ASC).
    let conn = schema::open_read_write(store.db_path()).unwrap();
    for (canon, ts) in [
        (&canon_alpha, 1000_i64),
        (&canon_beta, 1000_i64),
        (&canon_gamma, 2000_i64),
    ] {
        let hash = sha256_hex_for_test(canon.as_os_str().as_encoded_bytes());
        conn.execute(
            "UPDATE trusted_workspaces SET trusted_at_ms = ?1 WHERE workspace_hash = ?2",
            params![ts, hash],
        )
        .unwrap();
    }
    drop(conn);

    let listed = store.list_trusted_workspace_folders().unwrap();
    assert_eq!(
        listed,
        vec![
            TrustedWorkspaceFolder {
                workspace_root: canon_gamma.clone(),
                trusted_at_ms: 2000,
            },
            TrustedWorkspaceFolder {
                workspace_root: canon_alpha.clone(),
                trusted_at_ms: 1000,
            },
            TrustedWorkspaceFolder {
                workspace_root: canon_beta.clone(),
                trusted_at_ms: 1000,
            },
        ]
    );

    // Session deletion must preserve trusted_workspaces and workspace_trust.
    let sid = Uuid::now_v7().to_string();
    let writer = store.acquire_writer(&sid).unwrap();
    writer.commit(&op(&sid, None, "session")).unwrap();
    drop(writer);
    store
        .write_workspace_rules_trust(&ws_alpha, "version = 1\n")
        .unwrap();

    store.delete_session(&sid).unwrap();
    assert!(store.is_workspace_folder_trusted(&ws_alpha).unwrap());
    assert!(store.is_workspace_folder_trusted(&ws_beta).unwrap());
    assert!(store.is_workspace_folder_trusted(&ws_gamma).unwrap());
    assert_eq!(
        store
            .read_workspace_rules_trust(&ws_alpha)
            .unwrap()
            .as_deref(),
        Some("version = 1\n")
    );
}

#[test]
fn revoke_workspace_folder_trust_handles_existing_deleted_and_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("store"));
    let ws = dir.path().join("ws");
    let deleted_ws = dir.path().join("nested").join("deleted-ws");
    fs::create_dir_all(&ws).unwrap();
    fs::create_dir_all(&deleted_ws).unwrap();

    // Seed session + workspace_trust + folder trust.
    let sid = Uuid::now_v7().to_string();
    let writer = store.acquire_writer(&sid).unwrap();
    writer.commit(&op(&sid, None, "keep me")).unwrap();
    drop(writer);
    store
        .write_workspace_rules_trust(&ws, "version = 1\n")
        .unwrap();
    store.trust_workspace_folder(&ws).unwrap();
    store.trust_workspace_folder(&deleted_ws).unwrap();

    // Revoke existing folder.
    assert!(store.revoke_workspace_folder_trust(&ws).unwrap());
    assert!(!store.is_workspace_folder_trusted(&ws).unwrap());
    assert!(!store.revoke_workspace_folder_trust(&ws).unwrap());

    // Delete directory, then revoke via path containing `.`/`..` and missing leaf.
    fs::remove_dir_all(&deleted_ws).unwrap();
    let parent_dotdot_path = dir
        .path()
        .join("nested")
        .join("nonexistent-sub")
        .join("..")
        .join("deleted-ws");
    assert!(
        store
            .revoke_workspace_folder_trust(&parent_dotdot_path)
            .unwrap()
    );
    assert!(store.list_trusted_workspace_folders().unwrap().is_empty());

    // Verify sessions and workspace_trust were not touched.
    assert!(store.read_saved(&sid).unwrap().is_some());
    assert_eq!(
        store.read_workspace_rules_trust(&ws).unwrap().as_deref(),
        Some("version = 1\n")
    );
}

#[cfg(unix)]
#[test]
fn revoke_workspace_folder_trust_handles_deleted_dir_via_relative_and_symlinked_parent() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::new(dir.path().join("store"));
    let real_parent = dir.path().join("real-parent");
    let link_parent = dir.path().join("link-parent");
    fs::create_dir_all(&real_parent).unwrap();
    std::os::unix::fs::symlink(&real_parent, &link_parent).unwrap();

    // 1. Deleted directory revoked through a symlinked parent.
    let target_a = real_parent.join("target-a");
    fs::create_dir_all(&target_a).unwrap();
    store.trust_workspace_folder(&target_a).unwrap();
    fs::remove_dir_all(&target_a).unwrap();
    assert!(
        store
            .revoke_workspace_folder_trust(&link_parent.join("target-a"))
            .unwrap()
    );
    assert!(store.list_trusted_workspace_folders().unwrap().is_empty());

    // 2. Deleted directory revoked via a relative path without mutating process cwd.
    let target_b = real_parent.join("target-b");
    fs::create_dir_all(&target_b).unwrap();
    store.trust_workspace_folder(&target_b).unwrap();
    fs::remove_dir_all(&target_b).unwrap();

    let cwd = std::env::current_dir().unwrap();
    let abs_link_target = std::path::absolute(link_parent.join("target-b")).unwrap();
    let mut rel_path = std::path::PathBuf::new();
    for component in cwd.components() {
        if matches!(component, std::path::Component::Normal(_)) {
            rel_path.push("..");
        }
    }
    for component in abs_link_target.components() {
        if let std::path::Component::Normal(part) = component {
            rel_path.push(part);
        }
    }
    assert!(!rel_path.is_absolute());
    assert!(store.revoke_workspace_folder_trust(&rel_path).unwrap());
    assert!(store.list_trusted_workspace_folders().unwrap().is_empty());
}

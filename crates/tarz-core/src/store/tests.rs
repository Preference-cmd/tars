use super::json::MAX_RECORD_BYTES;
use super::*;
use crate::definition::{CredentialRef, Provider, SessionDefinition};
use causa::kernel::{BlockContent, BlockMeta, ContentPart, Context, ContextBlock, MediaRef};
use causa::runtime::new_block_id;
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
        credential: CredentialRef::Environment("TARZ_TEST_UNUSED".into()),
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
    let (folder, store, id, writer) = setup();
    let initial = op(&id, None, "old");
    writer.commit(&initial).unwrap();
    let update = op(&id, Some(1), "new");
    let path = folder.path().join("sessions").join(format!("{id}.json"));
    let held = folder.path().join("held.json");
    fs::rename(&path, &held).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(writer.commit(&update).is_err());
    fs::remove_dir(&path).unwrap();
    fs::rename(&held, &path).unwrap();
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
        let (folder, _store, id, writer) = setup();
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
        assert_eq!(
            fs::read_dir(folder.path().join("sessions"))
                .unwrap()
                .count(),
            2
        );
        writer.set_fault_hook(None);
        writer.commit(&update).unwrap();
    }
}
#[test]
fn advanced_or_changed_baseline_never_claims_not_applied() {
    let (folder, store, id, writer) = setup();
    let initial = op(&id, None, "old");
    writer.commit(&initial).unwrap();
    let update = op(&id, Some(1), "new");
    writer.set_fault_hook(Some(failure(CommitStage::Sync)));
    writer.commit(&update).unwrap_err();
    let mut changed = initial.record.clone();
    changed.context = context("other bytes same revision");
    fs::write(
        folder.path().join("sessions").join(format!("{id}.json")),
        serde_json::to_vec(&changed).unwrap(),
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
fn bad_records_fail_individually_and_list_ignores_lock_and_pending() {
    let (folder, store, id, writer) = setup();
    let initial = op(&id, None, "old");
    let path = folder.path().join("sessions").join(format!("{id}.json"));
    let value = serde_json::to_value(&initial.record).unwrap();
    for field in [
        "format",
        "conversation_id",
        "last_commit_id",
        "revision",
        "blocks",
    ] {
        let mut damaged = value.clone();
        match field {
            "format" => damaged["format"] = "future-version".into(),
            "conversation_id" => damaged["conversation_id"] = Uuid::now_v7().to_string().into(),
            "last_commit_id" => damaged["last_commit_id"] = "../escape".into(),
            "revision" => damaged["revision"] = 0.into(),
            "blocks" => {
                let block = damaged["context"]["blocks"][0].clone();
                damaged["context"]["blocks"] = serde_json::json!([block, block]);
            }
            _ => unreachable!(),
        }
        fs::write(&path, serde_json::to_vec(&damaged).unwrap()).unwrap();
        assert!(writer.read_latest().is_err(), "{field}");
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].record.is_err());
        assert_eq!(listed[0].active.as_ref().unwrap(), &true);
    }
    fs::write(&path, b"bad json").unwrap();
    assert!(matches!(writer.read_latest(), Err(StoreError::Json(_))));
    assert!(matches!(
        store.acquire_writer("../escape"),
        Err(StoreError::InvalidIdentity(_))
    ));
    let mut media = initial.record.clone();
    media.context = Context::from_blocks(vec![ContextBlock::new(
        new_block_id(),
        BlockContent::Message {
            role: causa::kernel::Role::Assistant,
            parts: vec![ContentPart::Media(MediaRef::new("image/png", "missing"))],
        },
        BlockMeta::default(),
    )])
    .unwrap();
    fs::write(&path, serde_json::to_vec(&media).unwrap()).unwrap();
    assert!(matches!(
        writer.read_latest(),
        Err(StoreError::References(_))
    ));
    fs::remove_file(path).unwrap();
    fs::write(
        folder
            .path()
            .join("sessions")
            .join(format!(".{id}.pending-left.json")),
        b"partial",
    )
    .unwrap();
    assert!(store.list().unwrap().is_empty());
}
#[test]
fn stable_lock_and_only_owned_pending_cleanup() {
    let (folder, store, id, writer) = setup();
    let directory = folder.path().join("sessions");
    let lock_path = directory.join(format!("{id}.lock"));
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(&lock_path).unwrap().ino()
    };
    let own = directory.join(format!(".{id}.pending-old"));
    fs::write(&own, b"old").unwrap();
    let other_id = Uuid::now_v7().to_string();
    let other = directory.join(format!(".{other_id}.pending-old"));
    fs::write(&other, b"other").unwrap();
    assert!(matches!(
        store.acquire_writer(&id),
        Err(StoreError::Occupied(_))
    ));
    assert!(own.exists());
    drop(writer);
    let next = store.acquire_writer(&id).unwrap();
    assert!(!own.exists());
    assert!(other.exists());
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
    let (folder, _store, id, writer) = setup();
    let path = folder.path().join("sessions").join(format!("{id}.json"));
    fs::File::create(path)
        .unwrap()
        .set_len(MAX_RECORD_BYTES as u64 + 1)
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
        )
        .is_err()
    );
}

// Test executable is the actual other process. The parent waits on a marker
// emitted from the operation itself, then kills the child at a known stage.
#[test]
fn subprocess_helper() {
    let Ok(root) = std::env::var("TARZ_STORE_CHILD_ROOT") else {
        return;
    };
    let id = std::env::var("TARZ_STORE_CHILD_ID").unwrap();
    let mode = std::env::var("TARZ_STORE_CHILD_MODE").unwrap();
    let store = SessionStore::new(root.clone().into());
    if mode == "contention" {
        assert!(matches!(
            store.acquire_writer(&id),
            Err(StoreError::Occupied(_))
        ));
        let other = Uuid::now_v7().to_string();
        store.acquire_writer(&other).unwrap();
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
        .env("TARZ_STORE_CHILD_ROOT", folder.path())
        .env("TARZ_STORE_CHILD_ID", id)
        .env("TARZ_STORE_CHILD_MODE", mode)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}
#[test]
fn two_real_processes_contend_while_distinct_sessions_remain_available() {
    let (folder, _store, id, _writer) = setup();
    assert!(
        child(&folder, &id, "contention")
            .wait_with_output()
            .unwrap()
            .status
            .success()
    );
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
            assert_eq!(
                fs::read_dir(folder.path().join("sessions"))
                    .unwrap()
                    .count(),
                2 - usize::from(!existing && stage == "before")
            );
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
    let (folder, _store, id, writer) = setup();
    let mut initial = op(&id, None, "unused");
    initial.record.context = Context::from_blocks(vec![
        ContextBlock::new(
            new_block_id(),
            BlockContent::text(Role::User, "question"),
            BlockMeta::from_source("user"),
        ),
        ContextBlock::new(
            new_block_id(),
            BlockContent::text(Role::System, "repair note"),
            BlockMeta::from_source("application:tarz:material-repair"),
        ),
    ])
    .unwrap();
    let path = folder.path().join("sessions").join(format!("{id}.json"));
    let mut earlier = serde_json::to_value(&initial.record).unwrap();
    for block in earlier["context"]["blocks"].as_array_mut().unwrap() {
        let parts = block["content"]["value"]["parts"].take();
        block["content"] = serde_json::json!({"shape": "parts", "value": parts});
    }
    fs::write(&path, serde_json::to_vec(&earlier).unwrap()).unwrap();

    let loaded = writer.read_latest().unwrap().unwrap();
    assert_eq!(loaded, initial.record);

    let update = PreparedCommit::new(
        Some(1),
        SavedSessionRecord::new(
            id.clone(),
            Uuid::now_v7().to_string(),
            Some(1),
            definition(),
            loaded.context.clone(),
            None,
        )
        .unwrap(),
    )
    .unwrap();
    writer.commit(&update).unwrap();
    let written = String::from_utf8(fs::read(&path).unwrap()).unwrap();
    assert!(written.contains(r#""shape":"message""#));
    assert!(!written.contains(r#""shape":"parts""#));
    assert!(written.contains(r#""format":"tarz-session-v2""#));
}

#[test]
fn record_omits_permission_mode_when_matching_initial_mode() {
    let id = Uuid::now_v7().to_string();
    let mut def = definition();
    def.permission_mode = Some(crate::permission::PermissionMode::AcceptEdits);

    let record_same = SavedSessionRecord::new(
        id.clone(),
        Uuid::now_v7().to_string(),
        None,
        def.clone(),
        Context::new(),
        Some(crate::permission::PermissionMode::AcceptEdits),
    )
    .unwrap();
    assert_eq!(record_same.permission_mode, None);
    let val_same = serde_json::to_value(&record_same).unwrap();
    assert!(val_same.get("permission_mode").is_none());

    let record_diff = SavedSessionRecord::new(
        id.clone(),
        Uuid::now_v7().to_string(),
        None,
        def,
        Context::new(),
        Some(crate::permission::PermissionMode::ReadOnly),
    )
    .unwrap();
    assert_eq!(
        record_diff.permission_mode,
        Some(crate::permission::PermissionMode::ReadOnly)
    );
    let val_diff = serde_json::to_value(&record_diff).unwrap();
    assert_eq!(val_diff["permission_mode"], "read-only");
}

#[test]
fn legacy_v2_record_deserializes_with_none_permission_fields() {
    let id = Uuid::now_v7().to_string();
    let commit = Uuid::now_v7().to_string();
    let json_text = format!(
        r#"{{"format":"tarz-session-v2","conversation_id":"{id}","revision":1,"last_commit_id":"{commit}","definition":{{"provider":"openai-chat","model":"legacy-model","endpoint":null,"credential":{{"environment":"LEGACY_KEY"}},"work_timeout_secs":null,"retained_work_capacity":16,"max_model_rounds":8,"max_tool_calls":0,"workspace_root":null,"tools":[],"tool_limits":{{"max_concurrent_operations":4,"call_timeout_secs":10,"max_output_bytes":65536,"max_directory_entries":200,"max_file_scan_bytes":2097152,"max_search_files":1000,"max_search_depth":32,"max_search_scan_bytes":8388608,"max_search_matches":200}}}},"context":{{"blocks":[]}}}}"#
    );

    let record: SavedSessionRecord = serde_json::from_str(&json_text).unwrap();
    assert_eq!(record.permission_mode, None);
    assert_eq!(record.definition.permission_mode, None);
    assert_eq!(record.definition.write_limits, None);
    assert_eq!(
        record.definition.initial_permission_mode(),
        crate::permission::PermissionMode::Ask
    );

    // Reserialization does not introduce permission_mode or write_limits
    let val = serde_json::to_value(&record).unwrap();
    assert!(val.get("permission_mode").is_none());
    assert!(val["definition"].get("permission_mode").is_none());
    assert!(val["definition"].get("write_limits").is_none());
}

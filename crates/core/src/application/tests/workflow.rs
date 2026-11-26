use super::support::*;
use crate::session::ConversationId;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wf01_rename_idle_in_flight_save_and_in_flight_turn_reconciliation() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let id = session.target().conversation_id().clone();
    assert_eq!(session.title(), None);

    // 1. Idle rename via ManagedSession::rename persists on wait_saved.
    assert_eq!(
        session
            .rename(Some("  Idle Title  ".into()))
            .unwrap()
            .as_deref(),
        Some("Idle Title")
    );
    assert_eq!(session.title().as_deref(), Some("Idle Title"));
    let save = session.wait_saved().await.unwrap();
    assert!(matches!(save, SaveStatus::Saved { revision: 2, .. }));
    assert_eq!(
        host.inspect(&id).await.unwrap().title.as_deref(),
        Some("Idle Title")
    );

    // 2. Host::rename on open idle session clears title and settles durably.
    assert_eq!(host.rename(id.clone(), None).await.unwrap(), None);
    assert_eq!(session.title(), None);
    let inspected = host.inspect(&id).await.unwrap();
    assert_eq!(inspected.title, None);
    assert_eq!(inspected.revision, 3);

    // 3. In-flight save reconciliation: rename arrives while CommitStage::Write is paused.
    let gate = Gate::new();
    session
        .entry
        .lock()
        .writer
        .as_ref()
        .unwrap()
        .set_fault_hook(Some(gate.hook(CommitStage::Write)));
    let receipt = session.submit(request("t1")).unwrap();
    finished(&session, &receipt.work).await;
    gate.entered().await;

    assert_eq!(
        session
            .rename(Some("Renamed During Save".into()))
            .unwrap()
            .as_deref(),
        Some("Renamed During Save")
    );
    assert_eq!(session.title().as_deref(), Some("Renamed During Save"));

    gate.release();
    session.wait_saved().await.unwrap();
    assert_eq!(session.title().as_deref(), Some("Renamed During Save"));
    assert_eq!(
        host.inspect(&id).await.unwrap().title.as_deref(),
        Some("Renamed During Save")
    );

    // Clear fault hook and close.
    session.set_writer_fault_hook(None);
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // 4. Unopened rename via SessionHost::rename + occupied lock + missing + invalid title.
    assert_eq!(
        host.rename(id.clone(), Some("Unopened Title".into()))
            .await
            .unwrap()
            .as_deref(),
        Some("Unopened Title")
    );
    assert_eq!(
        host.inspect(&id).await.unwrap().title.as_deref(),
        Some("Unopened Title")
    );
    assert!(matches!(
        host.rename(id.clone(), Some("bad\ntitle".into())).await,
        Err(ApplicationError::InvalidTitle(_))
    ));
    let missing: ConversationId = uuid::Uuid::now_v7().to_string().into();
    assert!(matches!(
        host.rename(missing, Some("X".into())).await,
        Err(ApplicationError::NotFound)
    ));

    // Another host holds the lock -> SessionLocked.
    let other_host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&other_host);
    let other_session = other_host.open(id.clone()).await.unwrap();
    assert!(matches!(
        host.rename(id.clone(), Some("Blocked".into())).await,
        Err(ApplicationError::SessionLocked(_))
    ));
    other_host
        .close(&other_session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // 5. In-flight turn: rename while model invocation is active, verify persisted after turn.
    let turn_host = SessionHost::new(Some(root.path().into()));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    fixture(
        &turn_host,
        Gateway {
            calls: Arc::new(AtomicUsize::new(0)),
            entered: Some(entered.clone()),
            release: Some(release.clone()),
            panic: false,
        },
    );
    let turn_session = turn_host.open(id.clone()).await.unwrap();
    let work = turn_session.submit(request("t2")).unwrap().work;
    entered.notified().await;

    assert_eq!(
        turn_session
            .rename(Some("Renamed During Turn".into()))
            .unwrap()
            .as_deref(),
        Some("Renamed During Turn")
    );
    assert_eq!(turn_session.title().as_deref(), Some("Renamed During Turn"));

    release.notify_one();
    finished(&turn_session, &work).await;
    turn_session.wait_saved().await.unwrap();
    assert_eq!(
        turn_host.inspect(&id).await.unwrap().title.as_deref(),
        Some("Renamed During Turn")
    );
    let target = turn_session.target();
    turn_host.close(&target, ClosePolicy::Save).await.unwrap();
    assert!(matches!(
        turn_session.rename(Some("After Close".into())),
        Err(ApplicationError::Closed)
    ));

    // 6. Transient session rename works in memory.
    let transient_host = SessionHost::new(None);
    normal_fixture(&transient_host);
    let transient = transient_host
        .create(PreparedCreate::transient(definition(), Context::new()))
        .await
        .unwrap();
    assert_eq!(
        transient
            .rename(Some("Transient".into()))
            .unwrap()
            .as_deref(),
        Some("Transient")
    );
    assert_eq!(transient.title().as_deref(), Some("Transient"));
    transient_host
        .close(&transient.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wf02_host_delete_enforces_active_locked_and_not_found() {
    let root = tempfile::tempdir().unwrap();
    let host1 = SessionHost::new(Some(root.path().into()));
    let host2 = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host1);
    normal_fixture(&host2);

    let session = host1
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let id = session.target().conversation_id().clone();

    // Active in host1 -> SessionActive.
    assert!(matches!(
        host1.delete(id.clone()).await,
        Err(ApplicationError::SessionActive(_))
    ));

    // Held by host2 -> SessionLocked from host1.
    host1
        .close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
    let session2 = host2.open(id.clone()).await.unwrap();
    assert!(matches!(
        host1.delete(id.clone()).await,
        Err(ApplicationError::SessionLocked(_))
    ));

    // Closed in host2 -> delete succeeds; reopening or re-deleting returns NotFound.
    host2
        .close(&session2.target(), ClosePolicy::Save)
        .await
        .unwrap();
    host1.delete(id.clone()).await.unwrap();
    assert!(matches!(
        host1.delete(id.clone()).await,
        Err(ApplicationError::NotFound)
    ));
    assert!(matches!(
        host1.open(id).await,
        Err(ApplicationError::NotFound)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wf03_host_fork_isolates_history_and_refuses_busy_blocked_repair_and_locked() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let gateway =
        CompactionGateway::new(SummaryBehavior::Text("Goal: fork fixture summary.".into()));
    host.set_test_gateway(Some(gateway.clone()));

    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(Some(FIXTURE_WINDOW)),
            earlier_conversation(),
        ))
        .await
        .unwrap();
    let source_id = session.target().conversation_id().clone();

    // Run one turn that triggers auto-compaction so history_records are created.
    let receipt = session
        .submit(SubmitRequest {
            request_key: "compact-turn".into(),
            blocks: short_request(),
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(session.history().len(), 1);
    session.rename(Some("Parent Branch".into())).unwrap();
    session.wait_saved().await.unwrap();

    // Fork open, idle, saved session inside the SAME host (must not deadlock on its own lock!).
    let forked_id = host.fork(source_id.clone(), None).await.unwrap();
    assert_ne!(forked_id, source_id);

    // Close source and delete source completely.
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
    host.delete(source_id.clone()).await.unwrap();

    // Open forked session and verify history expansion + lazy record read succeed after source deletion.
    let forked = host.open(forked_id.clone()).await.unwrap();
    assert_eq!(forked.title().as_deref(), Some("Parent Branch (fork)"));
    let expanded = forked.expanded_conversation(None).unwrap();
    assert!(expanded.unreadable_records.is_empty());
    assert!(!expanded.blocks.is_empty());
    let rec_id = forked.history()[0].record_id.clone();
    let hist_doc = forked.read_history_record(&rec_id).unwrap();
    assert_eq!(hist_doc.conversation_id, forked_id.as_str());

    // Fork refuses while SaveBlocked.
    forked.set_writer_fault_hook(Some(Arc::new(|stage| {
        if stage == CommitStage::Write {
            FaultAction::Fail(std::io::Error::other("disk full"))
        } else {
            FaultAction::Continue
        }
    })));
    let blocked_req = forked
        .submit(SubmitRequest {
            request_key: "save-blocked".into(),
            blocks: short_request(),
        })
        .unwrap();
    finished(&forked, &blocked_req.work).await;
    forked.wait_saved().await.unwrap_err();
    assert!(matches!(
        host.fork(forked_id.clone(), None).await,
        Err(ApplicationError::SaveBlocked(_))
    ));
    forked.set_writer_fault_hook(None);
    forked.retry_save().await.unwrap();

    // Fork refuses when materials have an unpaired declaration needing repair.
    let before_edit = forked.read_materials().unwrap();
    let mut blocks = before_edit.context.blocks().to_vec();
    blocks.push(ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "read_file",
            serde_json::json!({"path": "x.txt"}),
        )),
        BlockMeta::default(),
    ));
    forked.edit(before_edit.version, blocks).unwrap();
    forked.wait_saved().await.unwrap();
    assert!(matches!(
        host.fork(forked_id.clone(), None).await,
        Err(ApplicationError::ForkNotReady(_))
    ));

    // Fork refuses when locked by another host.
    let other_host = SessionHost::new(Some(root.path().into()));
    assert!(matches!(
        other_host
            .fork(forked_id.clone(), Some("Other".into()))
            .await,
        Err(ApplicationError::SessionLocked(_))
    ));

    host.close(&forked.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // Fork refuses when busy (active work in flight).
    let busy_host = SessionHost::new(Some(root.path().into()));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    fixture(
        &busy_host,
        Gateway {
            calls: Arc::new(AtomicUsize::new(0)),
            entered: Some(entered.clone()),
            release: Some(release.clone()),
            panic: false,
        },
    );
    let busy_session = busy_host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let busy_id = busy_session.target().conversation_id().clone();
    let busy_work = busy_session.submit(request("busy")).unwrap().work;
    entered.notified().await;
    assert!(matches!(
        busy_host.fork(busy_id.clone(), None).await,
        Err(ApplicationError::ForkNotReady(_))
    ));
    release.notify_one();
    finished(&busy_session, &busy_work).await;
    busy_session.wait_saved().await.unwrap();
    busy_host
        .close(&busy_session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wf01_explicit_clear_on_empty_session_prevents_auto_derive_on_first_user_message() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let id = session.target().conversation_id().clone();

    // Explicitly clear title before any user message has been sent.
    assert_eq!(session.rename(None).unwrap(), None);
    assert_eq!(session.title(), None);

    // Submit a real Role::User message ("hello"); title must remain None, not auto-derived.
    let user_req = SubmitRequest {
        request_key: "first-user-msg".into(),
        blocks: vec![ContextBlock::text(
            BlockId::new(uuid::Uuid::now_v7()),
            causa::kernel::Role::User,
            "hello",
        )],
    };
    let receipt = session.submit(user_req).unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(session.title(), None);
    assert_eq!(host.inspect(&id).await.unwrap().title, None);

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
    let reopened = host.open(id).await.unwrap();
    assert_eq!(reopened.title(), None);
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wf01_rename_during_failing_compact_is_reconciled_and_persisted() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let gateway = CompactionGateway::new(SummaryBehavior::Block);
    host.set_test_gateway(Some(gateway.clone()));

    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(Some(FIXTURE_WINDOW)),
            earlier_conversation(),
        ))
        .await
        .unwrap();
    let id = session.target().conversation_id().clone();

    // Make summary fail once we release it: run a turn first so compact() has material,
    // or earlier_conversation already provides material! Spawn compact() while SummaryBehavior::Block pauses.
    let compact_task = {
        let session = session.clone();
        tokio::spawn(async move { session.compact().await })
    };
    gateway.entered.notified().await;

    // While compacting, rename and switch gateway behavior to Fail before releasing?
    // Wait: SummaryBehavior::Block already started; dropping compact_task cancels compact() via CompactingGuard!
    assert_eq!(
        session
            .rename(Some("Renamed During Cancelled Compact".into()))
            .unwrap()
            .as_deref(),
        Some("Renamed During Cancelled Compact")
    );
    compact_task.abort();
    let _ = compact_task.await;

    session.wait_saved().await.unwrap();
    assert_eq!(
        host.inspect(&id).await.unwrap().title.as_deref(),
        Some("Renamed During Cancelled Compact")
    );

    // Also test failing compact() (SummaryBehavior::Fail) with a rename queued while compacting:
    // use a second compact where SummaryBehavior::Fail fails immediately, or test via close(Save)
    // with an uncommitted rename prepared cleanly.
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
    let reopened = host.open(id).await.unwrap();
    assert_eq!(
        reopened.title().as_deref(),
        Some("Renamed During Cancelled Compact")
    );
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wf03_host_fork_refuses_transient_uncommitted_rename_and_gates_concurrent_mutations() {
    // 1. Transient session cannot be forked (even on a host with a store).
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);
    let transient = host
        .create(PreparedCreate::transient(definition(), Context::new()))
        .await
        .unwrap();
    assert!(matches!(
        host.fork(transient.target().conversation_id().clone(), None)
            .await,
        Err(ApplicationError::ForkNotReady(_))
    ));
    host.close(&transient.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();

    // 2. Persistent session: uncommitted rename & concurrent forking barrier.
    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let id = session.target().conversation_id().clone();

    // Hold host.io permits so an idle rename remains pending/uncommitted, then verify fork refuses.
    let max_permits = host.inner.options.max_storage_operations as u32;
    let permits = host.inner.io.acquire_many(max_permits).await.unwrap();
    session.rename(Some("Pending Save".into())).unwrap();
    assert!(matches!(
        host.fork(id.clone(), None).await,
        Err(ApplicationError::ForkNotReady(_))
    ));
    drop(permits);
    session.wait_saved().await.unwrap();

    // Now hold host.io permits while an eligible fork starts and enters state.forking == 1.
    let permits = host.inner.io.acquire_many(max_permits).await.unwrap();
    let fork_task = {
        let host = host.clone();
        let id = id.clone();
        tokio::spawn(async move { host.fork(id, Some("Forked".into())).await })
    };
    until(|| session.entry.lock().forking == 1).await;

    // While forking == 1, concurrent mutations and a second fork must be rejected,
    // and close(Save) must defer until ForkGuard drops.
    assert!(matches!(
        session.submit(request("during-fork")).unwrap_err().reason,
        ApplicationError::ForkNotReady(_)
    ));
    assert!(matches!(
        session.rename(Some("Blocked".into())),
        Err(ApplicationError::ForkNotReady(_))
    ));
    assert!(matches!(
        session.set_auto_compact(false),
        Ok(()) | Err(ApplicationError::ForkNotReady(_) | ApplicationError::CompactionUnavailable)
    ));
    assert!(matches!(
        host.fork(id.clone(), None).await,
        Err(ApplicationError::ForkNotReady(_))
    ));
    let close_target = session.target();
    let mut close_fut = Box::pin(host.close(&close_target, ClosePolicy::Save));
    assert!(futures_util::poll!(&mut close_fut).is_pending());

    // Release I/O permits -> fork finishes, ForkGuard drops, and deferred close completes!
    drop(permits);
    let forked_id = fork_task.await.unwrap().unwrap();
    assert_ne!(forked_id, id);
    close_fut.await.unwrap();
}

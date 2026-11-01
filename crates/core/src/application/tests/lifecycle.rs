use super::support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_save_retries_without_model_and_close_failure_retains_writer() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let calls = normal_fixture(&host);
    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    session
        .entry
        .lock()
        .writer
        .as_ref()
        .unwrap()
        .set_fault_hook(Some(Arc::new(|stage| {
            if stage == CommitStage::Write {
                FaultAction::Fail(std::io::Error::other("disk"))
            } else {
                FaultAction::Continue
            }
        })));
    let req = request("once");
    let receipt = session.submit(req.clone()).unwrap();
    finished(&session, &receipt.work).await;
    assert!(matches!(
        session.wait_saved().await,
        Err(ApplicationError::SaveBlocked(_))
    ));
    assert_eq!(session.submit(req).unwrap(), receipt);
    assert!(matches!(
        session.submit(request("once")).unwrap_err().reason,
        ApplicationError::Session(crate::session::SessionError::Conflict { .. })
    ));
    assert!(matches!(
        session.submit(request("new")).unwrap_err().reason,
        ApplicationError::SaveBlocked(_)
    ));
    assert!(matches!(
        host.close(&session.target(), ClosePolicy::Save).await,
        Err(ApplicationError::SaveBlocked(_))
    ));
    assert!(
        crate::store::SessionStore::new(root.path().into())
            .acquire_writer(session.target().conversation_id().as_str())
            .is_err()
    );
    session
        .entry
        .lock()
        .writer
        .as_ref()
        .unwrap()
        .set_fault_hook(None);
    session.retry_save().await.unwrap();
    // retry resumes the already-authorized Save close.
    until(|| host.lookup(session.target().conversation_id()).is_none()).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discard_close_wait_is_supervised_and_never_releases_inflight_writer() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);
    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let gate = Gate::new();
    session
        .entry
        .lock()
        .writer
        .as_ref()
        .unwrap()
        .set_fault_hook(Some(gate.hook(CommitStage::Confirm)));
    let original = request("run");
    let receipt = session.submit(original.clone()).unwrap();
    finished(&session, &receipt.work).await;
    gate.entered().await;
    let close = {
        let host = host.clone();
        let target = session.target();
        tokio::spawn(async move { host.close(&target, ClosePolicy::DiscardLocal).await })
    };
    until(|| session.status().lifecycle == Lifecycle::Closing).await;
    assert_eq!(session.submit(original).unwrap(), receipt);
    assert!(matches!(
        session.submit(request("run")).unwrap_err().reason,
        ApplicationError::Session(crate::session::SessionError::Conflict { .. })
    ));
    assert!(matches!(
        session.submit(request("new")).unwrap_err().reason,
        ApplicationError::Closing
    ));
    close.abort();
    let _ = close.await;
    assert!(matches!(
        host.close(&session.target(), ClosePolicy::Save).await,
        Err(ApplicationError::ClosePolicyConflict)
    ));
    assert!(
        crate::store::SessionStore::new(root.path().into())
            .acquire_writer(session.target().conversation_id().as_str())
            .is_err()
    );
    gate.release();
    until(|| host.lookup(session.target().conversation_id()).is_none()).await;
    assert!(
        crate::store::SessionStore::new(root.path().into())
            .acquire_writer(session.target().conversation_id().as_str())
            .is_ok()
    );
}

#[tokio::test]
async fn transient_worker_panic_preserves_fault_and_exact_replay() {
    let host = SessionHost::new(None);
    fixture(
        &host,
        Gateway {
            calls: Arc::new(AtomicUsize::new(0)),
            entered: None,
            release: None,
            panic: true,
        },
    );
    let session = host
        .create(PreparedCreate::transient(definition(), Context::new()))
        .await
        .unwrap();
    let req = request("panic");
    let receipt = session.submit(req.clone()).unwrap();
    finished(&session, &receipt.work).await;
    assert!(session.wait_saved().await.is_err());
    assert_eq!(session.submit(req).unwrap(), receipt);
    assert!(matches!(
        session.submit(request("panic")).unwrap_err().reason,
        ApplicationError::Session(crate::session::SessionError::Conflict { .. })
    ));
    assert!(session.submit(request("new")).is_err());
    assert!(matches!(
        session.forget(&receipt.work),
        Err(ApplicationError::SavePending)
    ));
    assert!(matches!(session.status().save, SaveStatus::NotTracked));
    assert!(session.read_materials().is_err());
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn owner_panic_cancels_but_holds_writer_until_real_tool_stops() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicUsize::new(0));
    let runner = Arc::new(TurnRunner::new(
        Arc::new(ToolGateway),
        Arc::new(
            ToolExecutor::new(
                vec![Arc::new(HeldTool {
                    entered: entered.clone(),
                    release: release.clone(),
                    stopped: stopped.clone(),
                })],
                ToolExecutorOptions::default(),
            )
            .unwrap(),
        ),
    ));
    custom_runner(&host, runner);
    let mut def = definition();
    def.max_tool_calls = 1;
    let session = host
        .create(PreparedCreate::persistent(def, Context::new()))
        .await
        .unwrap();
    let req = request("live");
    let receipt = session.submit(req.clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    let selected = Arc::new(Notify::new());
    let publish = Arc::new(Notify::new());
    session.entry.lock().handle.as_ref().unwrap().hold_terminal(
        &receipt.work,
        selected.clone(),
        publish.clone(),
    );
    session.entry.panic_owner.notify_one();
    tokio::time::timeout(Duration::from_secs(5), selected.notified())
        .await
        .unwrap();
    until(|| session.status().lifecycle == Lifecycle::Faulted).await;
    assert_eq!(session.submit(req).unwrap(), receipt);
    assert_eq!(stopped.load(Ordering::SeqCst), 1);
    assert!(
        crate::store::SessionStore::new(root.path().into())
            .acquire_writer(session.target().conversation_id().as_str())
            .is_err()
    );
    let close = {
        let host = host.clone();
        let target = session.target();
        tokio::spawn(async move { host.close(&target, ClosePolicy::DiscardLocal).await })
    };
    tokio::task::yield_now().await;
    assert!(!close.is_finished());
    release.notify_one();
    publish.notify_one();
    tokio::time::timeout(Duration::from_secs(5), close)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(stopped.load(Ordering::SeqCst), 1);
    assert!(
        crate::store::SessionStore::new(root.path().into())
            .acquire_writer(session.target().conversation_id().as_str())
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_last_host_supervises_cleanup_but_dropping_views_does_not() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);
    let prepared = PreparedCreate::persistent(definition(), Context::new());
    let session = host.create(prepared.clone()).await.unwrap();
    let target = session.target();
    drop(session);
    assert_eq!(
        host.lookup(prepared.conversation_id()).unwrap().lifecycle,
        Lifecycle::Open
    );
    let session = host.open(prepared.conversation_id().clone()).await.unwrap();
    assert_eq!(session.target(), target);
    drop(host);
    until(|| session.status().lifecycle == Lifecycle::Closed).await;
    assert!(
        crate::store::SessionStore::new(root.path().into())
            .acquire_writer(prepared.conversation_id().as_str())
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_save_close_can_switch_policy_and_discard_confirms_not_applied() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);
    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    session
        .entry
        .lock()
        .writer
        .as_ref()
        .unwrap()
        .set_fault_hook(Some(Arc::new(|stage| {
            if stage == CommitStage::Write {
                FaultAction::Fail(std::io::Error::other("disk"))
            } else {
                FaultAction::Continue
            }
        })));
    let receipt = session.submit(request("work")).unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap_err();
    assert!(
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .is_err()
    );
    let outcome = host
        .close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
    assert!(outcome.discarded_unconfirmed);
    assert!(matches!(
        outcome.pending_commit,
        Some(CommitDisposition::NotApplied)
    ));
    let reopened = host
        .open(session.target().conversation_id().clone())
        .await
        .unwrap();
    assert!(
        reopened
            .read_materials()
            .unwrap()
            .context
            .blocks()
            .is_empty()
    );
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn final_host_disposal_cannot_be_overwritten_by_a_stale_save_close_continuation() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    fixture(
        &host,
        Gateway {
            calls: Arc::new(AtomicUsize::new(0)),
            entered: None,
            release: None,
            panic: true,
        },
    );
    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let receipt = session.submit(request("fault")).unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap_err();
    let selected = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    *session.entry.before_close.lock().unwrap() = Some((selected.clone(), release.clone()));
    let closing = {
        let host = host.clone();
        let target = session.target();
        tokio::spawn(async move { host.close(&target, ClosePolicy::Save).await })
    };
    tokio::time::timeout(Duration::from_secs(5), selected.notified())
        .await
        .unwrap();
    closing.abort();
    let _ = closing.await;
    drop(host);
    release.notify_one();
    until(|| session.status().lifecycle == Lifecycle::Closed).await;
    assert!(
        crate::store::SessionStore::new(root.path().into())
            .acquire_writer(session.target().conversation_id().as_str())
            .is_ok()
    );
}

#[tokio::test]
async fn concurrent_close_and_host_drop_do_not_resurrect_terminal_directory_slot() {
    let host = SessionHost::new(None);
    normal_fixture(&host);
    let session = host
        .create(PreparedCreate::transient(definition(), Context::new()))
        .await
        .unwrap();
    let target = session.target();
    // Model the synchronized terminal state just before its directory removal.
    {
        let mut state = session.entry.lock();
        state.lifecycle = Lifecycle::Closed;
        state.close_outcome = Some(CloseOutcome {
            policy: ClosePolicy::Save,
            save: SaveStatus::NotTracked,
            pending_commit: None,
            discarded_unconfirmed: false,
        });
    }
    let result = host
        .close(&target, ClosePolicy::DiscardLocal)
        .await
        .unwrap();
    assert_eq!(result.policy, ClosePolicy::Save);
    assert_eq!(session.status().lifecycle, Lifecycle::Closed);
    drop(host);
    assert_eq!(session.status().lifecycle, Lifecycle::Closed);
    // The synthetic slot bypassed its production remover. Wake the real driver
    // only to release this test fixture after checking the no-resurrection rule.
    {
        let mut state = session.entry.lock();
        state.lifecycle = Lifecycle::Closing;
        state.closing = Some(ClosePolicy::DiscardLocal);
        state.close_in_flight = true;
    }
    session.entry.notify();
    until(|| session.status().lifecycle == Lifecycle::Closed).await;
}

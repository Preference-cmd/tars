use super::support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_initial_wait_keeps_original_commit_and_merges_openers() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let calls = normal_fixture(&host);
    let gate = Gate::new();
    host.inner
        .store
        .as_ref()
        .unwrap()
        .set_fault_hook(Some(gate.hook(CommitStage::Write)));
    let prepared = PreparedCreate::persistent(definition(), Context::new());
    let task = {
        let host = host.clone();
        let prepared = prepared.clone();
        tokio::spawn(async move { host.create(prepared).await })
    };
    gate.entered().await;
    let target = host.lookup(prepared.conversation_id()).unwrap().target;
    task.abort();
    let _ = task.await;
    let one = {
        let host = host.clone();
        let id = prepared.conversation_id().clone();
        tokio::spawn(async move { host.open(id).await })
    };
    gate.release();
    let session = one.await.unwrap().unwrap();
    let replay = host.create(prepared).await.unwrap();
    assert_eq!(session.target(), target);
    assert_eq!(replay.target(), target);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    host.close(&target, ClosePolicy::Save).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initial_reply_loss_blocks_publication_and_retry_confirms_same_revision() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);
    host.inner
        .store
        .as_ref()
        .unwrap()
        .set_fault_hook(Some(Arc::new(|stage| {
            if stage == CommitStage::Reply {
                FaultAction::Fail(std::io::Error::other("lost response"))
            } else {
                FaultAction::Continue
            }
        })));
    let prepared = PreparedCreate::persistent(definition(), Context::new());
    assert!(matches!(
        host.create(prepared.clone()).await,
        Err(ApplicationError::SaveBlocked(_))
    ));
    let original = host.lookup(prepared.conversation_id()).unwrap();
    assert_eq!(original.lifecycle, Lifecycle::InitializationBlocked);
    assert!(matches!(
        host.open(prepared.conversation_id().clone()).await,
        Err(ApplicationError::SaveBlocked(_))
    ));
    let session = host.create(prepared).await.unwrap();
    assert_eq!(session.target(), original.target);
    assert!(matches!(
        session.status().save,
        SaveStatus::Saved { revision: 1, .. }
    ));
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opening_close_prevents_publication_and_joins_same_policy() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);
    let gate = Gate::new();
    host.inner
        .store
        .as_ref()
        .unwrap()
        .set_fault_hook(Some(gate.hook(CommitStage::Confirm)));
    let prepared = PreparedCreate::persistent(definition(), Context::new());
    let opening = {
        let host = host.clone();
        let prepared = prepared.clone();
        tokio::spawn(async move { host.create(prepared).await })
    };
    gate.entered().await;
    let target = host.lookup(prepared.conversation_id()).unwrap().target;
    let closing = {
        let host = host.clone();
        let target = target.clone();
        tokio::spawn(async move { host.close(&target, ClosePolicy::Save).await })
    };
    until(|| host.lookup(prepared.conversation_id()).unwrap().lifecycle == Lifecycle::Closing)
        .await;
    assert!(matches!(
        opening.await.unwrap(),
        Err(ApplicationError::Closing)
    ));
    assert!(matches!(
        host.close(&target, ClosePolicy::DiscardLocal).await,
        Err(ApplicationError::ClosePolicyConflict)
    ));
    let mut joined = Box::pin(host.close(&target, ClosePolicy::Save));
    assert!(futures_util::poll!(&mut joined).is_pending());
    closing.abort();
    let _ = closing.await;
    gate.release();
    joined.await.unwrap();
    // Already-started close retains sole responsibility after its waiter leaves.
    until(|| host.lookup(prepared.conversation_id()).is_none()).await;
    let reopened = host.open(prepared.conversation_id().clone()).await.unwrap();
    assert_ne!(target, reopened.target());
    assert!(matches!(
        reopened.status().save,
        SaveStatus::Saved { revision: 1, .. }
    ));
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_initial_close_can_explicitly_switch_to_discard_and_reports_saved_fact() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);
    host.inner
        .store
        .as_ref()
        .unwrap()
        .set_fault_hook(Some(Arc::new(|stage| {
            if stage == CommitStage::Reply {
                FaultAction::Fail(std::io::Error::other("unknown initial response"))
            } else {
                FaultAction::Continue
            }
        })));
    let prepared = PreparedCreate::persistent(definition(), Context::new());
    assert!(host.create(prepared.clone()).await.is_err());
    let target = host.lookup(prepared.conversation_id()).unwrap().target;
    let outcome = host
        .close(&target, ClosePolicy::DiscardLocal)
        .await
        .unwrap();
    assert!(matches!(
        outcome.pending_commit,
        Some(CommitDisposition::Applied { revision: 1 })
    ));
    assert!(!outcome.discarded_unconfirmed);
    assert!(matches!(
        outcome.save,
        SaveStatus::Saved { revision: 1, .. }
    ));
    // Discard releases local state but never asserts the successful replacement vanished.
    let reopened = host.open(prepared.conversation_id().clone()).await.unwrap();
    assert!(matches!(
        reopened.status().save,
        SaveStatus::Saved { revision: 1, .. }
    ));
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn all_resource_resolution_waiters_can_leave_and_original_opening_is_still_owned() {
    let host = SessionHost::new(None);
    let gate = Gate::new();
    let original = gate.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    *host.inner.assemble.lock().unwrap() = Some(Arc::new(move |definition, id, context| {
        count.fetch_add(1, Ordering::SeqCst);
        original.entered.notify_one();
        let mut released = original.released.lock().unwrap();
        while !*released {
            released = original.release.wait(released).unwrap();
        }
        Ok(Session::new(
            id,
            context,
            |_, _| async {
                Err(Box::new(std::io::Error::other("unused"))
                    as Box<dyn std::error::Error + Send + Sync>)
            },
            definition.session_config(),
        ))
    }));
    let prepared = PreparedCreate::transient(definition(), Context::new());
    let create = {
        let host = host.clone();
        let prepared = prepared.clone();
        tokio::spawn(async move { host.create(prepared).await })
    };
    gate.entered().await;
    let target = host.lookup(prepared.conversation_id()).unwrap().target;
    create.abort();
    let _ = create.await;
    gate.release();
    let session = host.create(prepared).await.unwrap();
    assert_eq!(session.target(), target);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    host.close(&target, ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn discard_reports_unresolved_initial_confirmation_without_claiming_rollback() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);
    host.inner
        .store
        .as_ref()
        .unwrap()
        .set_fault_hook(Some(Arc::new(|stage| {
            if stage == CommitStage::Reply {
                FaultAction::Fail(std::io::Error::other("response lost"))
            } else {
                FaultAction::Continue
            }
        })));
    let prepared = PreparedCreate::persistent(definition(), Context::new());
    host.create(prepared.clone())
        .await
        .err()
        .expect("initial blocked");
    let target = host.lookup(prepared.conversation_id()).unwrap().target;
    let file = root
        .path()
        .join("sessions")
        .join(format!("{}.json", prepared.conversation_id().as_str()));
    let committed = std::fs::read(&file).unwrap();
    std::fs::write(&file, b"corrupt interference").unwrap();
    let outcome = host
        .close(&target, ClosePolicy::DiscardLocal)
        .await
        .unwrap();
    assert!(outcome.discarded_unconfirmed);
    assert!(matches!(
        outcome.pending_commit,
        Some(CommitDisposition::Unresolved {
            error: ApplicationError::Store(_)
        })
    ));
    assert!(matches!(outcome.save, SaveStatus::SaveBlocked { .. }));
    assert_eq!(std::fs::read(&file).unwrap(), b"corrupt interference");
    std::fs::write(&file, committed).unwrap();
    let reopened = host.open(prepared.conversation_id().clone()).await.unwrap();
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_openers_share_assembly_and_conflicting_create_does_not_replace_operation() {
    let host = SessionHost::new(None);
    let gate = Gate::new();
    let original = gate.clone();
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    *host.inner.assemble.lock().unwrap() = Some(Arc::new(move |definition, id, context| {
        calls.fetch_add(1, Ordering::SeqCst);
        original.entered.notify_one();
        let mut released = original.released.lock().unwrap();
        while !*released {
            released = original.release.wait(released).unwrap();
        }
        Ok(Session::new(
            id,
            context,
            |_, _| async {
                Err(Box::new(std::io::Error::other("unused"))
                    as Box<dyn std::error::Error + Send + Sync>)
            },
            definition.session_config(),
        ))
    }));
    let prepared = PreparedCreate::transient(definition(), Context::new());
    let create = {
        let host = host.clone();
        let prepared = prepared.clone();
        tokio::spawn(async move { host.create(prepared).await })
    };
    gate.entered().await;
    let target = host.lookup(prepared.conversation_id()).unwrap().target;
    let mut conflict = prepared.clone();
    conflict.context = Context::from_blocks(request("different").blocks).unwrap();
    assert!(matches!(
        host.create(conflict).await,
        Err(ApplicationError::CreationConflict)
    ));
    let mut waiters = Vec::new();
    for _ in 0..8 {
        let host = host.clone();
        let id = prepared.conversation_id().clone();
        waiters.push(tokio::spawn(async move { host.open(id).await }));
    }
    create.abort();
    let _ = create.await;
    gate.release();
    for waiter in waiters {
        assert_eq!(waiter.await.unwrap().unwrap().target(), target);
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
    host.close(&target, ClosePolicy::Save).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_persistent_open_reuses_original_instance_and_holds_writer() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);
    let seed = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let id = seed.target().conversation_id().clone();
    host.close(&seed.target(), ClosePolicy::Save).await.unwrap();
    let gate = Gate::new();
    let original = gate.clone();
    let count = Arc::new(AtomicUsize::new(0));
    let calls = count.clone();
    *host.inner.assemble.lock().unwrap() = Some(Arc::new(move |definition, id, context| {
        calls.fetch_add(1, Ordering::SeqCst);
        original.entered.notify_one();
        let mut released = original.released.lock().unwrap();
        while !*released {
            released = original.release.wait(released).unwrap();
        }
        Ok(Session::new(
            id,
            context,
            |_, _| async {
                Err(Box::new(std::io::Error::other("unused"))
                    as Box<dyn std::error::Error + Send + Sync>)
            },
            definition.session_config(),
        ))
    }));
    let opening = {
        let host = host.clone();
        let id = id.clone();
        tokio::spawn(async move { host.open(id).await })
    };
    gate.entered().await;
    let target = host.lookup(&id).unwrap().target;
    opening.abort();
    let _ = opening.await;
    assert!(
        crate::store::SessionStore::new(root.path().into())
            .acquire_writer(id.as_str())
            .is_err()
    );
    gate.release();
    let session = host.open(id).await.unwrap();
    assert_eq!(session.target(), target);
    assert_eq!(count.load(Ordering::SeqCst), 1);
    host.close(&target, ClosePolicy::Save).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opening_discard_before_initial_write_reports_not_submitted() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let gate = Gate::new();
    let original = gate.clone();
    *host.inner.assemble.lock().unwrap() = Some(Arc::new(move |definition, id, context| {
        original.entered.notify_one();
        let mut released = original.released.lock().unwrap();
        while !*released {
            released = original.release.wait(released).unwrap();
        }
        Ok(Session::new(
            id,
            context,
            |_, _| async {
                Err(Box::new(std::io::Error::other("unused"))
                    as Box<dyn std::error::Error + Send + Sync>)
            },
            definition.session_config(),
        ))
    }));
    let prepared = PreparedCreate::persistent(definition(), Context::new());
    let opening = {
        let host = host.clone();
        let prepared = prepared.clone();
        tokio::spawn(async move { host.create(prepared).await })
    };
    gate.entered().await;
    let target = host.lookup(prepared.conversation_id()).unwrap().target;
    let mut close = Box::pin(host.close(&target, ClosePolicy::DiscardLocal));
    assert!(futures_util::poll!(&mut close).is_pending());
    gate.release();
    let outcome = close.await.unwrap();
    assert!(outcome.discarded_unconfirmed);
    assert!(matches!(
        outcome.pending_commit,
        Some(CommitDisposition::NotSubmitted)
    ));
    assert!(opening.await.unwrap().is_err());
    assert_eq!(host.inner.store.as_ref().unwrap().metrics().writes, 0);
    assert!(host.list().await.unwrap().is_empty());
}

#[tokio::test]
async fn failed_assembly_releases_slot_and_writer_before_same_prepared_retry() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = attempts.clone();
    *host.inner.assemble.lock().unwrap() = Some(Arc::new(move |_, _, _| {
        count.fetch_add(1, Ordering::SeqCst);
        Err(ApplicationError::Assembly(
            "fixture resource unavailable".into(),
        ))
    }));
    let prepared = PreparedCreate::persistent(definition(), Context::new());
    let id = prepared.conversation_id().clone();
    assert!(matches!(
        host.create(prepared.clone()).await,
        Err(ApplicationError::Assembly(_))
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(host.lookup(&id).is_none());
    let writer = crate::store::SessionStore::new(root.path().into())
        .acquire_writer(id.as_str())
        .unwrap();
    assert!(writer.read_latest().unwrap().is_none());
    drop(writer);
    assert_eq!(host.inner.store.as_ref().unwrap().metrics().writes, 0);
    let calls = normal_fixture(&host);
    let recovered = host.create(prepared).await.unwrap();
    assert_eq!(recovered.target().conversation_id(), &id);
    assert!(matches!(
        recovered.status().save,
        SaveStatus::Saved { revision: 1, .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    host.close(&recovered.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test]
async fn corrupt_open_releases_slot_without_overwrite_and_retries_after_explicit_repair() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let calls = normal_fixture(&host);
    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let original = session.target();
    host.close(&original, ClosePolicy::Save).await.unwrap();
    let path = root
        .path()
        .join("sessions")
        .join(format!("{}.json", original.conversation_id().as_str()));
    let valid_record = std::fs::read(&path).unwrap();
    std::fs::write(&path, b"invalid fixture JSON").unwrap();
    *host.inner.assemble.lock().unwrap() = Some(Arc::new(|_, _, _| {
        panic!("corrupt record must fail before assembly")
    }));
    assert!(matches!(
        host.open(original.conversation_id().clone()).await,
        Err(ApplicationError::Store(_))
    ));
    assert!(host.lookup(original.conversation_id()).is_none());
    assert_eq!(std::fs::read(&path).unwrap(), b"invalid fixture JSON");
    let writer = crate::store::SessionStore::new(root.path().into())
        .acquire_writer(original.conversation_id().as_str())
        .unwrap();
    assert!(writer.read_latest().is_err());
    drop(writer);
    // Only this explicit fixture repair changes the corrupt record.
    std::fs::write(&path, valid_record).unwrap();
    normal_fixture(&host);
    let reopened = host.open(original.conversation_id().clone()).await.unwrap();
    assert_ne!(reopened.target(), original);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(host.inner.store.as_ref().unwrap().metrics().writes, 1);
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

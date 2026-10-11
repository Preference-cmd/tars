use super::support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finished_work_keeps_exact_replay_but_blocks_t2_until_save_and_preserves_result() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let calls = normal_fixture(&host);
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
        .set_fault_hook(Some(gate.hook(CommitStage::Write)));
    let original = request("t1");
    let receipt = session.submit(original.clone()).unwrap();
    finished(&session, &receipt.work).await;
    gate.entered().await;
    assert_eq!(session.submit(original.clone()).unwrap(), receipt);
    assert!(matches!(
        session.submit(request("t1")).unwrap_err().reason,
        ApplicationError::Session(crate::session::SessionError::Conflict { .. })
    ));
    let t2 = request("t2");
    let rejected = session.submit(t2.clone()).unwrap_err();
    assert!(matches!(rejected.reason, ApplicationError::Saving));
    assert_eq!(rejected.request, t2);
    assert!(matches!(
        session.forget(&receipt.work),
        Err(ApplicationError::SavePending)
    ));
    let snapshot = session.read_materials().unwrap();
    assert_eq!(snapshot.context.blocks().len(), 2);
    gate.release();
    session.wait_saved().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        matches!(session.observe(&receipt.work).unwrap().state,WorkState::Finished(result) if matches!(&*result,WorkResult::Run(causa::runtime::TurnResult::Completed{final_output}) if final_output.response.text()=="completed original"))
    );
    let next = session.submit(t2).unwrap();
    finished(&session, &next.work).await;
    session.wait_saved().await.unwrap();
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_handle_never_routes_to_reopened_instance_and_edits_are_versioned() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);
    let a = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let old = a.read_materials().unwrap();
    a.edit(old.version.clone(), request("edit").blocks).unwrap();
    a.wait_saved().await.unwrap();
    assert!(matches!(
        a.edit(old.version.clone(), vec![]),
        Err(ApplicationError::StaleMaterial)
    ));
    let target = a.target();
    host.close(&target, ClosePolicy::Save).await.unwrap();
    let b = host.open(target.conversation_id().clone()).await.unwrap();
    assert_ne!(target, b.target());
    assert!(matches!(
        a.submit(request("old")).unwrap_err().reason,
        ApplicationError::Closed
    ));
    assert!(matches!(
        host.close(&target, ClosePolicy::DiscardLocal).await,
        Err(ApplicationError::StaleInstance)
    ));
    assert!(matches!(
        b.edit(old.version, vec![]),
        Err(ApplicationError::StaleInstance)
    ));
    assert_eq!(b.read_materials().unwrap().context.blocks().len(), 1);
    host.close(&b.target(), ClosePolicy::Save).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_retry_is_supervised_and_concurrent_retries_share_one_commit() {
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
                FaultAction::Fail(std::io::Error::other("first failure"))
            } else {
                FaultAction::Continue
            }
        })));
    let receipt = session.submit(request("only")).unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap_err();
    let commit = match session.status().save {
        SaveStatus::SaveBlocked { commit_id, .. } => commit_id,
        _ => panic!("blocked"),
    };
    let gate = Gate::new();
    session
        .entry
        .lock()
        .writer
        .as_ref()
        .unwrap()
        .set_fault_hook(Some(gate.hook(CommitStage::Write)));
    let retry = {
        let session = session.clone();
        tokio::spawn(async move { session.retry_save().await })
    };
    gate.entered().await;
    retry.abort();
    let _ = retry.await;
    let joined = {
        let session = session.clone();
        tokio::spawn(async move { session.retry_save().await })
    };
    assert!(matches!(session.status().save,SaveStatus::Saving{commit_id,..} if commit_id==commit));
    gate.release();
    assert!(matches!(
        joined.await.unwrap().unwrap(),
        SaveStatus::Saved { revision: 2, .. }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(host.inner.store.as_ref().unwrap().metrics().writes, 2);
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test]
async fn rejected_edit_preserves_materials_version_and_saved_baseline() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);
    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let before = session.read_materials().unwrap();
    let mut media = request("media").blocks;
    media[0] = ContextBlock::new(
        media[0].id(),
        BlockContent::Parts(vec![causa::kernel::ContentPart::Media(
            causa::kernel::MediaRef::new("image/png", "unretained://asset"),
        )]),
        BlockMeta::default(),
    );
    assert!(matches!(
        session.edit(before.version.clone(), media),
        Err(ApplicationError::Store(_))
    ));
    let after = session.read_materials().unwrap();
    assert_eq!(before.version, after.version);
    assert_eq!(before.context.blocks(), after.context.blocks());
    assert!(matches!(
        session.status().save,
        SaveStatus::Saved { revision: 1, .. }
    ));
    let block = request("duplicate").blocks.remove(0);
    assert!(
        session
            .edit(before.version.clone(), vec![block.clone(), block])
            .is_err()
    );
    assert_eq!(session.read_materials().unwrap().version, before.version);
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_work_wait_does_not_cancel_and_original_request_replays() {
    let host = SessionHost::new(None);
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    fixture(
        &host,
        Gateway {
            calls: calls.clone(),
            entered: Some(entered.clone()),
            release: Some(release.clone()),
            panic: false,
        },
    );
    let session = host
        .create(PreparedCreate::transient(definition(), Context::new()))
        .await
        .unwrap();
    let idle = session.read_materials().unwrap();
    let req = request("retained");
    let receipt = session.submit(req.clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    assert!(matches!(
        session.edit(idle.version, vec![]),
        Err(ApplicationError::Session(
            crate::session::SessionError::Busy { .. }
        ))
    ));
    let waiter = {
        let session = session.clone();
        let work = receipt.work.clone();
        tokio::spawn(async move { session.wait(&work, Duration::from_secs(30)).await })
    };
    waiter.abort();
    let _ = waiter.await;
    assert_eq!(session.submit(req).unwrap(), receipt);
    assert!(!matches!(
        session.observe(&receipt.work).unwrap().state,
        WorkState::Finished(_)
    ));
    release.notify_one();
    finished(&session, &receipt.work).await;
    assert!(matches!(
        session.wait_saved().await.unwrap(),
        SaveStatus::NotTracked
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

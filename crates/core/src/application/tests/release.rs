use super::support::*;

/// Wait for the work to settle and its save to confirm without observing it,
/// so the retained record stays "unread".
async fn settle_unread(session: &ManagedSession) {
    until(|| session.status().active_work.is_none()).await;
    session.wait_saved().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finished_works_are_released_at_capacity_and_history_stays_visible() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);
    let mut definition = definition();
    definition.retained_work_capacity = 2;
    let session = host
        .create(PreparedCreate::persistent(definition, Context::new()))
        .await
        .unwrap();
    let mut receipts = Vec::new();
    for index in 0..5 {
        let receipt = session.submit(request(&format!("work-{index}"))).unwrap();
        finished(&session, &receipt.work).await;
        session.wait_saved().await.unwrap();
        let capacity = session.work_capacity().unwrap();
        assert_eq!(capacity.capacity, 2);
        assert!(
            capacity.used <= 2,
            "the work sheet never exceeds its bound: {capacity:?}"
        );
        receipts.push(receipt);
    }
    // Releasing work records does not touch the ConversationId materials.
    let materials = session.read_materials().unwrap();
    assert_eq!(materials.context.blocks().len(), 10);
    // The oldest released work is no longer observable under its reference.
    assert!(session.observe(&receipts[0].work).is_err());
    // A released request key starts a new work instead of replaying.
    let replay = session.submit(request("work-0")).unwrap();
    assert_ne!(replay.work, receipts[0].work);
    finished(&session, &replay.work).await;
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unread_results_keep_the_sheet_full_until_a_caller_observes_them() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);
    let mut definition = definition();
    definition.retained_work_capacity = 2;
    let session = host
        .create(PreparedCreate::persistent(definition, Context::new()))
        .await
        .unwrap();
    let first = session.submit(request("first")).unwrap();
    settle_unread(&session).await;
    let second = session.submit(request("second")).unwrap();
    settle_unread(&session).await;
    assert_eq!(session.work_capacity().unwrap().used, 2);
    // Neither terminal result was handed to a caller, so nothing is released.
    let rejected = session.submit(request("third")).unwrap_err();
    assert!(matches!(
        rejected.reason,
        ApplicationError::Session(crate::session::SessionError::Capacity { .. })
    ));
    let message = rejected.reason.to_string();
    assert!(message.contains("reopen"), "{message}");
    assert!(message.contains("claim"), "{message}");
    // Reading the oldest terminal result makes it releasable at the next admission.
    session.observe(&first.work).unwrap();
    let third = session.submit(request("third")).unwrap();
    finished(&session, &third.work).await;
    assert!(session.observe(&second.work).is_ok());
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn work_capacity_is_reported_for_a_transient_session() {
    let host = SessionHost::new(None);
    normal_fixture(&host);
    let definition = definition();
    let capacity = definition.retained_work_capacity;
    let session = host
        .create(PreparedCreate::transient(definition, Context::new()))
        .await
        .unwrap();
    let reported = session.work_capacity().unwrap();
    assert_eq!(reported.capacity, capacity);
    assert_eq!(reported.used, 0);
    let receipt = session.submit(request("only")).unwrap();
    finished(&session, &receipt.work).await;
    assert_eq!(session.work_capacity().unwrap().used, 1);
}

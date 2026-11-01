use super::support::*;
use crate::session::progress_tests::{Ending, ScriptGateway, runner};
use crate::session::{ProgressUpdate, WorkProgress};
use causa::kernel::StreamDelta;
use causa::runtime::{RunEvent, RunObserver};
use tokio::sync::broadcast;

fn install(host: &SessionHost, gateway: ScriptGateway, observer: Option<Arc<RunObserver>>) {
    let runner = runner(gateway);
    *host.inner.assemble.lock().unwrap() = Some(Arc::new(move |definition, id, context| {
        let runner = runner.clone();
        let mut options = definition.run_options();
        options.observer = observer.clone();
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
async fn recv(progress: &mut broadcast::Receiver<Arc<WorkProgress>>) -> Arc<WorkProgress> {
    tokio::time::timeout(Duration::from_secs(3), progress.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn managed_preview_arrives_before_terminal_and_only_final_output_is_saved() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let release = Arc::new(Notify::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(AtomicUsize::new(0));
    let count = observed.clone();
    install(
        &host,
        ScriptGateway {
            deltas: vec![StreamDelta::TextDelta {
                delta: "temporary preview".into(),
            }],
            ending: Ending::Complete,
            release: Some(release.clone()),
            calls: calls.clone(),
        },
        Some(Arc::new(move |event| {
            if matches!(event, RunEvent::ModelDelta { .. }) {
                count.fetch_add(1, Ordering::SeqCst);
            }
        })),
    );
    let prepared = PreparedCreate::persistent(definition(), Context::new());
    let id = prepared.conversation_id().clone();
    let session = host.create(prepared).await.unwrap();
    let mut progress = session.subscribe_progress();
    let receipt = session.submit(request("stream")).unwrap();
    let started = recv(&mut progress).await;
    let text = recv(&mut progress).await;
    assert_eq!(started.update, ProgressUpdate::ModelStarted);
    assert_eq!(started.work, receipt.work);
    assert_eq!(text.invocation_id, started.invocation_id);
    assert!(
        matches!(&text.update,ProgressUpdate::Text{delta,truncated:false} if delta=="temporary preview")
    );
    assert!(matches!(
        session.observe(&receipt.work).unwrap().state,
        WorkState::Running
    ));
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let path = root
        .path()
        .join("sessions")
        .join(format!("{}.json", id.as_str()));
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("temporary preview")
    );
    release.notify_one();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(
        recv(&mut progress).await.update,
        ProgressUpdate::ModelOutputReady
    );
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(saved.contains("authoritative saved output"));
    assert!(!saved.contains("temporary preview"));
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test]
async fn managed_cancelled_failed_and_eof_streams_save_no_partial_model_text() {
    for ending in [Ending::Complete, Ending::Error, Ending::Eof] {
        let root = tempfile::tempdir().unwrap();
        let host = SessionHost::new(Some(root.path().into()));
        let release = Arc::new(Notify::new());
        install(
            &host,
            ScriptGateway {
                deltas: vec![StreamDelta::TextDelta {
                    delta: "partial-preview".into(),
                }],
                ending,
                release: Some(release.clone()),
                calls: Arc::new(AtomicUsize::new(0)),
            },
            None,
        );
        let prepared = PreparedCreate::persistent(definition(), Context::new());
        let id = prepared.conversation_id().clone();
        let session = host.create(prepared).await.unwrap();
        let mut progress = session.subscribe_progress();
        let receipt = session.submit(request("stopped")).unwrap();
        recv(&mut progress).await;
        recv(&mut progress).await;
        if matches!(ending, Ending::Complete) {
            session.cancel(&receipt.work).unwrap();
        } else {
            release.notify_one();
        }
        finished(&session, &receipt.work).await;
        session.wait_saved().await.unwrap();
        assert!(
            matches!(session.observe(&receipt.work).unwrap().state,WorkState::Finished(result) if matches!(&*result,WorkResult::Run(causa::runtime::TurnResult::Interrupted{..})))
        );
        assert_eq!(session.read_materials().unwrap().context.blocks().len(), 1);
        assert!(
            !std::fs::read_to_string(
                root.path()
                    .join("sessions")
                    .join(format!("{}.json", id.as_str()))
            )
            .unwrap()
            .contains("partial-preview")
        );
        assert!(matches!(
            progress.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn managed_lag_drop_and_reopen_keep_progress_instance_scoped_and_results_reliable() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    install(
        &host,
        ScriptGateway {
            deltas: (0..256)
                .map(|_| StreamDelta::TextDelta {
                    delta: "preview".into(),
                })
                .collect(),
            ending: Ending::Complete,
            release: None,
            calls: Arc::new(AtomicUsize::new(0)),
        },
        None,
    );
    let prepared = PreparedCreate::persistent(definition(), Context::new());
    let id = prepared.conversation_id().clone();
    let old = host.create(prepared).await.unwrap();
    let mut lagged = old.subscribe_progress();
    let mut old_receiver = old.subscribe_progress();
    let receipt = old.submit(request("burst")).unwrap();
    finished(&old, &receipt.work).await;
    old.wait_saved().await.unwrap();
    assert!(matches!(
        lagged.try_recv(),
        Err(broadcast::error::TryRecvError::Lagged(130))
    ));
    drop(lagged);
    while !matches!(
        old_receiver.try_recv(),
        Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed)
    ) {}
    host.close(&old.target(), ClosePolicy::Save).await.unwrap();
    let reopened = host.open(id).await.unwrap();
    assert_ne!(old.target(), reopened.target());
    let progress = reopened.subscribe_progress();
    drop(progress);
    let receipt = reopened.submit(request("unobserved")).unwrap();
    finished(&reopened, &receipt.work).await;
    reopened.wait_saved().await.unwrap();
    assert!(matches!(
        old_receiver.try_recv(),
        Err(broadcast::error::TryRecvError::Empty | broadcast::error::TryRecvError::Closed)
    ));
    assert!(matches!(
        reopened.observe(&receipt.work).unwrap().state,
        WorkState::Finished(_)
    ));
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

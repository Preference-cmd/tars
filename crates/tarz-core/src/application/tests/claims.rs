use super::support::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn original_batch_claim_is_once_in_saving_blocked_and_closing_without_touching_result() {
    for phase in 0..3 {
        let root = tempfile::tempdir().unwrap();
        let host = SessionHost::new(Some(root.path().into()));
        let existing = request("seed").blocks.remove(0);
        let executed = Arc::new(AtomicUsize::new(0));
        let runner = Arc::new(TurnRunner::new(
            Arc::new(ToolGateway),
            Arc::new(
                ToolExecutor::new(
                    vec![Arc::new(HeldTool {
                        entered: Arc::new(Notify::new()),
                        release: Arc::new(Notify::new()),
                        stopped: executed.clone(),
                    })],
                    ToolExecutorOptions::default()
                        .with_before(vec![Arc::new(CollisionBatch(existing.id()))]),
                )
                .unwrap(),
            ),
        ));
        custom_runner(&host, runner);
        let mut def = definition();
        def.max_tool_calls = 1;
        let session = host
            .create(PreparedCreate::persistent(
                def,
                Context::from_blocks(vec![existing]).unwrap(),
            ))
            .await
            .unwrap();
        let gate = Gate::new();
        session
            .entry
            .lock()
            .writer
            .as_ref()
            .unwrap()
            .set_fault_hook(Some(if phase == 1 {
                Arc::new(|stage| {
                    if stage == CommitStage::Write {
                        FaultAction::Fail(std::io::Error::other("disk"))
                    } else {
                        FaultAction::Continue
                    }
                })
            } else {
                gate.hook(CommitStage::Write)
            }));
        let receipt = session
            .submit(SubmitRequest {
                request_key: "u".into(),
                blocks: vec![],
            })
            .unwrap();
        finished(&session, &receipt.work).await;
        if phase == 1 {
            session.wait_saved().await.unwrap_err();
        } else {
            gate.entered().await;
        }
        let target = session.target();
        let mut close = Box::pin(host.close(&target, ClosePolicy::Save));
        if phase == 2 {
            assert!(futures_util::poll!(&mut close).is_pending());
            assert_eq!(session.status().lifecycle, Lifecycle::Closing);
        }
        let before = session.read_materials().unwrap();
        let result = match session.observe(&receipt.work).unwrap().state {
            WorkState::Finished(result) => result,
            _ => unreachable!(),
        };
        assert!(matches!(
            session.forget(&receipt.work),
            Err(ApplicationError::SavePending)
        ));
        let (a, b) = std::thread::scope(|scope| {
            let a = scope.spawn(|| host.take_uncommitted_tool_batch(&target, &receipt.work));
            let b = scope.spawn(|| host.take_uncommitted_tool_batch(&target, &receipt.work));
            (a.join().unwrap().unwrap(), b.join().unwrap().unwrap())
        });
        assert_ne!(a.is_some(), b.is_some());
        let batch = a.or(b).unwrap();
        assert_eq!(batch.completed_len(), 1);
        assert_eq!(
            batch.results()[0].result().unwrap().1.media[0].reference,
            "uncommitted original asset"
        );
        assert!(
            host.take_uncommitted_tool_batch(&target, &receipt.work)
                .unwrap()
                .is_none()
        );
        let after = session.read_materials().unwrap();
        assert_eq!(before.version, after.version);
        assert_eq!(before.context.blocks(), after.context.blocks());
        assert!(
            matches!(session.observe(&receipt.work).unwrap().state,WorkState::Finished(current) if Arc::ptr_eq(&result,&current))
        );
        assert_eq!(executed.load(Ordering::SeqCst), 0);
        if phase == 1 {
            session
                .entry
                .lock()
                .writer
                .as_ref()
                .unwrap()
                .set_fault_hook(None);
            session.retry_save().await.unwrap();
        } else {
            gate.release();
        }
        if phase == 2 {
            close.await.unwrap();
        } else {
            drop(close);
            session.wait_saved().await.unwrap();
            session.forget(&receipt.work).unwrap();
            assert!(matches!(
                host.take_uncommitted_tool_batch(&target, &receipt.work),
                Err(ApplicationError::Session(
                    crate::session::SessionError::NotFound { .. }
                ))
            ));
            host.close(&target, ClosePolicy::Save).await.unwrap();
        }
        let reopened = host.open(target.conversation_id().clone()).await.unwrap();
        assert!(matches!(
            reopened.observe(&receipt.work),
            Err(crate::session::SessionError::NotFound { .. })
        ));
        assert!(matches!(
            host.take_uncommitted_tool_batch(&target, &receipt.work),
            Err(ApplicationError::StaleInstance)
        ));
        assert_eq!(executed.load(Ordering::SeqCst), 0);
        host.close(&reopened.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn batch_claim_rejects_unfinished_and_linearizes_against_forget() {
    let host = SessionHost::new(None);
    let existing = request("seed").blocks.remove(0);
    let runner = Arc::new(TurnRunner::new(
        Arc::new(ToolGateway),
        Arc::new(
            ToolExecutor::new(
                vec![Arc::new(HeldTool {
                    entered: Arc::new(Notify::new()),
                    release: Arc::new(Notify::new()),
                    stopped: Arc::new(AtomicUsize::new(0)),
                })],
                ToolExecutorOptions::default()
                    .with_before(vec![Arc::new(CollisionBatch(existing.id()))]),
            )
            .unwrap(),
        ),
    ));
    custom_runner(&host, runner);
    let mut def = definition();
    def.max_tool_calls = 1;
    let session = host
        .create(PreparedCreate::transient(
            def,
            Context::from_blocks(vec![existing]).unwrap(),
        ))
        .await
        .unwrap();
    let target = session.target();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "claim".into(),
            blocks: vec![],
        })
        .unwrap();
    assert!(matches!(
        host.take_uncommitted_tool_batch(&target, &receipt.work),
        Err(ApplicationError::Session(
            crate::session::SessionError::NotFinished { .. }
        ))
    ));
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    let result = std::thread::scope(|scope| {
        let claim = scope.spawn(|| host.take_uncommitted_tool_batch(&target, &receipt.work));
        let forget = scope.spawn(|| session.forget(&receipt.work));
        forget.join().unwrap().unwrap();
        claim.join().unwrap()
    });
    assert!(matches!(
        result,
        Ok(Some(_))
            | Err(ApplicationError::Session(
                crate::session::SessionError::NotFound { .. }
            ))
    ));
    assert!(matches!(
        host.take_uncommitted_tool_batch(&target, &receipt.work),
        Err(ApplicationError::Session(
            crate::session::SessionError::NotFound { .. }
        ))
    ));
    host.close(&target, ClosePolicy::Save).await.unwrap();
}

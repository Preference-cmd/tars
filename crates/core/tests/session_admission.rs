mod common;
use causa::{kernel::*, runtime::*};
use common::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tars_core::session::*;
use tokio::sync::{Notify, Semaphore};

#[test]
fn construction_preserves_partial_tool_material_and_has_no_execution() {
    let old_call = ContextBlock::new(
        new_block_id(),
        BlockContent::ToolCall(ToolCallPayload::new("old", serde_json::json!({"old":true}))),
        BlockMeta::default(),
    );
    let blocks = vec![block("I"), old_call, block("B")];
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let mut session = Session::new(
        "conversation".into(),
        Context::from_blocks(blocks.clone()).unwrap(),
        move |_, _| {
            count.fetch_add(1, Ordering::SeqCst);
            async {
                Err(Box::new(std::io::Error::other("must not assemble"))
                    as Box<dyn std::error::Error + Send + Sync>)
            }
        },
        config(1),
    );
    assert_eq!(context(&mut session), blocks);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let foreign = WorkRef {
        conversation_id: "conversation".into(),
        turn_id: TurnId::new("old"),
    };
    assert!(matches!(
        session.handle().observe(&foreign),
        Err(SessionError::NotFound { .. })
    ));
}

#[test]
fn absent_runtime_and_capacity_refuse_before_appending() {
    for capacity in [0, 1] {
        let initial = vec![block("initial")];
        let mut session = simple_session(
            Context::from_blocks(initial.clone()).unwrap(),
            capacity,
            Gateway::new(vec![]),
        );
        let incoming = vec![block("first"), block("second")];
        let rejected = session
            .handle()
            .submit(request("new", incoming.clone()))
            .unwrap_err();
        assert_eq!(rejected.request.request_key, "new");
        assert_eq!(rejected.request.blocks, incoming);
        match capacity {
            0 => assert!(matches!(
                rejected.reason,
                SessionError::Capacity { capacity: 0 }
            )),
            _ => assert!(matches!(rejected.reason, SessionError::RuntimeUnavailable)),
        }
        assert_eq!(context(&mut session), initial);
    }
}

#[tokio::test]
async fn append_is_atomic_and_failed_key_remains_available() {
    let initial = block("initial");
    let mut session = simple_session(
        Context::from_blocks(vec![initial.clone()]).unwrap(),
        4,
        Gateway::new(vec![final_output("done")]),
    );
    let handle = session.handle();
    for incoming in [vec![block("new"), initial.clone()], {
        let duplicate = block("same");
        vec![duplicate.clone(), duplicate]
    }] {
        let rejected = handle.submit(request("key", incoming.clone())).unwrap_err();
        assert!(matches!(
            rejected.reason,
            SessionError::ContextEdit(EditError::DuplicateBlockId(_))
        ));
        assert_eq!(rejected.request.blocks, incoming);
        assert_eq!(context(&mut session), vec![initial.clone()]);
    }
    let changed_identity = ContextBlock::new(
        initial.id(),
        block("changed").content().clone(),
        initial.meta().clone(),
    );
    let incoming = vec![block("new prefix"), changed_identity];
    let refused = handle.submit(request("key", incoming.clone())).unwrap_err();
    assert!(
        matches!(refused.reason, SessionError::ContextEdit(EditError::BlockIdentityMismatch(id)) if id == initial.id())
    );
    assert_eq!(refused.request.blocks, incoming);
    assert_eq!(context(&mut session), vec![initial.clone()]);
    let incoming = vec![block("first"), block("second")];
    let receipt = handle.submit(request("key", incoming.clone())).unwrap();
    completed(&*finished(&handle, &receipt.work).await, "done");
    assert_eq!(
        &context(&mut session)[..3],
        &[vec![initial], incoming].concat()
    );
}

#[tokio::test]
async fn exact_replay_precedes_busy_edit_validation_and_conflict_checks_every_field() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let count = Arc::new(AtomicUsize::new(0));
    let runner = runner(Gateway::new(vec![final_output("done")]), vec![]);
    let mut session = Session::new(
        "conversation".into(),
        Context::new(),
        {
            let entered = entered.clone();
            let release = release.clone();
            let count = count.clone();
            move |_, _| {
                let entered = entered.clone();
                let release = release.clone();
                let count = count.clone();
                let runner = runner.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    entered.notify_one();
                    let _permit = release.acquire().await.unwrap();
                    Ok((runner, TurnRunOptions::new(ModelRef::new("fixture"))))
                }
            }
        },
        config(2),
    );
    let handle = session.handle();
    let original = vec![block("one"), block("two")];
    let receipt = handle.submit(request("key", original.clone())).unwrap();
    tokio::time::timeout(BOUND, entered.notified())
        .await
        .unwrap();
    let other = handle.clone();
    let retries = (0..8)
        .map(|_| {
            let other = other.clone();
            let original = original.clone();
            tokio::spawn(async move { other.submit(request("key", original)).unwrap() })
        })
        .collect::<Vec<_>>();
    for retry in retries {
        assert_eq!(retry.await.unwrap(), receipt);
    }
    let mut changes = vec![
        vec![original[1].clone(), original[0].clone()],
        vec![original[0].clone()],
        vec![original[0].clone(), original[1].clone(), block("three")],
    ];
    changes.push(vec![
        ContextBlock::new(
            original[0].id(),
            block("changed").content().clone(),
            original[0].meta().clone(),
        ),
        original[1].clone(),
    ]);
    changes.push(vec![
        ContextBlock::new(
            new_block_id(),
            original[0].content().clone(),
            original[0].meta().clone(),
        ),
        original[1].clone(),
    ]);
    changes.push(vec![
        ContextBlock::new(
            original[0].id(),
            original[0].content().clone(),
            BlockMeta::default(),
        ),
        original[1].clone(),
    ]);
    for changed in changes {
        let rejected = handle.submit(request("key", changed.clone())).unwrap_err();
        assert!(matches!(rejected.reason, SessionError::Conflict { work } if work == receipt.work));
        assert_eq!(rejected.request.blocks, changed);
    }
    let fresh = vec![block("busy")];
    let rejected = handle.submit(request("new", fresh.clone())).unwrap_err();
    assert!(matches!(rejected.reason, SessionError::Busy { work } if work == receipt.work));
    assert_eq!(rejected.request.blocks, fresh);
    let touched = Arc::new(AtomicUsize::new(0));
    let captured = touched.clone();
    assert!(matches!(
        session.with_context(move |_| captured.fetch_add(1, Ordering::SeqCst)),
        Err(SessionError::Busy { .. })
    ));
    assert_eq!(touched.load(Ordering::SeqCst), 0);
    assert!(matches!(
        session.forget(&receipt.work),
        Err(SessionError::NotFinished { .. })
    ));
    assert!(matches!(
        session.take_uncommitted_tool_batch(&receipt.work),
        Err(SessionError::NotFinished { .. })
    ));
    release.add_permits(1);
    finished(&handle, &receipt.work).await;
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(&context(&mut session)[..2], &original);
}

#[tokio::test]
async fn replay_survives_deleted_input_capacity_and_closed_session() {
    let gateway = Gateway::new(vec![final_output("first")]);
    let mut session = simple_session(Context::new(), 1, gateway.clone());
    let handle = session.handle();
    let original = vec![block("U1")];
    let receipt = handle.submit(request("key", original.clone())).unwrap();
    let old_result = finished(&handle, &receipt.work).await;
    let summary = block("summary");
    session
        .with_context(|c| {
            let len = c.blocks().len();
            c.edit().replace(0..len, [summary.clone()]).commit()
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        handle.submit(request("key", original.clone())).unwrap(),
        receipt
    );
    let fresh = vec![block("capacity")];
    let rejection = handle.submit(request("new", fresh.clone())).unwrap_err();
    assert!(matches!(
        rejection.reason,
        SessionError::Capacity { capacity: 1 }
    ));
    assert_eq!(rejection.request.blocks, fresh);
    session.shutdown().await;
    assert_eq!(
        handle.submit(request("key", original.clone())).unwrap(),
        receipt
    );
    let changed = vec![block("changed")];
    let rejected = handle.submit(request("key", changed.clone())).unwrap_err();
    assert!(matches!(rejected.reason, SessionError::Conflict { .. }));
    assert_eq!(rejected.request.blocks, changed);
    let rejected = handle.submit(request("closed", original)).unwrap_err();
    assert!(matches!(rejected.reason, SessionError::Closed));
    assert_eq!(context(&mut session), vec![summary]);
    completed(&old_result, "first");
    assert_eq!(gateway.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn empty_submission_replay_and_forget_keep_material_and_result_independent() {
    let gateway = Gateway::new(vec![final_output("first"), final_output("second")]);
    let mut session = simple_session(Context::new(), 1, gateway.clone());
    let handle = session.handle();
    let receipt = handle.submit(request("key", vec![])).unwrap();
    let old = finished(&handle, &receipt.work).await;
    assert!(gateway.requests.lock().unwrap()[0].frame.blocks.is_empty());
    session
        .with_context(|c| c.edit().append([block("edited")]).commit())
        .unwrap()
        .unwrap();
    let before = context(&mut session);
    assert_eq!(handle.submit(request("key", vec![])).unwrap(), receipt);
    let empty_parts = ContextBlock::new(
        new_block_id(),
        BlockContent::Message {
            role: causa::kernel::Role::Assistant,
            parts: vec![],
        },
        BlockMeta::default(),
    );
    assert!(matches!(
        handle
            .submit(request("key", vec![empty_parts]))
            .unwrap_err()
            .reason,
        SessionError::Conflict { .. }
    ));
    session.forget(&receipt.work).unwrap();
    assert_eq!(context(&mut session), before);
    assert!(matches!(
        handle.observe(&receipt.work),
        Err(SessionError::NotFound { .. })
    ));
    assert!(matches!(
        handle.cancel(&receipt.work),
        Err(SessionError::NotFound { .. })
    ));
    let next = handle.submit(request("key", vec![])).unwrap();
    assert_ne!(next.work, receipt.work);
    finished(&handle, &next.work).await;
    completed(&old, "first");
    assert_eq!(gateway.requests.lock().unwrap().len(), 2);
    assert_eq!(gateway.requests.lock().unwrap()[1].frame.blocks, before);
}

#[tokio::test]
async fn ordinary_closure_error_preserves_committed_edit() {
    let mut session = simple_session(Context::new(), 1, Gateway::new(vec![]));
    let added = block("retained");
    let outcome: Result<Result<(), &'static str>, SessionError> = session.with_context(|c| {
        c.edit().append([added.clone()]).commit().unwrap();
        Err("after commit")
    });
    assert_eq!(outcome.unwrap(), Err("after commit"));
    assert_eq!(context(&mut session), vec![added]);
}

#[tokio::test]
async fn registered_waiter_keeps_original_terminal_after_forget_and_key_reuse() {
    use futures_util::poll;
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let model = runner(
        Gateway::new(vec![final_output("first"), final_output("second")]),
        vec![],
    );
    let mut session = Session::new(
        "conversation".into(),
        Context::new(),
        {
            let entered = entered.clone();
            let release = release.clone();
            move |_, _| {
                let entered = entered.clone();
                let release = release.clone();
                let model = model.clone();
                async move {
                    entered.notify_one();
                    let _permit = release.acquire().await.unwrap();
                    Ok((model, TurnRunOptions::new(ModelRef::new("fixture"))))
                }
            }
        },
        config(1),
    );
    let handle = session.handle();
    let original = handle.submit(request("key", vec![])).unwrap();
    tokio::time::timeout(BOUND, entered.notified())
        .await
        .unwrap();
    let mut registered = Box::pin(handle.wait(&original.work, BOUND));
    assert!(poll!(&mut registered).is_pending());
    release.add_permits(1);
    let result = finished(&handle, &original.work).await;
    session.forget(&original.work).unwrap();
    let next = handle.submit(request("key", vec![])).unwrap();
    assert_ne!(next.work, original.work);
    let observation = registered.await.unwrap();
    assert_eq!(observation.work, original.work);
    let WorkState::Finished(old) = observation.state else {
        panic!("registered waiter lost terminal")
    };
    assert!(Arc::ptr_eq(&old, &result));
    completed(&old, "first");
    assert!(matches!(
        handle.wait(&original.work, BOUND).await,
        Err(SessionError::NotFound { .. })
    ));
    release.add_permits(1);
    completed(&*finished(&handle, &next.work).await, "second");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_edit_and_concurrent_submission_share_one_material_boundary() {
    let gateway = Gateway::new(vec![final_output("done")]);
    let mut session = simple_session(Context::new(), 1, gateway.clone());
    let handle = session.handle();
    let submitted = block("submitted");
    let edited = block("owner edit");
    let (start_tx, start_rx) = std::sync::mpsc::sync_channel(1);
    let (attempt_tx, attempt_rx) = std::sync::mpsc::sync_channel(1);
    let input = submitted.clone();
    let runtime = tokio::runtime::Handle::current();
    let thread = std::thread::spawn(move || {
        start_rx.recv_timeout(BOUND).unwrap();
        attempt_tx.send(()).unwrap();
        let _runtime = runtime.enter();
        handle.submit(request("key", vec![input])).unwrap()
    });
    session
        .with_context(|c| {
            start_tx.send(()).unwrap();
            attempt_rx.recv_timeout(BOUND).unwrap();
            c.edit().append([edited.clone()]).commit().unwrap();
        })
        .unwrap();
    let receipt = thread.join().unwrap();
    let handle = session.handle();
    finished(&handle, &receipt.work).await;
    assert_eq!(&context(&mut session)[..2], &[edited, submitted]);
    assert_eq!(gateway.requests.lock().unwrap()[0].frame.blocks.len(), 2);
}

#[tokio::test]
async fn deleted_input_replays_while_next_work_is_busy_and_new_key_can_reintroduce_it() {
    let gateway = Gateway::new(vec![
        final_output("first"),
        final_output("second"),
        final_output("third"),
    ]);
    let model = runner(gateway.clone(), vec![]);
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let mut session = Session::new(
        "conversation".into(),
        Context::new(),
        {
            let calls = calls.clone();
            let entered = entered.clone();
            let release = release.clone();
            move |_, _| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                let model = model.clone();
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    if call == 1 {
                        entered.notify_one();
                        let _permit = release.acquire().await.unwrap();
                    }
                    Ok((model, TurnRunOptions::new(ModelRef::new("fixture"))))
                }
            }
        },
        config(3),
    );
    let handle = session.handle();
    let original = vec![block("U1")];
    let first = handle.submit(request("k1", original.clone())).unwrap();
    finished(&handle, &first.work).await;
    let summary = block("S");
    session
        .with_context(|c| {
            let len = c.blocks().len();
            c.edit().replace(0..len, [summary.clone()]).commit()
        })
        .unwrap()
        .unwrap();
    assert_eq!(context(&mut session), vec![summary.clone()]);
    let second = handle.submit(request("k2", vec![])).unwrap();
    tokio::time::timeout(BOUND, entered.notified())
        .await
        .unwrap();
    assert_eq!(
        handle.submit(request("k1", original.clone())).unwrap(),
        first
    );
    let changed = vec![block("different")];
    assert!(
        matches!(handle.submit(request("k1", changed)).unwrap_err().reason, SessionError::Conflict { work } if work == first.work)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    release.add_permits(1);
    finished(&handle, &second.work).await;
    let before = context(&mut session);
    assert!(!before.contains(&original[0]));
    let third = handle.submit(request("new key", original.clone())).unwrap();
    assert_ne!(third.work, first.work);
    completed(&*finished(&handle, &third.work).await, "third");
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let requests = gateway.requests.lock().unwrap();
    assert!(!requests[1].frame.blocks.contains(&original[0]));
    assert_eq!(&requests[2].frame.blocks[..before.len()], &before);
    assert_eq!(requests[2].frame.blocks.last(), original.first());
}

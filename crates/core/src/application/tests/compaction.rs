use super::support::*;
use causa::kernel::ModelGateway;
use std::sync::atomic::Ordering;

fn scripted(behavior: SummaryBehavior) -> Arc<CompactionGateway> {
    CompactionGateway::new(behavior)
}

fn as_gateway(gateway: &Arc<CompactionGateway>) -> Option<Arc<dyn ModelGateway>> {
    Some(gateway.clone() as Arc<dyn ModelGateway>)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn auto_compaction_replaces_the_context_and_saves_one_record() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text(
        "Goal: finish the fixture task. Next steps: report the result.".into(),
    ));
    host.set_test_gateway(as_gateway(&gateway));
    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(Some(1_000)),
            Context::new(),
        ))
        .await
        .unwrap();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "first".into(),
            blocks: vec![text_block(1_200), text_block(1_200), text_block(1_200)],
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gateway.main_calls.load(Ordering::SeqCst), 1);
    let usage = session.context_usage().unwrap();
    assert_eq!(usage.context_window, Some(1_000));
    assert!(usage.auto_compact);
    assert_eq!(usage.compact_threshold, 70);
    assert!(
        usage.estimate_tokens < 1_000,
        "the compacted material is far below the window"
    );

    let id = session.target().conversation_id().as_str().to_owned();
    let record = saved_record(store.path(), &id);
    let history = record["history"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["trigger"], "auto");
    let blocks = record["context"]["blocks"].as_array().unwrap();
    // The replacement summary followed by the model's final answer.
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["meta"]["source"], "tars:compaction");
    assert_eq!(blocks[0]["content"]["value"]["role"], "user");
    assert_eq!(blocks[1]["content"]["value"]["role"], "assistant");

    let record_id_value = history[0]["record_id"]
        .as_str()
        .or_else(|| history[0]["id"].as_str())
        .unwrap();
    assert!(
        history_dir(store.path(), &id)
            .join(format!("{record_id_value}.json"))
            .exists()
    );
    let document = session.read_history_record(record_id_value).unwrap();
    assert_eq!(
        document
            .replacements
            .iter()
            .flat_map(|r| &r.originals)
            .count(),
        3
    );
    assert_eq!(document.trigger, crate::compact::HistoryTrigger::Auto);
    assert_eq!(session.history().len(), 1);

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn abandoned_summary_keeps_the_context_and_leaves_no_history() {
    for behavior in [SummaryBehavior::Fail, SummaryBehavior::Empty] {
        let store = tempfile::tempdir().unwrap();
        let host = SessionHost::new(Some(store.path().into()));
        let gateway = scripted(behavior);
        host.set_test_gateway(as_gateway(&gateway));
        let session = host
            .create(PreparedCreate::persistent(
                compacting_definition(Some(1_000)),
                Context::new(),
            ))
            .await
            .unwrap();
        let mut progress = session.subscribe_progress();
        let receipt = session
            .submit(SubmitRequest {
                request_key: "first".into(),
                blocks: vec![text_block(1_200), text_block(1_200), text_block(1_200)],
            })
            .unwrap();
        finished(&session, &receipt.work).await;
        session.wait_saved().await.unwrap();
        assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 1);
        let id = session.target().conversation_id().as_str().to_owned();
        let record = saved_record(store.path(), &id);
        assert!(
            record
                .get("history")
                .and_then(|history| history.as_array())
                .is_none_or(|history| history.is_empty())
        );
        assert_eq!(
            record["context"]["blocks"].as_array().unwrap().len(),
            4,
            "three submitted blocks and the final answer"
        );
        let mut abandoned = 0;
        while let Ok(event) = progress.try_recv() {
            if let crate::session::ProgressUpdate::Compaction(
                crate::compact::CompactionProgress::Abandoned { .. },
            ) = &event.update
            {
                abandoned += 1;
            }
        }
        assert!(abandoned >= 1, "abandonment is reported through progress");
        let history = history_dir(store.path(), &id);
        let files = std::fs::read_dir(&history)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(files, 0);
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overflow_refusal_forces_one_compaction_with_auto_off() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text(
        "Goal: continue. Next steps: finish the fixture work.".into(),
    ));
    gateway
        .main_overflow
        .store(true, std::sync::atomic::Ordering::SeqCst);
    host.set_test_gateway(as_gateway(&gateway));
    let session = host
        .create(PreparedCreate::persistent(
            // A window large enough that the estimate alone never triggers.
            compacting_definition(Some(1_000_000)),
            Context::new(),
        ))
        .await
        .unwrap();
    session.set_auto_compact(false).unwrap();
    session.wait_saved().await.unwrap();
    let first = session
        .submit(SubmitRequest {
            request_key: "overflow".into(),
            blocks: vec![text_block(200)],
        })
        .unwrap();
    finished(&session, &first.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 0);

    let second = session
        .submit(SubmitRequest {
            request_key: "after-overflow".into(),
            blocks: vec![text_block(200)],
        })
        .unwrap();
    finished(&session, &second.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 1);
    let id = session.target().conversation_id().as_str().to_owned();
    let record = saved_record(store.path(), &id);
    let history = record["history"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["trigger"], "passive");
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_compaction_feeds_the_old_summary_into_the_next_request() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text(
        "Goal: continue the fixture work. Next steps: finish.".into(),
    ));
    host.set_test_gateway(as_gateway(&gateway));
    let mut definition = compacting_definition(Some(2_000));
    definition.compact_threshold = 55;
    let session = host
        .create(PreparedCreate::persistent(definition, Context::new()))
        .await
        .unwrap();
    for key in ["one", "two"] {
        let receipt = session
            .submit(SubmitRequest {
                request_key: key.into(),
                blocks: vec![text_block(1_200), text_block(1_200), text_block(1_200)],
            })
            .unwrap();
        finished(&session, &receipt.work).await;
        session.wait_saved().await.unwrap();
    }
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 2);
    let (request_count, carries_previous_summary) = {
        let inputs = gateway.summary_inputs.lock().unwrap();
        (
            inputs.len(),
            inputs[1].iter().any(|block| {
                block.meta().source.as_deref() == Some(crate::compact::summary::SUMMARY_SOURCE)
            }),
        )
    };
    assert_eq!(request_count, 2);
    assert!(
        carries_previous_summary,
        "the previous summary is part of the second summary request"
    );
    let id = session.target().conversation_id().as_str().to_owned();
    let record = saved_record(store.path(), &id);
    let history = record["history"].as_array().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(
        record["context"]["blocks"].as_array().unwrap().len(),
        2,
        "one summary and the final answer"
    );
    let second_id = history[1]["record_id"]
        .as_str()
        .or_else(|| history[1]["id"].as_str())
        .unwrap();
    let document = session.read_history_record(second_id).unwrap();
    assert_eq!(
        document
            .replacements
            .iter()
            .flat_map(|r| &r.originals)
            .count(),
        5,
        "the previous summary, the first answer and the three new blocks"
    );
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn over_limit_material_is_trimmed_for_the_summary_but_kept_in_history() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text(
        "Goal: continue. Next steps: finish the fixture work.".into(),
    ));
    host.set_test_gateway(as_gateway(&gateway));
    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(Some(300)),
            Context::new(),
        ))
        .await
        .unwrap();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "huge".into(),
            blocks: vec![text_block(1_200), text_block(1_200), text_block(1_200)],
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    let summary_input_blocks = {
        let inputs = gateway.summary_inputs.lock().unwrap();
        (inputs.len(), inputs[0].len())
    };
    assert_eq!(summary_input_blocks.0, 1);
    assert!(
        summary_input_blocks.1 <= 2,
        "the summary request drops material until it fits the window"
    );
    let id = session.target().conversation_id().as_str().to_owned();
    let record = saved_record(store.path(), &id);
    let history = record["history"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    let record_id_value = history[0]["record_id"]
        .as_str()
        .or_else(|| history[0]["id"].as_str())
        .unwrap();
    let document = session.read_history_record(record_id_value).unwrap();
    assert_eq!(
        document
            .replacements
            .iter()
            .flat_map(|r| &r.originals)
            .count(),
        3,
        "the history record keeps the complete old material"
    );
    assert!(document.estimate_before >= document.estimate_after);
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_the_summary_leaves_the_context_and_history_untouched() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Block);
    host.set_test_gateway(as_gateway(&gateway));
    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(Some(1_000)),
            Context::new(),
        ))
        .await
        .unwrap();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "blocked".into(),
            blocks: vec![text_block(1_200), text_block(1_200), text_block(1_200)],
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), gateway.entered.notified())
        .await
        .unwrap();
    session.cancel(&receipt.work).unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 1);
    let id = session.target().conversation_id().as_str().to_owned();
    let record = saved_record(store.path(), &id);
    assert!(
        record
            .get("history")
            .and_then(|history| history.as_array())
            .is_none_or(|history| history.is_empty())
    );
    assert_eq!(
        record["context"]["blocks"].as_array().unwrap().len(),
        3,
        "cancellation returns the untouched material"
    );
    assert_eq!(
        std::fs::read_dir(history_dir(store.path(), &id))
            .map(|entries| entries.count())
            .unwrap_or(0),
        0
    );
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unwritable_history_abandons_the_compaction_and_keeps_the_round() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text(
        "summary that cannot be stored".into(),
    ));
    host.set_test_gateway(as_gateway(&gateway));
    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(Some(1_000)),
            Context::new(),
        ))
        .await
        .unwrap();
    let id = session.target().conversation_id().as_str().to_owned();
    // Block the history directory path with a file so create_dir_all fails.
    std::fs::write(history_dir(store.path(), &id), b"not a directory").unwrap();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "first".into(),
            blocks: vec![text_block(1_200), text_block(1_200), text_block(1_200)],
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    let record = saved_record(store.path(), &id);
    assert!(
        record
            .get("history")
            .and_then(|history| history.as_array())
            .is_none_or(|history| history.is_empty())
    );
    assert_eq!(
        record["context"]["blocks"].as_array().unwrap().len(),
        4,
        "the round still went out with the original material"
    );
    std::fs::remove_file(history_dir(store.path(), &id)).unwrap();
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reopening_restores_settings_and_cleans_unreferenced_records() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text("first summary body".into()));
    host.set_test_gateway(as_gateway(&gateway));
    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(Some(1_000)),
            Context::new(),
        ))
        .await
        .unwrap();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "first".into(),
            blocks: vec![text_block(1_200), text_block(1_200), text_block(1_200)],
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    let id = session.target().conversation_id().as_str().to_owned();
    let record = saved_record(store.path(), &id);
    let record_id_value = record["history"][0]["record_id"]
        .as_str()
        .or_else(|| record["history"][0]["id"].as_str())
        .unwrap()
        .to_owned();
    let stray = history_dir(store.path(), &id).join("stray.json");
    std::fs::write(&stray, b"{}").unwrap();
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    let reopened = host
        .open(session.target().conversation_id().clone())
        .await
        .unwrap();
    assert!(!stray.exists(), "unreferenced records are removed on open");
    assert!(
        history_dir(store.path(), &id)
            .join(format!("{record_id_value}.json"))
            .exists()
    );
    let settings = reopened.compaction_settings().unwrap();
    assert!(settings.auto_compact);
    assert_eq!(settings.compact_threshold, 70);
    assert_eq!(settings.context_window, Some(1_000));
    assert_eq!(reopened.history().len(), 1);
    // Opening and reading the current materials read no record file; the
    // expansion reads the one record it reaches.
    reopened.read_materials().unwrap();
    assert_eq!(reopened.record_reads(), 0);
    let expanded = reopened.expanded_conversation(None).unwrap();
    assert!(expanded.unreadable_records.is_empty());
    assert_eq!(reopened.record_reads(), 1);
    let summarized = expanded
        .blocks
        .iter()
        .filter(|block| {
            block.status == crate::compact::BlockStatus::Summarized(record_id_value.clone())
        })
        .count();
    assert_eq!(summarized, 3);
    assert_eq!(
        reopened
            .read_history_record(&record_id_value)
            .unwrap()
            .replacements
            .iter()
            .flat_map(|r| &r.originals)
            .count(),
        3
    );
    // A deleted referenced record is reported on read, not at open.
    std::fs::remove_file(history_dir(store.path(), &id).join(format!("{record_id_value}.json")))
        .unwrap();
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
    let reopened_again = host
        .open(session.target().conversation_id().clone())
        .await
        .unwrap();
    assert_eq!(reopened_again.history().len(), 1);
    assert!(
        reopened_again
            .read_history_record(&record_id_value)
            .is_err()
    );
    host.close(&reopened_again.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn running_compaction_settings_are_saved_and_reset_to_the_definition() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text("summary".into()));
    host.set_test_gateway(as_gateway(&gateway));
    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(Some(1_000_000)),
            Context::new(),
        ))
        .await
        .unwrap();
    let id = session.target().conversation_id().as_str().to_owned();
    let settings = session.compaction_settings().unwrap();
    assert!(settings.auto_compact);
    assert_eq!(settings.compact_threshold, 70);

    session.set_auto_compact(false).unwrap();
    session.wait_saved().await.unwrap();
    assert_eq!(saved_record(store.path(), &id)["auto_compact"], false);
    session.set_compact_threshold(55).unwrap();
    session.wait_saved().await.unwrap();
    assert_eq!(saved_record(store.path(), &id)["compact_threshold"], 55);

    // Equal to the definition's initial values again: the record omits them.
    session.set_auto_compact(true).unwrap();
    session.set_compact_threshold(70).unwrap();
    session.wait_saved().await.unwrap();
    let record = saved_record(store.path(), &id);
    assert!(record.get("auto_compact").is_none());
    assert!(record.get("compact_threshold").is_none());

    assert!(matches!(
        session.set_compact_threshold(5),
        Err(ApplicationError::Definition(
            crate::definition::DefinitionError::InvalidCompactThreshold
        ))
    ));
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
    let reopened = host
        .open(session.target().conversation_id().clone())
        .await
        .unwrap();
    let settings = reopened.compaction_settings().unwrap();
    assert!(settings.auto_compact);
    assert_eq!(settings.compact_threshold, 70);
    assert_eq!(reopened.definition().compact_threshold, 70);
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unknown_window_disables_auto_compaction_but_manual_compaction_works() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text(
        "Goal: finish. Next steps: report the result.".into(),
    ));
    host.set_test_gateway(as_gateway(&gateway));
    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(None),
            Context::new(),
        ))
        .await
        .unwrap();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "first".into(),
            blocks: vec![text_block(1_200), text_block(1_200), text_block(1_200)],
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 0);

    let outcome = session.compact().await.unwrap();
    assert_eq!(outcome.trigger, crate::compact::HistoryTrigger::Manual);
    assert!(outcome.estimate_after < outcome.estimate_before);
    session.wait_saved().await.unwrap();
    let id = session.target().conversation_id().as_str().to_owned();
    let record = saved_record(store.path(), &id);
    let history = record["history"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["trigger"], "manual");
    assert_eq!(
        record["context"]["blocks"].as_array().unwrap().len(),
        1,
        "manual compaction replaces the whole material"
    );
    let record_id_value = history[0]["record_id"]
        .as_str()
        .or_else(|| history[0]["id"].as_str())
        .unwrap();
    assert_eq!(
        session
            .read_history_record(record_id_value)
            .unwrap()
            .replacements
            .iter()
            .flat_map(|r| &r.originals)
            .count(),
        4
    );
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_output_reserve_triggers_compaction_below_the_threshold() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text("reserve summary".into()));
    host.set_test_gateway(as_gateway(&gateway));
    let mut definition = compacting_definition(Some(1_000));
    definition.compact_threshold = 95;
    definition.max_tokens = Some(900);
    let session = host
        .create(PreparedCreate::persistent(definition, Context::new()))
        .await
        .unwrap();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "reserve".into(),
            blocks: vec![text_block(600)],
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 1);
    let id = session.target().conversation_id().as_str().to_owned();
    assert_eq!(
        saved_record(store.path(), &id)["history"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_abandoned_summary_does_not_become_the_usage_base() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    // The summary reports 400 input tokens; ordinary rounds report none.
    let gateway = scripted(SummaryBehavior::Text("z".repeat(5_000)));
    gateway.main_without_usage.store(true, Ordering::SeqCst);
    host.set_test_gateway(as_gateway(&gateway));
    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(Some(1_000)),
            Context::new(),
        ))
        .await
        .unwrap();
    for (key, blocks) in [
        ("small", vec![text_block(30)]),
        (
            "large",
            vec![text_block(1_200), text_block(1_200), text_block(1_200)],
        ),
    ] {
        let receipt = session
            .submit(SubmitRequest {
                request_key: key.into(),
                blocks,
            })
            .unwrap();
        finished(&session, &receipt.work).await;
        session.wait_saved().await.unwrap();
    }
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 1);
    let bytes = session
        .read_materials()
        .map(|snapshot| crate::compact::estimate::context_bytes(&snapshot.context))
        .unwrap();
    // No ordinary round reported usage, so the estimate stays byte-based;
    // the summary request's own usage is not the conversation's size.
    assert_eq!(
        session.context_usage().unwrap().estimate_tokens,
        crate::compact::estimate::estimate_bytes(bytes)
    );
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_summary_that_does_not_shrink_is_abandoned() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text("z".repeat(5_000)));
    host.set_test_gateway(as_gateway(&gateway));
    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(Some(1_000)),
            Context::new(),
        ))
        .await
        .unwrap();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "no-shrink".into(),
            blocks: vec![text_block(1_200), text_block(1_200), text_block(1_200)],
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 1);
    let id = session.target().conversation_id().as_str().to_owned();
    let record = saved_record(store.path(), &id);
    assert!(
        record
            .get("history")
            .and_then(|history| history.as_array())
            .is_none_or(|history| history.is_empty())
    );
    assert_eq!(
        record["context"]["blocks"].as_array().unwrap().len(),
        4,
        "the original material and the final answer"
    );
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn manual_compaction_refuses_unpaired_material() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text("unused".into()));
    host.set_test_gateway(as_gateway(&gateway));
    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(None),
            Context::new(),
        ))
        .await
        .unwrap();
    let call = ContextBlock::new(
        causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
        causa::kernel::BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "alpha",
            serde_json::json!({}),
        )),
        Default::default(),
    );
    let version = session.read_materials().unwrap().version;
    session.edit(version, vec![call]).unwrap();
    session.wait_saved().await.unwrap();
    let error = session.compact().await.unwrap_err();
    assert!(
        matches!(error, ApplicationError::Repair(_)),
        "unpaired material is refused: {error}"
    );
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_sessions_have_no_compaction() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text("unused".into()));
    host.set_test_gateway(as_gateway(&gateway));
    let session = host
        .create(PreparedCreate::transient(
            compacting_definition(Some(1_000)),
            Context::new(),
        ))
        .await
        .unwrap();
    assert!(session.compaction_settings().is_none());
    let id = session.target().conversation_id().as_str().to_owned();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "transient".into(),
            blocks: vec![text_block(1_200), text_block(1_200), text_block(1_200)],
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    // The managed entry clears its active work shortly after the terminal
    // result; a manual mutation before that point reports `Saving`, so wait for
    // the cleared entry instead of depending on scheduling.
    until(|| session.status().active_work.is_none()).await;
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 0);
    assert!(!history_dir(store.path(), &id).exists());
    assert!(matches!(
        session.compact().await,
        Err(ApplicationError::CompactionUnavailable)
    ));
}

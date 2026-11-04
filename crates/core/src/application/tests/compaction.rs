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
            compacting_definition(Some(FIXTURE_WINDOW)),
            earlier_conversation(),
        ))
        .await
        .unwrap();
    let request = short_request();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "first".into(),
            blocks: request.clone(),
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gateway.main_calls.load(Ordering::SeqCst), 1);
    let usage = session.context_usage().unwrap();
    assert_eq!(usage.context_window, Some(FIXTURE_WINDOW));
    assert!(usage.auto_compact);
    assert_eq!(usage.compact_threshold, 70);
    assert!(
        usage.estimate_tokens < FIXTURE_WINDOW,
        "the compacted material is far below the window: {}",
        usage.estimate_tokens
    );

    let id = session.target().conversation_id().as_str().to_owned();
    let record = saved_record(store.path(), &id);
    let history = record["history"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["trigger"], "auto");
    let blocks = record["context"]["blocks"].as_array().unwrap();
    // The folded tool declaration and the brief, then the work's request,
    // kept with its own id, and the model's final answer.
    assert_eq!(blocks.len(), 4, "{blocks:?}");
    assert_eq!(blocks[0]["content"]["shape"], "tool_change");
    assert_eq!(blocks[1]["meta"]["source"], "tars:compaction");
    assert_eq!(blocks[1]["content"]["value"]["role"], "user");
    assert_eq!(blocks[2]["id"], request[0].id().0.to_string());
    assert_eq!(blocks[3]["content"]["value"]["role"], "assistant");

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
        6,
        "the earlier four blocks, then the request and the declaration after it"
    );
    // The request was put back, so the index counts five removed blocks.
    assert_eq!(history[0]["removed_block_count"], 5);
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
                compacting_definition(Some(FIXTURE_WINDOW)),
                earlier_conversation(),
            ))
            .await
            .unwrap();
        let mut progress = session.subscribe_progress();
        let receipt = session
            .submit(SubmitRequest {
                request_key: "first".into(),
                blocks: short_request(),
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
            7,
            "the four earlier blocks, the request, its tool declaration and the final answer"
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
            // Large enough that a brief in its place saves more than the
            // restated tool declaration costs.
            blocks: vec![text_block(3_000)],
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
async fn test_be11_a_second_compaction_keeps_the_first_brief_and_summarizes_after_it() {
    use crate::compact::BlockStatus;
    use crate::compact::summary::SUMMARY_SOURCE;
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text(
        "Goal: continue the fixture work. Next steps: finish.".into(),
    ));
    // Byte-based estimates, so the sizes below decide each trigger.
    gateway.main_without_usage.store(true, Ordering::SeqCst);
    host.set_test_gateway(as_gateway(&gateway));
    // A 1,200-token trigger line. The first work compacts the seeded
    // conversation and keeps its own 500-token input; the second work's
    // 200-token input brings that input back over the line.
    let mut definition = compacting_definition(Some(3_000));
    definition.compact_threshold = 40;
    let session = host
        .create(PreparedCreate::persistent(
            definition,
            earlier_conversation(),
        ))
        .await
        .unwrap();
    let seeded: Vec<BlockId> = session
        .read_materials()
        .unwrap()
        .context
        .blocks()
        .iter()
        .map(ContextBlock::id)
        .collect();
    let first = vec![text_block(1_500)];
    let first_id = first[0].id();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "one".into(),
            blocks: first,
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 1);
    let after_first = session.read_materials().unwrap().context;
    let after_first = after_first.blocks();
    // [tool declaration, S1, first request, first answer]
    assert_eq!(after_first.len(), 4);
    let brief_one = after_first[1].clone();
    assert_eq!(brief_one.meta().source.as_deref(), Some(SUMMARY_SOURCE));
    assert_eq!(after_first[2].id(), first_id);
    let first_answer = after_first[3].id();
    let first_declaration = after_first[0].id();

    let second = vec![text_block(600)];
    let second_id = second[0].id();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "two".into(),
            blocks: second,
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 2);

    // The second request is the normal material followed by the
    // instruction: the first brief comes along as background, unchanged.
    let request: Vec<BlockId> = {
        let inputs = gateway.summary_inputs.lock().unwrap();
        assert_eq!(inputs.len(), 2);
        assert!(
            inputs[1]
                .iter()
                .any(|block| block.id() == brief_one.id() && *block == brief_one)
        );
        inputs[1].iter().map(ContextBlock::id).collect()
    };
    assert_eq!(
        request[..request.len() - 1],
        [
            first_declaration,
            brief_one.id(),
            first_id,
            first_answer,
            second_id
        ]
    );

    let context = session.read_materials().unwrap().context;
    let blocks = context.blocks();
    assert_eq!(
        blocks.len(),
        5,
        "[declaration, S1, S2, second request, answer]"
    );
    assert_eq!(blocks[0].id(), first_declaration);
    assert_eq!(blocks[1], brief_one, "the first brief is never rewritten");
    assert_eq!(blocks[2].meta().source.as_deref(), Some(SUMMARY_SOURCE));
    assert_ne!(blocks[2].id(), brief_one.id());
    assert_eq!(blocks[3].id(), second_id);

    let history = session.history();
    assert_eq!(history.len(), 2);
    let second_record = session.read_history_record(&history[1].record_id).unwrap();
    let originals: Vec<BlockId> = second_record
        .replacements
        .iter()
        .flat_map(|r| &r.originals)
        .map(ContextBlock::id)
        .collect();
    assert_eq!(
        originals,
        [first_id, first_answer, second_id],
        "the first work and the protected request it put back; never the head"
    );
    assert_eq!(history[1].removed_block_count, 2);

    // Expansion puts every original back in order, each marked by the
    // record that removed it.
    let expanded = session.expanded_conversation(None).unwrap();
    assert!(expanded.unreadable_records.is_empty());
    let one = BlockStatus::Summarized(history[0].record_id.clone());
    let two = BlockStatus::Summarized(history[1].record_id.clone());
    let shown: Vec<(BlockId, BlockStatus)> = expanded
        .blocks
        .iter()
        .map(|block| (block.block.id(), block.status.clone()))
        .collect();
    let mut expected: Vec<(BlockId, BlockStatus)> =
        seeded.iter().map(|id| (*id, one.clone())).collect();
    expected.push((first_id, two.clone()));
    assert_eq!(&shown[..5], &expected[..]);
    // The first work's tool declaration sat after its request.
    assert_eq!(shown[5].1, one);
    assert!(matches!(
        expanded.blocks[5].block.content(),
        causa::kernel::BlockContent::ToolChange(_)
    ));
    assert_eq!(shown[6], (first_answer, two));
    assert_eq!(shown[7], (second_id, BlockStatus::Original));
    assert_eq!(shown[8], (blocks[4].id(), BlockStatus::Original));
    assert_eq!(shown.len(), 9);
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
    // About 3,000 tokens of earlier material, over the 1,400-token limit of a
    // brief request (70% of the window).
    let earlier = vec![
        text_block(3_000),
        text_block(3_000),
        text_block(3_000),
        ContextBlock::text(
            BlockId::new(uuid::Uuid::now_v7()),
            causa::kernel::Role::Assistant,
            "earlier answer",
        ),
    ];
    let earlier_ids: Vec<BlockId> = earlier.iter().map(ContextBlock::id).collect();
    let request = short_request();
    let request_id = request[0].id();
    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(Some(FIXTURE_WINDOW)),
            Context::from_blocks(earlier).unwrap(),
        ))
        .await
        .unwrap();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "huge".into(),
            blocks: request,
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    let input = {
        let inputs = gateway.summary_inputs.lock().unwrap();
        assert_eq!(inputs.len(), 1);
        inputs[0].clone()
    };
    let (material, _instruction) = input.split_at(input.len() - 1);
    assert!(
        crate::compact::segment::blocks_estimate(material) <= FIXTURE_WINDOW * 70 / 100,
        "the summary request drops material until it fits"
    );
    let sent: Vec<BlockId> = material.iter().map(ContextBlock::id).collect();
    for dropped in &earlier_ids[..3] {
        assert!(!sent.contains(dropped), "the earliest messages are dropped");
    }
    assert!(
        sent.contains(&request_id),
        "the protected request is always sent"
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
    let originals: Vec<BlockId> = document
        .replacements
        .iter()
        .flat_map(|r| &r.originals)
        .map(ContextBlock::id)
        .collect();
    assert_eq!(
        originals.len(),
        6,
        "the earlier material, the request and its tool declaration"
    );
    for kept in &earlier_ids {
        assert!(
            originals.contains(kept),
            "the history record keeps the dropped material"
        );
    }
    assert!(document.estimate_before > document.estimate_after);
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
            compacting_definition(Some(FIXTURE_WINDOW)),
            earlier_conversation(),
        ))
        .await
        .unwrap();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "blocked".into(),
            blocks: short_request(),
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
        6,
        "cancellation returns the untouched material: the earlier blocks, the request and its tool declaration"
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
            compacting_definition(Some(FIXTURE_WINDOW)),
            earlier_conversation(),
        ))
        .await
        .unwrap();
    let id = session.target().conversation_id().as_str().to_owned();
    // Block the history directory path with a file so create_dir_all fails.
    std::fs::write(history_dir(store.path(), &id), b"not a directory").unwrap();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "first".into(),
            blocks: short_request(),
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
        7,
        "the round still went out with the original material and its final answer"
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
            compacting_definition(Some(FIXTURE_WINDOW)),
            earlier_conversation(),
        ))
        .await
        .unwrap();
    let request = short_request();
    let request_id = request[0].id();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "first".into(),
            blocks: request,
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
    assert_eq!(settings.context_window, Some(FIXTURE_WINDOW));
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
    // The four earlier blocks and the first tool declaration were
    // summarized; the request was protected and still stands as it was.
    assert_eq!(summarized, 5);
    let kept: Vec<_> = expanded
        .blocks
        .iter()
        .filter(|block| block.block.id() == request_id)
        .map(|block| block.status.clone())
        .collect();
    assert_eq!(kept, vec![crate::compact::BlockStatus::Original]);
    assert_eq!(
        reopened
            .read_history_record(&record_id_value)
            .unwrap()
            .replacements
            .iter()
            .flat_map(|r| &r.originals)
            .count(),
        6,
        "the summarized blocks and the protected request it put back"
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
            // About 30,000 estimated tokens, over the 20,000-token tail that
            // applies when the window is unknown.
            Context::from_blocks(vec![
                text_block(30_000),
                text_block(30_000),
                text_block(30_000),
                ContextBlock::text(
                    BlockId::new(uuid::Uuid::now_v7()),
                    causa::kernel::Role::Assistant,
                    "earlier answer",
                ),
            ])
            .unwrap(),
        ))
        .await
        .unwrap();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "first".into(),
            blocks: short_request(),
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
        6,
        "the brief, then the newest material under 20,000 tokens kept verbatim: the third message, the earlier answer, the request, its tool declaration and the answer"
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
        2,
        "the first two earlier messages"
    );
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_output_reserve_triggers_compaction_below_the_threshold() {
    // The trigger line is 2,850 tokens and the request, about 1,900, stays
    // under it, so only the 1,200-token output reserve can cross the
    // 3,000-token window.
    for (max_tokens, expected_summaries) in [(None, 0), (Some(1_200), 1)] {
        let store = tempfile::tempdir().unwrap();
        let host = SessionHost::new(Some(store.path().into()));
        let gateway = scripted(SummaryBehavior::Text("reserve summary".into()));
        host.set_test_gateway(as_gateway(&gateway));
        let mut definition = compacting_definition(Some(3_000));
        definition.compact_threshold = 95;
        definition.max_tokens = max_tokens;
        let session = host
            .create(PreparedCreate::persistent(
                definition,
                earlier_conversation(),
            ))
            .await
            .unwrap();
        let receipt = session
            .submit(SubmitRequest {
                request_key: "reserve".into(),
                blocks: short_request(),
            })
            .unwrap();
        finished(&session, &receipt.work).await;
        session.wait_saved().await.unwrap();
        assert_eq!(
            gateway.summary_calls.load(Ordering::SeqCst),
            expected_summaries,
            "max_tokens {max_tokens:?}"
        );
        let id = session.target().conversation_id().as_str().to_owned();
        assert_eq!(
            saved_record(store.path(), &id)["history"]
                .as_array()
                .map_or(0, Vec::len),
            expected_summaries
        );
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
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
            compacting_definition(Some(FIXTURE_WINDOW)),
            Context::new(),
        ))
        .await
        .unwrap();
    // The first work stays under the 1,400-token line with the `recall`
    // declaration; the second crosses it, and its 5,000-byte summary is larger
    // than the first work it would replace, so it is abandoned.
    for (key, blocks) in [
        ("under-the-line", vec![text_block(2_400)]),
        ("over-the-line", vec![text_block(600)]),
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
    assert!(session.history().is_empty(), "the summary was abandoned");
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
            compacting_definition(Some(FIXTURE_WINDOW)),
            earlier_conversation(),
        ))
        .await
        .unwrap();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "no-shrink".into(),
            blocks: short_request(),
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
        7,
        "the original material, its tool declaration and the final answer"
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
            compacting_definition(Some(FIXTURE_WINDOW)),
            earlier_conversation(),
        ))
        .await
        .unwrap();
    assert!(session.compaction_settings().is_none());
    let id = session.target().conversation_id().as_str().to_owned();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "transient".into(),
            blocks: short_request(),
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    // The managed entry clears its active work shortly after the terminal
    // result; a manual mutation before that point reports `Saving`, so wait for
    // the cleared entry instead of depending on scheduling.
    until(|| session.status().active_work.is_none()).await;
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 0);
    assert!(!history_dir(store.path(), &id).exists());
    assert!(session.artifacts().is_none());
    assert!(matches!(
        session.compact().await,
        Err(ApplicationError::CompactionUnavailable)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be06_saved_context_tool_result_artifact_reference_roundtrip_reads_zero_records() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text("Summary brief text.".into()));
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
            request_key: "k1".into(),
            blocks: vec![text_block(1_200), text_block(1_200)],
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();

    let outcome = session.compact().await.unwrap();
    session.wait_saved().await.unwrap();
    let record_id = outcome.record.record_id;

    let history_doc = session.read_history_record(&record_id).unwrap();
    let orig_block = &history_doc.replacements[0].originals[0];
    let orig_block_id = orig_block.id().0;
    let orig_text = crate::compact::block_to_text(orig_block);

    assert!(session.artifacts().is_some());
    use causa::kernel::ArtifactStore;
    let artifacts = session.artifacts().unwrap();
    let data = artifacts
        .read(&format!("{record_id}/{orig_block_id}"), None)
        .await
        .unwrap();
    assert_eq!(data, orig_text.as_bytes());

    let call_id = causa::kernel::BlockId::new(uuid::Uuid::now_v7());
    let call_block = ContextBlock::new(
        call_id,
        causa::kernel::BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "echo",
            serde_json::json!({}),
        )),
        Default::default(),
    );
    let mut output = causa::kernel::ToolOutput::new(serde_json::Value::String("short note".into()));
    output.artifact = Some(causa::kernel::ArtifactRef::new(
        format!("{record_id}/{orig_block_id}"),
        orig_text.len(),
        causa::kernel::ArtifactKind::FullOutput,
        true,
    ));
    let result_block = ContextBlock::new(
        causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
        causa::kernel::BlockContent::ToolResult(causa::kernel::ToolResultPayload::new(
            call_id,
            causa::kernel::ToolResultStatus::Succeeded,
            output,
        )),
        Default::default(),
    );

    let mut blocks = session.read_materials().unwrap().context.blocks().to_vec();
    blocks.push(call_block);
    blocks.push(result_block.clone());
    let version = session.read_materials().unwrap().version;
    session.edit(version, blocks).unwrap();
    session.wait_saved().await.unwrap();

    let target = session.target().clone();
    host.close(&target, ClosePolicy::Save).await.unwrap();

    // Reopen session: context still carries reference, reads ZERO record files
    let reopened = host.open(target.conversation_id().clone()).await.unwrap();
    assert_eq!(
        reopened.record_reads(),
        0,
        "reopening must read zero record files"
    );
    let materials = reopened.read_materials().unwrap();
    assert_eq!(
        reopened.record_reads(),
        0,
        "reading materials must read zero record files"
    );

    let found_result = materials
        .context
        .blocks()
        .iter()
        .find(|b| b.id() == result_block.id())
        .expect("result block must be present");
    let causa::kernel::BlockContent::ToolResult(res) = found_result.content() else {
        panic!("expected ToolResult");
    };
    let art_ref = res
        .output
        .artifact
        .as_ref()
        .expect("artifact must be preserved");
    assert_eq!(art_ref.id, format!("{record_id}/{orig_block_id}"));
    assert_eq!(art_ref.size_bytes, orig_text.len());
    assert_eq!(art_ref.kind, causa::kernel::ArtifactKind::FullOutput);
    assert!(art_ref.persisted);

    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be06_save_refused_for_unindexed_record_or_foreign_session() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text("Summary.".into()));
    host.set_test_gateway(as_gateway(&gateway));

    // Session 1: compacted, has history record
    let s1 = host
        .create(PreparedCreate::persistent(
            compacting_definition(None),
            Context::new(),
        ))
        .await
        .unwrap();
    let r1 = s1
        .submit(SubmitRequest {
            request_key: "s1-k1".into(),
            blocks: vec![text_block(1_200)],
        })
        .unwrap();
    finished(&s1, &r1.work).await;
    s1.wait_saved().await.unwrap();
    let outcome1 = s1.compact().await.unwrap();
    s1.wait_saved().await.unwrap();
    let s1_record_id = outcome1.record.record_id;
    let s1_orig_id = s1.read_history_record(&s1_record_id).unwrap().replacements[0].originals[0]
        .id()
        .0;

    // Session 2: uncompacted, tries to reference session 1's record
    let s2 = host
        .create(PreparedCreate::persistent(
            compacting_definition(None),
            Context::new(),
        ))
        .await
        .unwrap();
    let before_record = saved_record(store.path(), s2.target().conversation_id().as_str());

    let call_id = causa::kernel::BlockId::new(uuid::Uuid::now_v7());
    let call_block = ContextBlock::new(
        call_id,
        causa::kernel::BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "echo",
            serde_json::json!({}),
        )),
        Default::default(),
    );
    let mut output = causa::kernel::ToolOutput::new(serde_json::Value::String("val".into()));
    output.artifact = Some(causa::kernel::ArtifactRef::new(
        format!("{s1_record_id}/{s1_orig_id}"),
        10,
        causa::kernel::ArtifactKind::FullOutput,
        true,
    ));
    let result_block = ContextBlock::new(
        causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
        causa::kernel::BlockContent::ToolResult(causa::kernel::ToolResultPayload::new(
            call_id,
            causa::kernel::ToolResultStatus::Succeeded,
            output,
        )),
        Default::default(),
    );

    let version = s2.read_materials().unwrap().version;
    let edit_res = s2.edit(version, vec![call_block, result_block]);
    assert!(
        matches!(edit_res, Err(ApplicationError::Store(_))),
        "referencing another session's record must be refused"
    );

    // Stored record remains unchanged
    let after_record = saved_record(store.path(), s2.target().conversation_id().as_str());
    assert_eq!(before_record, after_record);

    host.close(&s1.target(), ClosePolicy::Save).await.unwrap();
    host.close(&s2.target(), ClosePolicy::Save).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be06_save_refused_for_missing_block() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text("Summary.".into()));
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
            request_key: "k1".into(),
            blocks: vec![text_block(1_200)],
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    let outcome = session.compact().await.unwrap();
    session.wait_saved().await.unwrap();
    let record_id = outcome.record.record_id;
    let stored_before = saved_record(store.path(), session.target().conversation_id().as_str());

    // Block ID not in record originals
    let call_id = causa::kernel::BlockId::new(uuid::Uuid::now_v7());
    let call_block = ContextBlock::new(
        call_id,
        causa::kernel::BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "echo",
            serde_json::json!({}),
        )),
        Default::default(),
    );
    let random_block_id = uuid::Uuid::now_v7();
    let mut output = causa::kernel::ToolOutput::new(serde_json::Value::String("val".into()));
    output.artifact = Some(causa::kernel::ArtifactRef::new(
        format!("{record_id}/{random_block_id}"),
        10,
        causa::kernel::ArtifactKind::FullOutput,
        true,
    ));
    let result_block = ContextBlock::new(
        causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
        causa::kernel::BlockContent::ToolResult(causa::kernel::ToolResultPayload::new(
            call_id,
            causa::kernel::ToolResultStatus::Succeeded,
            output,
        )),
        Default::default(),
    );

    let mut blocks = session.read_materials().unwrap().context.blocks().to_vec();
    blocks.push(call_block);
    blocks.push(result_block);
    let version = session.read_materials().unwrap().version;
    session.edit(version, blocks).unwrap();
    let save_err = session.wait_saved().await.unwrap_err();
    assert!(
        matches!(save_err, ApplicationError::SaveBlocked(_)),
        "save must be blocked when referenced block is not among originals"
    );
    let stored_after = saved_record(store.path(), session.target().conversation_id().as_str());
    assert_eq!(
        stored_before, stored_after,
        "stored record must remain unchanged"
    );

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be06_save_refused_for_deleted_record_file() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text("Summary.".into()));
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
            request_key: "k1".into(),
            blocks: vec![text_block(1_200)],
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    let outcome = session.compact().await.unwrap();
    session.wait_saved().await.unwrap();
    let record_id = outcome.record.record_id;
    let orig_id = session
        .read_history_record(&record_id)
        .unwrap()
        .replacements[0]
        .originals[0]
        .id()
        .0;

    // Attach valid reference first and save successfully
    let call_id = causa::kernel::BlockId::new(uuid::Uuid::now_v7());
    let call_block = ContextBlock::new(
        call_id,
        causa::kernel::BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "echo",
            serde_json::json!({}),
        )),
        Default::default(),
    );
    let mut output = causa::kernel::ToolOutput::new(serde_json::Value::String("val".into()));
    output.artifact = Some(causa::kernel::ArtifactRef::new(
        format!("{record_id}/{orig_id}"),
        10,
        causa::kernel::ArtifactKind::FullOutput,
        true,
    ));
    let result_block = ContextBlock::new(
        causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
        causa::kernel::BlockContent::ToolResult(causa::kernel::ToolResultPayload::new(
            call_id,
            causa::kernel::ToolResultStatus::Succeeded,
            output,
        )),
        Default::default(),
    );

    let mut blocks = session.read_materials().unwrap().context.blocks().to_vec();
    blocks.push(call_block);
    blocks.push(result_block);
    let version = session.read_materials().unwrap().version;
    session.edit(version, blocks).unwrap();
    session.wait_saved().await.unwrap();

    let stored_before = saved_record(store.path(), session.target().conversation_id().as_str());

    // Now delete the record file
    let id = session.target().conversation_id().as_str().to_owned();
    let rec_file = history_dir(store.path(), &id).join(format!("{record_id}.json"));
    std::fs::remove_file(&rec_file).unwrap();

    // Now trigger another save by appending another block
    let mut new_blocks = session.read_materials().unwrap().context.blocks().to_vec();
    new_blocks.push(text_block(10));
    let version2 = session.read_materials().unwrap().version;
    session.edit(version2, new_blocks).unwrap();

    let save_err = session.wait_saved().await.unwrap_err();
    assert!(
        matches!(save_err, ApplicationError::SaveBlocked(_)),
        "save must be blocked when referenced record file is missing from disk: {save_err}"
    );
    let stored_after = saved_record(store.path(), session.target().conversation_id().as_str());
    assert_eq!(
        stored_before, stored_after,
        "stored record must remain untouched"
    );

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be07_assembly_declaration_persistent_vs_transient() {
    struct ToolCaptureGateway {
        requests: std::sync::Mutex<Vec<causa::kernel::ModelRequest>>,
    }
    #[async_trait::async_trait]
    impl causa::kernel::ModelGateway for ToolCaptureGateway {
        async fn invoke(
            &self,
            request: &causa::kernel::ModelRequest,
            _: &causa::kernel::CallControl,
        ) -> Result<causa::kernel::ModelOutput, causa::kernel::ModelInvokeError> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(causa::kernel::ModelOutput::new(
                causa::kernel::ModelResponse {
                    items: vec![causa::kernel::ModelItem::text("done")],
                },
                causa::kernel::ModelStopReason::EndTurn,
            ))
        }
    }

    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = Arc::new(ToolCaptureGateway {
        requests: std::sync::Mutex::new(Vec::new()),
    });
    host.set_test_gateway(Some(gateway.clone() as Arc<dyn ModelGateway>));

    // 1. Persistent session without workspace tools: declares recall (1 tool)
    let def_no_ws = compacting_definition(None);
    let s1 = host
        .create(PreparedCreate::persistent(def_no_ws, Context::new()))
        .await
        .unwrap();
    let r1 = s1.submit(request("turn1")).unwrap();
    finished(&s1, &r1.work).await;
    {
        let reqs = gateway.requests.lock().unwrap();
        let last_req = reqs.last().unwrap();
        let tools = causa::kernel::current_tools(&last_req.frame.blocks);
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(tool_names, vec!["recall"]);
    }
    host.close(&s1.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();

    // 2. Persistent session with workspace tools: declares read_file + recall (2 tools)
    let ws_dir = tempfile::tempdir().unwrap();
    let mut def_ws = compacting_definition(None);
    def_ws.workspace_root = Some(ws_dir.path().canonicalize().unwrap());
    def_ws.tools = vec![crate::definition::WorkspaceTool::ReadFile];
    let s2 = host
        .create(PreparedCreate::persistent(def_ws, Context::new()))
        .await
        .unwrap();
    let r2 = s2.submit(request("turn2")).unwrap();
    finished(&s2, &r2.work).await;
    {
        let reqs = gateway.requests.lock().unwrap();
        let last_req = reqs.last().unwrap();
        let tools = causa::kernel::current_tools(&last_req.frame.blocks);
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(tool_names.contains(&"recall"));
        assert!(tool_names.contains(&"read_file"));
        assert_eq!(tool_names.len(), 2);
    }
    host.close(&s2.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();

    // 3. Transient session: does NOT declare recall
    let mut def_transient = compacting_definition(None);
    def_transient.workspace_root = Some(ws_dir.path().canonicalize().unwrap());
    def_transient.tools = vec![crate::definition::WorkspaceTool::ReadFile];
    let s3 = host
        .create(PreparedCreate::transient(def_transient, Context::new()))
        .await
        .unwrap();
    let r3 = s3.submit(request("turn3")).unwrap();
    finished(&s3, &r3.work).await;
    {
        let reqs = gateway.requests.lock().unwrap();
        let last_req = reqs.last().unwrap();
        let tools = causa::kernel::current_tools(&last_req.frame.blocks);
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(!tool_names.contains(&"recall"));
        assert_eq!(tool_names, vec!["read_file"]);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be07_recall_runs_in_read_only_and_ask_without_approval() {
    use crate::permission::PermissionMode;

    struct RecallInvokingGateway {
        round: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl causa::kernel::ModelGateway for RecallInvokingGateway {
        async fn invoke(
            &self,
            _req: &causa::kernel::ModelRequest,
            _: &causa::kernel::CallControl,
        ) -> Result<causa::kernel::ModelOutput, causa::kernel::ModelInvokeError> {
            let r = self.round.fetch_add(1, Ordering::SeqCst);
            if r.is_multiple_of(2) {
                Ok(causa::kernel::ModelOutput::new(
                    causa::kernel::ModelResponse {
                        items: vec![causa::kernel::ModelItem::tool_call(
                            causa::kernel::ToolCallDraft::new(
                                "recall",
                                serde_json::json!({ "query": "hello" }),
                            ),
                        )],
                    },
                    causa::kernel::ModelStopReason::ToolUse,
                ))
            } else {
                Ok(causa::kernel::ModelOutput::new(
                    causa::kernel::ModelResponse {
                        items: vec![causa::kernel::ModelItem::text("done")],
                    },
                    causa::kernel::ModelStopReason::EndTurn,
                ))
            }
        }
    }

    for mode in [PermissionMode::ReadOnly, PermissionMode::Ask] {
        let store = tempfile::tempdir().unwrap();
        let ws_dir = tempfile::tempdir().unwrap();
        let host = SessionHost::new(Some(store.path().into()));
        let gateway = Arc::new(RecallInvokingGateway {
            round: std::sync::atomic::AtomicUsize::new(0),
        });
        host.set_test_gateway(Some(gateway as Arc<dyn ModelGateway>));

        let mut def = compacting_definition(None);
        def.workspace_root = Some(ws_dir.path().canonicalize().unwrap());
        def.tools = vec![crate::definition::WorkspaceTool::ReadFile];
        def.permission_mode = Some(mode);
        def.max_model_rounds = 3;
        def.max_tool_calls = 5;

        let session = host
            .create(PreparedCreate::persistent(def, Context::new()))
            .await
            .unwrap();

        let mut approval_rx = session.connect_approvals();
        let receipt = session.submit(request("recall-perm-test")).unwrap();
        finished(&session, &receipt.work).await;
        session.wait_saved().await.unwrap();

        assert!(
            approval_rx.try_recv().is_err(),
            "recall should not require approval in {mode:?}"
        );

        let snapshot = session.read_materials().unwrap();
        let tool_result = snapshot
            .context
            .blocks()
            .iter()
            .find_map(|b| match b.content() {
                causa::kernel::BlockContent::ToolResult(payload) => Some(payload),
                _ => None,
            })
            .expect("tool result block must reach Context");

        assert_eq!(
            tool_result.status,
            causa::kernel::ToolResultStatus::Succeeded
        );
        let content_str = tool_result.output.content.as_str().unwrap();
        assert!(content_str.contains("no hits found for query"));

        host.close(&session.target(), ClosePolicy::DiscardLocal)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be08_compaction_brief_ends_with_recall_pointer_and_recall_lists_originals() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));

    struct DynamicRecallGateway {
        target_record_id: std::sync::Mutex<Option<String>>,
        main_rounds: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl causa::kernel::ModelGateway for DynamicRecallGateway {
        async fn invoke(
            &self,
            request: &causa::kernel::ModelRequest,
            _: &causa::kernel::CallControl,
        ) -> Result<causa::kernel::ModelOutput, causa::kernel::ModelInvokeError> {
            if is_summary_request(request) {
                let mut out = causa::kernel::ModelOutput::new(
                    causa::kernel::ModelResponse {
                        items: vec![causa::kernel::ModelItem::text(
                            "Goal: test recall pointer in brief.",
                        )],
                    },
                    causa::kernel::ModelStopReason::EndTurn,
                );
                out.usage = Some(causa::kernel::ModelUsage::new(400, 30));
                return Ok(out);
            }
            let round = self.main_rounds.fetch_add(1, Ordering::SeqCst);
            let target = self.target_record_id.lock().unwrap().clone();
            if let Some(record_id) = target
                && round == 1
            {
                Ok(causa::kernel::ModelOutput::new(
                    causa::kernel::ModelResponse {
                        items: vec![causa::kernel::ModelItem::tool_call(
                            causa::kernel::ToolCallDraft::new(
                                "recall",
                                serde_json::json!({ "id": record_id }),
                            ),
                        )],
                    },
                    causa::kernel::ModelStopReason::ToolUse,
                ))
            } else {
                let mut out = causa::kernel::ModelOutput::new(
                    causa::kernel::ModelResponse {
                        items: vec![causa::kernel::ModelItem::text("done")],
                    },
                    causa::kernel::ModelStopReason::EndTurn,
                );
                out.usage = Some(causa::kernel::ModelUsage::new(MAIN_INPUT_TOKENS, 20));
                Ok(out)
            }
        }
    }

    let gateway = Arc::new(DynamicRecallGateway {
        target_record_id: std::sync::Mutex::new(None),
        main_rounds: std::sync::atomic::AtomicUsize::new(0),
    });
    host.set_test_gateway(Some(gateway.clone() as Arc<dyn ModelGateway>));

    let mut def = compacting_definition(None);
    def.max_tool_calls = 5;
    let session = host
        .create(PreparedCreate::persistent(def, Context::new()))
        .await
        .unwrap();

    let receipt = session
        .submit(SubmitRequest {
            request_key: "init".into(),
            blocks: vec![text_block(1_200), text_block(1_200)],
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();

    let outcome = session.compact().await.unwrap();
    session.wait_saved().await.unwrap();
    let record_id = outcome.record.record_id.clone();
    *gateway.target_record_id.lock().unwrap() = Some(record_id.clone());

    let snapshot = session.read_materials().unwrap();
    let brief_block = snapshot
        .context
        .blocks()
        .iter()
        .find(|b| b.meta().source.as_deref() == Some(crate::compact::SUMMARY_SOURCE))
        .expect("brief block must exist");
    let brief_text = match brief_block.content() {
        causa::kernel::BlockContent::Message { parts, .. } => parts
            .iter()
            .find_map(|p| match p {
                causa::kernel::ContentPart::Text(t) => Some(t.0.as_str()),
                _ => None,
            })
            .unwrap(),
        _ => panic!("expected message block"),
    };
    let expected_pointer = format!("Original messages: recall id={record_id}");
    assert_eq!(
        brief_text.lines().last().unwrap(),
        expected_pointer.as_str()
    );

    let receipt2 = session.submit(request("use recall")).unwrap();
    finished(&session, &receipt2.work).await;
    session.wait_saved().await.unwrap();

    let snapshot2 = session.read_materials().unwrap();
    let recall_result = snapshot2
        .context
        .blocks()
        .iter()
        .find_map(|b| match b.content() {
            causa::kernel::BlockContent::ToolResult(payload) => Some(payload),
            _ => None,
        })
        .expect("recall result must be present in context");
    assert_eq!(
        recall_result.status,
        causa::kernel::ToolResultStatus::Succeeded
    );
    let output_str = recall_result.output.content.as_str().unwrap();
    assert!(
        output_str.contains(&record_id),
        "recall output must contain record id: {output_str}"
    );

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be09_application_export_drops_own_artifact_references_and_imports() {
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text("Summary brief text.".into()));
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
            request_key: "k1".into(),
            blocks: vec![text_block(1_200), text_block(1_200)],
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();

    let outcome = session.compact().await.unwrap();
    session.wait_saved().await.unwrap();
    let record_id = outcome.record.record_id;

    let history_doc = session.read_history_record(&record_id).unwrap();
    let orig_block = &history_doc.replacements[0].originals[0];
    let orig_block_id = orig_block.id().0;

    let call_id = causa::kernel::BlockId::new(uuid::Uuid::now_v7());
    let call_block = ContextBlock::new(
        call_id,
        causa::kernel::BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "echo",
            serde_json::json!({}),
        )),
        Default::default(),
    );
    let mut output = causa::kernel::ToolOutput::new(serde_json::Value::String("short note".into()));
    output.artifact = Some(causa::kernel::ArtifactRef::new(
        format!("{record_id}/{orig_block_id}"),
        10,
        causa::kernel::ArtifactKind::FullOutput,
        true,
    ));
    let result_block = ContextBlock::new(
        causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
        causa::kernel::BlockContent::ToolResult(causa::kernel::ToolResultPayload::new(
            call_id,
            causa::kernel::ToolResultStatus::Succeeded,
            output,
        )),
        Default::default(),
    );

    let mut blocks = session.read_materials().unwrap().context.blocks().to_vec();
    blocks.push(call_block);
    blocks.push(result_block);
    let version = session.read_materials().unwrap().version;
    session.edit(version, blocks).unwrap();
    session.wait_saved().await.unwrap();

    // A reference to this session's own record is supported material: repair
    // inspection is ready and new input is admitted.
    assert!(session.inspect_repair(None).unwrap().diagnosis.is_ready());
    let receipt = session
        .submit(SubmitRequest {
            request_key: "k2".into(),
            blocks: vec![text_block(10)],
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();

    let snapshot = session.read_materials().unwrap();
    let export_folder = tempfile::tempdir().unwrap();
    let export_path = export_folder.path().join("exported_materials.json");
    crate::material::save_new(&export_path, &snapshot.detached_context()).unwrap();

    let loaded_context = crate::material::load(&export_path).unwrap();
    let exported_result = loaded_context
        .blocks()
        .iter()
        .find_map(|b| match b.content() {
            causa::kernel::BlockContent::ToolResult(res) => Some(res),
            _ => None,
        })
        .expect("exported tool result present");
    assert!(exported_result.output.artifact.is_none());
    assert!(
        exported_result
            .notes
            .iter()
            .any(|n| n.0 == crate::material::DETACHED_HISTORY_NOTE)
    );

    let new_store = tempfile::tempdir().unwrap();
    let new_host = SessionHost::new(Some(new_store.path().into()));
    new_host.set_test_gateway(as_gateway(&gateway));
    let new_session = new_host
        .create(PreparedCreate::persistent(
            compacting_definition(None),
            loaded_context,
        ))
        .await
        .unwrap();
    new_session.wait_saved().await.unwrap();

    let target = session.target().clone();
    host.close(&target, ClosePolicy::Save).await.unwrap();
    let reopened = host.open(target.conversation_id().clone()).await.unwrap();
    let reopened_snapshot = reopened.read_materials().unwrap();
    let reopened_result = reopened_snapshot
        .context
        .blocks()
        .iter()
        .find_map(|b| match b.content() {
            causa::kernel::BlockContent::ToolResult(res) => Some(res),
            _ => None,
        })
        .expect("tool result present in reopened session");
    assert!(
        reopened_result.output.artifact.is_some(),
        "original session must keep its reference"
    );
    assert_eq!(
        reopened_result.output.artifact.as_ref().unwrap().id,
        format!("{record_id}/{orig_block_id}")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be07_reopen_without_recall_appends_tool_change_and_succeeds() {
    let store = tempfile::tempdir().unwrap();
    let ws_dir = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));

    let gateway = Arc::new(Gateway {
        calls: Arc::new(AtomicUsize::new(0)),
        entered: None,
        release: None,
        panic: false,
    });
    host.set_test_gateway(Some(gateway as Arc<dyn ModelGateway>));

    let mut def = compacting_definition(None);
    def.workspace_root = Some(ws_dir.path().canonicalize().unwrap());
    def.tools = vec![crate::definition::WorkspaceTool::ReadFile];

    // Build context with an initial ToolChange that declared only read_file (lacking recall)
    let mut context = Context::new();
    let old_tool = causa::kernel::ToolDefinition::new(
        "read_file",
        "read file description",
        serde_json::json!({ "type": "object" }),
    );
    let tc_block = ContextBlock::new(
        causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
        causa::kernel::BlockContent::ToolChange(causa::kernel::ToolChange::between(
            &[],
            &[old_tool],
        )),
        Default::default(),
    );
    context.edit().append(vec![tc_block]).commit().unwrap();

    let session = host
        .create(PreparedCreate::persistent(def.clone(), context))
        .await
        .unwrap();
    let id = session.target().conversation_id().clone();
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // Reopen session: context's latest ToolChange lacks recall
    let reopened = host.open(id).await.unwrap();
    let mat_before = reopened.read_materials().unwrap();
    let tools_before = causa::kernel::current_tools(mat_before.context.blocks());
    assert_eq!(tools_before.len(), 1);
    assert_eq!(tools_before[0].name, "read_file");

    // Next turn runs: Causa appends a ToolChange declaring recall and the turn succeeds
    let receipt = reopened.submit(request("next-turn")).unwrap();
    finished(&reopened, &receipt.work).await;
    reopened.wait_saved().await.unwrap();

    let mat_after = reopened.read_materials().unwrap();
    let tools_after = causa::kernel::current_tools(mat_after.context.blocks());
    let tool_names: Vec<&str> = tools_after.iter().map(|t| t.name.as_str()).collect();
    assert!(tool_names.contains(&"recall"));
    assert!(tool_names.contains(&"read_file"));

    let tc_blocks: Vec<_> = mat_after
        .context
        .blocks()
        .iter()
        .filter_map(|b| match b.content() {
            causa::kernel::BlockContent::ToolChange(tc) => Some(tc),
            _ => None,
        })
        .collect();
    // At least 2 ToolChanges: the old one and the new one adding recall
    assert!(tc_blocks.len() >= 2);

    host.close(&reopened.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be28_a_fixed_prefix_over_half_the_line_is_reported_once_and_refused_manually() {
    use crate::compact::{CompactionAbandon, CompactionProgress};
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text("never asked".into()));
    host.set_test_gateway(as_gateway(&gateway));
    // A 1,400-token line; the 1,200-byte system prompt and the `recall`
    // declaration together are over its half.
    let mut blocks = vec![ContextBlock::text(
        BlockId::new(uuid::Uuid::now_v7()),
        causa::kernel::Role::System,
        "s".repeat(1_200),
    )];
    blocks.extend(earlier_conversation().blocks().iter().cloned());
    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(Some(FIXTURE_WINDOW)),
            Context::from_blocks(blocks).unwrap(),
        ))
        .await
        .unwrap();
    let mut progress = session.subscribe_progress();
    for key in ["first", "second"] {
        let receipt = session
            .submit(SubmitRequest {
                request_key: key.into(),
                blocks: short_request(),
            })
            .unwrap();
        finished(&session, &receipt.work).await;
        session.wait_saved().await.unwrap();
    }
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 0);
    assert_eq!(gateway.main_calls.load(Ordering::SeqCst), 2);
    let (mut started, mut too_large) = (0, 0);
    while let Ok(event) = progress.try_recv() {
        if let crate::session::ProgressUpdate::Compaction(update) = &event.update {
            match update {
                CompactionProgress::Started { .. } => started += 1,
                CompactionProgress::Abandoned {
                    reason: CompactionAbandon::PrefixTooLarge,
                    ..
                } => too_large += 1,
                other => panic!("unexpected compaction progress {other:?}"),
            }
        }
    }
    assert_eq!((started, too_large), (1, 1), "reported once per session");
    for _ in 0..2 {
        let refused = session.compact().await.unwrap_err();
        assert!(
            matches!(&refused, crate::application::ApplicationError::Compaction(reason) if reason == "prefix_too_large"),
            "{refused:?}"
        );
    }
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 0);
    assert!(session.history().is_empty());
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

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
    // The brief, then the work's request and its trailing tool declaration
    // kept in the retained tail, and the model's final answer.
    assert_eq!(blocks.len(), 4, "{blocks:?}");
    assert_eq!(blocks[0]["meta"]["source"], "tars:compaction");
    assert_eq!(blocks[0]["content"]["value"]["role"], "user");
    assert_eq!(blocks[1]["id"], request[0].id().0.to_string());
    assert_eq!(blocks[2]["content"]["shape"], "tool_change");
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
        4,
        "the earlier four blocks; round-0 request and declaration stay in the tail"
    );
    assert_eq!(history[0]["removed_block_count"], 4);
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
    // A 3,800-token trigger line, so the chain budget, min(4_000, (line − prefix) / 2),
    // holds two briefs and no merge follows. The first work compacts the
    // seeded conversation and keeps its own 2,200-token input; the second
    // work's 1,000-token input brings that input back over the line.
    let mut definition = compacting_definition(Some(4_000));
    definition.compact_threshold = 95;
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
    let first = vec![text_block(6_600)];
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
    // [S1, first request, tool declaration, first answer]
    assert_eq!(after_first.len(), 4);
    let brief_one = after_first[0].clone();
    assert_eq!(brief_one.meta().source.as_deref(), Some(SUMMARY_SOURCE));
    assert_eq!(after_first[1].id(), first_id);
    let first_declaration = after_first[2].id();
    let first_answer = after_first[3].id();

    let second = vec![text_block(3_000)];
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

    // The second request is the normal material up to `second` followed by
    // the instruction: the first brief comes along as background, unchanged.
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
        [brief_one.id(), first_id, first_declaration, first_answer,]
    );

    let context = session.read_materials().unwrap().context;
    let blocks = context.blocks();
    assert_eq!(
        blocks.len(),
        5,
        "[S1, declaration, S2, second request, answer]"
    );
    assert_eq!(blocks[0], brief_one, "the first brief is never rewritten");
    assert!(matches!(blocks[1].content(), BlockContent::ToolChange(_)));
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
        [first_id, first_declaration, first_answer],
        "the first work and its declaration; never the head or round-0 second request"
    );
    assert_eq!(history[1].removed_block_count, 3);

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
    // The first work's tool declaration sat after its request and was
    // summarized in the second compaction.
    assert_eq!(shown[5], (first_declaration, two.clone()));
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
async fn test_be13_an_automatic_summary_that_overfills_the_chain_is_followed_by_a_merge() {
    use crate::compact::{CompactionProgress, RewriteKind};
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text(format!(
        "Goal: continue the fixture work. Next steps: finish. {}",
        "detail ".repeat(80)
    )));
    gateway.main_without_usage.store(true, Ordering::SeqCst);
    host.set_test_gateway(as_gateway(&gateway));
    // A 1,200-token line gives the chain about 380 tokens (capped at available / 2),
    // so two ~260-token briefs exceed the budget and the second summary triggers a merge.
    let mut definition = compacting_definition(Some(3_000));
    definition.compact_threshold = 40;
    let session = host
        .create(PreparedCreate::persistent(
            definition,
            earlier_conversation(),
        ))
        .await
        .unwrap();
    let mut progress = session.subscribe_progress();
    let mut requests = Vec::new();
    for (key, bytes) in [("one", 1_500), ("two", 600)] {
        let input = vec![text_block(bytes)];
        requests.push(input[0].id());
        let receipt = session
            .submit(SubmitRequest {
                request_key: key.into(),
                blocks: input,
            })
            .unwrap();
        finished(&session, &receipt.work).await;
        session.wait_saved().await.unwrap();
    }
    // Two briefs and the merge.
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 3);
    let history = session.history();
    let kinds: Vec<RewriteKind> = history.iter().map(|entry| entry.kind).collect();
    assert_eq!(
        kinds,
        [
            RewriteKind::Summarize,
            RewriteKind::Summarize,
            RewriteKind::Merge
        ]
    );

    // [one declaration, the merged brief, second request, answer]
    let context = session.read_materials().unwrap().context;
    let blocks = context.blocks();
    assert_eq!(blocks.len(), 4, "{blocks:?}");
    assert!(matches!(
        blocks[0].content(),
        causa::kernel::BlockContent::ToolChange(_)
    ));
    assert_eq!(
        blocks[1].meta().source.as_deref(),
        Some(crate::compact::SUMMARY_SOURCE)
    );
    assert_eq!(blocks[2].id(), requests[1]);

    // The merge request is the chain and the instruction; the merge record
    // holds the chain it replaced.
    let merge_request = gateway.summary_inputs.lock().unwrap()[2].clone();
    let merge_record = session.read_history_record(&history[2].record_id).unwrap();
    let chain: Vec<BlockId> = merge_record.replacements[0]
        .originals
        .iter()
        .map(ContextBlock::id)
        .collect();
    assert_eq!(chain.len(), 3, "the declaration and two briefs");
    let sent: Vec<BlockId> = merge_request.iter().map(ContextBlock::id).collect();
    assert_eq!(sent[..3], chain[..]);
    assert_eq!(sent.len(), 4);
    assert_eq!(
        merge_record.replacements[0].inserted_ids,
        [blocks[0].id(), blocks[1].id()]
    );

    // Each step reported itself: three attempts, three records.
    let (mut started, mut records) = (0, Vec::new());
    while let Ok(event) = progress.try_recv() {
        if let crate::session::ProgressUpdate::Compaction(update) = &event.update {
            match update {
                CompactionProgress::Started { .. } => started += 1,
                CompactionProgress::Finished { record, .. } => records.push(record.clone()),
                other => panic!("unexpected compaction progress {other:?}"),
            }
        }
    }
    assert_eq!(started, 3);
    let indexed: Vec<String> = history
        .iter()
        .map(|entry| entry.record_id.clone())
        .collect();
    assert_eq!(records, indexed);

    // Expanded, the whole conversation comes back once, in order.
    let expanded = session.expanded_conversation(None).unwrap();
    assert!(expanded.unreadable_records.is_empty());
    let shown: Vec<BlockId> = expanded
        .blocks
        .iter()
        .map(|block| block.block.id())
        .collect();
    assert_eq!(shown.iter().filter(|id| **id == requests[0]).count(), 1);
    assert_eq!(shown.iter().filter(|id| **id == requests[1]).count(), 1);
    assert!(
        expanded
            .blocks
            .iter()
            .all(|block| block.block.meta().source.as_deref()
                != Some(crate::compact::SUMMARY_SOURCE))
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
    // About 3,000 tokens of earlier material: over the 2,000-token window, so
    // the brief request is trimmed to 70% of it.
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
    for dropped in &earlier_ids[..2] {
        assert!(!sent.contains(dropped), "the earliest messages are dropped");
    }
    assert!(
        !sent.contains(&request_id),
        "the round-0 request stays in the retained tail and is excluded from the brief request"
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
        4,
        "the earlier material; round-0 request and its tool declaration stay in the tail"
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
    // The four earlier blocks were summarized; the round-0 request and the
    // tool declaration after it stayed in the retained tail.
    assert_eq!(summarized, 4);
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
        4,
        "the four summarized earlier blocks"
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
    assert!(outcome.merged.is_none());
    assert_eq!(
        session.read_materials().unwrap().context.blocks()[0].id(),
        outcome.brief,
        "the outcome names the new brief"
    );
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
    let (mut started, mut too_large, mut trimmed) = (0, 0, 0);
    while let Ok(event) = progress.try_recv() {
        if let crate::session::ProgressUpdate::Compaction(update) = &event.update {
            match update {
                CompactionProgress::Started { .. } => started += 1,
                CompactionProgress::Abandoned {
                    reason: CompactionAbandon::PrefixTooLarge,
                    ..
                } => too_large += 1,
                CompactionProgress::Trimmed {
                    before_tokens,
                    after_tokens,
                } => {
                    assert!(*before_tokens > FIXTURE_WINDOW);
                    assert!(*after_tokens <= FIXTURE_WINDOW);
                    trimmed += 1;
                }
                other => panic!("unexpected compaction progress {other:?}"),
            }
        }
    }
    assert_eq!((started, too_large), (1, 1), "reported once per session");
    assert_eq!(trimmed, 2, "both over-window rounds fall back to trimming");
    {
        let main_inputs = gateway.main_inputs.lock().unwrap();
        assert_eq!(main_inputs.len(), 2);
        for frame_blocks in main_inputs.iter() {
            assert!(
                crate::compact::segment::blocks_estimate(frame_blocks) <= FIXTURE_WINDOW,
                "trimmed frame fits window"
            );
        }
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be16_save_and_repair_accept_own_artifact_and_refuse_missing_unreadable_and_foreign() {
    use causa::kernel::{ArtifactHint, ArtifactKind, ArtifactStore};

    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text("Summary.".into()));
    host.set_test_gateway(as_gateway(&gateway));

    let s1 = host
        .create(PreparedCreate::persistent(
            compacting_definition(None),
            Context::new(),
        ))
        .await
        .unwrap();

    let payload = b"spilled full output for save & repair test";
    let art_ref = s1
        .artifacts()
        .unwrap()
        .persist(
            payload,
            ArtifactHint::new(
                "read_file",
                causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
                ArtifactKind::FullOutput,
            ),
        )
        .await
        .unwrap();

    let call_id = causa::kernel::BlockId::new(uuid::Uuid::now_v7());
    let call_block = ContextBlock::new(
        call_id,
        causa::kernel::BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "read_file",
            serde_json::json!({}),
        )),
        Default::default(),
    );
    let mut output = causa::kernel::ToolOutput::new(serde_json::Value::String("preview".into()));
    output.truncation = causa::kernel::Truncation::Middle;
    output.artifact = Some(art_ref.clone());
    let result_block = ContextBlock::new(
        causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
        causa::kernel::BlockContent::ToolResult(causa::kernel::ToolResultPayload::new(
            call_id,
            causa::kernel::ToolResultStatus::Succeeded,
            output,
        )),
        Default::default(),
    );

    let v1 = s1.read_materials().unwrap().version;
    s1.edit(v1, vec![call_block.clone(), result_block.clone()])
        .unwrap();
    s1.wait_saved().await.unwrap();

    // Own existing readable artifact is accepted by repair inspection and admission.
    assert!(s1.inspect_repair(None).unwrap().diagnosis.is_ready());
    let r1 = s1
        .submit(SubmitRequest {
            request_key: "own-artifact-ok".into(),
            blocks: vec![text_block(10)],
        })
        .unwrap();
    finished(&s1, &r1.work).await;
    s1.wait_saved().await.unwrap();

    // Foreign session referencing s1's artifact:<id> is refused by repair and save.
    let s2 = host
        .create(PreparedCreate::persistent(
            compacting_definition(None),
            Context::new(),
        ))
        .await
        .unwrap();
    let v2 = s2.read_materials().unwrap().version;
    s2.edit(v2, vec![call_block, result_block]).unwrap();
    let s2_save_err = s2.wait_saved().await.unwrap_err();
    assert!(
        matches!(s2_save_err, ApplicationError::SaveBlocked(_)),
        "foreign spilled artifact must block save: {s2_save_err}"
    );
    assert!(
        !s2.inspect_repair(None).unwrap().diagnosis.is_ready(),
        "foreign spilled artifact must be reported by repair inspection"
    );
    host.close(&s2.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();

    // Missing artifact file on s1: repair flags UnsupportedReference, submit refuses, save blocks.
    let s1_id = s1.target().conversation_id().as_str().to_owned();
    let art_uuid = art_ref.id.strip_prefix("artifact:").unwrap();
    let art_path = store
        .path()
        .join("sessions")
        .join(format!("{s1_id}.artifacts"))
        .join(art_uuid);
    std::fs::remove_file(&art_path).unwrap();

    let diag = s1.inspect_repair(None).unwrap().diagnosis;
    assert!(!diag.is_ready());
    assert!(
        diag.issues
            .iter()
            .any(|i| i.kind == crate::application::RepairIssueKind::UnsupportedReference)
    );
    assert!(s1.submit(request("blocked-by-missing-artifact")).is_err());

    let mut blocks = s1.read_materials().unwrap().context.blocks().to_vec();
    blocks.push(text_block(10));
    let v_missing = s1.read_materials().unwrap().version;
    s1.edit(v_missing, blocks).unwrap();
    let missing_err = s1.wait_saved().await.unwrap_err();
    assert!(matches!(missing_err, ApplicationError::SaveBlocked(_)));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&art_path, b"locked").unwrap();
        let mut perms = std::fs::metadata(&art_path).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&art_path, perms).unwrap();

        if std::fs::File::open(&art_path).is_err() {
            assert!(!s1.inspect_repair(None).unwrap().diagnosis.is_ready());
            let retry_err = s1.retry_save().await.unwrap_err();
            assert!(matches!(retry_err, ApplicationError::SaveBlocked(_)));
        }
        let mut restore = std::fs::metadata(&art_path).unwrap().permissions();
        restore.set_mode(0o600);
        std::fs::set_permissions(&art_path, restore).unwrap();
    }

    host.close(&s1.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be16_reopen_under_write_lock_cleans_orphan_artifacts_and_keeps_context_and_history_refs()
 {
    use causa::kernel::{ArtifactHint, ArtifactKind, ArtifactStore};

    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text("Goal: summary.".into()));
    host.set_test_gateway(as_gateway(&gateway));

    let session = host
        .create(PreparedCreate::persistent(
            compacting_definition(None),
            Context::new(),
        ))
        .await
        .unwrap();
    let artifacts = session.artifacts().unwrap();

    // 1. Artifact that will be summarized into a history record
    let art_hist = artifacts
        .persist(
            b"history-referenced payload",
            ArtifactHint::new(
                "read_file",
                causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
                ArtifactKind::FullOutput,
            ),
        )
        .await
        .unwrap();
    let call_hist = causa::kernel::BlockId::new(uuid::Uuid::now_v7());
    let mut out_hist = causa::kernel::ToolOutput::new(serde_json::Value::String("p-hist".into()));
    out_hist.truncation = causa::kernel::Truncation::Middle;
    out_hist.artifact = Some(art_hist.clone());
    let v = session.read_materials().unwrap().version;
    session
        .edit(
            v,
            vec![
                text_block(1_200),
                ContextBlock::new(
                    call_hist,
                    causa::kernel::BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
                        "read_file",
                        serde_json::json!({}),
                    )),
                    Default::default(),
                ),
                ContextBlock::new(
                    causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
                    causa::kernel::BlockContent::ToolResult(causa::kernel::ToolResultPayload::new(
                        call_hist,
                        causa::kernel::ToolResultStatus::Succeeded,
                        out_hist,
                    )),
                    Default::default(),
                ),
            ],
        )
        .unwrap();
    session.wait_saved().await.unwrap();

    session.compact().await.unwrap();
    session.wait_saved().await.unwrap();
    assert!(!session.history().is_empty());

    // 2. Artifact that remains in current Context
    let art_ctx = artifacts
        .persist(
            b"context-referenced payload",
            ArtifactHint::new(
                "read_file",
                causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
                ArtifactKind::FullOutput,
            ),
        )
        .await
        .unwrap();
    let call_ctx = causa::kernel::BlockId::new(uuid::Uuid::now_v7());
    let mut out_ctx = causa::kernel::ToolOutput::new(serde_json::Value::String("p-ctx".into()));
    out_ctx.truncation = causa::kernel::Truncation::Middle;
    out_ctx.artifact = Some(art_ctx.clone());
    let mut blocks = session.read_materials().unwrap().context.blocks().to_vec();
    blocks.push(ContextBlock::new(
        call_ctx,
        causa::kernel::BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "read_file",
            serde_json::json!({}),
        )),
        Default::default(),
    ));
    blocks.push(ContextBlock::new(
        causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
        causa::kernel::BlockContent::ToolResult(causa::kernel::ToolResultPayload::new(
            call_ctx,
            causa::kernel::ToolResultStatus::Succeeded,
            out_ctx,
        )),
        Default::default(),
    ));
    let v2 = session.read_materials().unwrap().version;
    session.edit(v2, blocks).unwrap();
    session.wait_saved().await.unwrap();

    // 3. Orphan artifact persisted on disk but never referenced by Context or HistoryRecord
    let art_orphan = artifacts
        .persist(
            b"orphan payload",
            ArtifactHint::new(
                "read_file",
                causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
                ArtifactKind::FullOutput,
            ),
        )
        .await
        .unwrap();

    let id = session.target().conversation_id().clone();
    let art_dir = store
        .path()
        .join("sessions")
        .join(format!("{}.artifacts", id.as_str()));
    let path_hist = art_dir.join(art_hist.id.strip_prefix("artifact:").unwrap());
    let path_ctx = art_dir.join(art_ctx.id.strip_prefix("artifact:").unwrap());
    let path_orphan = art_dir.join(art_orphan.id.strip_prefix("artifact:").unwrap());
    assert!(path_hist.is_file());
    assert!(path_ctx.is_file());
    assert!(path_orphan.is_file());

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // Reopen under write lock: orphan removed, Context + History references kept, 0 artifact reads.
    let reopened = host.open(id).await.unwrap();
    assert_eq!(
        reopened.artifact_reads(),
        0,
        "reopening a session must not read .artifacts/ files"
    );
    assert!(path_hist.is_file(), "history-referenced artifact must stay");
    assert!(path_ctx.is_file(), "context-referenced artifact must stay");
    assert!(!path_orphan.exists(), "orphan artifact must be removed");
    host.close(&reopened.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be16_persistent_list_directory_spills_at_entry_while_transient_does_not_and_internal_cap_stays()
 {
    struct ScriptedWorkspaceGateway {
        calls: Vec<(&'static str, serde_json::Value)>,
        step: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ModelGateway for ScriptedWorkspaceGateway {
        async fn invoke(
            &self,
            _request: &causa::kernel::ModelRequest,
            _control: &causa::kernel::CallControl,
        ) -> Result<causa::kernel::ModelOutput, causa::kernel::ModelInvokeError> {
            let idx = self.step.fetch_add(1, Ordering::SeqCst);
            let output = if let Some((name, args)) = self.calls.get(idx) {
                causa::kernel::ModelOutput::new(
                    causa::kernel::ModelResponse {
                        items: vec![causa::kernel::ModelItem::tool_call(
                            causa::kernel::ToolCallDraft::new(*name, args.clone()),
                        )],
                    },
                    causa::kernel::ModelStopReason::ToolUse,
                )
            } else {
                causa::kernel::ModelOutput::new(
                    causa::kernel::ModelResponse {
                        items: vec![causa::kernel::ModelItem::text("done listing and reading")],
                    },
                    causa::kernel::ModelStopReason::EndTurn,
                )
            };
            Ok(output)
        }
    }

    let workspace = tempfile::tempdir().unwrap();
    let ws_root = workspace.path().canonicalize().unwrap();
    let big_dir = ws_root.join("many");
    std::fs::create_dir(&big_dir).unwrap();
    let pad = "x".repeat(90);
    for i in 1..=150 {
        std::fs::write(big_dir.join(format!("entry-{i:03}-{pad}.txt")), b"").unwrap();
    }
    let huge_content = "h".repeat(80_000);
    std::fs::write(ws_root.join("huge.txt"), &huge_content).unwrap();

    // 1. Persistent session: list_directory spills to .artifacts/, huge.txt still hits read_file's internal 64 KiB cap
    let store = tempfile::tempdir().unwrap();
    let persistent_host = SessionHost::new(Some(store.path().into()));
    persistent_host.set_test_gateway(Some(Arc::new(ScriptedWorkspaceGateway {
        calls: vec![
            ("list_directory", serde_json::json!({ "path": "many" })),
            ("read_file", serde_json::json!({ "path": "huge.txt" })),
        ],
        step: AtomicUsize::new(0),
    })));
    let mut def = compacting_definition(None);
    def.workspace_root = Some(ws_root.clone());
    def.tools = vec![
        crate::definition::WorkspaceTool::ListDirectory,
        crate::definition::WorkspaceTool::ReadFile,
    ];
    def.max_model_rounds = 4;
    def.max_tool_calls = 4;

    let session = persistent_host
        .create(PreparedCreate::persistent(def.clone(), Context::new()))
        .await
        .unwrap();
    let r = session
        .submit(SubmitRequest {
            request_key: "list-and-read".into(),
            blocks: short_request(),
        })
        .unwrap();
    finished(&session, &r.work).await;
    session.wait_saved().await.unwrap();

    assert!(
        session.history().is_empty(),
        "entry spilling produces no elide rewrite records"
    );
    let blocks = session.read_materials().unwrap().context;
    let tool_results: Vec<_> = blocks
        .blocks()
        .iter()
        .filter_map(|b| match b.content() {
            causa::kernel::BlockContent::ToolResult(res) => Some(res),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 2);

    // First result (list_directory) was spilled by after processor
    let spilled = &tool_results[0].output;
    assert_eq!(spilled.truncation, causa::kernel::Truncation::Middle);
    assert!(spilled.meta.as_ref().unwrap().original_tokens.unwrap() > 4_000);
    let art = spilled.artifact.as_ref().expect("list_directory spilled");
    assert!(art.id.starts_with("artifact:"));
    let preview = spilled.content.as_str().unwrap();
    assert!(preview.contains(&format!("recall id={}", art.id)));
    let full = session.read_spilled_artifact(&art.id).unwrap();
    let full_count = (1..=150)
        .filter(|i| full.contains(&format!("entry-{i:03}-")))
        .count();
    let preview_count = (1..=150)
        .filter(|i| preview.contains(&format!("entry-{i:03}-")))
        .count();
    assert_eq!(full_count, 150);
    assert!(
        preview_count < 60,
        "preview only keeps head/tail: {preview_count}"
    );

    // Second result (huge.txt) hit read_file's internal output_bytes cap (not relaxed!)
    let capped_obj = tool_results[1].output.content.as_object();
    assert_eq!(
        capped_obj
            .and_then(|o| o.get("limit_reason"))
            .and_then(|v| v.as_str()),
        Some("output_bytes"),
        "built-in read_file internal cap must not be relaxed: {:?}",
        tool_results[1].output.content
    );

    persistent_host
        .close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // 2. Transient run: no artifact store / spill processor, list_directory stays intact
    let transient_host = SessionHost::new(None);
    transient_host.set_test_gateway(Some(Arc::new(ScriptedWorkspaceGateway {
        calls: vec![("list_directory", serde_json::json!({ "path": "many" }))],
        step: AtomicUsize::new(0),
    })));
    let transient = transient_host
        .create(PreparedCreate::transient(def, Context::new()))
        .await
        .unwrap();
    let rt = transient
        .submit(SubmitRequest {
            request_key: "list-transient".into(),
            blocks: short_request(),
        })
        .unwrap();
    finished(&transient, &rt.work).await;
    transient.wait_saved().await.unwrap();

    let t_blocks = transient.read_materials().unwrap().context;
    let t_res = t_blocks
        .blocks()
        .iter()
        .find_map(|b| match b.content() {
            causa::kernel::BlockContent::ToolResult(res) => Some(res),
            _ => None,
        })
        .unwrap();
    assert_eq!(t_res.output.truncation, causa::kernel::Truncation::None);
    assert!(t_res.output.artifact.is_none());
    let t_json = t_res.output.content.to_string();
    assert_eq!(
        (1..=150)
            .filter(|i| t_json.contains(&format!("entry-{i:03}-")))
            .count(),
        150,
        "transient session must keep full output in Context"
    );
    transient_host
        .close(&transient.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be17_spill_recall_and_subsequent_summary_uses_preview_not_original() {
    use super::support::is_summary_request;

    struct SpillRecallGateway {
        phase: AtomicUsize,
        summary_requests: Arc<Mutex<Vec<String>>>,
        recall_outputs: Arc<Mutex<Vec<String>>>,
        art_id: Mutex<Option<String>>,
        record_id: Mutex<Option<String>>,
        omitted_marker: Mutex<Option<String>>,
    }

    #[async_trait::async_trait]
    impl ModelGateway for SpillRecallGateway {
        async fn invoke(
            &self,
            request: &causa::kernel::ModelRequest,
            _control: &causa::kernel::CallControl,
        ) -> Result<causa::kernel::ModelOutput, causa::kernel::ModelInvokeError> {
            if is_summary_request(request) {
                let rendered = serde_json::to_string(&request.frame.blocks).unwrap();
                self.summary_requests.lock().unwrap().push(rendered);
                let mut out = causa::kernel::ModelOutput::new(
                    causa::kernel::ModelResponse {
                        items: vec![causa::kernel::ModelItem::text("Summary of spilled read.")],
                    },
                    causa::kernel::ModelStopReason::EndTurn,
                );
                out.usage = Some(causa::kernel::ModelUsage::new(300, 30));
                return Ok(out);
            }
            let phase = self.phase.fetch_add(1, Ordering::SeqCst);
            let item = match phase {
                // Work 1, round 0: call list_directory on many -> spills at entry
                0 => causa::kernel::ModelItem::tool_call(causa::kernel::ToolCallDraft::new(
                    "list_directory",
                    serde_json::json!({ "path": "many" }),
                )),
                // Work 1, round 1: observe preview, finish turn
                1 => {
                    let frame_json = serde_json::to_string(&request.frame.blocks).unwrap();
                    assert!(frame_json.contains("output spilled"));
                    causa::kernel::ModelItem::text("spilled preview observed")
                }
                // Work 2 (after manual compact), round 0: call recall on the history record id
                2 => {
                    let rec_id = self.record_id.lock().unwrap().clone().unwrap();
                    causa::kernel::ModelItem::tool_call(causa::kernel::ToolCallDraft::new(
                        "recall",
                        serde_json::json!({ "id": rec_id }),
                    ))
                }
                // Work 2, round 1: verify recall list showed artifact:<id>, now call recall on artifact:<id>
                3 => {
                    let frame_json = serde_json::to_string(&request.frame.blocks).unwrap();
                    let art_id = self.art_id.lock().unwrap().clone().unwrap();
                    assert!(
                        frame_json.contains(&format!("artifact: recall id={art_id}")),
                        "recall listing of summarized spilled block must expose artifact id: {frame_json}"
                    );
                    causa::kernel::ModelItem::tool_call(causa::kernel::ToolCallDraft::new(
                        "recall",
                        serde_json::json!({ "id": art_id }),
                    ))
                }
                // Work 2, round 2: recall on artifact:<id> returned full original including omitted entry
                4 => {
                    let frame_json = serde_json::to_string(&request.frame.blocks).unwrap();
                    self.recall_outputs.lock().unwrap().push(frame_json);
                    causa::kernel::ModelItem::text("verified full original via recall")
                }
                _ => causa::kernel::ModelItem::text("done"),
            };
            let stop = if matches!(phase, 0 | 2 | 3) {
                causa::kernel::ModelStopReason::ToolUse
            } else {
                causa::kernel::ModelStopReason::EndTurn
            };
            let mut out = causa::kernel::ModelOutput::new(
                causa::kernel::ModelResponse { items: vec![item] },
                stop,
            );
            out.usage = Some(causa::kernel::ModelUsage::new(400, 30));
            Ok(out)
        }
    }

    let workspace = tempfile::tempdir().unwrap();
    let ws_root = workspace.path().canonicalize().unwrap();
    let big_dir = ws_root.join("many");
    std::fs::create_dir(&big_dir).unwrap();
    let pad = "m".repeat(90);
    for i in 1..=95 {
        std::fs::write(big_dir.join(format!("entry-{i:03}-{pad}.txt")), b"").unwrap();
    }

    let summary_requests = Arc::new(Mutex::new(Vec::new()));
    let recall_outputs = Arc::new(Mutex::new(Vec::new()));
    let gateway = Arc::new(SpillRecallGateway {
        phase: AtomicUsize::new(0),
        summary_requests: summary_requests.clone(),
        recall_outputs: recall_outputs.clone(),
        art_id: Mutex::new(None),
        record_id: Mutex::new(None),
        omitted_marker: Mutex::new(None),
    });

    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    host.set_test_gateway(Some(gateway.clone() as Arc<dyn ModelGateway>));

    let mut def = compacting_definition(Some(40_000));
    def.workspace_root = Some(ws_root);
    def.tools = vec![crate::definition::WorkspaceTool::ListDirectory];
    def.max_model_rounds = 6;
    def.max_tool_calls = 6;

    let session = host
        .create(PreparedCreate::persistent(def, Context::new()))
        .await
        .unwrap();

    // Work 1: list_directory -> spilled at entry
    let r1 = session
        .submit(SubmitRequest {
            request_key: "work-list".into(),
            blocks: short_request(),
        })
        .unwrap();
    finished(&session, &r1.work).await;
    session.wait_saved().await.unwrap();

    let (art_id, preview_text) = session
        .read_materials()
        .unwrap()
        .context
        .blocks()
        .iter()
        .find_map(|b| match b.content() {
            causa::kernel::BlockContent::ToolResult(res) => res.output.artifact.as_ref().map(|a| {
                (
                    a.id.clone(),
                    res.output.content.as_str().unwrap().to_owned(),
                )
            }),
            _ => None,
        })
        .expect("spilled artifact present");
    let full_text = session.read_spilled_artifact(&art_id).unwrap();
    // Pick an entry name present in the first 12,000 chars of full_text (page 1 of recall)
    // that was omitted from preview_text.
    let first_page: String = full_text.chars().take(11_000).collect();
    let omitted = (1..=95)
        .map(|i| format!("entry-{i:03}-"))
        .find(|tag| first_page.contains(tag.as_str()) && !preview_text.contains(tag.as_str()))
        .expect("at least one entry omitted from preview on page 1");
    *gateway.art_id.lock().unwrap() = Some(art_id.clone());
    *gateway.omitted_marker.lock().unwrap() = Some(omitted.clone());

    // Compact: brief request must receive ONLY the preview, never re-pull the full original
    let outcome = session.compact().await.unwrap();
    session.wait_saved().await.unwrap();
    *gateway.record_id.lock().unwrap() = Some(outcome.record.record_id.clone());

    {
        let summaries = summary_requests.lock().unwrap();
        assert_eq!(summaries.len(), 1);
        assert!(summaries[0].contains("output spilled"));
        assert!(summaries[0].contains(&format!("recall id={art_id}")));
        assert!(
            !summaries[0].contains(&omitted),
            "summary brief request must use preview and never re-pull full artifact"
        );
    }

    // Work 2: recall lists record (seeing artifact pointer) -> reads artifact:<id> -> gets omitted entry
    let r2 = session
        .submit(SubmitRequest {
            request_key: "work-recall".into(),
            blocks: short_request(),
        })
        .unwrap();
    finished(&session, &r2.work).await;
    session.wait_saved().await.unwrap();

    {
        let recall_seen = recall_outputs.lock().unwrap();
        assert_eq!(recall_seen.len(), 1);
        assert!(recall_seen[0].contains(&omitted));
    }

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be18_abandoned_compaction_and_auto_off_trim_frame_when_over_window() {
    use crate::compact::CompactionProgress;

    // 1. Brief request fails, and frame exceeds window (estimate + max_tokens > window):
    //    frame is trimmed at cut points, U and head stay, Context and .history/ stay untouched.
    {
        let store = tempfile::tempdir().unwrap();
        let host = SessionHost::new(Some(store.path().into()));
        let gateway = scripted(SummaryBehavior::Fail);
        host.set_test_gateway(as_gateway(&gateway));
        let mut def = compacting_definition(Some(1_600));
        def.max_tokens = Some(200);
        let session = host
            .create(PreparedCreate::persistent(def, earlier_conversation()))
            .await
            .unwrap();
        let mut progress = session.subscribe_progress();
        let request = short_request();
        let request_id = request[0].id();
        let receipt = session
            .submit(SubmitRequest {
                request_key: "be18-fail".into(),
                blocks: request,
            })
            .unwrap();
        finished(&session, &receipt.work).await;
        session.wait_saved().await.unwrap();

        let id = session.target().conversation_id().as_str().to_owned();
        assert!(session.history().is_empty());
        assert_eq!(
            std::fs::read_dir(history_dir(store.path(), &id))
                .map(|entries| entries.count())
                .unwrap_or(0),
            0
        );
        // Context still has all 4 earlier + request + ToolChange(recall) + answer = 7 blocks.
        assert_eq!(session.read_materials().unwrap().context.blocks().len(), 7);

        {
            let sent = gateway.main_inputs.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert!(sent[0].len() < 6, "oldest intervals were dropped");
            assert!(sent[0].iter().any(|b| b.id() == request_id), "U is kept");
            assert!(
                crate::compact::segment::blocks_estimate(&sent[0]) + 200 <= 1_600,
                "trimmed frame + max_tokens fits window"
            );
        }

        let mut trimmed_events = 0;
        while let Ok(event) = progress.try_recv() {
            if let crate::session::ProgressUpdate::Compaction(CompactionProgress::Trimmed {
                before_tokens,
                after_tokens,
            }) = &event.update
            {
                assert!(*before_tokens + 200 > 1_600);
                assert!(*after_tokens + 200 <= 1_600);
                trimmed_events += 1;
            }
        }
        assert_eq!(trimmed_events, 1);

        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }

    // 2. Record write failure (`UnwritableHistory`) when frame > window -> trims frame without touching Context.
    {
        let store = tempfile::tempdir().unwrap();
        let host = SessionHost::new(Some(store.path().into()));
        let gateway = scripted(SummaryBehavior::Text("brief that cannot be saved".into()));
        host.set_test_gateway(as_gateway(&gateway));
        let mut def = compacting_definition(Some(1_600));
        def.max_tokens = Some(200);
        let session = host
            .create(PreparedCreate::persistent(def, earlier_conversation()))
            .await
            .unwrap();
        let id = session.target().conversation_id().as_str().to_owned();
        std::fs::write(history_dir(store.path(), &id), b"not a directory").unwrap();
        let mut progress = session.subscribe_progress();
        let receipt = session
            .submit(SubmitRequest {
                request_key: "be18-unwritable".into(),
                blocks: short_request(),
            })
            .unwrap();
        finished(&session, &receipt.work).await;
        session.wait_saved().await.unwrap();

        assert!(session.history().is_empty());
        assert_eq!(session.read_materials().unwrap().context.blocks().len(), 7);
        {
            let sent = gateway.main_inputs.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert!(crate::compact::segment::blocks_estimate(&sent[0]) + 200 <= 1_600);
        }
        let mut trimmed_events = 0;
        while let Ok(event) = progress.try_recv() {
            if matches!(
                &event.update,
                crate::session::ProgressUpdate::Compaction(CompactionProgress::Trimmed { .. })
            ) {
                trimmed_events += 1;
            }
        }
        assert_eq!(trimmed_events, 1);

        std::fs::remove_file(history_dir(store.path(), &id)).unwrap();
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }

    // 3. `auto_compact = off` with known window -> no summary call, trims when frame > window.
    {
        let store = tempfile::tempdir().unwrap();
        let host = SessionHost::new(Some(store.path().into()));
        let gateway = scripted(SummaryBehavior::Text("unused".into()));
        host.set_test_gateway(as_gateway(&gateway));
        let mut def = compacting_definition(Some(1_600));
        def.auto_compact = false;
        def.max_tokens = Some(200);
        let session = host
            .create(PreparedCreate::persistent(def, earlier_conversation()))
            .await
            .unwrap();
        let mut progress = session.subscribe_progress();
        let receipt = session
            .submit(SubmitRequest {
                request_key: "be18-auto-off".into(),
                blocks: short_request(),
            })
            .unwrap();
        finished(&session, &receipt.work).await;
        session.wait_saved().await.unwrap();

        assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 0);
        assert!(session.history().is_empty());
        assert_eq!(session.read_materials().unwrap().context.blocks().len(), 7);
        {
            let sent = gateway.main_inputs.lock().unwrap();
            assert!(crate::compact::segment::blocks_estimate(&sent[0]) + 200 <= 1_600);
        }
        let mut trimmed_events = 0;
        while let Ok(event) = progress.try_recv() {
            if matches!(
                &event.update,
                crate::session::ProgressUpdate::Compaction(CompactionProgress::Trimmed { .. })
            ) {
                trimmed_events += 1;
            }
        }
        assert_eq!(trimmed_events, 1);
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }

    // 4. Rewrite succeeds, but rewritten frame (head + retained tail + U + max_tokens) still exceeds
    //    window -> trims the retained tail on a copy without modifying the rewritten Context.
    {
        let store = tempfile::tempdir().unwrap();
        let host = SessionHost::new(Some(store.path().into()));
        let gateway = scripted(SummaryBehavior::Text("brief ".repeat(900)));
        host.set_test_gateway(as_gateway(&gateway));
        let mut def = compacting_definition(Some(10_000));
        def.max_tokens = Some(7_500);
        let tail_user = text_block(1_000);
        let tail_answer = ContextBlock::text(
            BlockId::new(uuid::Uuid::now_v7()),
            causa::kernel::Role::Assistant,
            "tail answer ".repeat(80),
        );
        let context = Context::from_blocks(vec![
            text_block(6_000),
            text_block(6_000),
            text_block(6_000),
            text_block(6_000),
            tail_user.clone(),
            tail_answer.clone(),
        ])
        .unwrap();
        let session = host
            .create(PreparedCreate::persistent(def, context))
            .await
            .unwrap();
        let mut progress = session.subscribe_progress();
        let request = short_request();
        let request_id = request[0].id();
        let receipt = session
            .submit(SubmitRequest {
                request_key: "be18-still-over".into(),
                blocks: request,
            })
            .unwrap();
        finished(&session, &receipt.work).await;
        session.wait_saved().await.unwrap();

        assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(session.history().len(), 1);
        // Context keeps the brief + retained tail + U + answer.
        let saved_blocks = session.read_materials().unwrap().context;
        assert!(
            saved_blocks
                .blocks()
                .iter()
                .any(|b| b.id() == tail_user.id())
        );
        assert!(
            saved_blocks
                .blocks()
                .iter()
                .any(|b| b.id() == tail_answer.id())
        );

        {
            let sent = gateway.main_inputs.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert!(
                !sent[0].iter().any(|b| b.id() == tail_user.id()),
                "retained tail interval dropped from frame so frame + max_tokens fits window"
            );
            assert!(sent[0].iter().any(|b| b.id() == request_id), "U is kept");
            assert!(
                crate::compact::segment::blocks_estimate(&sent[0]) + 7_500 <= 10_000,
                "trimmed frame + max_tokens fits window"
            );
        }

        let mut finished_count = 0;
        let mut trimmed_count = 0;
        while let Ok(event) = progress.try_recv() {
            if let crate::session::ProgressUpdate::Compaction(update) = &event.update {
                match update {
                    CompactionProgress::Finished { .. } => finished_count += 1,
                    CompactionProgress::Trimmed { .. } => trimmed_count += 1,
                    _ => {}
                }
            }
        }
        assert_eq!((finished_count, trimmed_count), (1, 1));
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }

    // 5. Anti-example: when frame fits window (`estimate + max_tokens <= window`), never trim.
    {
        let store = tempfile::tempdir().unwrap();
        let host = SessionHost::new(Some(store.path().into()));
        let gateway = scripted(SummaryBehavior::Text("unused".into()));
        host.set_test_gateway(as_gateway(&gateway));
        let mut def = compacting_definition(Some(10_000));
        def.auto_compact = false;
        def.max_tokens = Some(500);
        let session = host
            .create(PreparedCreate::persistent(def, earlier_conversation()))
            .await
            .unwrap();
        let mut progress = session.subscribe_progress();
        let receipt = session
            .submit(SubmitRequest {
                request_key: "be18-fits".into(),
                blocks: short_request(),
            })
            .unwrap();
        finished(&session, &receipt.work).await;
        session.wait_saved().await.unwrap();
        {
            let sent = gateway.main_inputs.lock().unwrap();
            // All 4 earlier + short_request + recall ToolChange = 6 blocks sent verbatim.
            assert_eq!(sent[0].len(), 6);
        }
        while let Ok(event) = progress.try_recv() {
            assert!(!matches!(
                &event.update,
                crate::session::ProgressUpdate::Compaction(CompactionProgress::Trimmed { .. })
            ));
        }
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }

    // 6. Empty/whitespace brief (`EmptySummary`) and `NoShrink` brief when frame > window ->
    //    both abandon without touching Context or history and fall back to trimming.
    for (key, behavior, expected_reason) in [
        (
            "be18-empty",
            SummaryBehavior::Text("   ".into()),
            crate::compact::CompactionAbandon::EmptySummary,
        ),
        (
            "be18-noshrink",
            SummaryBehavior::Text("oversized brief ".repeat(400)),
            crate::compact::CompactionAbandon::NoShrink,
        ),
    ] {
        let store = tempfile::tempdir().unwrap();
        let host = SessionHost::new(Some(store.path().into()));
        let gateway = scripted(behavior);
        host.set_test_gateway(as_gateway(&gateway));
        let mut def = compacting_definition(Some(1_600));
        def.max_tokens = Some(200);
        let session = host
            .create(PreparedCreate::persistent(def, earlier_conversation()))
            .await
            .unwrap();
        let mut progress = session.subscribe_progress();
        let request = short_request();
        let request_id = request[0].id();
        let receipt = session
            .submit(SubmitRequest {
                request_key: key.into(),
                blocks: request,
            })
            .unwrap();
        finished(&session, &receipt.work).await;
        session.wait_saved().await.unwrap();

        assert!(session.history().is_empty(), "{key}");
        assert_eq!(session.read_materials().unwrap().context.blocks().len(), 7);
        {
            let sent = gateway.main_inputs.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert!(sent[0].iter().any(|b| b.id() == request_id));
            assert!(crate::compact::segment::blocks_estimate(&sent[0]) + 200 <= 1_600);
        }
        let (mut abandoned, mut trimmed) = (0, 0);
        while let Ok(event) = progress.try_recv() {
            if let crate::session::ProgressUpdate::Compaction(update) = &event.update {
                match update {
                    CompactionProgress::Abandoned { reason, .. } if *reason == expected_reason => {
                        abandoned += 1;
                    }
                    CompactionProgress::Trimmed { .. } => trimmed += 1,
                    _ => {}
                }
            }
        }
        assert_eq!((abandoned, trimmed), (1, 1), "{key}");
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_be19_transient_with_known_window_trims_without_history_or_artifacts() {
    use crate::compact::CompactionProgress;

    // Known window: trims at cut points, keeps U, keeps Context untouched, creates neither .history/ nor .artifacts/.
    let store = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(store.path().into()));
    let gateway = scripted(SummaryBehavior::Text("unused".into()));
    host.set_test_gateway(as_gateway(&gateway));
    let mut def = compacting_definition(Some(1_000));
    def.max_tokens = Some(100);
    let session = host
        .create(PreparedCreate::transient(def, earlier_conversation()))
        .await
        .unwrap();
    let id = session.target().conversation_id().as_str().to_owned();
    let mut progress = session.subscribe_progress();
    let request = short_request();
    let request_id = request[0].id();
    let receipt = session
        .submit(SubmitRequest {
            request_key: "be19-known".into(),
            blocks: request,
        })
        .unwrap();
    finished(&session, &receipt.work).await;
    until(|| session.status().active_work.is_none()).await;

    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 0);
    assert!(!history_dir(store.path(), &id).exists());
    assert!(
        !store
            .path()
            .join("sessions")
            .join(format!("{id}.artifacts"))
            .exists()
    );
    assert!(session.artifacts().is_none());
    assert!(session.compaction_settings().is_none());
    // Full Context still holds all 4 earlier blocks + request + answer = 6 blocks.
    assert_eq!(session.read_materials().unwrap().context.blocks().len(), 6);

    {
        let sent = gateway.main_inputs.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert!(sent[0].len() < 5);
        assert!(
            sent[0].iter().any(|b| b.id() == request_id),
            "U is protected"
        );
        assert!(crate::compact::segment::blocks_estimate(&sent[0]) + 100 <= 1_000);
    }

    let mut trimmed = 0;
    while let Ok(event) = progress.try_recv() {
        if matches!(
            &event.update,
            crate::session::ProgressUpdate::Compaction(CompactionProgress::Trimmed { .. })
        ) {
            trimmed += 1;
        }
    }
    assert_eq!(trimmed, 1);
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();

    // Unknown window: no trimming preparer, full frame sent unchanged.
    let gateway_unknown = scripted(SummaryBehavior::Text("unused".into()));
    host.set_test_gateway(as_gateway(&gateway_unknown));
    let session_unknown = host
        .create(PreparedCreate::transient(
            compacting_definition(None),
            earlier_conversation(),
        ))
        .await
        .unwrap();
    let receipt = session_unknown
        .submit(SubmitRequest {
            request_key: "be19-unknown".into(),
            blocks: short_request(),
        })
        .unwrap();
    finished(&session_unknown, &receipt.work).await;
    until(|| session_unknown.status().active_work.is_none()).await;
    assert_eq!(gateway_unknown.main_inputs.lock().unwrap()[0].len(), 5);
    host.close(&session_unknown.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

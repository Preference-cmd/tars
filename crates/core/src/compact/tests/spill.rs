//! Tests for BE16 (tool output spilling at entry, artifact store, Elide removal)
//! and BE17 (recall reading spilled artifacts).

use super::super::artifacts::HistoryArtifacts;
use super::super::estimate;
use super::super::history::{HistoryRecord, HistoryStore, MAX_ARTIFACT_BYTES, RewriteKind};
use super::super::spill::{
    OutputSpillProcessor, SPILL_PREVIEW_HEAD_LINES, SPILL_PREVIEW_MAX_TOKENS,
    SPILL_PREVIEW_TAIL_LINES, SPILL_THRESHOLD_TOKENS,
};
use super::{recall_call, recall_control, setup_recall_test};
use async_trait::async_trait;
use causa::kernel::{
    ArtifactHint, ArtifactKind, ArtifactRef, ArtifactStore, BlockId, CallControl,
    CancellationToken, MediaRef, ProcessorContext, RoundId, StoreError, TextPayload, Tool,
    ToolBatch, ToolBatchProcessor, ToolCallContext, ToolCallPayload, ToolOutput, ToolOutputMeta,
    ToolResultPayload, ToolResultStatus, Truncation, TurnId,
};
use std::ops::Range;
use std::sync::Arc;

struct FailingArtifactStore;

#[async_trait]
impl ArtifactStore for FailingArtifactStore {
    async fn persist(&self, _data: &[u8], _hint: ArtifactHint) -> Result<ArtifactRef, StoreError> {
        Err(StoreError::Persist("simulated disk full".into()))
    }

    async fn read(&self, _id: &str, _range: Option<Range<u64>>) -> Result<Vec<u8>, StoreError> {
        Err(StoreError::Read("not implemented".into()))
    }
}

type BatchCall<'a> = (
    &'a str,
    ToolResultStatus,
    ToolOutput,
    Vec<MediaRef>,
    Vec<TextPayload>,
);

fn batch_for(calls: Vec<BatchCall<'_>>) -> (ToolBatch, Vec<BlockId>) {
    let mut call_ctxs = Vec::with_capacity(calls.len());
    let mut call_ids = Vec::with_capacity(calls.len());
    for (name, _, _, _, _) in &calls {
        let id = BlockId::new(uuid::Uuid::now_v7());
        call_ids.push(id);
        call_ctxs.push(ToolCallContext::new(
            id,
            ToolCallPayload::new(*name, serde_json::json!({})),
        ));
    }
    let mut batch = ToolBatch::new(call_ctxs).unwrap();
    for (index, (_, status, output, media, notes)) in calls.into_iter().enumerate() {
        let result_id = BlockId::new(uuid::Uuid::now_v7());
        let payload = ToolResultPayload::new(call_ids[index], status, output)
            .with_media(media)
            .with_notes(notes);
        batch.resolve_at(index, result_id, payload).unwrap();
    }
    (batch, call_ids)
}

#[tokio::test]
async fn test_be16_artifacts_persist_atomic_and_read_ranges_and_64mib_cap() {
    let dir = tempfile::tempdir().unwrap();
    let conv_id = "0190a000-0000-7000-8000-000000000016";
    let store = Arc::new(HistoryStore::new(dir.path(), conv_id, Vec::new()));
    let artifacts = HistoryArtifacts::new(store.clone());

    let content = b"alpha\nbeta\ngamma\ndelta\n";
    let hint = ArtifactHint::new(
        "read_file",
        BlockId::new(uuid::Uuid::now_v7()),
        ArtifactKind::FullOutput,
    );
    let art_ref = artifacts.persist(content, hint).await.unwrap();
    let artifact_uuid = art_ref
        .id
        .strip_prefix("artifact:")
        .expect("artifact: prefix");
    assert_eq!(art_ref.size_bytes, content.len());
    assert_eq!(art_ref.kind, ArtifactKind::FullOutput);
    assert!(art_ref.persisted);

    let artifacts_dir = dir.path().join(format!("{conv_id}.artifacts"));
    let stored_file = artifacts_dir.join(artifact_uuid);
    assert!(stored_file.is_file());
    assert_eq!(std::fs::read(&stored_file).unwrap(), content);
    let entries: Vec<_> = std::fs::read_dir(&artifacts_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec![artifact_uuid.to_owned()]);

    // Range reads (full, slice, clamped, invalid)
    assert_eq!(artifacts.read(&art_ref.id, None).await.unwrap(), content);
    assert_eq!(
        artifacts.read(&art_ref.id, Some(6..10)).await.unwrap(),
        b"beta"
    );
    assert_eq!(
        artifacts.read(&art_ref.id, Some(17..999)).await.unwrap(),
        b"delta\n"
    );
    assert!(matches!(
        artifacts.read(&art_ref.id, Some(100..105)).await,
        Err(StoreError::Read(_))
    ));
    assert!(matches!(
        artifacts
            .read(&art_ref.id, Some(Range { start: 8, end: 4 }))
            .await,
        Err(StoreError::Read(_))
    ));

    // Reject oversized (> 64 MiB) artifact on read
    let huge_id = uuid::Uuid::now_v7().to_string();
    let huge_path = artifacts_dir.join(&huge_id);
    let huge_file = std::fs::File::create(&huge_path).unwrap();
    huge_file.set_len((MAX_ARTIFACT_BYTES as u64) + 1).unwrap();
    let huge_err = artifacts
        .read(&format!("artifact:{huge_id}"), None)
        .await
        .unwrap_err();
    assert!(matches!(huge_err, StoreError::Read(_)));
}

#[tokio::test]
async fn test_be16_after_processor_spills_multiline_and_structured_outputs_and_preserves_fields() {
    let dir = tempfile::tempdir().unwrap();
    let conv_id = "0190a000-0000-7000-8000-000000000017";
    let store = Arc::new(HistoryStore::new(dir.path(), conv_id, Vec::new()));
    let artifacts = Arc::new(HistoryArtifacts::new(store));
    let processor = OutputSpillProcessor::new(artifacts.clone());

    // 1. Multiline string > 4k tokens (> 12,000 bytes), 120 lines
    let multiline: String = (1..=120)
        .map(|i| format!("line-{i:03}: {}", "x".repeat(120)))
        .collect::<Vec<_>>()
        .join("\n");
    let expected_tokens = estimate::estimate_bytes(multiline.len() as u64);
    assert!(expected_tokens > SPILL_THRESHOLD_TOKENS);

    let mut out1 = ToolOutput::new(serde_json::Value::String(multiline.clone()));
    let mut meta1 = ToolOutputMeta::default();
    meta1.duration_ms = Some(42);
    out1.meta = Some(meta1);
    let media1 = vec![MediaRef::new("image/png", "img-keep")];
    let notes1 = vec![TextPayload::new("pre-existing note")];

    // 2. Structured JSON object > 4k tokens
    let big_obj = serde_json::json!({
        "path": "src/main.rs",
        "text": "中文内容 ".repeat(2_000),
    });
    let out2 = ToolOutput::new(big_obj.clone());

    let (mut batch, ids) = batch_for(vec![
        (
            "run_command",
            ToolResultStatus::Failed,
            out1,
            media1.clone(),
            notes1.clone(),
        ),
        (
            "read_file",
            ToolResultStatus::Succeeded,
            out2,
            Vec::new(),
            Vec::new(),
        ),
    ]);

    let turn_id = TurnId::new("t1");
    let control = CallControl::new(CancellationToken::new(), None);
    let ctx = ProcessorContext::new(&turn_id, RoundId(1), &ids, &control);
    processor.process(&mut batch, &ctx).await.unwrap();

    // Check first result (multiline string)
    let (_, res1) = batch.results()[0].result().unwrap();
    assert_eq!(res1.status, ToolResultStatus::Failed);
    assert_eq!(res1.media, media1);
    assert_eq!(res1.notes, notes1);
    assert_eq!(res1.output.truncation, Truncation::Middle);
    let meta = res1.output.meta.as_ref().unwrap();
    assert_eq!(meta.duration_ms, Some(42));
    assert_eq!(
        meta.original_tokens,
        Some(usize::try_from(expected_tokens).unwrap())
    );
    let art1 = res1.output.artifact.as_ref().expect("spilled artifact ref");
    assert!(art1.id.starts_with("artifact:"));
    assert_eq!(art1.size_bytes, multiline.len());
    assert_eq!(art1.kind, ArtifactKind::FullOutput);
    assert!(art1.persisted);
    let preview1 = res1.output.content.as_str().expect("string preview");
    assert!(preview1.contains(&format!("recall id={}", art1.id)));
    assert!(preview1.contains(&format!("{} bytes", multiline.len())));
    assert!(preview1.contains("line-001:"));
    assert!(preview1.contains("line-120:"));
    assert!(!preview1.contains("line-060:"));
    assert!(estimate::estimate_bytes(preview1.len() as u64) <= SPILL_PREVIEW_MAX_TOKENS);
    // Count head/tail lines around notice
    let (head_part, tail_part) = preview1.split_once("[... output spilled:").unwrap();
    let head_lines = head_part.trim_end_matches('\n').lines().count();
    let tail_after_notice = tail_part.split_once("...]\n").unwrap().1;
    let tail_lines = tail_after_notice.lines().count();
    assert!(head_lines <= SPILL_PREVIEW_HEAD_LINES);
    assert!(tail_lines <= SPILL_PREVIEW_TAIL_LINES);
    assert_eq!(
        String::from_utf8(artifacts.read(&art1.id, None).await.unwrap()).unwrap(),
        multiline
    );

    // Check second result (structured JSON with CJK)
    let (_, res2) = batch.results()[1].result().unwrap();
    assert_eq!(res2.output.truncation, Truncation::Middle);
    let art2 = res2
        .output
        .artifact
        .as_ref()
        .expect("spilled json artifact");
    let preview2 = res2.output.content.as_str().expect("string preview");
    assert!(preview2.contains(&format!("recall id={}", art2.id)));
    assert!(estimate::estimate_bytes(preview2.len() as u64) <= SPILL_PREVIEW_MAX_TOKENS);
    assert_eq!(
        String::from_utf8(artifacts.read(&art2.id, None).await.unwrap()).unwrap(),
        serde_json::to_string_pretty(&big_obj).unwrap()
    );
}

#[tokio::test]
async fn test_be16_after_processor_skips_under_threshold_recall_and_existing_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let conv_id = "0190a000-0000-7000-8000-000000000018";
    let store = Arc::new(HistoryStore::new(dir.path(), conv_id, Vec::new()));
    let artifacts = Arc::new(HistoryArtifacts::new(store));
    let processor = OutputSpillProcessor::new(artifacts);

    let at_threshold = "a".repeat((SPILL_THRESHOLD_TOKENS as usize) * 3); // exactly 4,000 tokens
    let recall_large = "r".repeat(20_000);
    let mut existing_out = ToolOutput::new(serde_json::Value::String("e".repeat(20_000)));
    let existing_ref = ArtifactRef::new(
        format!("artifact:{}", uuid::Uuid::now_v7()),
        20_000,
        ArtifactKind::FullOutput,
        true,
    );
    existing_out.artifact = Some(existing_ref.clone());

    let (mut batch, ids) = batch_for(vec![
        (
            "read_file",
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::Value::String(at_threshold.clone())),
            Vec::new(),
            Vec::new(),
        ),
        (
            "recall",
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::Value::String(recall_large.clone())),
            Vec::new(),
            Vec::new(),
        ),
        (
            "custom_tool",
            ToolResultStatus::Succeeded,
            existing_out,
            Vec::new(),
            Vec::new(),
        ),
    ]);

    let turn_id = TurnId::new("t1");
    let control = CallControl::new(CancellationToken::new(), None);
    let ctx = ProcessorContext::new(&turn_id, RoundId(1), &ids, &control);
    processor.process(&mut batch, &ctx).await.unwrap();

    // 1. At threshold: untouched
    let (_, r0) = batch.results()[0].result().unwrap();
    assert_eq!(r0.output.truncation, Truncation::None);
    assert!(r0.output.artifact.is_none());
    assert_eq!(r0.output.content.as_str().unwrap(), at_threshold);

    // 2. recall self: untouched
    let (_, r1) = batch.results()[1].result().unwrap();
    assert_eq!(r1.output.truncation, Truncation::None);
    assert!(r1.output.artifact.is_none());
    assert_eq!(r1.output.content.as_str().unwrap(), recall_large);

    // 3. Existing artifact: untouched
    let (_, r2) = batch.results()[2].result().unwrap();
    assert_eq!(r2.output.artifact.as_ref(), Some(&existing_ref));
    assert_eq!(r2.output.truncation, Truncation::None);

    // No .artifacts directory should even have been created
    assert!(!dir.path().join(format!("{conv_id}.artifacts")).exists());
}

#[tokio::test]
async fn test_be16_after_processor_degrades_on_persist_failure_without_failing_batch() {
    let processor = OutputSpillProcessor::new(Arc::new(FailingArtifactStore));
    let large = "z".repeat(20_000);
    let (mut batch, ids) = batch_for(vec![(
        "run_command",
        ToolResultStatus::Succeeded,
        ToolOutput::new(serde_json::Value::String(large.clone())),
        Vec::new(),
        Vec::new(),
    )]);

    let turn_id = TurnId::new("t1");
    let control = CallControl::new(CancellationToken::new(), None);
    let ctx = ProcessorContext::new(&turn_id, RoundId(1), &ids, &control);
    processor.process(&mut batch, &ctx).await.unwrap();

    let (_, res) = batch.results()[0].result().unwrap();
    assert_eq!(res.status, ToolResultStatus::Succeeded);
    assert_eq!(res.output.truncation, Truncation::None);
    assert!(res.output.artifact.is_none());
    assert_eq!(res.output.content.as_str().unwrap(), large);
}

#[test]
fn test_be16_elide_kind_is_rejected_on_decode() {
    let raw = serde_json::json!({
        "format": "tars-history-v1.1",
        "conversation_id": uuid::Uuid::now_v7().to_string(),
        "record_id": uuid::Uuid::now_v7().to_string(),
        "created_at_ms": 1,
        "trigger": "auto",
        "kind": "elide",
        "estimate_before": 100,
        "estimate_after": 20,
        "replacements": []
    });
    assert!(serde_json::from_value::<HistoryRecord>(raw).is_err());
    assert!(serde_json::from_value::<RewriteKind>(serde_json::json!("elide")).is_err());
}

#[tokio::test]
async fn test_be17_recall_reads_spilled_artifact_with_cjk_paging_output_cap_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _runtime, tool) = setup_recall_test(dir.path());
    let artifacts = HistoryArtifacts::new(store);
    let control = recall_control();

    // 1. Multi-byte CJK spilled artifact spanning multiple pages (> 12,000 chars)
    let cjk_text = format!(
        "{}{}{}",
        "前".repeat(12_000),
        "中".repeat(12_000),
        "尾".repeat(500)
    );
    let art_ref = artifacts
        .persist(
            cjk_text.as_bytes(),
            ArtifactHint::new(
                "read_file",
                BlockId::new(uuid::Uuid::now_v7()),
                ArtifactKind::FullOutput,
            ),
        )
        .await
        .unwrap();

    // Page 1 (default limit 12,000 chars)
    let res1 = tool
        .execute(
            &recall_call(serde_json::json!({ "id": art_ref.id })),
            &control,
            None,
        )
        .await;
    assert_eq!(res1.status, ToolResultStatus::Succeeded);
    let text1 = res1.output.content.as_str().unwrap();
    assert!(text1.contains("24500 chars, range 0..12000"));
    assert!(text1.contains(&"前".repeat(100)));
    assert!(!text1.contains('中'));
    assert!(text1.contains(&format!("more: recall id={} offset=12000", art_ref.id)));

    // Page 2 (offset=12000, limit=12000)
    let res2 = tool
        .execute(
            &recall_call(
                serde_json::json!({ "id": art_ref.id, "offset": 12_000, "limit": 12_000 }),
            ),
            &control,
            None,
        )
        .await;
    assert_eq!(res2.status, ToolResultStatus::Succeeded);
    let text2 = res2.output.content.as_str().unwrap();
    assert!(text2.contains("24500 chars, range 12000..24000"));
    assert!(text2.contains(&"中".repeat(100)));
    assert!(!text2.contains('前'));
    assert!(!text2.contains('尾'));
    assert!(text2.contains(&format!("more: recall id={} offset=24000", art_ref.id)));

    // Page 3 (offset=24000 -> final 500 chars, no more pointer)
    let res3 = tool
        .execute(
            &recall_call(serde_json::json!({ "id": art_ref.id, "offset": 24_000 })),
            &control,
            None,
        )
        .await;
    assert_eq!(res3.status, ToolResultStatus::Succeeded);
    let text3 = res3.output.content.as_str().unwrap();
    assert!(text3.contains("24500 chars, range 24000..24500"));
    assert!(text3.contains(&"尾".repeat(500)));
    assert!(!text3.contains("more: recall"));

    // 2. Output cap: 12,000 4-byte emoji chars fits under 64 KiB, while missing / malformed fail cleanly
    let missing_id = format!("artifact:{}", uuid::Uuid::now_v7());
    let missing_res = tool
        .execute(
            &recall_call(serde_json::json!({ "id": missing_id })),
            &control,
            None,
        )
        .await;
    assert_eq!(missing_res.status, ToolResultStatus::Failed);
    let err_str = missing_res.output.content.to_string();
    assert!(!err_str.contains(".artifacts"));
    assert!(!err_str.contains('/'));

    let bad_res = tool
        .execute(
            &recall_call(serde_json::json!({ "id": "artifact:../escape" })),
            &control,
            None,
        )
        .await;
    assert_eq!(bad_res.status, ToolResultStatus::Failed);
}

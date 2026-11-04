use super::*;
use causa::kernel::{
    ArtifactKind, ArtifactRef, BlockContent, BlockMeta, ContentPart, Context, ContextBlock,
    MediaRef, ToolOutput, ToolResultPayload, ToolResultStatus,
};
use causa::runtime::new_block_id;
use std::fs;

fn block(content: BlockContent) -> ContextBlock {
    ContextBlock::new(new_block_id(), content, BlockMeta::default())
}
fn text_context() -> Context {
    Context::from_blocks(vec![block(BlockContent::text(
        causa::kernel::Role::Assistant,
        "saved material",
    ))])
    .unwrap()
}

#[test]
fn text_round_trip_preserves_blocks_and_replaces_existing_file() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("material.json");
    fs::write(&path, "previous contents").unwrap();
    let context = text_context();
    save(&path, &context).unwrap();
    let restored = load(&path).unwrap();
    assert_eq!(restored.blocks(), context.blocks());
    assert_eq!(fs::read_dir(folder.path()).unwrap().count(), 1);
    let json: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        json.as_object().unwrap().keys().collect::<Vec<_>>(),
        ["blocks"]
    );
}

#[test]
fn recovery_export_never_overwrites_an_existing_file() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("recovered.json");
    let original = text_context();
    save_new(&path, &original).unwrap();
    assert_eq!(load(&path).unwrap().blocks(), original.blocks());
    assert!(matches!(
        save_new(&path, &Context::new()),
        Err(MaterialError::Io(_))
    ));
    assert_eq!(load(&path).unwrap().blocks(), original.blocks());
    assert_eq!(fs::read_dir(folder.path()).unwrap().count(), 1);
}

#[test]
fn default_rejects_media_and_preserves_previous_destination() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("material.json");
    let previous = text_context();
    save(&path, &previous).unwrap();
    let context = Context::from_blocks(vec![block(BlockContent::Message {
        role: causa::kernel::Role::Assistant,
        parts: vec![ContentPart::Media(MediaRef::new(
            "image/png",
            "missing-asset",
        ))],
    })])
    .unwrap();
    assert!(matches!(
        save(&path, &context),
        Err(MaterialError::ReferenceValidationRequired { .. })
    ));
    assert_eq!(load(&path).unwrap().blocks(), previous.blocks());
    fs::write(&path, serde_json::to_vec(&context).unwrap()).unwrap();
    assert!(matches!(
        load(&path),
        Err(MaterialError::ReferenceValidationRequired { .. })
    ));
}

#[test]
fn asset_readability_is_rechecked_on_load_and_errors_retain_the_cause() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("material.json");
    let asset = folder.path().join("asset");
    fs::write(&asset, b"image bytes").unwrap();
    let context = Context::from_blocks(vec![block(BlockContent::Message {
        role: causa::kernel::Role::Assistant,
        parts: vec![ContentPart::Media(MediaRef::new(
            "image/png",
            "host-asset-key",
        ))],
    })])
    .unwrap();
    let verify = |reference: MaterialReference<'_>| {
        assert!(
            matches!(reference, MaterialReference::Media(media) if media.reference == "host-asset-key")
        );
        fs::read(&asset).map(|_| ())
    };
    save_with_references(&path, &context, verify).unwrap();
    assert_eq!(
        load_with_references(&path, verify).unwrap().blocks(),
        context.blocks()
    );
    fs::remove_file(&asset).unwrap();
    let error = load_with_references(&path, verify).unwrap_err();
    assert!(
        matches!(error, MaterialError::ReferenceUnavailable { source, .. } if source.kind() == std::io::ErrorKind::NotFound)
    );
}

#[test]
fn tool_result_media_and_artifacts_are_both_verified() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("material.json");
    let mut output = ToolOutput::new(serde_json::json!("shortened"));
    output.artifact = Some(ArtifactRef::new(
        "full-output",
        10,
        ArtifactKind::FullOutput,
        true,
    ));
    let context = Context::from_blocks(vec![block(BlockContent::ToolResult(
        ToolResultPayload::new(new_block_id(), ToolResultStatus::Succeeded, output)
            .with_media(vec![MediaRef::new("image/png", "result-image")]),
    ))])
    .unwrap();
    assert!(matches!(
        save(&path, &context),
        Err(MaterialError::ReferenceValidationRequired { .. })
    ));
    let mut seen = vec![];
    save_with_references(&path, &context, |reference| {
        seen.push(match reference {
            MaterialReference::Media(media) => media.reference.clone(),
            MaterialReference::Artifact(artifact) => artifact.id.clone(),
        });
        Ok(())
    })
    .unwrap();
    assert_eq!(seen, ["result-image", "full-output"]);
    let error = load_with_references(&path, |reference| match reference {
        MaterialReference::Media(_) => Ok(()),
        MaterialReference::Artifact(_) => Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
    })
    .unwrap_err();
    assert!(matches!(error, MaterialError::ReferenceUnavailable { .. }));
}

#[test]
fn malformed_json_and_duplicate_block_ids_are_rejected() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("material.json");
    fs::write(&path, "not json").unwrap();
    assert!(matches!(load(&path), Err(MaterialError::Json(_))));
    let context = text_context();
    let duplicate = &context.blocks()[0];
    fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({"blocks":[duplicate,duplicate]})).unwrap(),
    )
    .unwrap();
    assert!(matches!(load(&path), Err(MaterialError::Json(_))));
}

#[test]
fn failed_replacement_leaves_destination_directory_intact_and_removes_tempfile() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("existing-directory");
    fs::create_dir(&path).unwrap();
    assert!(matches!(
        save(&path, &text_context()),
        Err(MaterialError::Io(_))
    ));
    assert!(path.is_dir());
    assert_eq!(fs::read_dir(folder.path()).unwrap().count(), 1);
}

/// A user input and the repair note, in the current form and in the form
/// written before the message role became a fact of the block.
fn message_role_fixture() -> (Context, serde_json::Value) {
    use causa::kernel::Role;
    let context = Context::from_blocks(vec![
        ContextBlock::new(
            new_block_id(),
            BlockContent::text(Role::User, "question"),
            BlockMeta::from_source("user"),
        ),
        ContextBlock::new(
            new_block_id(),
            BlockContent::text(Role::System, "repair note"),
            BlockMeta::from_source("application:tars:material-repair"),
        ),
    ])
    .unwrap();
    let mut earlier = serde_json::to_value(&context).unwrap();
    for block in earlier["blocks"].as_array_mut().unwrap() {
        let parts = block["content"]["value"]["parts"].take();
        block["content"] = serde_json::json!({"shape": "parts", "value": parts});
    }
    (context, earlier)
}

#[test]
fn material_files_and_session_exports_written_before_message_roles_still_load() {
    let folder = tempfile::tempdir().unwrap();
    let (context, earlier) = message_role_fixture();
    let path = folder.path().join("material.json");
    fs::write(&path, serde_json::to_vec(&earlier).unwrap()).unwrap();
    assert_eq!(load(&path).unwrap().blocks(), context.blocks());

    // export-materials reads a saved session record.
    let record = folder.path().join("session.json");
    let envelope = serde_json::json!({
        "format": "tars-session-v2",
        "conversation_id": uuid::Uuid::now_v7().to_string(),
        "revision": 1,
        "last_commit_id": uuid::Uuid::now_v7().to_string(),
        "definition": {},
        "context": earlier,
    });
    fs::write(&record, serde_json::to_vec(&envelope).unwrap()).unwrap();
    let exported = load_session_materials(&record).unwrap();
    assert_eq!(exported.blocks(), context.blocks());

    // The next write stores the current form.
    save(&path, &exported).unwrap();
    let written: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(written, serde_json::to_value(&context).unwrap());
    assert!(
        written["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|block| block["content"]["shape"] == "message")
    );
}

fn record_definition() -> crate::definition::SessionDefinition {
    crate::definition::SessionDefinition {
        provider: crate::definition::Provider::OpenAiChat,
        model: "fixture".into(),
        endpoint: None,
        credential: crate::definition::CredentialRef::Environment("TARS_FIXTURE_KEY".into()),
        work_timeout_secs: None,
        retained_work_capacity: 8,
        max_model_rounds: 2,
        max_tool_calls: 4,
        max_tokens: None,
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: None,
        tools: Vec::new(),
        tool_limits: Default::default(),
        write_limits: None,
        permission_mode: None,
        command: None,
        mcp_servers: Vec::new(),
        context_window: None,
        auto_compact: true,
        compact_threshold: 70,
        prompt_cache: true,
    }
}

#[test]
fn offline_export_extracts_context_from_a_compacted_record() {
    let id = uuid::Uuid::now_v7().to_string();
    let record = crate::compact::HistoryRecordEntry {
        record_id: uuid::Uuid::now_v7().to_string(),
        created_at_ms: 1,
        trigger: crate::compact::HistoryTrigger::Auto,
        kind: crate::compact::RewriteKind::Summarize,
        removed_block_count: 1,
        inserted_block_ids: vec![causa::kernel::BlockId::new(uuid::Uuid::now_v7())],
    };
    let context = Context::from_blocks(vec![causa::kernel::ContextBlock::text(
        causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
        causa::kernel::Role::User,
        "compacted summary",
    )])
    .unwrap();
    let mut definition = record_definition();
    definition.context_window = Some(1_000);
    definition.auto_compact = false;
    definition.compact_threshold = 50;
    let record = crate::store::SavedSessionRecord::new_with_compaction(
        id.clone(),
        uuid::Uuid::now_v7().to_string(),
        None,
        definition,
        context,
        None,
        Some(false),
        Some(50),
        vec![record],
        None,
        None,
        1_000,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("record.json");
    std::fs::write(&path, serde_json::to_vec(&record).unwrap()).unwrap();
    let loaded = load_session_materials(&path).unwrap();
    assert_eq!(loaded.blocks().len(), 1);
}

#[test]
fn test_be09_detach_history_references_unit() {
    let own_record_id = uuid::Uuid::now_v7().to_string();
    let own_block_uuid = uuid::Uuid::now_v7();
    let foreign_record_id = uuid::Uuid::now_v7().to_string();
    let foreign_block_uuid = uuid::Uuid::now_v7();

    let history = vec![crate::compact::HistoryRecordEntry {
        record_id: own_record_id.clone(),
        created_at_ms: 1000,
        trigger: crate::compact::HistoryTrigger::Auto,
        kind: crate::compact::RewriteKind::Summarize,
        removed_block_count: 2,
        inserted_block_ids: vec![causa::kernel::BlockId::new(uuid::Uuid::now_v7())],
    }];

    let call_id_1 = new_block_id();
    let call_id_2 = new_block_id();
    let block1_id = new_block_id();
    let block2_id = new_block_id();

    let mut out1 = ToolOutput::new(serde_json::json!({"output": "data"}));
    out1.truncation = causa::kernel::Truncation::Middle;
    out1.artifact = Some(ArtifactRef::new(
        format!("{own_record_id}/{own_block_uuid}"),
        64,
        ArtifactKind::FullOutput,
        true,
    ));
    let result1 = ToolResultPayload::new(call_id_1, ToolResultStatus::Succeeded, out1)
        .with_notes(vec![causa::kernel::TextPayload::new("pre-existing note")])
        .with_media(vec![MediaRef::new("image/png", "img-1")]);

    let mut out2 = ToolOutput::new(serde_json::json!({"other": "result"}));
    out2.artifact = Some(ArtifactRef::new(
        format!("{foreign_record_id}/{foreign_block_uuid}"),
        128,
        ArtifactKind::FullOutput,
        true,
    ));
    let result2 = ToolResultPayload::new(call_id_2, ToolResultStatus::Failed, out2)
        .with_notes(vec![causa::kernel::TextPayload::new("foreign note")]);

    let block1 = ContextBlock::new(
        block1_id,
        BlockContent::ToolResult(result1),
        BlockMeta::from_source("tool"),
    );
    let block2 = ContextBlock::new(
        block2_id,
        BlockContent::ToolResult(result2),
        BlockMeta::default(),
    );

    let context = Context::from_blocks(vec![block1.clone(), block2.clone()]).unwrap();

    let detached = detach_history_references(&context, &history);

    assert_eq!(detached.blocks().len(), 2);
    assert_eq!(detached.blocks()[0].id(), block1_id);
    assert_eq!(detached.blocks()[1].id(), block2_id);

    let BlockContent::ToolResult(res1) = detached.blocks()[0].content() else {
        panic!("expected ToolResult");
    };
    assert!(
        res1.output.artifact.is_none(),
        "own artifact reference must be removed"
    );
    assert_eq!(res1.output.content, serde_json::json!({"output": "data"}));
    assert_eq!(res1.status, ToolResultStatus::Succeeded);
    assert_eq!(res1.call_block_id, call_id_1);
    assert_eq!(res1.media, vec![MediaRef::new("image/png", "img-1")]);
    assert_eq!(res1.output.truncation, causa::kernel::Truncation::Middle);
    assert_eq!(detached.blocks()[0].meta().source.as_deref(), Some("tool"));
    assert_eq!(res1.notes.len(), 2);
    assert_eq!(res1.notes[0].0, "pre-existing note");
    assert_eq!(res1.notes[1].0, DETACHED_HISTORY_NOTE);

    let BlockContent::ToolResult(res2) = detached.blocks()[1].content() else {
        panic!("expected ToolResult");
    };
    assert!(
        res2.output.artifact.is_some(),
        "foreign reference must remain untouched"
    );
    assert_eq!(
        res2.output.artifact.as_ref().unwrap().id,
        format!("{foreign_record_id}/{foreign_block_uuid}")
    );
    assert_eq!(res2.notes.len(), 1);
    assert_eq!(res2.notes[0].0, "foreign note");

    let detached_again = detach_history_references(&detached, &history);
    let BlockContent::ToolResult(res1_again) = detached_again.blocks()[0].content() else {
        panic!("expected ToolResult");
    };
    assert_eq!(
        res1_again.notes.len(),
        2,
        "must not add duplicate note on second detach"
    );
    assert_eq!(res1_again.notes[1].0, DETACHED_HISTORY_NOTE);
    assert_eq!(detached_again.blocks(), detached.blocks());
}

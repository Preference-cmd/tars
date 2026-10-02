use super::*;
use causa::kernel::{
    ArtifactKind, ArtifactRef, BlockContent, BlockMeta, ContentPart, Context, ContextBlock,
    MediaRef, TextPayload, ToolOutput, ToolResultPayload, ToolResultStatus,
};
use causa::runtime::new_block_id;
use std::fs;

fn block(content: BlockContent) -> ContextBlock {
    ContextBlock::new(new_block_id(), content, BlockMeta::default())
}
fn text_context() -> Context {
    Context::from_blocks(vec![block(BlockContent::Parts(vec![ContentPart::Text(
        TextPayload::new("saved material"),
    )]))])
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
fn default_rejects_media_and_preserves_previous_destination() {
    let folder = tempfile::tempdir().unwrap();
    let path = folder.path().join("material.json");
    let previous = text_context();
    save(&path, &previous).unwrap();
    let context = Context::from_blocks(vec![block(BlockContent::Parts(vec![ContentPart::Media(
        MediaRef::new("image/png", "missing-asset"),
    )]))])
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
    let context = Context::from_blocks(vec![block(BlockContent::Parts(vec![ContentPart::Media(
        MediaRef::new("image/png", "host-asset-key"),
    )]))])
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
    output.artifact = Some(ArtifactRef {
        id: "full-output".into(),
        size_bytes: 10,
        kind: ArtifactKind::FullOutput,
        persisted: true,
    });
    let context = Context::from_blocks(vec![block(BlockContent::ToolResult(ToolResultPayload {
        call_block_id: new_block_id(),
        status: ToolResultStatus::Succeeded,
        output,
        media: vec![MediaRef::new("image/png", "result-image")],
        notes: vec![],
    }))])
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

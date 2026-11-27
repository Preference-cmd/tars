use super::*;
use crate::compact::history::{HistoryRecordEntry, HistoryStore};
use crate::definition::{CredentialRef, Provider, SessionDefinition};
use crate::store::SavedSessionRecord;
use base64::Engine as _;
use causa::kernel::{
    ArtifactHint, ArtifactKind, ArtifactStore, BlockContent, BlockMeta, ContentPart, Context,
    ContextBlock, MediaRef, Role, ToolOutput, ToolResultPayload, ToolResultStatus,
};
use causa::providers::{MediaResolver, MediaSource};
use causa::runtime::new_block_id;
use std::io::Write as _;
use std::sync::Arc;

fn minimal_png() -> Vec<u8> {
    let mut bytes = Vec::from(*b"\x89PNG\r\n\x1a\n");
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D', b'R']);
    bytes.extend_from_slice(&[0x00; 17]);
    bytes
}

fn minimal_jpeg() -> Vec<u8> {
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00,
    ]
}

fn minimal_gif87a() -> Vec<u8> {
    b"GIF87a\x01\x00\x01\x00\x80\x00\x00\xff\xff\xff\x00\x00\x00,".to_vec()
}

fn minimal_gif89a() -> Vec<u8> {
    b"GIF89a\x01\x00\x01\x00\x80\x00\x00\xff\xff\xff\x00\x00\x00,".to_vec()
}

fn minimal_webp() -> Vec<u8> {
    let mut bytes = Vec::from(*b"RIFF");
    bytes.extend_from_slice(&12u32.to_le_bytes());
    bytes.extend_from_slice(b"WEBP");
    bytes.extend_from_slice(b"VP8 ");
    bytes.extend_from_slice(&[0u8; 8]);
    bytes
}

fn png_of_exact_size(len: usize, seed: u8) -> Vec<u8> {
    assert!(len >= 8);
    let mut bytes = vec![seed; len];
    bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    bytes
}

fn test_definition() -> SessionDefinition {
    SessionDefinition {
        provider: Provider::Anthropic,
        model: "claude-sonnet-4-5".into(),
        endpoint: None,
        credential: CredentialRef::Environment("TARS_UNUSED_TEST_KEY".into()),
        work_timeout_secs: None,
        retained_work_capacity: 8,
        max_model_rounds: 1,
        max_tool_calls: 0,
        max_tokens: Some(1024),
        reasoning_effort: None,
        temperature: None,
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
        prelude: Default::default(),
        code_execution: None,
    }
}

#[test]
fn mm01_constants_match_contract() {
    assert_eq!(MAX_IMAGE_BYTES, 5_242_880);
    assert_eq!(MAX_IMAGES_PER_MESSAGE, 8);
    assert_eq!(MAX_SESSION_MEDIA_BYTES, 64 * 1024 * 1024);
    assert_eq!(GATEWAY_INLINE_IMAGE_LIMIT_BYTES, 5_242_883);
}

#[test]
fn mm01_magic_byte_detection_and_spoofed_extension_rejection() {
    assert_eq!(detect_image_mime(&minimal_png()), Some("image/png"));
    assert_eq!(detect_image_mime(&minimal_jpeg()), Some("image/jpeg"));
    assert_eq!(detect_image_mime(&minimal_gif87a()), Some("image/gif"));
    assert_eq!(detect_image_mime(&minimal_gif89a()), Some("image/gif"));
    assert_eq!(detect_image_mime(&minimal_webp()), Some("image/webp"));

    // Reject SVG, plain text, truncated RIFF/WebP, empty.
    assert_eq!(
        detect_image_mime(b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>"),
        None
    );
    assert_eq!(detect_image_mime(b"not an image"), None);
    assert_eq!(detect_image_mime(b"RIFF1234WEBQ"), None);
    assert_eq!(detect_image_mime(b"RIFF1234WEB"), None);
    assert_eq!(detect_image_mime(b""), None);

    let dir = tempfile::tempdir().unwrap();
    // Spoofed .png extension carrying JPEG bytes -> classified by magic bytes as image/jpeg.
    let spoofed_jpeg_path = dir.path().join("photo.png");
    std::fs::write(&spoofed_jpeg_path, minimal_jpeg()).unwrap();
    let prepared = prepare_image_from_path(&spoofed_jpeg_path).unwrap();
    assert_eq!(prepared.media_type, "image/jpeg");
    assert!(is_managed_media_ref(&prepared.reference));

    // Spoofed .png extension carrying text or SVG -> rejected.
    let spoofed_text_path = dir.path().join("evil.png");
    std::fs::write(&spoofed_text_path, b"hello world").unwrap();
    assert!(matches!(
        prepare_image_from_path(&spoofed_text_path),
        Err(MediaError::UnsupportedFormat)
    ));

    let svg_path = dir.path().join("vector.svg");
    std::fs::write(&svg_path, b"<svg xmlns=\"http://www.w3.org/2000/svg\"/>").unwrap();
    assert!(matches!(
        prepare_image_from_path(&svg_path),
        Err(MediaError::UnsupportedFormat)
    ));

    // Empty file -> EmptyFile.
    let empty_path = dir.path().join("empty.png");
    std::fs::write(&empty_path, b"").unwrap();
    assert!(matches!(
        prepare_image_from_path(&empty_path),
        Err(MediaError::EmptyFile)
    ));

    // Directory -> NotRegularFile.
    assert!(matches!(
        prepare_image_from_path(dir.path()),
        Err(MediaError::NotRegularFile { .. })
    ));
}

#[cfg(unix)]
#[test]
fn mm01_rejects_non_regular_fifo_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let fifo_path = dir.path().join("pipe.png");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo_path)
        .status()
        .expect("mkfifo should exist on unix");
    assert!(status.success());

    let err = prepare_image_from_path(&fifo_path).unwrap_err();
    assert!(matches!(err, MediaError::NotRegularFile { .. }));
}

#[test]
fn mm01_exact_5mib_accepted_and_5mib_plus_1_rejected() {
    let dir = tempfile::tempdir().unwrap();

    let exact_bytes = png_of_exact_size(MAX_IMAGE_BYTES, 0x42);
    let exact_path = dir.path().join("exact.png");
    std::fs::write(&exact_path, &exact_bytes).unwrap();
    let prepared = prepare_image_from_path(&exact_path).unwrap();
    assert_eq!(prepared.size_bytes, MAX_IMAGE_BYTES);
    assert_eq!(prepared.bytes.len(), MAX_IMAGE_BYTES);
    assert_eq!(prepared.media_type, "image/png");

    let over_bytes = png_of_exact_size(MAX_IMAGE_BYTES + 1, 0x42);
    let over_path = dir.path().join("over.png");
    std::fs::write(&over_path, &over_bytes).unwrap();
    assert!(matches!(
        prepare_image_from_path(&over_path),
        Err(MediaError::FileTooLarge {
            size_bytes: 5_242_881,
            max_bytes: 5_242_880,
        })
    ));
    assert!(matches!(
        prepare_image_from_bytes(over_bytes, Some("over.png")),
        Err(MediaError::FileTooLarge {
            size_bytes: 5_242_881,
            max_bytes: 5_242_880,
        })
    ));
}

#[test]
fn mm01_name_sanitization_and_multimodal_input_variants() {
    let prepared =
        prepare_image_from_bytes(minimal_png(), Some("/secret/host/\x1b[31mshot\n.png")).unwrap();
    assert_eq!(prepared.name.as_deref(), Some("[31mshot.png"));

    let media_ref = prepared.media_ref();
    // Text + image.
    let both = multimodal_input(Some("describe this"), std::slice::from_ref(&media_ref)).unwrap();
    assert!(matches!(
        both.content(),
        BlockContent::Message { parts, .. } if parts.len() == 2
    ));

    // Image-only (None or Some("")).
    let img_only = multimodal_input(None, std::slice::from_ref(&media_ref)).unwrap();
    assert!(matches!(
        img_only.content(),
        BlockContent::Message { parts, .. } if parts.len() == 1
    ));
    let img_empty_text = multimodal_input(Some(""), std::slice::from_ref(&media_ref)).unwrap();
    assert!(matches!(
        img_empty_text.content(),
        BlockContent::Message { parts, .. } if parts.len() == 1
    ));

    // Empty input (neither non-empty text nor images) rejected.
    assert!(matches!(
        multimodal_input(None, &[]),
        Err(MediaError::EmptyInput)
    ));
    assert!(matches!(
        multimodal_input(Some(""), &[]),
        Err(MediaError::EmptyInput)
    ));

    // > 8 images rejected.
    let nine_refs = vec![media_ref.clone(); MAX_IMAGES_PER_MESSAGE + 1];
    assert!(matches!(
        multimodal_input(Some("too many"), &nine_refs),
        Err(MediaError::TooManyImages {
            count: 9,
            max_images: 8
        })
    ));

    // Exactly 8 images accepted.
    let eight_refs = vec![media_ref; MAX_IMAGES_PER_MESSAGE];
    assert!(multimodal_input(None, &eight_refs).is_ok());

    // Unmanaged URL rejected.
    let external = MediaRef::new("image/png", "https://example.com/a.png");
    assert!(matches!(
        multimodal_input(Some("bad"), &[external]),
        Err(MediaError::InvalidReference { .. })
    ));
}

#[test]
fn mm01_vault_dedup_quota_and_rollback() {
    let vault = MediaVault::new();
    let img_a =
        prepare_image_from_bytes(png_of_exact_size(4 * 1024 * 1024, 1), Some("a.png")).unwrap();
    let img_b =
        prepare_image_from_bytes(png_of_exact_size(4 * 1024 * 1024, 2), Some("b.png")).unwrap();

    // Insert A twice in one batch -> deduplicated to 4 MiB.
    let (refs, added) = vault
        .insert_batch_with_added(&[img_a.clone(), img_a.clone()])
        .unwrap();
    assert_eq!(refs.len(), 2);
    assert_eq!(added.len(), 1);
    assert_eq!(vault.len(), 1);
    assert_eq!(vault.total_bytes(), 4 * 1024 * 1024);

    // Insert A + B -> only B is newly added; rollback removes B while preserving A.
    let (_, added2) = vault
        .insert_batch_with_added(&[img_a.clone(), img_b.clone()])
        .unwrap();
    assert_eq!(added2, vec![img_b.reference.clone()]);
    assert_eq!(vault.total_bytes(), 8 * 1024 * 1024);

    vault.remove_keys(&added2);
    assert!(vault.contains(&img_a.reference));
    assert!(!vault.contains(&img_b.reference));
    assert_eq!(vault.len(), 1);
    assert_eq!(vault.total_bytes(), 4 * 1024 * 1024);

    // Fill another vault to exact 64 MiB (12 * 5 MiB + 4 MiB = 64 MiB), then reject +1 byte.
    let quota_vault = MediaVault::new();
    for i in 0..12u8 {
        let five_mib =
            prepare_image_from_bytes(png_of_exact_size(MAX_IMAGE_BYTES, i + 10), None).unwrap();
        quota_vault.insert_batch(&[five_mib]).unwrap();
    }
    let four_mib = prepare_image_from_bytes(png_of_exact_size(4 * 1024 * 1024, 99), None).unwrap();
    quota_vault
        .insert_batch(std::slice::from_ref(&four_mib))
        .unwrap();
    assert_eq!(quota_vault.total_bytes(), MAX_SESSION_MEDIA_BYTES);

    // Re-inserting an already-present image still succeeds (0 additional bytes).
    quota_vault
        .insert_batch(std::slice::from_ref(&four_mib))
        .unwrap();

    // Inserting a brand-new image fails without mutating vault.
    let extra = prepare_image_from_bytes(minimal_png(), Some("extra.png")).unwrap();
    let err = quota_vault
        .insert_batch(std::slice::from_ref(&extra))
        .unwrap_err();
    assert!(matches!(
        err,
        MediaError::SessionQuotaExceeded {
            current_bytes,
            additional_bytes,
            max_bytes,
        } if current_bytes == MAX_SESSION_MEDIA_BYTES
            && additional_bytes == extra.size_bytes
            && max_bytes == MAX_SESSION_MEDIA_BYTES
    ));
    assert!(!quota_vault.contains(&extra.reference));
    assert_eq!(quota_vault.total_bytes(), MAX_SESSION_MEDIA_BYTES);
}

#[tokio::test]
async fn mm01_vault_resolves_managed_and_mcp_artifact_references() {
    let dir = tempfile::tempdir().unwrap();
    let session_id = uuid::Uuid::now_v7().to_string();
    let store = Arc::new(HistoryStore::new(dir.path(), &session_id, vec![]));
    let artifacts = crate::compact::artifacts::HistoryArtifacts::new(store.clone());

    let png_bytes = minimal_png();
    let artifact_ref = artifacts
        .persist(
            &png_bytes,
            ArtifactHint::new("mcp_tool", new_block_id(), ArtifactKind::FullOutput),
        )
        .await
        .unwrap();

    let vault = MediaVault::new();
    vault.bind_history_store(store.clone());
    let prepared = prepare_image_from_bytes(minimal_jpeg(), Some("cam.jpg")).unwrap();
    vault.insert_batch(std::slice::from_ref(&prepared)).unwrap();

    // Resolve media:sha256:*
    let resolved_user = vault.resolve(&prepared.media_ref()).expect("user image");
    assert_eq!(resolved_user.media_type, "image/jpeg");
    let MediaSource::Inline { data_base64 } = resolved_user.source else {
        panic!("expected inline payload");
    };
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&data_base64)
            .unwrap(),
        prepared.bytes.as_ref()
    );

    // Mismatched MIME on managed ref returns None.
    let wrong_mime = MediaRef::new("image/png", &prepared.reference);
    assert!(vault.resolve(&wrong_mime).is_none());

    // Resolve MCP artifact:<uuid>
    let mcp_media_ref = MediaRef::new("image/png", &artifact_ref.id);
    let resolved_mcp = vault.resolve(&mcp_media_ref).expect("mcp artifact image");
    assert_eq!(resolved_mcp.media_type, "image/png");
    let MediaSource::Inline {
        data_base64: mcp_b64,
    } = resolved_mcp.source
    else {
        panic!("expected inline MCP payload");
    };
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&mcp_b64)
            .unwrap(),
        png_bytes
    );

    // Non-media MIME or missing artifact returns None.
    assert!(
        vault
            .resolve(&MediaRef::new("text/plain", &artifact_ref.id))
            .is_none()
    );
    let missing_artifact = format!("artifact:{}", uuid::Uuid::now_v7());
    assert!(
        vault
            .resolve(&MediaRef::new("image/png", &missing_artifact))
            .is_none()
    );

    // Oversized artifact (> MAX_IMAGE_BYTES) fails bounded check and returns None.
    let oversized_ref = artifacts
        .persist(
            &vec![0x11u8; MAX_IMAGE_BYTES + 1],
            ArtifactHint::new("mcp_tool", new_block_id(), ArtifactKind::FullOutput),
        )
        .await
        .unwrap();
    assert!(
        vault
            .resolve(&MediaRef::new("image/png", &oversized_ref.id))
            .is_none()
    );
}

#[test]
fn mm02_managed_reference_validator_and_record_validation_counterexamples() {
    let prepared = prepare_image_from_bytes(minimal_png(), Some("ok.png")).unwrap();
    assert!(validate_managed_media_ref(&prepared.media_ref()).is_ok());

    let valid_artifact_img =
        MediaRef::new("image/webp", format!("artifact:{}", uuid::Uuid::now_v7()));
    assert!(validate_managed_media_ref(&valid_artifact_img).is_ok());

    let valid_artifact_pdf = MediaRef::new(
        "application/pdf",
        format!("artifact:{}", uuid::Uuid::now_v7()),
    );
    assert!(validate_managed_media_ref(&valid_artifact_pdf).is_ok());

    // Counterexamples rejected by validate_managed_media_ref:
    let bad_refs = vec![
        MediaRef::new("image/png", "https://example.com/a.png"),
        MediaRef::new("image/png", "/tmp/a.png"),
        MediaRef::new("image/png", "file:///tmp/a.png"),
        MediaRef::new("image/png", "data:image/png;base64,iVBOR"),
        MediaRef::new("image/png", "unknown:1234"),
        MediaRef::new("image/png", format!("media:sha256:{}", "A".repeat(64))),
        MediaRef::new("image/png", "media:sha256:deadbeef"),
        MediaRef::new("image/svg+xml", &prepared.reference),
        MediaRef::new("application/pdf", &prepared.reference),
        MediaRef::new("image/png", "artifact:not-a-uuid"),
        MediaRef::new("text/plain", format!("artifact:{}", uuid::Uuid::now_v7())),
    ];
    for bad in &bad_refs {
        assert!(
            validate_managed_media_ref(bad).is_err(),
            "expected rejection for {bad:?}"
        );
    }

    // SavedSessionRecord::validate accepts valid media refs and rejects counterexamples.
    let session_id = uuid::Uuid::now_v7().to_string();
    let valid_context = Context::from_blocks(vec![
        multimodal_input(Some("look"), &[prepared.media_ref()]).unwrap(),
        ContextBlock::new(
            new_block_id(),
            BlockContent::ToolResult(
                ToolResultPayload::new(
                    new_block_id(),
                    ToolResultStatus::Succeeded,
                    ToolOutput::new(serde_json::json!({"ok": true})),
                )
                .with_media(vec![valid_artifact_img]),
            ),
            BlockMeta::default(),
        ),
    ])
    .unwrap();
    let valid_record = SavedSessionRecord::new_with_compaction(
        session_id.clone(),
        uuid::Uuid::now_v7().to_string(),
        None,
        test_definition(),
        valid_context,
        None,
        None,
        None,
        Vec::<HistoryRecordEntry>::new(),
        None,
        None,
        1_700_000_000_000,
    )
    .unwrap();
    assert!(valid_record.validate(&session_id).is_ok());

    for bad in bad_refs {
        let bad_ctx = Context::from_blocks(vec![ContextBlock::new(
            new_block_id(),
            BlockContent::Message {
                role: Role::User,
                parts: vec![ContentPart::Media(bad.clone())],
            },
            BlockMeta::from_source("user"),
        )])
        .unwrap();
        let bad_record_res = SavedSessionRecord::new_with_compaction(
            session_id.clone(),
            uuid::Uuid::now_v7().to_string(),
            None,
            test_definition(),
            bad_ctx.clone(),
            None,
            None,
            None,
            Vec::new(),
            None,
            None,
            1_700_000_000_000,
        );
        assert!(
            bad_record_res.is_err(),
            "expected SavedSessionRecord::new_with_compaction to reject {bad:?}"
        );
        let mut raw_record = valid_record.clone();
        raw_record.context = bad_ctx;
        assert!(
            raw_record.validate(&session_id).is_err(),
            "expected SavedSessionRecord::validate to reject {bad:?}"
        );
    }

    // Verify verify_managed_blob catches tampered bytes and empty/corrupt blobs.
    let mut tampered = prepared.bytes.to_vec();
    let last = tampered.last_mut().unwrap();
    *last ^= 0xFF;
    assert!(matches!(
        types::verify_managed_blob(&prepared.reference, &tampered),
        Err(MediaError::CorruptMedia { .. })
    ));

    // Also test prepared_image_from_path reading exact boundary via Write + File.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("written.webp");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(&minimal_webp()).unwrap();
    f.flush().unwrap();
    assert_eq!(
        prepare_image_from_path(&path).unwrap().media_type,
        "image/webp"
    );
}

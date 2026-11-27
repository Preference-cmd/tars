use super::support::*;
use base64::Engine as _;
use causa::kernel::{
    ArtifactHint, ArtifactKind, ArtifactStore, Context, ContextBlock, MediaRef, ToolOutput,
    ToolResultPayload, ToolResultStatus,
};
use causa::providers::{MediaResolver, MediaSource};
use causa::runtime::new_block_id;
use std::collections::BTreeMap;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn png_bytes(len: usize, seed: u8) -> Vec<u8> {
    assert!(len >= 8);
    let mut bytes = vec![seed; len];
    bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    bytes
}

fn minimal_png() -> Vec<u8> {
    png_bytes(33, 0x2a)
}

fn minimal_jpeg() -> Vec<u8> {
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00,
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mm01_submit_with_images_admission_and_rollback() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    fixture(
        &host,
        Gateway {
            calls: Arc::new(AtomicUsize::new(0)),
            entered: Some(entered.clone()),
            release: Some(release.clone()),
            panic: false,
        },
    );

    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();

    let img_a = crate::media::prepare_image_from_bytes(minimal_png(), Some("a.png")).unwrap();
    let img_b = crate::media::prepare_image_from_bytes(minimal_jpeg(), Some("b.jpg")).unwrap();

    // 1. Rejects > 8 images before touching media_vault.
    let nine = vec![img_a.clone(); crate::media::MAX_IMAGES_PER_MESSAGE + 1];
    let err = session
        .submit_with_images("too-many", Some("hi"), &nine)
        .unwrap_err();
    assert!(matches!(
        err,
        ApplicationError::Media(crate::media::MediaError::TooManyImages { .. })
    ));
    assert!(session.media_vault().is_empty());

    // 2. Rejects empty submission (no text, no images).
    let err = session
        .submit_with_images("empty", Some(""), &[])
        .unwrap_err();
    assert!(matches!(
        err,
        ApplicationError::Media(crate::media::MediaError::EmptyInput)
    ));
    assert!(session.media_vault().is_empty());

    // 3. Submit image A (image-only) while turn pauses on gateway.
    let receipt = session
        .submit_with_images("turn-1", None, std::slice::from_ref(&img_a))
        .unwrap();
    entered.notified().await;
    assert!(session.media_vault().contains(&img_a.reference));
    assert_eq!(session.media_vault().len(), 1);

    // 4. Submitting image B while Busy fails and does NOT add B to media_vault.
    let err = session
        .submit_with_images("busy-reject", Some("with b"), std::slice::from_ref(&img_b))
        .unwrap_err();
    assert!(matches!(err, ApplicationError::Session(_)));
    assert!(!session.media_vault().contains(&img_b.reference));
    assert_eq!(session.media_vault().len(), 1);

    release.notify_one();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();

    // 5. Submit text + image B -> succeeds.
    let receipt2 = session
        .submit_with_images("turn-2", Some("now b"), std::slice::from_ref(&img_b))
        .unwrap();
    release.notify_one();
    finished(&session, &receipt2.work).await;
    session.wait_saved().await.unwrap();
    assert!(session.media_vault().contains(&img_b.reference));
    assert_eq!(session.media_vault().len(), 2);

    // 6. Close session -> subsequent submit_with_images with img_c fails without mutating vault.
    let img_c = crate::media::prepare_image_from_bytes(png_bytes(40, 0x77), Some("c.png")).unwrap();
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
    let err = session
        .submit_with_images("after-close", Some("c"), std::slice::from_ref(&img_c))
        .unwrap_err();
    assert!(matches!(err, ApplicationError::Closed));
    assert!(!session.media_vault().contains(&img_c.reference));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mm02_sqlite_persistence_dedup_discard_reopen_and_tamper_fail_closed() {
    assert_eq!(crate::store::schema::SCHEMA_VERSION, 2);

    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);

    // 1. Discarding a transient/unsaved session leaves zero orphan BLOBs.
    let transient = host
        .create(PreparedCreate::transient(definition(), Context::new()))
        .await
        .unwrap();
    let img = crate::media::prepare_image_from_bytes(minimal_png(), Some("shot.png")).unwrap();
    let t_work = transient
        .submit_with_images("t-img", Some("transient"), std::slice::from_ref(&img))
        .unwrap()
        .work;
    finished(&transient, &t_work).await;
    let transient_id = transient.target().conversation_id().as_str().to_owned();
    host.close(&transient.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
    assert!(!root.path().join("tars.db").exists());

    // 2. Persistent session persists image in `artifacts` inside Writer::commit and deduplicates.
    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let id = session.target().conversation_id().clone();
    let id_str = id.as_str().to_owned();
    assert_ne!(id_str, transient_id);

    let w1 = session
        .submit_with_images("p-1", Some("first"), std::slice::from_ref(&img))
        .unwrap()
        .work;
    finished(&session, &w1).await;
    session.wait_saved().await.unwrap();
    assert!(artifact_row_exists(root.path(), &id_str, &img.reference));
    assert_eq!(artifact_row_count(root.path(), &id_str), 1);

    // Submitting identical image again keeps 1 deduplicated row in `artifacts`.
    let w2 = session
        .submit_with_images("p-2", Some("again"), std::slice::from_ref(&img))
        .unwrap()
        .work;
    finished(&session, &w2).await;
    session.wait_saved().await.unwrap();
    assert_eq!(artifact_row_count(root.path(), &id_str), 1);

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // 3. Reopen preloads MediaVault and resolves exact bytes.
    let reopened = host.open(id.clone()).await.unwrap();
    assert!(reopened.media_vault().contains(&img.reference));
    let resolved = reopened.media_vault().resolve(&img.media_ref()).unwrap();
    let MediaSource::Inline { data_base64 } = resolved.source else {
        panic!("expected inline media payload");
    };
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&data_base64)
            .unwrap(),
        img.bytes.as_ref()
    );
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // 4. Tampered BLOB in SQLite -> reopen fails closed.
    let mut tampered = img.bytes.to_vec();
    *tampered.last_mut().unwrap() ^= 0xFF;
    {
        let conn = open_db(root.path());
        conn.execute(
            "UPDATE artifacts SET content = ?3
             WHERE session_id = ?1 AND agent_id = ?1 AND artifact_id = ?2",
            rusqlite::params![&id_str, &img.reference, &tampered],
        )
        .unwrap();
    }
    assert!(host.open(id.clone()).await.is_err());

    // 5. Missing BLOB in SQLite -> reopen fails closed.
    delete_artifact_row(root.path(), &id_str, &img.reference);
    assert!(host.open(id).await.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mm02_compaction_retains_referenced_media_and_cleanup_collects_orphans() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let gateway = CompactionGateway::new(SummaryBehavior::Text(
        "Summary of conversation including image.".into(),
    ));
    host.set_test_gateway(Some(gateway.clone() as Arc<dyn causa::kernel::ModelGateway>));

    let img_a = crate::media::prepare_image_from_bytes(png_bytes(64, 0x31), Some("a.png")).unwrap();
    let img_b = crate::media::prepare_image_from_bytes(png_bytes(64, 0x32), Some("b.png")).unwrap();

    // Build an earlier conversation whose user block contains img_a, enough tokens to trigger auto-compact.
    let mut def = compacting_definition(Some(FIXTURE_WINDOW));
    def.max_tokens = Some(1024);
    let mut initial_blocks = earlier_conversation().blocks().to_vec();
    initial_blocks.insert(
        0,
        crate::media::multimodal_input(Some("inspect this image"), &[img_a.media_ref()]).unwrap(),
    );
    let initial_ctx = Context::from_blocks(initial_blocks).unwrap();
    let mut initial_blobs = BTreeMap::new();
    initial_blobs.insert(img_a.reference.clone(), img_a.bytes.to_vec());

    let session = host
        .create(
            PreparedCreate::persistent(def, initial_ctx)
                .with_media_blobs(&initial_blobs)
                .unwrap(),
        )
        .await
        .unwrap();
    let id = session.target().conversation_id().clone();
    let id_str = id.as_str().to_owned();

    // Trigger compaction on the next turn.
    let receipt = session
        .submit_with_images(
            "turn-compact",
            Some("follow-up"),
            std::slice::from_ref(&img_b),
        )
        .unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(gateway.summary_calls.load(Ordering::SeqCst), 1);

    // img_a was compacted into history_records, img_b stays in live context; both must remain in artifacts!
    let record = crate::store::SessionStore::new(root.path().into())
        .read_saved(&id_str)
        .unwrap()
        .unwrap();
    assert!(!record.history.is_empty());
    assert!(
        !crate::media::collect_context_media_refs(&record.context).contains(&img_a.reference),
        "img_a should have been moved into history_records by compaction"
    );
    assert!(artifact_row_exists(root.path(), &id_str, &img_a.reference));
    assert!(artifact_row_exists(root.path(), &id_str, &img_b.reference));

    // Close and reopen: opening runs cleanup_artifacts + load_managed_media_blobs across both context and history_records.
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // Insert an unreferenced orphan media row and verify cleanup removes ONLY the orphan.
    let orphan_img =
        crate::media::prepare_image_from_bytes(png_bytes(48, 0x99), Some("orphan.png")).unwrap();
    {
        let conn = open_db(root.path());
        conn.execute(
            "INSERT INTO artifacts (session_id, agent_id, artifact_id, content)
             VALUES (?1, ?1, ?2, ?3)",
            rusqlite::params![&id_str, &orphan_img.reference, orphan_img.bytes.as_ref()],
        )
        .unwrap();
    }
    assert!(artifact_row_exists(
        root.path(),
        &id_str,
        &orphan_img.reference
    ));

    let reopened = host.open(id).await.unwrap();
    assert!(artifact_row_exists(root.path(), &id_str, &img_a.reference));
    assert!(artifact_row_exists(root.path(), &id_str, &img_b.reference));
    assert!(
        !artifact_row_exists(root.path(), &id_str, &orphan_img.reference),
        "unreferenced orphan media should be collected on open"
    );
    assert!(reopened.media_vault().contains(&img_a.reference));
    assert!(reopened.media_vault().contains(&img_b.reference));
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mm02_repair_inspect_permits_managed_and_artifact_media_and_flags_missing() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let gateway = CompactionGateway::new(SummaryBehavior::Text("ok".into()));
    host.set_test_gateway(Some(gateway as Arc<dyn causa::kernel::ModelGateway>));

    let img = crate::media::prepare_image_from_bytes(minimal_png(), Some("ok.png")).unwrap();
    let mut blobs = BTreeMap::new();
    blobs.insert(img.reference.clone(), img.bytes.to_vec());

    let call_id = new_block_id();
    let ctx = Context::from_blocks(vec![
        crate::media::multimodal_input(Some("see image"), &[img.media_ref()]).unwrap(),
        ContextBlock::new(
            call_id,
            BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
                "mcp_tool",
                serde_json::json!({}),
            )),
            BlockMeta::default(),
        ),
        ContextBlock::new(
            new_block_id(),
            BlockContent::ToolResult(ToolResultPayload::new(
                call_id,
                ToolResultStatus::Succeeded,
                ToolOutput::new(serde_json::json!({"ok": true})),
            )),
            BlockMeta::default(),
        ),
    ])
    .unwrap();

    let mut def = compacting_definition(Some(FIXTURE_WINDOW));
    def.max_tokens = Some(1024);
    let session = host
        .create(
            PreparedCreate::persistent(def, ctx)
                .with_media_blobs(&blobs)
                .unwrap(),
        )
        .await
        .unwrap();

    // Persist an MCP artifact:<uuid> into the session's HistoryStore and verify inspect succeeds with it.
    let history_store_for_check = {
        let state = session.entry.lock();
        state
            .owner
            .as_ref()
            .unwrap()
            .compaction()
            .unwrap()
            .history()
            .unwrap()
    };
    let artifacts =
        crate::compact::artifacts::HistoryArtifacts::new(history_store_for_check.clone());
    let art_ref = artifacts
        .persist(
            &minimal_jpeg(),
            ArtifactHint::new("mcp_tool", call_id, ArtifactKind::FullOutput),
        )
        .await
        .unwrap();
    let mcp_media_ref = MediaRef::new("image/jpeg", &art_ref.id);
    assert!(crate::media::validate_managed_media_ref(&mcp_media_ref).is_ok());
    assert!(
        session.media_vault().resolve(&mcp_media_ref).is_some(),
        "opening should bind HistoryStore to MediaVault so artifact:<uuid> resolves"
    );

    // Also verify admission_check_with_media accepts BOTH ContentPart::Media (in vault)
    // and ToolResult.media (artifact:<uuid> in HistoryStore).
    let call_id_2 = new_block_id();
    let ctx_with_both = Context::from_blocks(vec![
        crate::media::multimodal_input(Some("see image"), &[img.media_ref()]).unwrap(),
        ContextBlock::new(
            call_id_2,
            BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
                "mcp_tool",
                serde_json::json!({}),
            )),
            BlockMeta::default(),
        ),
        ContextBlock::new(
            new_block_id(),
            BlockContent::ToolResult(
                ToolResultPayload::new(
                    call_id_2,
                    ToolResultStatus::Succeeded,
                    ToolOutput::new(serde_json::json!({"ok": true})),
                )
                .with_media(vec![mcp_media_ref]),
            ),
            BlockMeta::default(),
        ),
    ])
    .unwrap();
    assert!(
        crate::application::repair::admission_check_with_media(
            &ctx_with_both,
            &[],
            Some(&history_store_for_check),
            Some(&session.media_vault()),
        )
        .is_ok()
    );

    let inspection = session.inspect_repair(None).unwrap();
    assert!(
        inspection.diagnosis.is_ready(),
        "{:?}",
        inspection.diagnosis
    );

    // Removing the managed media key from the vault causes inspect_repair to flag UnsupportedReference.
    session
        .media_vault()
        .remove_keys(std::slice::from_ref(&img.reference));
    let broken = session.inspect_repair(None).unwrap();
    assert!(!broken.diagnosis.is_ready());
    assert!(
        broken
            .diagnosis
            .issues
            .iter()
            .any(|issue| issue.kind == crate::application::RepairIssueKind::UnsupportedReference)
    );

    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mm05_fork_session_and_delete_source_keep_media_independent() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);

    let img_a = crate::media::prepare_image_from_bytes(minimal_png(), Some("a.png")).unwrap();
    let img_b = crate::media::prepare_image_from_bytes(minimal_jpeg(), Some("b.jpg")).unwrap();

    let source = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let source_id = source.target().conversation_id().clone();

    let w1 = source
        .submit_with_images("t1", Some("first"), std::slice::from_ref(&img_a))
        .unwrap()
        .work;
    finished(&source, &w1).await;
    source.wait_saved().await.unwrap();

    // Fork from the open idle session.
    let fork_id = host
        .fork(source_id.clone(), Some("Forked".into()))
        .await
        .unwrap();

    // Close and delete the source session completely.
    host.close(&source.target(), ClosePolicy::Save)
        .await
        .unwrap();
    host.delete(source_id.clone()).await.unwrap();
    assert!(!artifact_row_exists(
        root.path(),
        source_id.as_str(),
        &img_a.reference
    ));

    // Forked session still owns its copied artifacts row and opens + resolves + accepts new images!
    assert!(artifact_row_exists(
        root.path(),
        fork_id.as_str(),
        &img_a.reference
    ));
    let forked = host.open(fork_id.clone()).await.unwrap();
    assert!(forked.media_vault().contains(&img_a.reference));
    let resolved = forked.media_vault().resolve(&img_a.media_ref()).unwrap();
    let MediaSource::Inline { data_base64 } = resolved.source else {
        panic!("expected inline payload");
    };
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(&data_base64)
            .unwrap(),
        img_a.bytes.as_ref()
    );

    let w2 = forked
        .submit_with_images("t2", Some("fork turn"), std::slice::from_ref(&img_b))
        .unwrap()
        .work;
    finished(&forked, &w2).await;
    forked.wait_saved().await.unwrap();
    assert!(artifact_row_exists(
        root.path(),
        fork_id.as_str(),
        &img_b.reference
    ));

    host.close(&forked.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mm05_host_export_session_materials_open_and_closed_and_reimport() {
    let root = tempfile::tempdir().unwrap();
    let export_dir = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    normal_fixture(&host);

    let img = crate::media::prepare_image_from_bytes(minimal_png(), Some("export.png")).unwrap();
    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let id = session.target().conversation_id().clone();
    let w = session
        .submit_with_images("e1", Some("export me"), std::slice::from_ref(&img))
        .unwrap()
        .work;
    finished(&session, &w).await;
    session.wait_saved().await.unwrap();

    // 1. Export while open idle.
    let open_export = export_dir.path().join("open.json");
    let res_open = host
        .export_session_materials(&id, &open_export)
        .await
        .unwrap();
    assert_eq!(res_open.media_count, 1);
    assert!(res_open.media_dir.as_ref().unwrap().is_dir());

    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // 2. Export while closed (loads directly from SQLite).
    let closed_export = export_dir.path().join("closed.json");
    let res_closed = host
        .export_session_materials(&id, &closed_export)
        .await
        .unwrap();
    assert_eq!(res_closed.media_count, 1);

    // 3. Reimport via load_with_media + PreparedCreate::with_media_blobs.
    let (ctx, blobs) = crate::material::load_with_media(&closed_export).unwrap();
    let imported = host
        .create(
            PreparedCreate::persistent(definition(), ctx)
                .with_media_blobs(&blobs)
                .unwrap(),
        )
        .await
        .unwrap();
    let imported_id = imported.target().conversation_id().clone();
    assert!(imported.media_vault().contains(&img.reference));
    host.close(&imported.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // Reopen imported session to confirm SQLite persistence of imported blobs.
    let reopened = host.open(imported_id).await.unwrap();
    assert!(reopened.media_vault().contains(&img.reference));
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

async fn capture_single_http_request(
    render_options: crate::definition::RenderOptions,
    resolver: Option<Arc<dyn MediaResolver>>,
    context: &Context,
    response_json: &'static str,
) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buf = Vec::new();
        let mut chunk = [0u8; 65536];
        let header_end;
        loop {
            let n = socket.read(&mut chunk).await.unwrap();
            assert!(n > 0, "unexpected EOF before HTTP headers");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                header_end = pos + 4;
                break;
            }
        }
        let headers = String::from_utf8_lossy(&buf[..header_end]);
        let content_length: usize = headers
            .lines()
            .find_map(|line| {
                let (name, val) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| val.trim().parse().ok())
                    .flatten()
            })
            .unwrap_or(0);
        while buf.len() < header_end + content_length {
            let n = socket.read(&mut chunk).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let body =
            String::from_utf8_lossy(&buf[header_end..header_end + content_length]).into_owned();
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_json.len(),
            response_json
        );
        socket.write_all(resp.as_bytes()).await.unwrap();
        body
    });

    let gateway = crate::assembly::protocol_gateway(
        render_options,
        "test-key".into(),
        Some(format!("http://{addr}")),
        resolver,
    );
    let mut request = causa::kernel::ModelRequest::new(
        causa::kernel::InvocationId {
            turn_id: causa::kernel::TurnId::new("test-turn"),
            round_id: causa::kernel::RoundId(0),
        },
        context.frame(),
        causa::kernel::ModelRef::new("test-model"),
    );
    request.generation.max_tokens = Some(64);
    gateway
        .invoke(
            &request,
            &causa::kernel::CallControl::new(causa::kernel::CancellationToken::new(), None),
        )
        .await
        .unwrap();
    server.await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mm03_protocol_gateway_renders_exact_5mib_image_and_omits_summary_resolver() {
    use crate::definition::RenderOptions;

    let exact_bytes = png_bytes(crate::media::MAX_IMAGE_BYTES, 0x5a);
    let prepared =
        crate::media::prepare_image_from_bytes(exact_bytes.clone(), Some("5mib.png")).unwrap();
    let expected_b64 = base64::engine::general_purpose::STANDARD.encode(&exact_bytes);
    let vault = Arc::new(crate::media::MediaVault::new());
    vault.insert_batch(std::slice::from_ref(&prepared)).unwrap();
    let context = Context::from_blocks(vec![
        crate::media::multimodal_input(Some("check 5mib"), &[prepared.media_ref()]).unwrap(),
    ])
    .unwrap();

    // 1. Anthropic Messages
    let anthropic_resp = r#"{"id":"msg_1","type":"message","role":"assistant","model":"test-model","content":[{"type":"text","text":"ok"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
    let body = capture_single_http_request(
        RenderOptions::Anthropic(Default::default()),
        Some(vault.clone()),
        &context,
        anthropic_resp,
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["messages"][0]["content"][1]["type"], "image");
    assert_eq!(
        json["messages"][0]["content"][1]["source"]["data"],
        expected_b64
    );

    // 2. OpenAI Chat Completions
    let chat_resp = r#"{"id":"chat_1","object":"chat.completion","model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#;
    let body = capture_single_http_request(
        RenderOptions::Chat(Default::default()),
        Some(vault.clone()),
        &context,
        chat_resp,
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["messages"][0]["content"][1]["type"], "image_url");
    assert_eq!(
        json["messages"][0]["content"][1]["image_url"]["url"],
        format!("data:image/png;base64,{expected_b64}")
    );

    // 3. OpenAI Responses
    let responses_resp = r#"{"id":"resp_1","object":"response","status":"completed","model":"test-model","output":[{"type":"message","id":"m1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"ok"}]}],"usage":{"input_tokens":1,"output_tokens":1}}"#;
    let body = capture_single_http_request(
        RenderOptions::Responses(Default::default()),
        Some(vault),
        &context,
        responses_resp,
    )
    .await;
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["input"][0]["content"][1]["type"], "input_image");
    assert_eq!(
        json["input"][0]["content"][1]["image_url"],
        format!("data:image/png;base64,{expected_b64}")
    );

    // 4. Summary gateway without MediaResolver degrades to text placeholder without uploading bytes.
    let summary_body = capture_single_http_request(
        RenderOptions::Chat(Default::default()),
        None,
        &context,
        chat_resp,
    )
    .await;
    assert!(!summary_body.contains(&expected_b64));
    assert!(summary_body.contains("5mib.png") || summary_body.contains("image/png"));
}

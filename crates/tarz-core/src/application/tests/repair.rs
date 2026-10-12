use super::support::*;
use causa::kernel::{ToolCallPayload, ToolOutput, ToolResultPayload, ToolResultStatus};

fn call(wire: &str) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolCall(ToolCallPayload::new(
            "read_file",
            serde_json::json!({"path":"example.txt"}),
        )),
        BlockMeta::default()
            .with_provider_call_id(wire)
            .with_source("assistant"),
    )
}
fn result(call: &ContextBlock, status: ToolResultStatus) -> ContextBlock {
    ContextBlock::new(
        BlockId::new(uuid::Uuid::now_v7()),
        BlockContent::ToolResult(
            ToolResultPayload::new(
                call.id(),
                status,
                ToolOutput::new(serde_json::json!({"actual":"kept"})),
            )
            .with_notes(vec![causa::kernel::TextPayload::new("original note")]),
        ),
        BlockMeta::default(),
    )
}
async fn session(
    host: &SessionHost,
    blocks: Vec<ContextBlock>,
    provider: Provider,
    persistent: bool,
) -> ManagedSession {
    normal_fixture(host);
    let mut def = definition();
    def.provider = provider;
    let context = Context::from_blocks(blocks).unwrap();
    host.create(if persistent {
        PreparedCreate::persistent(def, context)
    } else {
        PreparedCreate::transient(def, context)
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn repair_remove_preserves_facts_and_protocol_readiness_for_all_providers() {
    for provider in [
        Provider::OpenAiChat,
        Provider::OpenAiResponses,
        Provider::Anthropic,
    ] {
        let host = SessionHost::new(None);
        let paired = call("kept-call");
        let unknown = result(&paired, ToolResultStatus::UnknownOutcome);
        let unpaired = call("removed-call");
        let originals = vec![
            text_input("original user"),
            paired,
            unknown,
            unpaired.clone(),
        ];
        let session = session(&host, originals.clone(), provider, false).await;
        let before = session.read_materials().unwrap();
        let inspection = session.inspect_repair(None).unwrap();
        assert_eq!(inspection.version, before.version);
        assert_eq!(inspection.diagnosis.unpaired_count, 1);
        assert_eq!(
            inspection.diagnosis.issues[0].kind,
            RepairIssueKind::UnpairedDeclaration
        );
        let input = request("blocked");
        let original_input = input.blocks.clone();
        let rejected = session.submit(input).unwrap_err();
        assert!(matches!(
            rejected.reason,
            ApplicationError::Repair(RepairError::Materials(_))
        ));
        assert_eq!(rejected.request.blocks, original_input);
        assert_eq!(
            session.read_materials().unwrap().context.blocks(),
            originals
        );
        assert!(session.status().active_work.is_none());

        let plan = session.plan_repair(RepairMode::RemoveUnpaired).unwrap();
        assert_eq!(plan.summary().removed, vec![unpaired.id()]);
        assert_eq!(plan.summary().adopted_count, 0);
        assert_eq!(&plan.candidate().blocks()[..3], &originals[..3]);
        assert!(
            plan.candidate().blocks()[3]
                .meta()
                .source
                .as_deref()
                .unwrap()
                .starts_with("application:tarz:")
        );
        assert_eq!(session.read_materials().unwrap().version, before.version);
        session.apply_repair(&plan).unwrap();
        assert!(session.inspect_repair(None).unwrap().diagnosis.is_ready());
        assert!(matches!(
            session.apply_repair(&plan),
            Err(ApplicationError::StaleMaterial)
        ));
        assert!(matches!(
            session.plan_repair(RepairMode::RemoveUnpaired),
            Err(ApplicationError::Repair(RepairError::NotNeeded))
        ));
        let receipt = session.submit(rejected.request).unwrap();
        finished(&session, &receipt.work).await;
        session.wait_saved().await.unwrap();
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn repair_rejects_orphans_duplicate_results_wire_ambiguity_and_media_without_edits() {
    let declaration = call("wire");
    let orphan = result(&declaration, ToolResultStatus::Succeeded);
    let cases = [
        (vec![orphan.clone()], RepairIssueKind::OrphanResult),
        (
            vec![
                declaration.clone(),
                orphan.clone(),
                result(&declaration, ToolResultStatus::Failed),
            ],
            RepairIssueKind::DuplicateResult,
        ),
        (
            vec![declaration.clone(), call("wire")],
            RepairIssueKind::AmbiguousWireIdentity,
        ),
        (
            vec![
                ContextBlock::new(
                    BlockId::new(uuid::Uuid::now_v7()),
                    BlockContent::Parts(vec![causa::kernel::ContentPart::Media(
                        causa::kernel::MediaRef::new("image/png", "not resolved"),
                    )]),
                    BlockMeta::default(),
                ),
                declaration,
            ],
            RepairIssueKind::UnsupportedReference,
        ),
    ];
    for (blocks, kind) in cases {
        let host = SessionHost::new(None);
        let session = session(&host, blocks.clone(), Provider::OpenAiChat, false).await;
        let before = session.read_materials().unwrap();
        let inspection = session.inspect_repair(None).unwrap();
        assert!(
            inspection
                .diagnosis
                .issues
                .iter()
                .any(|issue| issue.kind == kind)
        );
        assert!(matches!(
            session.plan_repair(RepairMode::RemoveUnpaired),
            Err(ApplicationError::Repair(RepairError::Materials(_)))
        ));
        let after = session.read_materials().unwrap();
        assert_eq!(after.version, before.version);
        assert_eq!(after.context.blocks(), blocks);
        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn repair_render_rejects_interleaved_history_and_diagnostics_are_bounded() {
    let host = SessionHost::new(None);
    let declaration = call("interleaved");
    let blocks = vec![
        declaration.clone(),
        text_input("interleaving input"),
        result(&declaration, ToolResultStatus::Succeeded),
        call("pending"),
    ];
    let session = session(&host, blocks.clone(), Provider::OpenAiChat, false).await;
    let error = session.plan_repair(RepairMode::RemoveUnpaired).unwrap_err();
    assert!(
        matches!(error, ApplicationError::Repair(RepairError::Materials(ref diagnosis)) if diagnosis.issues.iter().any(|issue|issue.kind == RepairIssueKind::ProtocolStructure))
    );
    assert_eq!(session.read_materials().unwrap().context.blocks(), blocks);
    let snapshot = session.read_materials().unwrap();
    let many: Vec<_> = (0..1000)
        .map(|index| call(&format!("call-{index}")))
        .collect();
    session.edit(snapshot.version, many).unwrap();
    let diagnosis = session.inspect_repair(None).unwrap().diagnosis;
    assert_eq!(diagnosis.unpaired_count, 1000);
    assert_eq!(diagnosis.issue_count, 1000);
    assert_eq!(diagnosis.issues.len(), 128);
    let plan = session.plan_repair(RepairMode::RemoveUnpaired).unwrap();
    assert_eq!(plan.summary().removed_count, 1000);
    assert_eq!(plan.summary().removed.len(), 128);
    assert_eq!(plan.candidate().blocks().len(), 1);
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test]
async fn repair_reopen_has_no_original_batch_and_rejects_old_instance_or_material_plan() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let session = session(&host, vec![call("pending")], Provider::Anthropic, true).await;
    let old_plan = session.plan_repair(RepairMode::RemoveUnpaired).unwrap();
    let id = session.target().conversation_id().clone();
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
    let reopened = host.open(id).await.unwrap();
    assert!(matches!(
        reopened.apply_repair(&old_plan),
        Err(ApplicationError::StaleInstance)
    ));
    assert!(reopened.inspect_repair(None).unwrap().original.is_none());
    let plan = reopened.plan_repair(RepairMode::RemoveUnpaired).unwrap();
    let snapshot = reopened.read_materials().unwrap();
    reopened
        .edit(snapshot.version, snapshot.context.into_blocks())
        .unwrap();
    reopened.wait_saved().await.unwrap();
    assert!(matches!(
        reopened.apply_repair(&plan),
        Err(ApplicationError::StaleMaterial)
    ));
    let plan = reopened.plan_repair(RepairMode::RemoveUnpaired).unwrap();
    reopened.apply_repair(&plan).unwrap();
    assert!(matches!(
        reopened.wait_saved().await.unwrap(),
        SaveStatus::Saved { revision: 3, .. }
    ));
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test]
async fn repair_save_failure_retries_the_same_commit_and_retained_key_replay_wins() {
    let root = tempfile::tempdir().unwrap();
    let host = SessionHost::new(Some(root.path().into()));
    let calls = normal_fixture(&host);
    let session = host
        .create(PreparedCreate::persistent(definition(), Context::new()))
        .await
        .unwrap();
    let first = request("retained");
    let blocks = first.blocks.clone();
    let receipt = session.submit(first).unwrap();
    finished(&session, &receipt.work).await;
    session.wait_saved().await.unwrap();
    let snapshot = session.read_materials().unwrap();
    let mut materials = snapshot.context.into_blocks();
    materials.push(call("pending"));
    session.edit(snapshot.version, materials).unwrap();
    session.wait_saved().await.unwrap();
    assert_eq!(
        session
            .submit(SubmitRequest {
                request_key: "retained".into(),
                blocks
            })
            .unwrap()
            .work,
        receipt.work
    );
    let plan = session.plan_repair(RepairMode::RemoveUnpaired).unwrap();
    session
        .entry
        .lock()
        .writer
        .as_ref()
        .unwrap()
        .set_fault_hook(Some(Arc::new(|stage| {
            if stage == CommitStage::Write {
                FaultAction::Fail(std::io::Error::other("repair disk failure"))
            } else {
                FaultAction::Continue
            }
        })));
    session.apply_repair(&plan).unwrap();
    assert!(session.wait_saved().await.is_err());
    let commit = match session.status().save {
        SaveStatus::SaveBlocked { commit_id, .. } => commit_id,
        _ => panic!("blocked"),
    };
    assert!(matches!(
        session.submit(request("new")).unwrap_err().reason,
        ApplicationError::SaveBlocked(_)
    ));
    assert!(matches!(
        session.plan_repair(RepairMode::RemoveUnpaired),
        Err(ApplicationError::SaveBlocked(_))
    ));
    assert!(session.inspect_repair(None).unwrap().diagnosis.is_ready());
    session
        .entry
        .lock()
        .writer
        .as_ref()
        .unwrap()
        .set_fault_hook(None);
    session.retry_save().await.unwrap();
    let record = crate::store::SessionStore::new(root.path().into())
        .list()
        .unwrap()
        .remove(0)
        .record
        .unwrap();
    assert_eq!(record.last_commit_id, commit);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let next = session.submit(request("new")).unwrap();
    finished(&session, &next.work).await;
    session.wait_saved().await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repair_busy_closing_and_lost_context_reject_without_manufacturing_materials() {
    let host = SessionHost::new(None);
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
        .create(PreparedCreate::transient(
            definition(),
            Context::from_blocks(vec![call("pending")]).unwrap(),
        ))
        .await
        .unwrap();
    let old = session.plan_repair(RepairMode::RemoveUnpaired).unwrap();
    session.apply_repair(&old).unwrap();
    let work = session.submit(request("running")).unwrap().work;
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .unwrap();
    assert!(matches!(
        session.plan_repair(RepairMode::RemoveUnpaired),
        Err(ApplicationError::Session(
            crate::session::SessionError::Busy { .. }
        ))
    ));
    assert!(session.inspect_repair(None).is_err());
    assert!(session.apply_repair(&old).is_err());
    release.notify_one();
    finished(&session, &work).await;
    session.wait_saved().await.unwrap();
    let snapshot = session.read_materials().unwrap();
    let mut blocks = snapshot.context.into_blocks();
    blocks.push(call("next-unpaired"));
    session.edit(snapshot.version, blocks).unwrap();
    let plan = session.plan_repair(RepairMode::RemoveUnpaired).unwrap();
    let selected = Arc::new(Notify::new());
    let finish = Arc::new(Notify::new());
    *session.entry.before_close.lock().unwrap() = Some((selected.clone(), finish.clone()));
    let closing = {
        let host = host.clone();
        let target = session.target();
        tokio::spawn(async move { host.close(&target, ClosePolicy::Save).await })
    };
    tokio::time::timeout(Duration::from_secs(5), selected.notified())
        .await
        .unwrap();
    assert!(matches!(
        session.apply_repair(&plan),
        Err(ApplicationError::Closing)
    ));
    finish.notify_one();
    closing.await.unwrap().unwrap();

    let host = SessionHost::new(None);
    fixture(
        &host,
        Gateway {
            calls: Arc::new(AtomicUsize::new(0)),
            entered: None,
            release: None,
            panic: true,
        },
    );
    let session = host
        .create(PreparedCreate::transient(
            definition(),
            Context::from_blocks(vec![call("before-fault")]).unwrap(),
        ))
        .await
        .unwrap();
    let plan = session.plan_repair(RepairMode::RemoveUnpaired).unwrap();
    session.apply_repair(&plan).unwrap();
    let work = session.submit(request("fault")).unwrap().work;
    finished(&session, &work).await;
    assert!(session.wait_saved().await.is_err());
    assert!(session.apply_repair(&plan).is_err());
    assert!(session.plan_repair(RepairMode::RemoveUnpaired).is_err());
    assert!(matches!(
        session.inspect_repair(None),
        Err(ApplicationError::Session(
            crate::session::SessionError::ContextUnavailable
        ))
    ));
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

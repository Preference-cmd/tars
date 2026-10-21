use super::support::*;
use super::workspace_http as wire;
use crate::definition::WorkspaceTool;
use causa::kernel::ToolResultStatus;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn repair_real_three_protocols_cancel_adopt_or_reopen_remove_save_and_continue() {
    for (protocol, provider) in [
        ("openai-chat", Provider::OpenAiChat),
        ("openai-responses", Provider::OpenAiResponses),
        ("anthropic", Provider::Anthropic),
    ] {
        for adopt in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let workspace = dir.path().join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            std::fs::write(
                workspace.join("evidence.txt"),
                "actual later file content\n",
            )
            .unwrap();
            let fixture = wire::Fixture::new(
                protocol,
                vec![
                    wire::Reply::Read("evidence.txt"),
                    wire::Reply::Answer,
                    wire::Reply::Read("evidence.txt"),
                    wire::Reply::Answer,
                ],
                None,
            );
            fixture.release();
            let host = SessionHost::new(Some(dir.path().join("store")));
            let permits = host
                .inner
                .tool_resources
                .slots
                .clone()
                .acquire_many_owned(16)
                .await
                .unwrap();
            let mut def = definition();
            def.provider = provider;
            def.endpoint = Some(fixture.endpoint.clone());
            def.credential = CredentialRef::Environment("PATH".into());
            def.workspace_root = Some(std::fs::canonicalize(workspace).unwrap());
            def.tools = vec![WorkspaceTool::ReadFile];
            def.max_model_rounds = 4;
            def.max_tool_calls = 4;
            def.max_tokens = (provider == Provider::Anthropic).then_some(1024);
            let mut session = host
                .create(PreparedCreate::persistent(def, Context::new()))
                .await
                .unwrap();
            let original = session
                .submit(SubmitRequest {
                    request_key: "original".into(),
                    blocks: vec![text_input("first actual request")],
                })
                .unwrap()
                .work;
            until(|| {
                host.inner
                    .tool_resources
                    .counters
                    .test_wrappers_entered
                    .load(Ordering::SeqCst)
                    == 1
            })
            .await;
            session.cancel(&original).unwrap();
            finished(&session, &original).await;
            session.wait_saved().await.unwrap();
            drop(permits);
            let before = session.read_materials().unwrap();
            let original_call = before
                .context
                .blocks()
                .iter()
                .find(|block| matches!(block.content(), BlockContent::ToolCall(_)))
                .unwrap()
                .id();
            assert_eq!(
                host.inner
                    .tool_resources
                    .counters
                    .started
                    .load(Ordering::SeqCst),
                0
            );
            let result = match session.observe(&original).unwrap().state {
                WorkState::Finished(result) => result,
                _ => unreachable!(),
            };
            assert!(matches!(
                result.as_ref(),
                WorkResult::Run(causa::runtime::TurnResult::Interrupted {
                    cause: causa::runtime::TurnInterruption::Cancelled { .. }
                })
            ));
            let inspection = session.inspect_repair(Some(&original)).unwrap();
            let batch = inspection.original.unwrap();
            assert_eq!(batch.completed_count, 1);
            assert_eq!(batch.results[0].status, ToolResultStatus::UnknownOutcome);
            assert!(batch.adoption_error.is_none());
            if !adopt {
                let target = session.target();
                host.close(&target, ClosePolicy::Save).await.unwrap();
                session = host.open(target.conversation_id().clone()).await.unwrap();
                assert!(session.inspect_repair(None).unwrap().original.is_none());
            }
            let plan = session
                .plan_repair(if adopt {
                    RepairMode::AdoptOriginal(original.clone())
                } else {
                    RepairMode::RemoveUnpaired
                })
                .unwrap();
            assert_eq!(fixture.requests.lock().unwrap().len(), 1);
            session.apply_repair(&plan).unwrap();
            session.wait_saved().await.unwrap();
            assert_eq!(fixture.requests.lock().unwrap().len(), 1);
            assert_eq!(
                host.inner
                    .tool_resources
                    .counters
                    .started
                    .load(Ordering::SeqCst),
                0
            );
            if adopt {
                assert!(
                    matches!(session.observe(&original).unwrap().state, WorkState::Finished(current) if Arc::ptr_eq(&result, &current))
                );
                assert!(
                    session
                        .inspect_repair(Some(&original))
                        .unwrap()
                        .original
                        .unwrap()
                        .available
                );
            }
            let next = session
                .submit(SubmitRequest {
                    request_key: "new-answer".into(),
                    blocks: vec![text_input("answer using the repaired history")],
                })
                .unwrap();
            finished(&session, &next.work).await;
            session.wait_saved().await.unwrap();
            assert!(
                matches!(session.observe(&next.work).unwrap().state, WorkState::Finished(current) if matches!(current.as_ref(), WorkResult::Run(causa::runtime::TurnResult::Completed { .. })))
            );
            assert_eq!(
                host.inner
                    .tool_resources
                    .counters
                    .started
                    .load(Ordering::SeqCst),
                0,
                "history must never execute old tools"
            );
            let next = session
                .submit(SubmitRequest {
                    request_key: "new-tool".into(),
                    blocks: vec![text_input("make a new read declaration")],
                })
                .unwrap();
            finished(&session, &next.work).await;
            session.wait_saved().await.unwrap();
            fixture.wait_for_requests(4).await;
            assert_eq!(fixture.accepted.load(Ordering::SeqCst), 4);
            assert_eq!(
                host.inner
                    .tool_resources
                    .counters
                    .started
                    .load(Ordering::SeqCst),
                1
            );
            assert_eq!(
                host.inner
                    .tool_resources
                    .counters
                    .completed
                    .load(Ordering::SeqCst),
                1
            );
            let context = session.read_materials().unwrap().context;
            let succeeded: Vec<_> = context
                .blocks()
                .iter()
                .filter_map(|block| {
                    if let BlockContent::ToolResult(result) = block.content()
                        && result.status == ToolResultStatus::Succeeded
                    {
                        Some(result)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(succeeded.len(), 1);
            assert_ne!(succeeded[0].call_block_id, original_call);
            assert!(
                succeeded[0]
                    .output
                    .content
                    .to_string()
                    .contains("actual later file content")
            );
            {
                let requests = fixture.requests.lock().unwrap();
                assert!(requests[1].to_string().contains("material_repair"));
                assert_eq!(requests[1].to_string().contains("provider-call-0"), adopt);
                assert!(
                    requests[3]
                        .to_string()
                        .contains("actual later file content")
                );
            }
            let target = session.target();
            host.close(&target, ClosePolicy::Save).await.unwrap();
            let reopened = host.open(target.conversation_id().clone()).await.unwrap();
            assert_eq!(
                reopened.read_materials().unwrap().context.blocks(),
                context.blocks()
            );
            assert!(reopened.inspect_repair(None).unwrap().diagnosis.is_ready());
            assert_eq!(fixture.requests.lock().unwrap().len(), 4);
            host.close(&reopened.target(), ClosePolicy::Save)
                .await
                .unwrap();
        }
    }
}

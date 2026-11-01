use super::support::*;
use crate::sandbox::{BackendId, SandboxStatus};

/// Display-only coverage: the unavailable status is injected at the fixture
/// seam, so this covers only the `ManagedSession` accessor and the display
/// value. Assembly propagation of a real probe failure is covered by
/// `a_failed_probe_binds_an_unavailable_session_and_rejects_every_call` below.
#[tokio::test]
async fn managed_session_reports_an_injected_unavailable_command_status() {
    let host = SessionHost::new(None);
    let runner = Arc::new(TurnRunner::new(
        Arc::new(Gateway {
            calls: Arc::new(AtomicUsize::new(0)),
            entered: None,
            release: None,
            panic: false,
        }),
        Arc::new(ToolExecutor::new(vec![], ToolExecutorOptions::default()).unwrap()),
    ));
    *host.inner.assemble.lock().unwrap() = Some(Arc::new(move |definition, id, context| {
        let runner = runner.clone();
        let options = definition.run_options();
        let session = Session::new(
            id,
            context,
            move |_, _| {
                let runner = runner.clone();
                let options = options.clone();
                async move { Ok((runner, options)) }
            },
            definition.session_config(),
        );
        Ok(session.with_command_status(SandboxStatus::Unavailable(
            tars_sandbox::test_support::unavailable(
                BackendId::UNAVAILABLE,
                "no backend for this platform",
            ),
        )))
    }));

    let session = host
        .create(PreparedCreate::transient(definition(), Context::default()))
        .await
        .unwrap();
    let status = session.command_status().expect("command status");
    assert!(!status.is_available());
    assert_eq!(status.to_string(), "no backend for this platform");
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

/// A failed probe bound through the real assembly path leaves the session open
/// with an unavailable command status, and every bound call is Rejected with
/// the probe reason before a process can start.
///
/// Unix-only: it binds a real workspace, and workspace tools have no
/// implementation on other platforms. The cross-platform failed-probe tool test
/// uses a recording workspace and lives in `tools::command_tool_tests`.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_probe_binds_an_unavailable_session_and_rejects_every_call() {
    use crate::definition::{CommandSettings, WorkspaceTool};
    use causa::kernel::{
        BlockId, CallControl, InvocationId, RoundId, ToolBatch, ToolCallContext, ToolCallPayload,
        ToolResultStatus, TurnId,
    };
    use tokio_util::sync::CancellationToken;

    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let backend = tars_sandbox::test_support::RecordingBackend::unavailable();
    let launches = backend.calls();
    let error = crate::sandbox::Sandbox::open_with(Box::new(backend)).unwrap_err();
    let host = SessionHost::new(None);
    host.set_test_command_sandbox(Some(Err(error)));

    let mut selected = definition();
    selected.credential = CredentialRef::Environment("PATH".into());
    selected.workspace_root = Some(workspace.clone());
    selected.tools = vec![WorkspaceTool::RunCommand];
    selected.command = Some(CommandSettings::default());
    let session = host
        .create(PreparedCreate::transient(selected, Context::default()))
        .await
        .expect("a failed probe binds the session with the tool disabled");

    let status = session
        .command_status()
        .expect("the bound command tool reports its probe result");
    assert!(!status.is_available(), "{status}");
    assert!(
        status.to_string().contains("sandbox is unavailable"),
        "{status}"
    );
    assert!(status.to_string().contains("probe failed"), "{status}");

    let calls: Vec<ToolCallContext> = ["first", "second"]
        .into_iter()
        .map(|name| {
            ToolCallContext::new(
                BlockId::new(uuid::Uuid::now_v7()),
                ToolCallPayload::new(
                    "run_command",
                    serde_json::json!({ "command": format!("printf ran > {name}.txt") }),
                ),
            )
        })
        .collect();
    let turn_id = TurnId::new(uuid::Uuid::now_v7().to_string());
    let report = crate::session::ToolReporter::new(
        crate::session::WorkRef {
            conversation_id: "command-status".into(),
            turn_id: turn_id.clone(),
        },
        tokio::sync::broadcast::channel(128).0,
    );
    report.batch_ready(
        &InvocationId {
            turn_id,
            round_id: RoundId(0),
        },
        &ToolBatch::new(calls.clone()).unwrap(),
    );
    let tools = session
        .workspace_tools()
        .expect("the bound command tool set");
    let tool = tools
        .bind_for_test(&[WorkspaceTool::RunCommand], report.clone())
        .pop()
        .unwrap();
    let control = CallControl::new(CancellationToken::new(), None);
    for invocation in &calls {
        let output = tool.execute(invocation, &control, None).await;
        assert_eq!(output.status, ToolResultStatus::Rejected);
        let reason = output.output.content["error"].as_str().unwrap_or_default();
        assert!(reason.contains("unavailable"), "{reason}");
        assert!(reason.contains("probe failed"), "{reason}");
    }
    assert!(
        launches.lock().unwrap().is_empty(),
        "a failed probe must not launch a process"
    );
    assert!(!workspace.join("first.txt").exists());
    assert!(!workspace.join("second.txt").exists());
    report.drain().await;
    host.close(&session.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
}

//! The command sandbox status captured while a session is assembled.
#![cfg(target_os = "macos")]

use causa::kernel::Context;
use tars_core::application::{ClosePolicy, PreparedCreate, SessionHost};
use tars_core::definition::{CredentialRef, Provider, SessionDefinition, WorkspaceTool};

fn definition(
    tools: Vec<WorkspaceTool>,
    workspace_root: Option<std::path::PathBuf>,
) -> SessionDefinition {
    SessionDefinition {
        provider: Provider::OpenAiChat,
        model: "fixture".into(),
        endpoint: None,
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(10),
        retained_work_capacity: 1,
        max_model_rounds: 1,
        max_tool_calls: 1,
        max_tokens: None,
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root,
        tools,
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn command_status_is_none_without_the_tool_and_reports_the_probe_with_it() {
    let host = SessionHost::new(None);
    let plain = host
        .create(PreparedCreate::transient(
            definition(Vec::new(), None),
            Context::default(),
        ))
        .await
        .unwrap();
    assert!(plain.command_status().is_none());

    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let with_command = host
        .create(PreparedCreate::transient(
            definition(vec![WorkspaceTool::RunCommand], Some(root)),
            Context::default(),
        ))
        .await
        .unwrap();
    let status = with_command
        .command_status()
        .expect("a command tool has a probed sandbox");
    assert!(status.is_available());
    assert!(
        status
            .to_string()
            .starts_with("seatbelt: file read full; file write full; network full; "),
        "{status}"
    );
    assert!(
        status.brief().starts_with("seatbelt: "),
        "{}",
        status.brief()
    );

    // Closing releases the instance; the view keeps reporting no live status.
    host.close(&plain.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
    host.close(&with_command.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();
    assert!(with_command.command_status().is_none());
}

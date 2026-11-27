use super::provider::{AssemblyError, assemble, assemble_with_runner};
use crate::definition::{CredentialRef, DefinitionError, Provider, SessionDefinition};
use crate::session::{SubmitRequest, WorkResult, WorkState};
use async_trait::async_trait;
use causa::kernel::{
    CallControl, Context, ModelGateway, ModelInvokeError, ModelOutput, ModelRequest,
};
use causa::runtime::{ToolExecutor, TurnInterruption, TurnResult, TurnRunner};
use std::sync::Arc;
use std::time::Duration;
#[cfg(unix)]
use tokio_util::sync::CancellationToken;

fn definition() -> SessionDefinition {
    SessionDefinition {
        provider: Provider::Anthropic,
        model: "explicit-model".into(),
        endpoint: None,
        credential: CredentialRef::Environment(format!(
            "TARS_MISSING_{}",
            uuid::Uuid::now_v7().simple()
        )),
        work_timeout_secs: None,
        retained_work_capacity: 1,
        max_model_rounds: 0,
        max_tool_calls: 0,
        max_tokens: None,
        reasoning_effort: None,
        temperature: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: None,
        tools: vec![],
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
fn invalid_definition_is_rejected_before_credential_resolution() {
    let mut definition = definition();
    definition.model.clear();
    assert!(matches!(
        assemble(
            &definition,
            "id".into(),
            Context::default(),
            crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16)),
            crate::permission::PermissionMode::Ask,
            None,
            true,
        ),
        Err(AssemblyError::Definition(DefinitionError::BlankModel))
    ));
}

#[test]
fn missing_credentials_report_only_a_sanitized_resolution_error() {
    let definition = definition();
    let error = match assemble(
        &definition,
        "id".into(),
        Context::default(),
        crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16)),
        crate::permission::PermissionMode::Ask,
        None,
        true,
    ) {
        Ok(_) => panic!("missing credential accepted"),
        Err(error) => error,
    };
    assert!(matches!(error, AssemblyError::CredentialUnavailable(_)));
    assert!(!error.to_string().contains("TARS_MISSING_"));
}

struct MustNotInvoke;
#[async_trait]
impl ModelGateway for MustNotInvoke {
    async fn invoke(
        &self,
        _: &ModelRequest,
        _: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        panic!("zero saved model rounds must prevent invocation")
    }
}

#[tokio::test]
async fn assembled_work_uses_frozen_limits_and_supplied_conversation_identity() {
    let mut definition = definition();
    let runner = Arc::new(TurnRunner::new(
        Arc::new(MustNotInvoke),
        Arc::new(ToolExecutor::new(vec![], Default::default()).unwrap()),
    ));
    let mut session = assemble_with_runner(
        &definition,
        "supplied-id".into(),
        Context::default(),
        runner,
    );
    definition.max_model_rounds = 99;
    definition.retained_work_capacity = 0;
    let handle = session.handle();
    let receipt = handle
        .submit(SubmitRequest {
            request_key: "work".into(),
            blocks: vec![],
        })
        .unwrap();
    assert_eq!(receipt.work.conversation_id.as_str(), "supplied-id");
    let observation = handle
        .wait(&receipt.work, Duration::from_secs(5))
        .await
        .unwrap();
    let WorkState::Finished(result) = observation.state else {
        panic!("work unfinished")
    };
    assert!(matches!(
        &*result,
        WorkResult::Run(TurnResult::Interrupted {
            cause: TurnInterruption::MaxModelRounds { limit: 0, .. }
        })
    ));
    session.shutdown().await;
}

#[test]
fn unavailable_workspace_is_rejected_before_credentials_or_model_access() {
    let mut definition = definition();
    definition.workspace_root = Some(
        std::env::current_dir()
            .unwrap()
            .join(uuid::Uuid::now_v7().to_string()),
    );
    definition.tools = vec![crate::definition::WorkspaceTool::ReadFile];
    assert!(matches!(
        assemble(
            &definition,
            "id".into(),
            Context::default(),
            crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16)),
            crate::permission::PermissionMode::Ask,
            None,
            true,
        ),
        Err(AssemblyError::Workspace(_))
    ));
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn host_reopens_only_saved_workspace_definition_and_rejects_missing_root() {
    use crate::application::{ClosePolicy, PreparedCreate, SessionHost};
    let workspace = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let root = workspace.path().join("selected");
    std::fs::create_dir(&root).unwrap();
    let mut chosen = definition();
    chosen.credential = CredentialRef::Environment("PATH".into());
    chosen.workspace_root = Some(root.canonicalize().unwrap());
    chosen.tools = vec![crate::definition::WorkspaceTool::ReadFile];
    chosen.tool_limits.max_concurrent_operations = 2;
    let saved_definition = chosen.clone();
    let host = SessionHost::new(Some(storage.path().to_owned()));
    let prepared = PreparedCreate::persistent(chosen, Context::default());
    let id = prepared.conversation_id().clone();
    let session = host.create(prepared).await.unwrap();
    assert_eq!(session.definition(), saved_definition);
    host.close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();
    let reopened = host.open(id.clone()).await.unwrap();
    assert_eq!(reopened.definition(), saved_definition);
    host.close(&reopened.target(), ClosePolicy::Save)
        .await
        .unwrap();
    std::fs::remove_dir(&root).unwrap();
    let error = match host.open(id).await {
        Ok(_) => panic!("missing saved workspace accepted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("workspace resource unavailable"));
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn unsupported_platform_rejects_enabled_workspace_but_still_assembles_empty_catalog() {
    let mut chosen = definition();
    chosen.credential = CredentialRef::Environment("PATH".into());
    chosen.workspace_root = Some(std::env::current_dir().unwrap());
    chosen.tools = vec![crate::definition::WorkspaceTool::ReadFile];
    assert!(matches!(
        assemble(&chosen, "id".into(), Context::default(), crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16)), crate::permission::PermissionMode::Ask, None, true),
        Err(AssemblyError::Workspace(error)) if matches!(&*error, crate::workspace::WorkspaceError::UnsupportedPlatform)
    ));
    chosen.tools.clear();
    assert!(
        assemble(
            &chosen,
            "id".into(),
            Context::default(),
            crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16)),
            crate::permission::PermissionMode::Ask,
            None,
            true,
        )
        .is_ok()
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn command_runtime_is_allowed_on_reopen_but_not_on_create_without_a_backend() {
    let directory = tempfile::tempdir().unwrap();
    let mut chosen = definition();
    chosen.credential = CredentialRef::Environment("PATH".into());
    chosen.workspace_root = Some(directory.path().canonicalize().unwrap());
    chosen.tools = vec![crate::definition::WorkspaceTool::RunCommand];
    chosen.command = Some(crate::definition::CommandSettings::default());
    let unavailable = tars_sandbox::test_support::unavailable(
        tars_sandbox::BackendId::UNAVAILABLE,
        "no sandbox backend on test platform",
    );
    assert!(matches!(
        assemble(
            &chosen,
            "id".into(),
            Context::default(),
            crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16))
                .with_test_command_sandbox(Err(unavailable.clone())),
            crate::permission::PermissionMode::Ask,
            None,
            false,
        ),
        Err(AssemblyError::CommandUnavailable(_))
    ));
    assert!(
        assemble(
            &chosen,
            "id".into(),
            Context::default(),
            crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16))
                .with_test_command_sandbox(Err(unavailable)),
            crate::permission::PermissionMode::Ask,
            None,
            true,
        )
        .is_ok()
    );
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[test]
fn command_creation_without_a_backend_names_the_build_platform() {
    let directory = tempfile::tempdir().unwrap();
    let mut chosen = definition();
    chosen.credential = CredentialRef::Environment("PATH".into());
    chosen.workspace_root = Some(directory.path().canonicalize().unwrap());
    chosen.tools = vec![crate::definition::WorkspaceTool::RunCommand];
    chosen.command = Some(crate::definition::CommandSettings::default());
    let error = match assemble(
        &chosen,
        "id".into(),
        Context::default(),
        crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16)),
        crate::permission::PermissionMode::Ask,
        None,
        false,
    ) {
        Ok(_) => panic!("a platform without a sandbox backend must fail creation"),
        Err(error) => error,
    };
    let reason = error.to_string();
    assert!(
        reason.contains("requires a platform sandbox backend"),
        "{reason}"
    );
    assert!(
        reason.contains(std::env::consts::OS),
        "the error must name the platform: {reason}"
    );
}

/// Unix-only: assembly opens a real workspace, and workspace tools have no
/// implementation on other platforms. The platform-naming creation test above
/// runs on every non-macOS platform because command binding fails first there.
#[cfg(unix)]
#[test]
fn a_failed_probe_binds_the_session_with_an_unavailable_status() {
    let directory = tempfile::tempdir().unwrap();
    let mut chosen = definition();
    chosen.credential = CredentialRef::Environment("PATH".into());
    chosen.workspace_root = Some(directory.path().canonicalize().unwrap());
    chosen.tools = vec![crate::definition::WorkspaceTool::RunCommand];
    chosen.command = Some(crate::definition::CommandSettings::default());
    let backend = tars_sandbox::test_support::RecordingBackend::unavailable();
    let launches = backend.calls();
    let error = crate::sandbox::Sandbox::open_with(Box::new(backend)).unwrap_err();
    let resources = crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16))
        .with_test_command_sandbox(Err(error));
    let session = assemble(
        &chosen,
        "id".into(),
        Context::default(),
        resources,
        crate::permission::PermissionMode::Ask,
        None,
        false,
    )
    .expect("a failed probe binds the tool with the session still open")
    .session;
    let status = session.command_status().expect("command status");
    assert!(!status.is_available(), "{status}");
    assert!(status.to_string().contains("probe failed"), "{status}");
    assert!(
        launches.lock().unwrap().is_empty(),
        "a failed probe must not launch a process"
    );
}

fn search_definition(directory: &std::path::Path) -> SessionDefinition {
    let mut chosen = definition();
    chosen.credential = CredentialRef::Environment("PATH".into());
    chosen.workspace_root = Some(directory.canonicalize().unwrap());
    chosen.tools = vec![crate::definition::WorkspaceTool::SearchText];
    chosen
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn search_call(arguments: serde_json::Value) -> causa::kernel::ToolCallContext {
    causa::kernel::ToolCallContext::new(
        causa::kernel::BlockId::new(uuid::Uuid::now_v7()),
        causa::kernel::ToolCallPayload::new("search_text", arguments),
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn search_reporter(calls: &[causa::kernel::ToolCallContext]) -> crate::session::ToolReporter {
    use causa::kernel::{InvocationId, RoundId, ToolBatch, TurnId};
    let turn_id = TurnId::new(uuid::Uuid::now_v7().to_string());
    let reporter = crate::session::ToolReporter::new(
        crate::session::WorkRef {
            conversation_id: "assembly-search-tests".into(),
            turn_id: turn_id.clone(),
        },
        tokio::sync::broadcast::channel(128).0,
    );
    reporter.batch_ready(
        &InvocationId {
            turn_id,
            round_id: RoundId(0),
        },
        &ToolBatch::new(calls.to_vec()).unwrap(),
    );
    reporter
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn a_failed_search_probe_keeps_the_session_open_and_rejects_every_call() {
    use crate::permission::PermissionMode;
    let directory = tempfile::tempdir().unwrap();
    let chosen = search_definition(directory.path());
    let backend = tars_sandbox::test_support::RecordingBackend::unavailable();
    let launches = backend.calls();
    let error = crate::sandbox::Sandbox::open_with(Box::new(backend)).unwrap_err();
    let resources = crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16))
        .with_test_search_sandbox(Err(error));
    let session = assemble(
        &chosen,
        "id".into(),
        Context::default(),
        resources,
        PermissionMode::Ask,
        None,
        false,
    )
    .expect("a failed search probe must not close the session")
    .session;
    let shared = session.workspace_tools().expect("workspace tools");
    let invocation = search_call(serde_json::json!({"path": ".", "query": "needle"}));
    let tool = shared
        .bind_for_test(
            &[crate::definition::WorkspaceTool::SearchText],
            search_reporter(std::slice::from_ref(&invocation)),
        )
        .pop()
        .unwrap();
    let output = tool
        .execute(
            &invocation,
            &CallControl::new(CancellationToken::new(), None),
            None,
        )
        .await;
    assert_eq!(output.status, causa::kernel::ToolResultStatus::Rejected);
    let reason = output.output.content["error"].as_str().unwrap();
    assert!(reason.contains("requires a platform sandbox"), "{reason}");
    assert!(reason.contains("probe failed"), "{reason}");
    assert!(
        launches.lock().unwrap().is_empty(),
        "a failed probe must not launch a process"
    );
}

/// Platforms without a workspace/sandbox implementation (e.g. Windows):
/// creating a session with `search_text` rejects workspace opening rather than
/// failing with `CommandUnavailable`.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
#[tokio::test]
async fn search_creation_without_a_backend_keeps_the_session_open() {
    use crate::permission::PermissionMode;
    let directory = tempfile::tempdir().unwrap();
    let chosen = search_definition(directory.path());
    let assembled = assemble(
        &chosen,
        "id".into(),
        Context::default(),
        crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16)),
        PermissionMode::Ask,
        None,
        false,
    );
    assert!(matches!(
        assembled,
        Err(super::provider::AssemblyError::Workspace(error))
            if matches!(&*error, crate::workspace::WorkspaceError::UnsupportedPlatform)
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn the_host_hands_resolved_external_tools_to_the_search_runtime() {
    use crate::application::{PreparedCreate, SessionHost};
    use crate::config::{Config, ConfigSource};
    use crate::external_tools::ExternalTools;
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let rg = directory.path().join("fixture-rg");
    std::fs::write(
        &rg,
        "#!/bin/sh\nprintf 'ripgrep 15.2.0 (test fixture)\\n'\n",
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&rg, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let config_path = directory.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("version = 1\n[external_tools]\nrg = \"{}\"\n", rg.display()),
    )
    .unwrap();
    let config = Config::load(ConfigSource::File(config_path)).unwrap();
    let tools = ExternalTools::resolve(&config);
    assert!(tools.rg().is_ready(), "{:?}", tools.rg());
    let host = SessionHost::new(None).with_external_tools(tools);
    let chosen = search_definition(&workspace);
    let session = host
        .create(PreparedCreate::transient(chosen, Context::default()))
        .await
        .expect("the host must create a session with a bound search runtime");
    let shared = session.workspace_tools().expect("workspace tools");
    assert!(
        shared.search().is_some(),
        "selecting search_text must bind the search runtime"
    );
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let runtime = shared.search().unwrap();
        let params = crate::tools::parameters::SearchParams {
            path: ".".into(),
            query: "needle".into(),
            max_matches: None,
            regex: None,
            ignore_case: None,
            glob: None,
            context_lines: None,
            include_ignored: None,
        };
        runtime
            .check(&params, shared.limits())
            .expect("the resolved external-tool path must reach the search runtime");
    }
    host.close(
        &session.target(),
        crate::application::ClosePolicy::DiscardLocal,
    )
    .await
    .unwrap();
}

#[test]
fn store_credential_assembly_enforces_isolation_permissions_and_zero_secret_echo() {
    use super::provider::CredentialUnavailableReason;
    use crate::application::WorkOutcome;
    use crate::config::CredentialsStore;

    let dir = tempfile::tempdir().unwrap();
    let mut def = definition();
    def.credential = CredentialRef::Store("work".into());

    // 1. Disabled (`credentials_path: None`, e.g. --no-config) fails closed even if env var exists.
    let err = match assemble(
        &def,
        "id".into(),
        Context::default(),
        crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16)),
        crate::permission::PermissionMode::Ask,
        None,
        true,
    ) {
        Ok(_) => panic!("store credential without path must fail closed"),
        Err(err) => err,
    };
    assert!(matches!(
        &err,
        AssemblyError::CredentialUnavailable(CredentialUnavailableReason::StoreDisabled { name })
            if name == "work"
    ));
    let outcome = WorkOutcome::from_assembly_error(&err);
    assert!(outcome.summary.contains("API key"));
    assert!(
        outcome
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("tars auth set work") && h.contains("--api-key-env"))
    );

    // 2. Missing credentials.toml fails closed with StoreMissing.
    let creds_path = dir.path().join("credentials.toml");
    let err = match assemble(
        &def,
        "id".into(),
        Context::default(),
        crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16))
            .with_credentials_path(Some(creds_path.clone())),
        crate::permission::PermissionMode::Ask,
        None,
        true,
    ) {
        Ok(_) => panic!("missing credentials file must fail"),
        Err(err) => err,
    };
    assert!(matches!(
        &err,
        AssemblyError::CredentialUnavailable(CredentialUnavailableReason::StoreMissing { name })
            if name == "work"
    ));

    // 3. Valid 0600 credentials.toml with another key -> StoreMissing for "work".
    let secret = "sk-store-secret-do-not-leak-98765";
    CredentialsStore::set(&creds_path, "other", secret).unwrap();
    let err = match assemble(
        &def,
        "id".into(),
        Context::default(),
        crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16))
            .with_credentials_path(Some(creds_path.clone())),
        crate::permission::PermissionMode::Ask,
        None,
        true,
    ) {
        Ok(_) => panic!("missing key in store must fail"),
        Err(err) => err,
    };
    assert!(matches!(
        &err,
        AssemblyError::CredentialUnavailable(CredentialUnavailableReason::StoreMissing { name })
            if name == "work"
    ));

    // 4. Valid 0600 credentials.toml with "work" -> succeeds!
    CredentialsStore::set(&creds_path, "work", secret).unwrap();
    assert!(
        assemble(
            &def,
            "id".into(),
            Context::default(),
            crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16))
                .with_credentials_path(Some(creds_path.clone())),
            crate::permission::PermissionMode::Ask,
            None,
            true,
        )
        .is_ok()
    );

    // 5. Corrupt TOML containing secret -> StoreInvalid, zero secret echo.
    let corrupt_path = dir.path().join("corrupt.toml");
    std::fs::write(
        &corrupt_path,
        format!("version = 1\n[credentials.work]\napi_key = \"{secret}\"\nbad_toml = ["),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&corrupt_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let err = match assemble(
        &def,
        "id".into(),
        Context::default(),
        crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16))
            .with_credentials_path(Some(corrupt_path)),
        crate::permission::PermissionMode::Ask,
        None,
        true,
    ) {
        Ok(_) => panic!("corrupt credentials.toml must fail"),
        Err(err) => err,
    };
    assert!(matches!(
        &err,
        AssemblyError::CredentialUnavailable(CredentialUnavailableReason::StoreInvalid { .. })
    ));
    let outcome = WorkOutcome::from_assembly_error(&err);
    assert!(!err.to_string().contains(secret));
    assert!(!format!("{err:?}").contains(secret));
    assert!(!outcome.summary.contains(secret));
    assert!(!outcome.detail.as_deref().unwrap_or("").contains(secret));

    // 6. Unix 0644 permissions and symlink fail closed with zero secret echo.
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};

        std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = match assemble(
            &def,
            "id".into(),
            Context::default(),
            crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16))
                .with_credentials_path(Some(creds_path.clone())),
            crate::permission::PermissionMode::Ask,
            None,
            true,
        ) {
            Ok(_) => panic!("0644 credentials.toml must fail closed"),
            Err(err) => err,
        };
        assert!(matches!(
            &err,
            AssemblyError::CredentialUnavailable(
                CredentialUnavailableReason::StoreInsecurePermissions { .. }
            )
        ));
        let outcome = WorkOutcome::from_assembly_error(&err);
        assert!(
            outcome
                .hint
                .as_deref()
                .is_some_and(|h| h.contains("chmod 600"))
        );
        assert!(!err.to_string().contains(secret));
        assert!(!format!("{err:?}").contains(secret));

        std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link_path = dir.path().join("link.toml");
        symlink(&creds_path, &link_path).unwrap();
        let err = match assemble(
            &def,
            "id".into(),
            Context::default(),
            crate::assembly::SessionResources::new(crate::tools::ToolResources::new(16))
                .with_credentials_path(Some(link_path)),
            crate::permission::PermissionMode::Ask,
            None,
            true,
        ) {
            Ok(_) => panic!("symlink credentials.toml must fail closed"),
            Err(err) => err,
        };
        assert!(matches!(
            &err,
            AssemblyError::CredentialUnavailable(CredentialUnavailableReason::StoreInvalid { .. })
        ));
        assert!(!err.to_string().contains(secret));
        assert!(!format!("{err:?}").contains(secret));
    }
}

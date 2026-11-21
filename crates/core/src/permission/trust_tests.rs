use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use causa::kernel::{
    CallControl, Context, ModelGateway, ModelInvokeError, ModelItem, ModelOutput, ModelRequest,
    ModelResponse, ModelStopReason,
};
use serde_json::json;

use crate::application::{
    ApplicationError, ClosePolicy, HostOptions, ManagedSession, PreparedCreate, SessionHost,
    text_input,
};
use crate::definition::{
    CommandSettings, CredentialRef, Provider, SessionDefinition, WorkspaceTool, WorkspaceToolLimits,
};
use crate::permission::{CommandRule, RuleDecision, RuleScope, RuleSource};
use crate::session::{SubmitRequest, WorkRef, WorkResult, WorkState};
use crate::store::SessionStore;

struct StaticAnswerGateway;

#[async_trait]
impl ModelGateway for StaticAnswerGateway {
    async fn invoke(
        &self,
        _request: &ModelRequest,
        _control: &CallControl,
    ) -> Result<ModelOutput, ModelInvokeError> {
        Ok(ModelOutput::new(
            ModelResponse {
                items: vec![ModelItem::text("ok")],
            },
            ModelStopReason::EndTurn,
        ))
    }
}

fn minimal_definition(workspace_root: Option<&Path>) -> SessionDefinition {
    SessionDefinition {
        provider: Provider::OpenAiChat,
        model: "rules-test-model".into(),
        endpoint: Some("http://127.0.0.1:9/v1/chat/completions".into()),
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(10),
        retained_work_capacity: 8,
        max_model_rounds: 4,
        max_tool_calls: 5,
        max_tokens: None,
        reasoning_effort: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: workspace_root.map(|p| p.canonicalize().unwrap()),
        tools: Vec::new(),
        tool_limits: WorkspaceToolLimits::default(),
        write_limits: None,
        permission_mode: None,
        command: None,
        mcp_servers: Vec::new(),
        context_window: None,
        auto_compact: true,
        compact_threshold: 70,
        prompt_cache: true,
        prelude: Default::default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_user_or_workspace_rules_fail_open_with_location_without_echoing_values() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".tars")).unwrap();
    let user_rules = temp.path().join("user-permissions.toml");

    // 1. Invalid user rules (syntax error, secret value must not appear in error).
    std::fs::write(
        &user_rules,
        "version = 1\n[[command]]\nprefix = [\"SECRET_TOKEN_XYZ\"]\ndecision = \"bogus\"\n",
    )
    .unwrap();
    let host = SessionHost::new(Some(store.clone())).with_user_rules_path(Some(user_rules.clone()));
    host.set_test_gateway(Some(Arc::new(StaticAnswerGateway)));
    let err = match host
        .create(PreparedCreate::persistent(
            minimal_definition(Some(&workspace)),
            Context::default(),
        ))
        .await
    {
        Ok(_) => panic!("invalid user rules must fail open"),
        Err(err) => err,
    };
    let msg = err.to_string();
    assert!(msg.contains("line 4, column"), "got: {msg}");
    assert!(msg.contains("user-permissions.toml"), "got: {msg}");
    assert!(!msg.contains("SECRET_TOKEN_XYZ"), "got: {msg}");
    assert!(!msg.contains("bogus"), "got: {msg}");

    // 2. --no-config equivalent (`user_rules_path = None`) ignores the broken user file.
    let isolated_host = SessionHost::new(Some(store.clone()));
    isolated_host.set_test_gateway(Some(Arc::new(StaticAnswerGateway)));
    let session = isolated_host
        .create(PreparedCreate::persistent(
            minimal_definition(Some(&workspace)),
            Context::default(),
        ))
        .await
        .unwrap();
    isolated_host
        .close(&session.target(), ClosePolicy::Save)
        .await
        .unwrap();

    // 3. Invalid workspace rules (forbidden single-word bash allow).
    std::fs::remove_file(&user_rules).unwrap();
    std::fs::write(
        workspace.join(".tars/permissions.toml"),
        "version = 1\n[[command]]\nprefix = [\"bash\"]\ndecision = \"allow\"\n",
    )
    .unwrap();
    let host2 = SessionHost::new(Some(store)).with_user_rules_path(Some(user_rules));
    host2.set_test_gateway(Some(Arc::new(StaticAnswerGateway)));
    let err = match host2
        .create(PreparedCreate::persistent(
            minimal_definition(Some(&workspace)),
            Context::default(),
        ))
        .await
    {
        Ok(_) => panic!("invalid workspace rules must fail open"),
        Err(err) => err,
    };
    let msg = err.to_string();
    assert!(msg.contains("permissions.toml"), "got: {msg}");
    assert!(!msg.contains("bash"), "must not echo rule value: {msg}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_isolation_and_workspace_trust_lifecycle_with_diff_and_toctou_protection() {
    let temp = tempfile::tempdir().unwrap();
    let store = temp.path().join("store");
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".tars")).unwrap();
    let user_rules = temp.path().join("user-permissions.toml");

    std::fs::write(
        &user_rules,
        "version = 1\n[[command]]\nprefix = [\"pwd\"]\ndecision = \"allow\"\n",
    )
    .unwrap();
    let initial_ws_toml = "\
version = 1
[[command]]
prefix = [\"rm\"]
decision = \"deny\"

[[command]]
prefix = [\"echo\"]
decision = \"allow\"
";
    std::fs::write(workspace.join(".tars/permissions.toml"), initial_ws_toml).unwrap();

    // Transient host (`store = None`) cannot trust workspace allow rules, but loads user rules.
    let transient_host = SessionHost::new(None).with_user_rules_path(Some(user_rules.clone()));
    transient_host.set_test_gateway(Some(Arc::new(StaticAnswerGateway)));
    let transient = transient_host
        .create(PreparedCreate::transient(
            minimal_definition(Some(&workspace)),
            Context::default(),
        ))
        .await
        .unwrap();
    let t_report = transient.open_report();
    assert!(!t_report.workspace_rules.can_trust);
    assert!(!t_report.workspace_rules.trusted);
    assert_eq!(t_report.workspace_rules.allow_rules.len(), 1);
    assert_eq!(t_report.workspace_rules.allow_rules[0].prefix, vec!["echo"]);
    assert!(matches!(
        transient.trust_workspace_rules().await,
        Err(ApplicationError::StoreUnavailable)
    ));
    let t_snapshot = transient.command_rules_snapshot();
    assert_eq!(t_snapshot.user_rules().len(), 1);
    assert_eq!(t_snapshot.workspace_rules().len(), 1);
    assert_eq!(t_snapshot.workspace_rules()[0].decision, RuleDecision::Deny);
    transient_host
        .close(&transient.target(), ClosePolicy::DiscardLocal)
        .await
        .unwrap();

    // Persistent host: untrusted initially; deny active, allow ignored.
    let host = SessionHost::with_options(
        Some(store.clone()),
        HostOptions {
            user_rules_path: Some(user_rules.clone()),
            ..HostOptions::default()
        },
    )
    .unwrap();
    host.set_test_gateway(Some(Arc::new(StaticAnswerGateway)));
    let s1 = host
        .create(PreparedCreate::persistent(
            minimal_definition(Some(&workspace)),
            Context::default(),
        ))
        .await
        .unwrap();
    let id = s1.target().conversation_id().clone();
    assert!(!s1.workspace_rules_trusted());
    let report1 = s1.open_report();
    assert!(report1.workspace_rules.can_trust);
    assert!(!report1.workspace_rules.trusted);
    assert_eq!(report1.workspace_rules.allow_rules.len(), 1);
    assert!(report1.workspace_rules.diff.is_none());

    // Modify file on disk *after* open to verify TOCTOU snapshot protection and no hot reload.
    let modified_ws_toml = format!(
        "{initial_ws_toml}\n[[command]]\nprefix = [\"ls\"]\ndecision = \"allow\"\noutside_sandbox = true\n"
    );
    std::fs::write(workspace.join(".tars/permissions.toml"), &modified_ws_toml).unwrap();

    // Calling trust_workspace_rules trusts the snapshot captured on open (1 allow rule: `echo`),
    // NOT `ls` written afterwards.
    assert_eq!(s1.trust_workspace_rules().await.unwrap(), 1);
    assert!(s1.workspace_rules_trusted());
    let active = s1.command_rules_snapshot();
    assert_eq!(active.workspace_rules().len(), 2);
    assert!(
        !active
            .workspace_rules()
            .iter()
            .any(|r| r.prefix == vec!["ls"])
    );
    host.close(&s1.target(), ClosePolicy::Save).await.unwrap();

    // Reopening sees the modified file on disk as untrusted, with previous_trusted_text & diff.
    let s2 = host.open(id.clone()).await.unwrap();
    let report2 = s2.open_report();
    assert!(!s2.workspace_rules_trusted());
    assert_eq!(
        report2.workspace_rules.previous_trusted_text.as_deref(),
        Some(initial_ws_toml)
    );
    let diff = report2.workspace_rules.diff.as_deref().unwrap();
    assert!(diff.contains("+prefix = [\"ls\"]"), "diff: {diff}");
    assert!(diff.contains("+outside_sandbox = true"), "diff: {diff}");
    assert_eq!(report2.workspace_rules.allow_rules.len(), 2);
    assert!(
        report2
            .workspace_rules
            .allow_rules
            .iter()
            .any(|r| r.prefix == vec!["ls"] && r.outside_sandbox)
    );

    // Trust new version and reopen: next open must be trusted immediately.
    assert_eq!(s2.trust_workspace_rules().await.unwrap(), 2);
    assert!(s2.workspace_rules_trusted());
    host.close(&s2.target(), ClosePolicy::Save).await.unwrap();

    let s3 = host.open(id.clone()).await.unwrap();
    assert!(s3.workspace_rules_trusted());
    assert!(s3.open_report().workspace_rules.allow_rules.is_empty());
    assert_eq!(s3.command_rules_snapshot().workspace_rules().len(), 3);
    host.close(&s3.target(), ClosePolicy::Save).await.unwrap();

    // Simulate TARS appending a rule block and updating the Store trust copy: reopen stays trusted.
    let appended = format!(
        "{modified_ws_toml}\n{}",
        crate::permission::rules::format_command_rule_block(&CommandRule {
            prefix: vec!["cat".into()],
            decision: RuleDecision::Allow,
            outside_sandbox: false,
        })
    );
    std::fs::write(workspace.join(".tars/permissions.toml"), &appended).unwrap();
    SessionStore::new(store)
        .write_workspace_rules_trust(&workspace, &appended)
        .unwrap();

    let s4 = host.open(id).await.unwrap();
    assert!(s4.workspace_rules_trusted());
    assert_eq!(s4.command_rules_snapshot().workspace_rules().len(), 4);
    host.close(&s4.target(), ClosePolicy::Save).await.unwrap();
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix_execution {
    use super::*;
    use crate::application::tests::workspace_http::*;
    use crate::permission::PermissionDecisionKind;
    use causa::kernel::ToolResultStatus;

    fn command_definition(endpoint: &str, root: &Path) -> SessionDefinition {
        let mut def = minimal_definition(Some(root));
        def.endpoint = Some(endpoint.into());
        def.tools = vec![WorkspaceTool::RunCommand];
        def.command = Some(CommandSettings::default());
        def
    }

    fn call_cmd(cmd: &str) -> Reply {
        Reply::Call("run_command".into(), json!({ "command": cmd }))
    }

    fn submit(session: &ManagedSession, key: &str) -> WorkRef {
        session
            .submit(SubmitRequest {
                request_key: key.into(),
                blocks: vec![text_input(key)],
            })
            .unwrap()
            .work
    }

    async fn finish(session: &ManagedSession, work: &WorkRef) -> crate::session::WorkObservation {
        let obs = session.wait(work, Duration::from_secs(10)).await.unwrap();
        assert!(matches!(&obs.state, WorkState::Finished(_)));
        session.wait_saved().await.unwrap();
        obs
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rc06_two_layer_rules_and_trust_enforcement_end_to_end() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("store");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(workspace.join(".tars")).unwrap();
        let user_rules = temp.path().join("user-permissions.toml");

        // User layer: allow `pwd`, deny `uname`, ask `date`.
        std::fs::write(
            &user_rules,
            r#"version = 1
[[command]]
prefix = ["pwd"]
decision = "allow"

[[command]]
prefix = ["uname"]
decision = "deny"

[[command]]
prefix = ["date"]
decision = "ask"
"#,
        )
        .unwrap();

        // Workspace layer: deny `id`, ask `whoami`, allow `echo`, and attempt to override user `uname` + `date` with allow.
        std::fs::write(
            workspace.join(".tars/permissions.toml"),
            r#"version = 1
[[command]]
prefix = ["id"]
decision = "deny"

[[command]]
prefix = ["whoami"]
decision = "ask"

[[command]]
prefix = ["echo"]
decision = "allow"

[[command]]
prefix = ["uname"]
decision = "allow"

[[command]]
prefix = ["date"]
decision = "allow"
"#,
        )
        .unwrap();

        let replies = vec![
            // Turn 1: user allow `pwd` succeeds without prompt.
            call_cmd("pwd"),
            Reply::Answer,
            // Turn 2: workspace deny `id` rejects immediately.
            call_cmd("id"),
            Reply::Answer,
            // Turn 3: workspace untrusted allow `echo` is ignored -> NoConfirmationRoute in ask mode.
            call_cmd("echo hi"),
            Reply::Answer,
            // Turn 4 (after trust_workspace_rules): workspace `echo` now executes via RuleAllowed.
            call_cmd("echo hi"),
            Reply::Answer,
            // Turn 5 (after trust): user deny `uname` still wins over trusted workspace allow.
            call_cmd("uname"),
            Reply::Answer,
            // Turn 6 (after trust): user ask `date` still wins over trusted workspace allow (NoConfirmationRoute).
            call_cmd("date"),
            Reply::Answer,
        ];
        let fixture = Fixture::new("openai-chat", replies, None);
        let host = SessionHost::new(Some(store.clone())).with_user_rules_path(Some(user_rules));
        let session = host
            .create(PreparedCreate::persistent(
                command_definition(&fixture.endpoint, &workspace),
                Context::default(),
            ))
            .await
            .unwrap();
        let id = session.target().conversation_id().clone();
        assert!(!session.workspace_rules_trusted());

        // Turn 1: user allow `pwd`
        let obs1 = finish(&session, &submit(&session, "t1")).await;
        assert_eq!(
            obs1.tools.calls[0].status,
            Some(ToolResultStatus::Succeeded)
        );
        assert_eq!(
            obs1.tools.calls[0]
                .permission_decision
                .as_ref()
                .unwrap()
                .kind,
            PermissionDecisionKind::RuleAllowed
        );
        assert_eq!(
            obs1.tools.calls[0]
                .permission_decision
                .as_ref()
                .unwrap()
                .rule,
            Some(RuleSource {
                scope: RuleScope::User,
                prefix: vec!["pwd".into()],
            })
        );

        // Turn 2: workspace deny `id`
        let obs2 = finish(&session, &submit(&session, "t2")).await;
        assert_eq!(obs2.tools.calls[0].status, Some(ToolResultStatus::Rejected));
        assert_eq!(
            obs2.tools.calls[0]
                .permission_decision
                .as_ref()
                .unwrap()
                .kind,
            PermissionDecisionKind::RuleDenied
        );

        // Turn 3: untrusted workspace allow `echo` is NOT active
        let obs3 = finish(&session, &submit(&session, "t3")).await;
        assert_eq!(obs3.tools.calls[0].status, Some(ToolResultStatus::Rejected));
        assert_eq!(
            obs3.tools.calls[0]
                .permission_decision
                .as_ref()
                .unwrap()
                .kind,
            PermissionDecisionKind::NoConfirmationRoute
        );

        // Trust workspace rules -> `echo` immediately activates
        assert_eq!(session.trust_workspace_rules().await.unwrap(), 3);
        assert!(session.workspace_rules_trusted());

        // Turn 4: `echo hi` now succeeds
        let obs4 = finish(&session, &submit(&session, "t4")).await;
        assert_eq!(
            obs4.tools.calls[0].status,
            Some(ToolResultStatus::Succeeded)
        );
        assert_eq!(
            obs4.tools.calls[0]
                .permission_decision
                .as_ref()
                .unwrap()
                .kind,
            PermissionDecisionKind::RuleAllowed
        );
        assert_eq!(
            obs4.tools.calls[0]
                .permission_decision
                .as_ref()
                .unwrap()
                .rule,
            Some(RuleSource {
                scope: RuleScope::Workspace,
                prefix: vec!["echo".into()],
            })
        );

        // Turn 5: user deny `uname` overrides trusted workspace allow `uname`
        let obs5 = finish(&session, &submit(&session, "t5")).await;
        assert_eq!(obs5.tools.calls[0].status, Some(ToolResultStatus::Rejected));
        let d5 = obs5.tools.calls[0].permission_decision.as_ref().unwrap();
        assert_eq!(d5.kind, PermissionDecisionKind::RuleDenied);
        assert_eq!(
            d5.rule,
            Some(RuleSource {
                scope: RuleScope::User,
                prefix: vec!["uname".into()],
            })
        );

        // Turn 6: user ask `date` overrides trusted workspace allow `date`
        let obs6 = finish(&session, &submit(&session, "t6")).await;
        assert_eq!(obs6.tools.calls[0].status, Some(ToolResultStatus::Rejected));
        let d6 = obs6.tools.calls[0].permission_decision.as_ref().unwrap();
        assert_eq!(d6.kind, PermissionDecisionKind::NoConfirmationRoute);
        assert_eq!(
            d6.rule,
            Some(RuleSource {
                scope: RuleScope::User,
                prefix: vec!["date".into()],
            })
        );
        assert!(matches!(
            &obs6.state,
            WorkState::Finished(result) if matches!(result.as_ref(), WorkResult::Run(_))
        ));

        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();

        let reopened = host.open(id).await.unwrap();
        assert!(reopened.workspace_rules_trusted());
        host.close(&reopened.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }
}

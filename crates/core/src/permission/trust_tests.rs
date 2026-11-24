use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use causa::kernel::{
    CallControl, Context, ModelGateway, ModelInvokeError, ModelItem, ModelOutput, ModelRequest,
    ModelResponse, ModelStopReason,
};

use crate::application::{ApplicationError, ClosePolicy, HostOptions, PreparedCreate, SessionHost};
use crate::definition::{CredentialRef, Provider, SessionDefinition, WorkspaceToolLimits};
use crate::permission::{CommandRule, RuleDecision};
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
        temperature: None,
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
        code_execution: None,
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

#[cfg(target_os = "macos")]
mod macos_execution {
    use super::*;
    use crate::application::tests::workspace_http::*;
    use crate::application::{ManagedSession, text_input};
    use crate::definition::{CommandSettings, WorkspaceTool};
    use crate::permission::{PermissionDecisionKind, RuleScope, RuleSource};
    use crate::session::{SubmitRequest, WorkRef, WorkResult, WorkState};
    use causa::kernel::ToolResultStatus;
    use serde_json::json;
    use std::time::Duration;

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rc07_rc10_in_memory_session_rules_and_remove_clear_and_reopen_clear() {
        use crate::permission::{ApprovalDecision, ApprovalSubject, CommandTarget};

        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("store");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let user_rules = temp.path().join("user-permissions.toml");
        let outside_file = temp.path().join("outside-session-rule.txt");

        let replies = vec![
            // 1. Ask -> AllowForSession (`echo first`)
            call_cmd("echo first"),
            Reply::Answer,
            // 2. Same prefix `echo first` runs via RuleAllowed without prompt!
            call_cmd("echo first second"),
            Reply::Answer,
            // 3. Outside sandbox -> AllowForSession (`touch <outside_file>`)
            Reply::Call(
                "run_command".into(),
                json!({
                    "command": format!("touch '{}'", outside_file.display()),
                    "outside_sandbox": true,
                    "justification": "outside session rule test",
                }),
            ),
            Reply::Answer,
            // 4. Same prefix outside sandbox -> RuleAllowed without prompt!
            Reply::Call(
                "run_command".into(),
                json!({
                    "command": format!("touch '{}'", outside_file.display()),
                    "outside_sandbox": true,
                    "justification": "outside session rule reuse",
                }),
            ),
            Reply::Answer,
            // 5. After remove_session_rule(0) removes `echo first`, `echo first` asks again!
            call_cmd("echo first after-remove"),
            Reply::Answer,
            // 6. After clear_session_rules(), `touch <outside_file>` outside asks again!
            Reply::Call(
                "run_command".into(),
                json!({
                    "command": format!("touch '{}'", outside_file.display()),
                    "outside_sandbox": true,
                    "justification": "after clear",
                }),
            ),
            Reply::Answer,
            // 7. Re-add session rule before close, then close & reopen: must ask again!
            call_cmd("echo first before-reopen"),
            Reply::Answer,
            // 8. After reopen: session rule is gone -> asks again (we deny to verify prompt arrived).
            call_cmd("echo first after-reopen"),
            Reply::Answer,
        ];
        let fixture = Fixture::new("openai-chat", replies, None);
        let host =
            SessionHost::new(Some(store.clone())).with_user_rules_path(Some(user_rules.clone()));
        let session = host
            .create(PreparedCreate::persistent(
                command_definition(&fixture.endpoint, &workspace),
                Context::default(),
            ))
            .await
            .unwrap();
        let id = session.target().conversation_id().clone();
        let mut rx = session.connect_approvals();

        let approver = tokio::spawn(async move {
            // Prompt 1: `echo first` -> AllowForSession
            let req1 = rx.recv().await.unwrap();
            let ApprovalSubject::Command {
                suggested_prefixes,
                can_save_workspace_rule,
                ..
            } = &req1.subject
            else {
                panic!("expected Command subject");
            };
            assert_eq!(
                suggested_prefixes,
                &vec![vec!["echo".to_owned(), "first".to_owned()]]
            );
            assert!(*can_save_workspace_rule);
            req1.reply.send(ApprovalDecision::AllowForSession).unwrap();

            // Prompt 2: `touch` outside -> AllowForSession
            let req2 = rx.recv().await.unwrap();
            let ApprovalSubject::Command { target, .. } = &req2.subject else {
                panic!("expected Command subject");
            };
            assert!(matches!(target, CommandTarget::Outside { .. }));
            req2.reply.send(ApprovalDecision::AllowForSession).unwrap();

            // Prompt 3: `echo first after-remove` -> Allow once
            let req3 = rx.recv().await.unwrap();
            req3.reply.send(ApprovalDecision::Allow).unwrap();

            // Prompt 4: `touch` after clear -> Allow once
            let req4 = rx.recv().await.unwrap();
            req4.reply.send(ApprovalDecision::Allow).unwrap();

            // Prompt 5: `echo first before-reopen` -> AllowForSession
            let req5 = rx.recv().await.unwrap();
            req5.reply.send(ApprovalDecision::AllowForSession).unwrap();
        });

        let obs1 = finish(&session, &submit(&session, "s1")).await;
        assert_eq!(
            obs1.tools.calls[0]
                .permission_decision
                .as_ref()
                .unwrap()
                .kind,
            PermissionDecisionKind::AskedAndAllowed
        );
        assert_eq!(session.command_rules_snapshot().session_rules().len(), 1);

        let obs2 = finish(&session, &submit(&session, "s2")).await;
        let d2 = obs2.tools.calls[0].permission_decision.as_ref().unwrap();
        assert_eq!(d2.kind, PermissionDecisionKind::RuleAllowed);
        assert_eq!(
            d2.rule,
            Some(RuleSource {
                scope: RuleScope::Session,
                prefix: vec!["echo".into(), "first".into()],
            })
        );

        let obs3 = finish(&session, &submit(&session, "s3")).await;
        assert_eq!(
            obs3.tools.calls[0].status,
            Some(ToolResultStatus::Succeeded)
        );
        assert_eq!(session.command_rules_snapshot().session_rules().len(), 2);

        let obs4 = finish(&session, &submit(&session, "s4")).await;
        assert_eq!(
            obs4.tools.calls[0].status,
            Some(ToolResultStatus::Succeeded)
        );
        let d4 = obs4.tools.calls[0].permission_decision.as_ref().unwrap();
        assert_eq!(d4.kind, PermissionDecisionKind::RuleAllowed);
        assert_eq!(d4.rule.as_ref().map(|r| r.scope), Some(RuleScope::Session));

        assert!(session.remove_session_rule(0).unwrap());
        assert!(!session.remove_session_rule(99).unwrap());
        assert_eq!(session.command_rules_snapshot().session_rules().len(), 1);

        let obs5 = finish(&session, &submit(&session, "s5")).await;
        assert_eq!(
            obs5.tools.calls[0]
                .permission_decision
                .as_ref()
                .unwrap()
                .kind,
            PermissionDecisionKind::AskedAndAllowed
        );

        assert_eq!(session.clear_session_rules().unwrap(), 1);
        assert!(session.command_rules_snapshot().session_rules().is_empty());

        let obs6 = finish(&session, &submit(&session, "s6")).await;
        assert_eq!(
            obs6.tools.calls[0]
                .permission_decision
                .as_ref()
                .unwrap()
                .kind,
            PermissionDecisionKind::AskedAndAllowed
        );

        // Add one session rule before closing.
        let _obs7 = finish(&session, &submit(&session, "s7")).await;
        assert_eq!(session.command_rules_snapshot().session_rules().len(), 1);
        approver.await.unwrap();

        host.close(&session.target(), ClosePolicy::Save)
            .await
            .unwrap();

        // Saved record must remain tars-session-v2.4 without session_rules, and user rules untouched.
        let record_json = crate::application::tests::support::saved_record(&store, id.as_str());
        assert_eq!(record_json["format"], "tars-session-v2.4");
        assert!(record_json.get("session_rules").is_none());
        assert!(!user_rules.exists());
        assert!(!workspace.join(".tars/permissions.toml").exists());

        // Reopen: session rules must be cleared automatically.
        let reopened = host.open(id).await.unwrap();
        assert!(reopened.command_rules_snapshot().session_rules().is_empty());
        let mut rx2 = reopened.connect_approvals();
        let reopen_approver = tokio::spawn(async move {
            let req = rx2.recv().await.unwrap();
            req.reply
                .send(ApprovalDecision::Deny {
                    reason: Some("session rules cleared on reopen".into()),
                })
                .unwrap();
        });
        let obs8 = finish(&reopened, &submit(&reopened, "s8")).await;
        reopen_approver.await.unwrap();
        assert_eq!(obs8.tools.calls[0].status, Some(ToolResultStatus::Rejected));
        assert_eq!(
            obs8.tools.calls[0]
                .permission_decision
                .as_ref()
                .unwrap()
                .kind,
            PermissionDecisionKind::AskedAndDenied
        );
        let _ = std::fs::remove_file(&outside_file);
        host.close(&reopened.target(), ClosePolicy::Save)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rc07_rc10_workspace_append_preserves_comments_creates_gitignore_and_guards_toctou() {
        use crate::permission::{ApprovalDecision, ApprovalSubject};

        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("store");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();
        let user_rules = temp.path().join("user-permissions.toml");

        // Pre-seed `.tars` missing; first AllowInWorkspace creates `.tars/.gitignore` (`*\n`)
        // and `.tars/permissions.toml` without touching `user_rules`.
        let replies = vec![
            // Turn 1: `echo one` -> AllowInWorkspace (creates .tars/.gitignore + .tars/permissions.toml)
            call_cmd("echo one"),
            Reply::Answer,
            // Turn 2: `echo one two` -> RuleAllowed immediately without asking
            call_cmd("echo one two"),
            Reply::Answer,
            // Turn 3 (after reopen): `echo one three` -> auto-trusted, RuleAllowed immediately
            call_cmd("echo one three"),
            Reply::Answer,
            // Turn 4 (after appending to existing commented file): `pwd` -> AllowInWorkspace preserves comments
            call_cmd("pwd"),
            Reply::Answer,
            // Turn 5 (after external tampering): `ls` -> AllowInWorkspace refuses to overwrite/trust tampered file!
            call_cmd("ls"),
            Reply::Answer,
            // Turn 6: `ls` again still asks because tampered file was NOT trusted/saved!
            call_cmd("ls"),
            Reply::Answer,
        ];
        let fixture = Fixture::new("openai-chat", replies, None);
        let host =
            SessionHost::new(Some(store.clone())).with_user_rules_path(Some(user_rules.clone()));
        let s1 = host
            .create(PreparedCreate::persistent(
                command_definition(&fixture.endpoint, &workspace),
                Context::default(),
            ))
            .await
            .unwrap();
        let id = s1.target().conversation_id().clone();
        let mut rx1 = s1.connect_approvals();
        let approver1 = tokio::spawn(async move {
            let req = rx1.recv().await.unwrap();
            let ApprovalSubject::Command {
                suggested_prefixes,
                can_save_workspace_rule,
                ..
            } = &req.subject
            else {
                panic!("expected Command");
            };
            assert_eq!(
                suggested_prefixes,
                &vec![vec!["echo".to_owned(), "one".to_owned()]]
            );
            assert!(*can_save_workspace_rule);
            req.reply.send(ApprovalDecision::AllowInWorkspace).unwrap();
        });

        let obs1 = finish(&s1, &submit(&s1, "w1")).await;
        approver1.await.unwrap();
        assert_eq!(
            obs1.tools.calls[0].status,
            Some(ToolResultStatus::Succeeded)
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join(".tars/.gitignore")).unwrap(),
            "*\n"
        );
        let created_toml =
            std::fs::read_to_string(workspace.join(".tars/permissions.toml")).unwrap();
        assert!(created_toml.starts_with("version = 1\n\n"));
        assert!(created_toml.contains("prefix = [\"echo\", \"one\"]"));
        assert!(!user_rules.exists());

        let obs2 = finish(&s1, &submit(&s1, "w2")).await;
        let d2 = obs2.tools.calls[0].permission_decision.as_ref().unwrap();
        assert_eq!(d2.kind, PermissionDecisionKind::RuleAllowed);
        assert_eq!(
            d2.rule,
            Some(RuleSource {
                scope: RuleScope::Workspace,
                prefix: vec!["echo".into(), "one".into()],
            })
        );
        host.close(&s1.target(), ClosePolicy::Save).await.unwrap();

        // Reopen: workspace rules must be automatically trusted!
        let s2 = host.open(id.clone()).await.unwrap();
        assert!(s2.workspace_rules_trusted());
        let obs3 = finish(&s2, &submit(&s2, "w3")).await;
        assert_eq!(
            obs3.tools.calls[0]
                .permission_decision
                .as_ref()
                .unwrap()
                .kind,
            PermissionDecisionKind::RuleAllowed
        );
        host.close(&s2.target(), ClosePolicy::Save).await.unwrap();

        // Replace `.tars/permissions.toml` with custom comments & formatting and custom `.tars/.gitignore`,
        // trust it via Store, then test verbatim preservation + TOCTOU tamper refusal.
        let custom_gitignore = "# custom\n*.toml\n";
        std::fs::write(workspace.join(".tars/.gitignore"), custom_gitignore).unwrap();
        let commented_toml = "\
# Leading comment preserved
version = 1

# Rule comment preserved
[[command]]
prefix = [\"echo\"]
decision = \"allow\"
";
        std::fs::write(workspace.join(".tars/permissions.toml"), commented_toml).unwrap();
        SessionStore::new(store.clone())
            .write_workspace_rules_trust(&workspace, commented_toml)
            .unwrap();

        let s3 = host.open(id).await.unwrap();
        assert!(s3.workspace_rules_trusted());
        let mut rx3 = s3.connect_approvals();
        let ws_clone = workspace.clone();
        let approver3 = tokio::spawn(async move {
            // Turn 4: `pwd` -> AllowInWorkspace appends verbatim after `commented_toml`
            let req4 = rx3.recv().await.unwrap();
            req4.reply.send(ApprovalDecision::AllowInWorkspace).unwrap();

            // Wait for Turn 5 (`ls`): tamper with disk file BEFORE answering AllowInWorkspace
            let req5 = rx3.recv().await.unwrap();
            std::fs::write(
                ws_clone.join(".tars/permissions.toml"),
                "version = 1\n[[command]]\nprefix = [\"whoami\"]\ndecision = \"allow\"\n",
            )
            .unwrap();
            req5.reply.send(ApprovalDecision::AllowInWorkspace).unwrap();

            // Turn 6: `ls` must still prompt because TOCTOU refused to save/trust!
            let req6 = rx3.recv().await.unwrap();
            req6.reply.send(ApprovalDecision::Allow).unwrap();
        });

        let obs4 = finish(&s3, &submit(&s3, "w4")).await;
        assert_eq!(
            obs4.tools.calls[0].status,
            Some(ToolResultStatus::Succeeded)
        );
        let after_append =
            std::fs::read_to_string(workspace.join(".tars/permissions.toml")).unwrap();
        assert!(
            after_append.starts_with(commented_toml),
            "existing comments and text must be preserved verbatim:\n{after_append}"
        );
        assert!(after_append.contains("prefix = [\"pwd\"]"));
        assert_eq!(
            std::fs::read_to_string(workspace.join(".tars/.gitignore")).unwrap(),
            custom_gitignore
        );

        let obs5 = finish(&s3, &submit(&s3, "w5")).await;
        assert_eq!(
            obs5.tools.calls[0].status,
            Some(ToolResultStatus::Succeeded)
        );
        // Tampered file was NOT overwritten!
        let disk_after_toctou =
            std::fs::read_to_string(workspace.join(".tars/permissions.toml")).unwrap();
        assert!(disk_after_toctou.contains("whoami"));
        assert!(!disk_after_toctou.contains("prefix = [\"ls\"]"));
        assert!(
            !s3.command_rules_snapshot()
                .workspace_rules()
                .iter()
                .any(|r| r.prefix == vec!["ls"])
        );

        let obs6 = finish(&s3, &submit(&s3, "w6")).await;
        assert_eq!(
            obs6.tools.calls[0]
                .permission_decision
                .as_ref()
                .unwrap()
                .kind,
            PermissionDecisionKind::AskedAndAllowed
        );
        approver3.await.unwrap();
        assert!(!user_rules.exists());

        host.close(&s3.target(), ClosePolicy::Save).await.unwrap();
    }
}

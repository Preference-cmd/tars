use super::support::{Server, advance_past_approval_window, app, ctrl, key, until};
#[cfg(unix)]
use crate::app::state::CONFIRM_GUARD_WINDOW;
use crate::{
    app::{App, Panel, jobs::Message, state::PendingApproval},
    input::Form,
    options, render,
    transcript::Transcript,
};
use causa::kernel::{BlockContent, BlockMeta, Context, ContextBlock, Role};
use crossterm::event::KeyCode;
use ratatui::{Terminal, backend::TestBackend};
use ratatui_textarea::TextArea;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tars_core::{
    application::{ClosePolicy, SpawnAgentOutcome, SpawnAgentRequest},
    definition::ChildNarrowingRequest,
    events::{
        AgentInteraction, OnParentComplete, OperationExecutionMode, OperationStatus, SessionBus,
        SessionEvent, TASK_NOTIFICATION_SOURCE,
    },
    permission::{ApprovalDecision, ApprovalOrigin, ApprovalSubject, PermissionMode},
};

fn screen(app: &App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(120, 32)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn command(app: &mut App, text: &str) {
    app.event(key(KeyCode::F(1)));
    for c in text.chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Enter));
}

fn create_form(endpoint: &str, workspace: Option<&std::path::Path>) -> Form {
    let ws = workspace
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    Form {
        fields: [
            "openai-chat",
            "fixture-model",
            endpoint,
            "PATH",
            "120",
            &ws,
            "",
            "ask",
            "",
            "on",
            "off",
        ]
        .into_iter()
        .map(|v| TextArea::from([v]))
        .collect(),
        selected: 1,
        error: String::new(),
        limits: (256, 10, 64),
        generation: (None, None, Default::default()),
        max_model_attempts: None,
        render_options: Default::default(),
        tool_limits: Default::default(),
        write_limits: None,
        ..Form::default()
    }
}

#[tokio::test]
async fn session_event_bg_count_reactive_wakeup_cancel_and_lagged_self_heal() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let mut app = app(dir.path());

    app.form = Some(create_form(&server.endpoint, None));
    app.event(ctrl('s'));
    until(&mut app, |a| a.entries[a.selected].session.is_some()).await;
    let id = app.entries[app.selected].id().to_owned();
    let session = app.entries[app.selected].session.clone().unwrap();

    // 1. Spawn a background child agent while server holds its response -> bg 1 running.
    let SpawnAgentOutcome::Background(spawn) = session
        .spawn_agent(SpawnAgentRequest {
            caller_agent_id: id.clone(),
            caller_operation_id: None,
            source_block_id: None,
            name: "researcher".into(),
            preset_name: Some("explore".into()),
            role_prompt: None,
            prompt: "investigate module".into(),
            narrowing: ChildNarrowingRequest::default(),
            interaction: AgentInteraction::Reusable,
            execution_mode: OperationExecutionMode::Background,
            on_parent_complete: OnParentComplete::Continue,
            timeout_secs: None,
            max_rounds: None,
            coalesce_key: None,
        })
        .await
        .unwrap()
    else {
        panic!("expected background spawn receipt");
    };

    until(&mut app, |a| a.entries[a.selected].active_bg_count == 1).await;
    let s = screen(&app);
    assert!(s.contains("bg 1 running"), "screen: {s}");

    // 2. Test Lagged recovery on SessionBus: artificially flood a capacity-2 bus,
    //    bind to entry, and verify poll() self-heals active_bg_count & dirty flag.
    let bus = SessionBus::new(2);
    let lagged_rx = bus.subscribe();
    for i in 0..8 {
        bus.publish(SessionEvent::SessionLifecycleChanged {
            session_id: id.clone(),
            lifecycle: format!("tick-{i}"),
        });
    }
    app.entries[app.selected].active_bg_count = 99;
    app.entries[app.selected].viewed_agent_id = Some(spawn.agent_id.clone());
    app.entries[app.selected].viewed_agent_dirty = false;
    app.entries[app.selected].session_events_rx = Some(lagged_rx);
    app.dirty = false;
    app.poll();
    assert_eq!(app.entries[app.selected].active_bg_count, 1);
    assert!(app.entries[app.selected].viewed_agent_dirty);
    assert!(app.dirty);

    // Switch back to main and re-attach real session event stream.
    command(&mut app, "agents main");
    app.entries[app.selected].session_events_rx = Some(session.subscribe_events());

    // 3. Test AutoWakeupLimitReached notice + reactive wakeup execution updates.
    let bus2 = SessionBus::new(16);
    app.entries[app.selected].session_events_rx = Some(bus2.subscribe());
    bus2.publish(SessionEvent::OperationStateChanged {
        session_id: id.clone(),
        operation_id: "wakeup-op-1".into(),
        target_agent_id: id.clone(),
        parent_operation_id: None,
        status: OperationStatus::Running,
    });
    app.poll();
    assert_eq!(
        app.entries[app.selected].execution,
        "Running (background wakeup)"
    );
    bus2.publish(SessionEvent::AutoWakeupLimitReached {
        session_id: id.clone(),
        agent_id: id.clone(),
        max_auto_wakeups: 16,
    });
    app.poll();
    assert!(
        app.entries[app.selected]
            .notice
            .contains("Auto-wakeup limit (16) reached"),
        "notice: {}",
        app.entries[app.selected].notice
    );
    bus2.publish(SessionEvent::OperationStateChanged {
        session_id: id.clone(),
        operation_id: "wakeup-op-1".into(),
        target_agent_id: id.clone(),
        parent_operation_id: None,
        status: OperationStatus::Completed,
    });
    app.entries[app.selected].requested_material_version = None;
    app.poll();
    assert_eq!(
        app.entries[app.selected].execution,
        "Completed (background wakeup)"
    );

    // 3b. Test Lagged self-healing when root execution was stuck at "Running (background wakeup)".
    let bus3 = SessionBus::new(2);
    let lagged_rx3 = bus3.subscribe();
    for i in 0..8 {
        bus3.publish(SessionEvent::SessionLifecycleChanged {
            session_id: id.clone(),
            lifecycle: format!("lag-{i}"),
        });
    }
    app.entries[app.selected].execution = "Running (background wakeup)".into();
    app.entries[app.selected].requested_material_version =
        Some(session.read_materials().unwrap().version);
    app.entries[app.selected].session_events_rx = Some(lagged_rx3);
    app.poll();
    assert_eq!(
        app.entries[app.selected].execution,
        "Completed (background wakeup)"
    );
    assert!(
        app.entries[app.selected]
            .requested_material_version
            .is_none()
    );

    // 4. Re-bind real event stream, verify cancel() without root work cancels background task.
    app.entries[app.selected].session_events_rx = Some(session.subscribe_events());
    assert!(app.entries[app.selected].work.is_none());
    assert_eq!(session.active_finite_background_tasks(), 1);
    app.event(key(KeyCode::Esc));
    assert_eq!(
        app.entries[app.selected].notice,
        "Cancellation requested for active background tasks"
    );
    until(&mut app, |a| {
        a.entries[a.selected]
            .session
            .as_ref()
            .unwrap()
            .active_finite_background_tasks()
            == 0
            && a.entries[a.selected].active_bg_count == 0
    })
    .await;

    let target = session.target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
}

#[tokio::test]
async fn agents_panel_readonly_child_view_input_interception_and_stale_discard() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let mut app = app(dir.path());
    app.view_width = render::transcript_width(120);
    app.view_height = 32;
    app.viewport_height = render::conversation_height(32, 120, &app);

    app.form = Some(create_form(&server.endpoint, None));
    app.event(ctrl('s'));
    until(&mut app, |a| a.entries[a.selected].session.is_some()).await;
    let id = app.entries[app.selected].id().to_owned();
    let session = app.entries[app.selected].session.clone().unwrap();

    // Release 2 permits: 1 for child agent, 1 for root reactive wakeup.
    server.release.add_permits(2);

    let SpawnAgentOutcome::Background(spawn) = session
        .spawn_agent(SpawnAgentRequest {
            caller_agent_id: id.clone(),
            caller_operation_id: None,
            source_block_id: None,
            name: "analyzer".into(),
            preset_name: Some("explore".into()),
            role_prompt: None,
            prompt: "CHILD_UNIQUE_PROMPT_123".into(),
            narrowing: ChildNarrowingRequest::default(),
            interaction: AgentInteraction::Reusable,
            execution_mode: OperationExecutionMode::Background,
            on_parent_complete: OnParentComplete::Continue,
            timeout_secs: None,
            max_rounds: None,
            coalesce_key: None,
        })
        .await
        .unwrap()
    else {
        panic!("expected background spawn receipt");
    };

    session.wait_settled().await.unwrap();
    until(&mut app, |a| {
        a.entries[a.selected].rows.is_some()
            && a.entries[a.selected].active_bg_count == 0
            && !a.entries[a.selected].refreshing
    })
    .await;

    // 1. Open `/agents` panel and verify authoritative snapshot items.
    command(&mut app, "agents");
    assert!(matches!(app.panel, Some(Panel::Agents { .. })));
    let panel_screen = screen(&app);
    assert!(panel_screen.contains("main (entry)"), "{panel_screen}");
    assert!(panel_screen.contains("(viewing)"), "{panel_screen}");
    assert!(panel_screen.contains("analyzer"), "{panel_screen}");
    assert!(panel_screen.contains("preset explore"), "{panel_screen}");
    assert!(panel_screen.contains("reusable"), "{panel_screen}");

    // Also verify `/status` includes agent & background task counts.
    app.event(key(KeyCode::Esc));
    assert!(app.panel.is_none());
    command(&mut app, "status");
    let status_screen = screen(&app);
    assert!(status_screen.contains("Agents: 2 total"), "{status_screen}");
    assert!(
        status_screen.contains("Background tasks: 0 running"),
        "{status_screen}"
    );
    app.event(key(KeyCode::Esc));

    // 2. Select child agent via `/agents` panel (Down + Enter).
    command(&mut app, "agents");
    app.event(key(KeyCode::Down));
    app.event(key(KeyCode::Enter));
    assert!(app.panel.is_none());
    assert_eq!(
        app.entries[app.selected].viewed_agent_id.as_deref(),
        Some(spawn.agent_id.as_str())
    );

    until(&mut app, |a| {
        a.entries[a.selected]
            .transcript
            .text
            .contains("CHILD_UNIQUE_PROMPT_123")
            && a.entries[a.selected].rows.is_some()
    })
    .await;

    let child_screen = screen(&app);
    assert!(
        child_screen.contains("viewing agent: analyzer (read-only)"),
        "{child_screen}"
    );
    assert!(
        child_screen.contains("Read-only child agent view · /agents to return to main"),
        "{child_screen}"
    );
    assert!(child_screen.contains("/agents main"), "{child_screen}");

    // 3. Stale root `Message::Materials` and `Message::Compacted` arriving while viewing child
    //    MUST NOT overwrite child transcript.
    let key_now = app.entries[app.selected].key.clone();
    let root_snap = session.read_materials().unwrap();
    let root_tx = Arc::new(Transcript::from_materials(
        &root_snap.context,
        &root_snap.history,
    ));
    app.complete(Message::Materials(
        key_now.clone(),
        Ok(crate::app::jobs::MaterialView {
            version: root_snap.version.clone(),
            transcript: root_tx.clone(),
            history: root_snap.history.clone(),
            prefix: Vec::new(),
            live_tail_ids: Default::default(),
            records: Vec::new(),
        }),
    ));
    assert!(
        app.entries[app.selected]
            .transcript
            .text
            .contains("CHILD_UNIQUE_PROMPT_123")
    );
    app.complete(Message::Compacted(
        key_now.clone(),
        Ok(crate::app::jobs::CompactedReload {
            outcome: tars_core::compact::CompactionOutcome {
                trigger: tars_core::compact::HistoryTrigger::Manual,
                estimate_before: 1000,
                estimate_after: 200,
                record: tars_core::compact::HistoryRecordEntry {
                    record_id: "rec-1".into(),
                    created_at_ms: 1,
                    trigger: tars_core::compact::HistoryTrigger::Manual,
                    kind: tars_core::compact::RewriteKind::Summarize,
                    removed_block_count: 1,
                    inserted_block_ids: Vec::new(),
                },
                merged: None,
                brief: causa::runtime::new_block_id(),
            },
            view: crate::app::jobs::MaterialView {
                version: root_snap.version,
                transcript: root_tx,
                history: root_snap.history,
                prefix: Vec::new(),
                live_tail_ids: Default::default(),
                records: Vec::new(),
            },
        }),
    ));
    assert!(
        app.entries[app.selected]
            .transcript
            .text
            .contains("CHILD_UNIQUE_PROMPT_123")
    );
    assert!(
        app.entries[app.selected]
            .requested_material_version
            .is_none()
    );

    // 3b. `/compact` is intercepted while viewing a child agent.
    command(&mut app, "compact");
    assert_eq!(
        app.entries[app.selected].notice,
        "Viewing child agent (read-only); run /agents main to compact"
    );
    assert!(app.entries[app.selected].busy.is_none());

    // 3c. Root `preview` must not render inside the child agent view, and returns on `/agents main`.
    let preview_inv = causa::kernel::InvocationId {
        turn_id: causa::kernel::TurnId::new("t-root"),
        round_id: causa::kernel::RoundId(1),
    };
    app.entries[app.selected].preview.start(preview_inv);
    app.entries[app.selected]
        .preview
        .append("ROOT_PREVIEW_HIDDEN_IN_CHILD", false);
    app.entries[app.selected].preview.layout(app.view_width);
    assert_eq!(
        app.entries[app.selected].conversation_len(),
        app.entries[app.selected].committed_rows()
    );
    let child_with_root_preview = screen(&app);
    assert!(
        !child_with_root_preview.contains("ROOT_PREVIEW_HIDDEN_IN_CHILD"),
        "{child_with_root_preview}"
    );

    // 4. Input interception in read-only child view: draft is preserved and never submitted.
    for c in "should stay in draft".chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Enter));
    assert_eq!(
        app.entries[app.selected].notice,
        "Viewing child agent (read-only); run /agents main to send input"
    );
    assert_eq!(
        app.entries[app.selected].editor.lines(),
        &["should stay in draft"]
    );
    assert!(app.entries[app.selected].work.is_none());

    // 5. Switch back to `main` via `/agents main` and verify root transcript restores
    //    and stale `AgentMaterials` for child is discarded.
    command(&mut app, "agents main");
    assert!(app.entries[app.selected].viewed_agent_id.is_none());
    let stale_child = Arc::new(Transcript {
        text: "STALE_CHILD_SHOULD_NOT_WIN".into(),
        ..Transcript::default()
    });
    app.complete(Message::AgentMaterials(
        key_now,
        spawn.agent_id.clone(),
        Ok(stale_child),
    ));
    assert!(
        !app.entries[app.selected]
            .transcript
            .text
            .contains("STALE_CHILD_SHOULD_NOT_WIN")
    );

    until(&mut app, |a| {
        a.entries[a.selected].rows.is_some()
            && a.entries[a.selected]
                .transcript
                .text
                .contains("Task notification")
    })
    .await;

    let root_screen = screen(&app);
    assert!(!root_screen.contains("viewing agent:"), "{root_screen}");
    assert!(
        root_screen.contains("Task notification"),
        "expected compacted task notification on root transcript: {root_screen}"
    );

    // 6. Unknown selector gives clean notice.
    command(&mut app, "agents missing-agent");
    assert!(app.notice.contains("No agent matching"), "{}", app.notice);

    // 7. Closing session while viewing a child agent clears child transcript & resets view state.
    command(&mut app, &format!("agents {}", spawn.agent_id));
    until(&mut app, |a| {
        a.entries[a.selected]
            .transcript
            .text
            .contains("CHILD_UNIQUE_PROMPT_123")
            && a.entries[a.selected].rows.is_some()
    })
    .await;
    app.close(app.selected, ClosePolicy::Save);
    until(&mut app, |a| a.entries[a.selected].session.is_none()).await;
    assert!(app.entries[app.selected].viewed_agent_id.is_none());
    assert!(app.entries[app.selected].transcript.text.is_empty());
    assert!(app.entries[app.selected].rows.is_none());
    assert!(app.entries[app.selected].scroll.is_none());
}

#[tokio::test]
async fn child_agent_approval_prefix_escapes_controls_and_responds() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::new().await;
    let mut app = app(dir.path());
    app.view_width = render::transcript_width(120);
    app.view_height = 32;

    app.form = Some(create_form(&server.endpoint, None));
    app.event(ctrl('s'));
    until(&mut app, |a| a.entries[a.selected].session.is_some()).await;

    // Open `/status` panel first so we can verify the `self_waiting` footer hint while obscured.
    command(&mut app, "status");
    assert!(app.panel.is_some());

    let (tx, rx) = tokio::sync::oneshot::channel();
    let replied = Arc::new(AtomicBool::new(false));
    let replied_flag = replied.clone();
    app.entries[app.selected]
        .approval_queue
        .push_back(PendingApproval::mock_with_origin(
            "write_file".into(),
            PermissionMode::Ask,
            ApprovalSubject::FileWrite {
                path: "src/lib.rs".into(),
                action: tars_core::permission::WriteAction::Updated,
                diff: "+hello\n".into(),
            },
            ApprovalOrigin::new("child-agent-42", "evil\u{202e}\x1b[31mscout", "op-99"),
            move |decision| {
                replied_flag.store(true, Ordering::SeqCst);
                let _ = tx.send(decision);
            },
        ));
    app.update_approval_guard();

    let status_s = screen(&app);
    assert!(
        status_s.contains(
            "[agent: evil\\u{202e}\\x1b[31mscout] write_file is waiting for confirmation"
        ),
        "status screen: {status_s}"
    );
    assert!(!status_s.contains('\x1b'));
    assert!(!status_s.contains('\u{202e}'));

    // Close `/status` panel to reveal the approval modal.
    app.event(key(KeyCode::Esc));
    assert!(app.panel.is_none());

    let s = screen(&app);
    assert!(
        s.contains("[agent: evil\\u{202e}\\x1b[31mscout] write_file wants to update src/lib.rs"),
        "screen: {s}"
    );
    assert!(!s.contains('\x1b'));
    assert!(!s.contains('\u{202e}'));

    advance_past_approval_window(&mut app);
    app.event(key(KeyCode::Char('y')));
    assert!(replied.load(Ordering::SeqCst));
    assert_eq!(rx.await.unwrap(), ApprovalDecision::Allow);

    // Root origin approval must NOT display `[agent: ...]` prefix.
    let root_id = app.entries[app.selected].id().to_owned();
    let (tx2, rx2) = tokio::sync::oneshot::channel();
    app.entries[app.selected]
        .approval_queue
        .push_back(PendingApproval::mock_with_origin(
            "write_file".into(),
            PermissionMode::Ask,
            ApprovalSubject::FileWrite {
                path: "README.md".into(),
                action: tars_core::permission::WriteAction::Created,
                diff: "+hi\n".into(),
            },
            ApprovalOrigin::new(root_id, "main", "op-root"),
            move |decision| {
                let _ = tx2.send(decision);
            },
        ));
    app.update_approval_guard();
    let root_s = screen(&app);
    assert!(!root_s.contains("[agent:"), "root screen: {root_s}");
    advance_past_approval_window(&mut app);
    app.event(key(KeyCode::Char('n')));
    app.event(key(KeyCode::Enter));
    assert!(matches!(rx2.await.unwrap(), ApprovalDecision::Deny { .. }));

    let target = app.entries[app.selected].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn workspace_agents_trust_popup_guard_decline_confirm_and_rules_priority() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(workspace.join(".tars/agents")).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    std::fs::write(
        workspace.join(".tars/permissions.toml"),
        "version = 1\n[[command]]\nprefix = [\"echo\"]\ndecision = \"allow\"\n",
    )
    .unwrap();
    std::fs::write(
        workspace.join(".tars/agents/reviewer.md"),
        "---\nname: reviewer\ndescription: Reviews code safely\n---\nReview the diff.\n",
    )
    .unwrap();

    let server = Server::new().await;
    let mut app = app(dir.path());

    // 1. Open with both untrusted rules AND untrusted workspace agents:
    //    rules prompt takes priority first, then agents prompt after rules are handled.
    app.form = Some(create_form(&server.endpoint, Some(&workspace)));
    app.event(ctrl('s'));
    until(&mut app, |a| a.entries[a.selected].session.is_some()).await;

    assert!(app.entries[app.selected].workspace_rules_trust.is_some());
    assert!(app.entries[app.selected].workspace_agents_trust.is_some());
    assert!(
        !app.entries[app.selected]
            .session
            .as_ref()
            .unwrap()
            .workspace_agents_trusted()
    );

    let s_rules = screen(&app);
    assert!(
        s_rules.contains("Trust workspace command allow rules?"),
        "screen: {s_rules}"
    );
    assert!(
        !s_rules.contains("Trust workspace agent presets?"),
        "screen: {s_rules}"
    );

    // Trust rules after guard window -> agents prompt becomes visible and guard RE-ARMS.
    app.advance_clock(CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Char('y')));
    until(&mut app, |a| {
        a.entries[a.selected].workspace_rules_trust.is_none()
            && a.entries[a.selected].busy.is_none()
    })
    .await;

    let s_agents = screen(&app);
    assert!(
        s_agents.contains("Trust workspace agent presets?"),
        "screen: {s_agents}"
    );
    assert!(s_agents.contains("agent: reviewer"), "screen: {s_agents}");

    // Guard window (500ms) re-armed on transition: immediate y/n/Enter must NOT decide.
    app.event(key(KeyCode::Char('y')));
    assert!(app.entries[app.selected].workspace_agents_trust.is_some());
    app.event(key(KeyCode::Char('n')));
    assert!(app.entries[app.selected].workspace_agents_trust.is_some());
    app.event(key(KeyCode::Enter));
    assert!(app.entries[app.selected].workspace_agents_trust.is_some());

    // Advance past guard window and decline (`n`).
    app.advance_clock(CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Char('n')));
    assert!(app.entries[app.selected].workspace_agents_trust.is_none());
    assert!(
        !app.entries[app.selected]
            .session
            .as_ref()
            .unwrap()
            .workspace_agents_trusted()
    );
    assert!(
        app.entries[app.selected]
            .notice
            .contains("Declined workspace agent presets"),
        "notice: {}",
        app.entries[app.selected].notice
    );

    // Close and reopen: rules are now trusted, agents prompt appears again.
    let target = app.entries[app.selected].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
    app.entries[app.selected].session = None;

    app.open();
    until(&mut app, |a| a.entries[a.selected].session.is_some()).await;
    assert!(app.entries[app.selected].workspace_rules_trust.is_none());
    assert!(app.entries[app.selected].workspace_agents_trust.is_some());

    // Advance past guard window and confirm trust (`y`).
    app.advance_clock(CONFIRM_GUARD_WINDOW);
    app.event(key(KeyCode::Char('y')));
    until(&mut app, |a| {
        a.entries[a.selected].workspace_agents_trust.is_none()
            && a.entries[a.selected].busy.is_none()
    })
    .await;

    let session = app.entries[app.selected].session.as_ref().unwrap();
    assert!(session.workspace_agents_trusted());
    assert!(session.agent_registry_snapshot().contains("reviewer"));
    assert!(
        app.entries[app.selected]
            .notice
            .contains("Trusted workspace agents; 1 preset(s) active"),
        "notice: {}",
        app.entries[app.selected].notice
    );

    // Close and reopen again: persisted trust means no popup appears.
    let target = session.target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
    app.entries[app.selected].session = None;

    app.open();
    until(&mut app, |a| a.entries[a.selected].session.is_some()).await;
    assert!(app.entries[app.selected].workspace_agents_trust.is_none());
    assert!(
        app.entries[app.selected]
            .session
            .as_ref()
            .unwrap()
            .workspace_agents_trusted()
    );

    let target = app.entries[app.selected].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
}

#[test]
fn app_host_attaches_config_model_profiles() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        r#"
version = 1
store = "store"
[profiles.fast]
provider = "openai-chat"
model = "fast-fixture"
"#,
    )
    .unwrap();
    let opts = options::parse(
        ["--config", cfg_path.to_str().unwrap()]
            .into_iter()
            .map(str::to_owned),
    )
    .unwrap()
    .unwrap();
    assert!(opts.config.model_profiles().contains_key("fast"));
    let app = App::new(opts);
    // Host initialized cleanly with model profiles from config.
    assert!(app.options.config.model_profiles().contains_key("fast"));
}

#[test]
fn transcript_compacts_multiple_task_notifications() {
    let body = [
        "<task-notification source=\"agent_operation\" id=\"1\" operation_id=\"op-1\" agent_id=\"a1\" agent_name=\"explore\" status=\"completed\" priority=\"normal\">\nfound 3 files\n</task-notification>",
        "<task-notification source=\"agent_operation\" id=\"2\" operation_id=\"op-2\" agent_id=\"a2\" agent_name=\"build\" status=\"failed\" priority=\"high\">\ncompile error\n</task-notification>",
    ]
    .join("\n");
    let block = ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::text(Role::User, &body),
        BlockMeta::from_source(TASK_NOTIFICATION_SOURCE),
    );
    let context = Context::from_blocks(vec![block]).unwrap();
    let transcript = Transcript::from_materials(&context, &[]);
    assert!(
        transcript
            .text
            .contains("● Task notification · explore (completed) · op op-1"),
        "{}",
        transcript.text
    );
    assert!(
        transcript
            .text
            .contains("● Task notification · build (failed) · op op-2"),
        "{}",
        transcript.text
    );
    assert!(transcript.text.contains("found 3 files"));
    assert!(transcript.text.contains("compile error"));
}

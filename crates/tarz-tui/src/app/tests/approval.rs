use super::support::{app, ctrl, key, until};
use crate::{input::Form, render};
use crossterm::event::{Event, KeyCode};
use ratatui::{Terminal, backend::TestBackend};
use ratatui_textarea::TextArea;
use serde_json::{Value, json};
use tarz_core::{
    application::{ClosePolicy, SaveStatus},
    definition::WriteToolLimits,
    permission::PermissionMode,
};

use super::tools::wire;

fn screen(app: &crate::app::App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

fn sse_single_tool(name: &str, args: Value, id: &str) -> String {
    wire::tool("openai-chat", name, args, id)
}

fn sse_batch_tools(tools: &[(&str, Value, &str)]) -> String {
    let calls: Vec<Value> = tools
        .iter()
        .enumerate()
        .map(|(idx, (name, args, id))| {
            json!({
                "index": idx,
                "id": id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": args.to_string(),
                }
            })
        })
        .collect();
    let chunk1 = json!({
        "id": "chat1",
        "choices": [{
            "index": 0,
            "delta": {
                "tool_calls": calls
            },
            "finish_reason": null
        }]
    });
    let chunk2 = json!({
        "id": "chat1",
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": "tool_calls"
        }]
    });
    format!("data: {chunk1}\n\ndata: {chunk2}\n\ndata: [DONE]\n\n")
}

async fn create_test_session(
    app: &mut crate::app::App,
    endpoint: &str,
    workspace: &std::path::Path,
    mode: &str,
) {
    app.form = Some(Form {
        fields: [
            "openai-chat",
            "fixture-model",
            endpoint,
            "PATH",
            "120",
            &workspace.display().to_string(),
            "list_directory,read_file,search_text,write_file,edit_file",
            mode,
            "",
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
    });
    app.event(ctrl('s'));
    until(app, |a| a.entries[a.selected].session.is_some()).await;
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wm05_allow_write_via_enter_and_y() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let responses = vec![
        sse_single_tool(
            "write_file",
            json!({"path": "test.txt", "content": "hello world\n"}),
            "call_write_1",
        ),
        wire::answer_with_text("openai-chat", "file written successfully"),
    ];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);

    let mut app = app(dir.path());
    create_test_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    // Send input
    app.entries[0].editor.insert_str("create test.txt");
    app.event(key(KeyCode::Enter));

    // Wait for approval request
    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;

    // Check panel screen
    let view = screen(&app);
    assert!(view.contains("write_file wants to create test.txt (permission mode: ask)"));
    assert!(view.contains("+hello world"));
    assert!(view.contains(
        "Enter / y allow · n deny · Esc cancel turn · Shift-Tab mode · ↑↓ PgUp PgDn scroll"
    ));

    // Allow via Enter
    app.event(key(KeyCode::Enter));

    // Wait for turn completion
    until(&mut app, |a| a.entries[a.selected].execution == "Completed").await;

    // Verify file content
    let content = std::fs::read_to_string(workspace.join("test.txt")).unwrap();
    assert_eq!(content, "hello world\n");
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wm05_allow_write_via_y_key() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let responses = vec![
        sse_single_tool(
            "write_file",
            json!({"path": "y_test.txt", "content": "from y\n"}),
            "call_write_y",
        ),
        wire::answer_with_text("openai-chat", "y file done"),
    ];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);

    let mut app = app(dir.path());
    create_test_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.entries[0].editor.insert_str("create y_test.txt");
    app.event(key(KeyCode::Enter));

    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;
    app.event(key(KeyCode::Char('y')));

    until(&mut app, |a| a.entries[a.selected].execution == "Completed").await;
    let content = std::fs::read_to_string(workspace.join("y_test.txt")).unwrap();
    assert_eq!(content, "from y\n");
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wm05_deny_write_with_reason() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let responses = vec![
        sse_single_tool(
            "write_file",
            json!({"path": "denied.txt", "content": "should not exist"}),
            "call_deny_1",
        ),
        wire::answer_with_text("openai-chat", "acknowledged refusal"),
    ];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);

    let mut app = app(dir.path());
    create_test_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.entries[0].editor.insert_str("create denied.txt");
    app.event(key(KeyCode::Enter));

    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;

    // Press 'n' to enter reason input
    app.event(key(KeyCode::Char('n')));
    let view = screen(&app);
    assert!(view.contains("Reason for the model (optional, Enter to skip):"));

    // Type reason
    for c in "wrong path".chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Enter));

    // Wait for completion
    until(&mut app, |a| a.entries[a.selected].execution == "Completed").await;

    // File must not exist
    assert!(!workspace.join("denied.txt").exists());

    // Verify model second request contains Rejected with reason
    let requests = fixture.requests.lock().unwrap();
    assert!(requests.len() >= 2);
    let last_req = requests.last().unwrap();
    let req_str = serde_json::to_string(last_req).unwrap();
    assert!(req_str.contains("user declined this write: wrong path"));
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wm05_deny_write_without_reason() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let responses = vec![
        sse_single_tool(
            "write_file",
            json!({"path": "denied_empty.txt", "content": "none"}),
            "call_deny_empty",
        ),
        wire::answer_with_text("openai-chat", "acknowledged empty refusal"),
    ];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);

    let mut app = app(dir.path());
    create_test_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.entries[0].editor.insert_str("create denied_empty.txt");
    app.event(key(KeyCode::Enter));

    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;

    // Press 'n', then Enter without typing anything
    app.event(key(KeyCode::Char('n')));
    app.event(key(KeyCode::Enter));

    until(&mut app, |a| a.entries[a.selected].execution == "Completed").await;
    assert!(!workspace.join("denied_empty.txt").exists());

    let requests = fixture.requests.lock().unwrap();
    let last_req = requests.last().unwrap();
    let req_str = serde_json::to_string(last_req).unwrap();
    assert!(req_str.contains("user declined this write"));
    assert!(!req_str.contains("user declined this write:"));
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wm05_deny_reason_esc_returns_to_question_then_allows() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let responses = vec![
        sse_single_tool(
            "write_file",
            json!({"path": "esc_back.txt", "content": "written after esc"}),
            "call_esc_back",
        ),
        wire::answer_with_text("openai-chat", "done"),
    ];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);

    let mut app = app(dir.path());
    create_test_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.entries[0].editor.insert_str("test esc back");
    app.event(key(KeyCode::Enter));

    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;

    // Press 'n'
    app.event(key(KeyCode::Char('n')));
    assert!(screen(&app).contains("Reason for the model"));

    // Press Esc in reason mode -> returns to question
    app.event(key(KeyCode::Esc));
    let view = screen(&app);
    assert!(!view.contains("Reason for the model"));
    assert!(view.contains("Enter / y allow · n deny"));

    // Now press Enter -> allows write!
    app.event(key(KeyCode::Enter));

    until(&mut app, |a| a.entries[a.selected].execution == "Completed").await;
    let content = std::fs::read_to_string(workspace.join("esc_back.txt")).unwrap();
    assert_eq!(content, "written after esc");
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wm05_two_writes_different_files_and_duplicate_file_in_batch() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    // Part 1: Two different files in one batch
    let batch1 = sse_batch_tools(&[
        (
            "write_file",
            json!({"path": "file1.txt", "content": "c1"}),
            "c1",
        ),
        (
            "write_file",
            json!({"path": "file2.txt", "content": "c2"}),
            "c2",
        ),
    ]);
    let responses = vec![
        batch1,
        wire::answer_with_text("openai-chat", "batch 1 complete"),
    ];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);

    let mut app = app(dir.path());
    create_test_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.entries[0].editor.insert_str("write two files");
    app.event(key(KeyCode::Enter));

    // Wait for first approval (file1.txt)
    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;
    assert!(screen(&app).contains("wants to create file1.txt"));
    app.event(key(KeyCode::Enter));

    // Wait for second approval (file2.txt)
    until(&mut app, |a| {
        a.entries[a.selected]
            .approval_queue
            .front()
            .is_some_and(|r| match &r.subject {
                tarz_core::permission::ApprovalSubject::FileWrite { path, .. } => {
                    path == "file2.txt"
                }
                _ => false,
            })
    })
    .await;
    assert!(screen(&app).contains("wants to create file2.txt"));
    app.event(key(KeyCode::Enter));

    until(&mut app, |a| a.entries[a.selected].execution == "Completed").await;
    assert_eq!(
        std::fs::read_to_string(workspace.join("file1.txt")).unwrap(),
        "c1"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("file2.txt")).unwrap(),
        "c2"
    );

    // Part 2: Same file twice in one batch
    let batch2 = sse_batch_tools(&[
        (
            "write_file",
            json!({"path": "same.txt", "content": "first"}),
            "s1",
        ),
        (
            "write_file",
            json!({"path": "same.txt", "content": "second"}),
            "s2",
        ),
    ]);
    let fixture2 = wire::Fixture::with_responses(
        "openai-chat",
        vec![
            batch2,
            wire::answer_with_text("openai-chat", "batch 2 done"),
        ],
    );
    let mut app2 = super::support::app(dir.path());
    create_test_session(&mut app2, &fixture2.endpoint, &workspace, "ask").await;

    app2.entries[0]
        .editor
        .insert_str("duplicate write in batch");
    app2.event(key(KeyCode::Enter));

    // First one asks
    until(&mut app2, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;
    assert!(screen(&app2).contains("wants to create same.txt"));
    app2.event(key(KeyCode::Enter));

    // Turn completes without asking a second time (second is rejected by precheck)
    until(&mut app2, |a| {
        a.entries[a.selected].execution == "Completed"
    })
    .await;
    assert_eq!(
        std::fs::read_to_string(workspace.join("same.txt")).unwrap(),
        "first"
    );
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wm05_approval_panel_esc_cancels_turn() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let responses = vec![sse_single_tool(
        "write_file",
        json!({"path": "cancel_me.txt", "content": "cancel"}),
        "c_cancel",
    )];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);

    let mut app = app(dir.path());
    create_test_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.entries[0].editor.insert_str("cancel this write");
    app.event(key(KeyCode::Enter));

    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;

    // Press Esc in question view
    app.event(key(KeyCode::Esc));

    // Wait for turn cancellation
    until(&mut app, |a| a.entries[a.selected].execution == "Cancelled").await;
    assert!(app.entries[0].approval_queue.is_empty());
    assert!(!workspace.join("cancel_me.txt").exists());

    // Check session save status
    until(&mut app, |a| a.entries[a.selected].status.contains("saved")).await;
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wm05_untrusted_text_escaped_in_rendered_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let dangerous_name = "dan\x1b[31mg\u{061C}er.txt";
    let dangerous_content = "line1\r\x1b[32m\u{061C}line2\n";
    let responses = vec![sse_single_tool(
        "write_file",
        json!({"path": dangerous_name, "content": dangerous_content}),
        "call_esc_test",
    )];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);

    let mut app = app(dir.path());
    create_test_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.entries[0].editor.insert_str("dangerous write");
    app.event(key(KeyCode::Enter));

    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;

    let view = screen(&app);
    // Rendered output contains escaped forms
    assert!(view.contains(r"\x1b"));
    assert!(view.contains(r"\r"));
    assert!(view.contains(r"\u{61c}"));

    // Rendered output MUST NOT contain raw control chars
    assert!(!view.contains('\x1b'));
    assert!(!view.contains('\r'));
    assert!(!view.contains('\u{061C}'));

    // Cancel out
    app.event(key(KeyCode::Esc));
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wm05_diff_scroll_page_down() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let mut long_content = String::new();
    for i in 1..=100 {
        long_content.push_str(&format!("line_{i}_content_for_scroll\n"));
    }

    let responses = vec![sse_single_tool(
        "write_file",
        json!({"path": "long.txt", "content": long_content}),
        "call_long",
    )];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);

    let mut app = app(dir.path());
    create_test_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.entries[0].editor.insert_str("long diff");
    app.event(key(KeyCode::Enter));

    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;

    let top_view = screen(&app);
    assert!(top_view.contains("line_1_content"));
    assert!(!top_view.contains("line_95_content"));

    // Scroll down
    for _ in 0..20 {
        app.event(key(KeyCode::PageDown));
    }
    let scrolled_view = screen(&app);
    assert!(scrolled_view.contains("line_"));
    assert_ne!(top_view, scrolled_view);

    app.event(key(KeyCode::Esc));
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wm05_letters_ignored_when_panel_open_draft_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let responses = vec![sse_single_tool(
        "write_file",
        json!({"path": "ignore_keys.txt", "content": "text"}),
        "call_ignore",
    )];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);

    let mut app = app(dir.path());
    create_test_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.entries[0].editor.insert_str("my draft input");
    app.event(key(KeyCode::Enter));

    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;

    // Draft was cleared on submit, let us set draft on editor to check it is not modified
    app.entries[0].editor.insert_str("draft_preserved");
    app.event(key(KeyCode::Char('a')));
    app.event(key(KeyCode::Char('b')));
    app.event(key(KeyCode::Char('z')));

    let lines = app.entries[0].editor.lines().join("");
    assert_eq!(lines, "draft_preserved");

    app.event(key(KeyCode::Esc));
}

#[tokio::test]
async fn wm17_mode_switching_cycle_and_command() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());

    // Create session
    let fixture = wire::Fixture::with_responses("openai-chat", vec![]);
    create_test_session(&mut app, &fixture.endpoint, dir.path(), "ask").await;

    // 1. Initial mode is ask
    let view = screen(&app);
    assert!(view.contains("mode ask"));

    // 2. Shift-Tab -> accept-edits
    app.event(key(KeyCode::BackTab));
    assert_eq!(
        app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .permission_mode()
            .unwrap()
            .0,
        PermissionMode::AcceptEdits
    );
    assert!(screen(&app).contains("mode accept-edits"));
    assert_eq!(app.notice, "Permission mode: accept-edits");

    // 3. Shift-Tab -> read-only
    app.event(key(KeyCode::BackTab));
    assert_eq!(
        app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .permission_mode()
            .unwrap()
            .0,
        PermissionMode::ReadOnly
    );
    assert!(screen(&app).contains("mode read-only"));
    assert_eq!(app.notice, "Permission mode: read-only");

    // 4. Shift-Tab -> ask
    app.event(key(KeyCode::BackTab));
    assert_eq!(
        app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .permission_mode()
            .unwrap()
            .0,
        PermissionMode::Ask
    );
    assert!(screen(&app).contains("mode ask"));
    assert_eq!(app.notice, "Permission mode: ask");

    // 5. /mode full-access via command panel
    app.event(key(KeyCode::Char('/')));
    for c in "mode full-access".chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Enter));
    assert_eq!(
        app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .permission_mode()
            .unwrap()
            .0,
        PermissionMode::FullAccess
    );
    assert!(screen(&app).contains("mode full-access"));

    // 6. Shift-Tab from full-access -> ask
    app.event(key(KeyCode::BackTab));
    assert_eq!(
        app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .permission_mode()
            .unwrap()
            .0,
        PermissionMode::Ask
    );
    assert!(screen(&app).contains("mode ask"));

    // 7. /mode without args opens choice panel
    app.event(key(KeyCode::Char('/')));
    for c in "mode".chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Enter));
    assert!(matches!(
        app.panel,
        Some(crate::app::Panel::ModeSelect { .. })
    ));
    // Select accept-edits (Down from ask(1) -> accept-edits(2))
    app.event(key(KeyCode::Down));
    app.event(key(KeyCode::Enter));
    assert!(app.panel.is_none());
    assert_eq!(
        app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .permission_mode()
            .unwrap()
            .0,
        PermissionMode::AcceptEdits
    );

    // 8. /mode bogus shows legal names and leaves mode unchanged
    app.event(key(KeyCode::Char('/')));
    for c in "mode bogus".chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Enter));
    assert!(app.notice.contains("read-only"));
    assert!(app.notice.contains("ask"));
    assert!(app.notice.contains("accept-edits"));
    assert!(app.notice.contains("full-access"));
    assert_eq!(
        app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .permission_mode()
            .unwrap()
            .0,
        PermissionMode::AcceptEdits
    );
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wm17_mode_switch_inside_panel() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    // Round 1: Two writes in ask mode
    let batch = sse_batch_tools(&[
        (
            "write_file",
            json!({"path": "sw1.txt", "content": "sw1"}),
            "c_sw1",
        ),
        (
            "write_file",
            json!({"path": "sw2.txt", "content": "sw2"}),
            "c_sw2",
        ),
    ]);
    // Round 2: A write after switching back to ask
    let write_round2 = sse_single_tool(
        "write_file",
        json!({"path": "sw3.txt", "content": "sw3"}),
        "c_sw3",
    );
    let responses = vec![
        batch,
        wire::answer_with_text("openai-chat", "round 1 done"),
        write_round2,
        wire::answer_with_text("openai-chat", "round 2 done"),
    ];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);

    let mut app = app(dir.path());
    create_test_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.entries[0].editor.insert_str("batch writes");
    app.event(key(KeyCode::Enter));

    // First panel arrives
    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;
    assert!(screen(&app).contains("wants to create sw1.txt"));

    // Switch mode inside panel: Shift-Tab to accept-edits
    app.event(key(KeyCode::BackTab));
    let view = screen(&app);
    assert!(view.contains("Permission mode changed to accept-edits"));

    // Allow the first write
    app.event(key(KeyCode::Enter));

    // Turn completes without showing second panel (auto allowed!)
    until(&mut app, |a| a.entries[a.selected].execution == "Completed").await;
    assert_eq!(
        std::fs::read_to_string(workspace.join("sw1.txt")).unwrap(),
        "sw1"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("sw2.txt")).unwrap(),
        "sw2"
    );

    // Switch back to ask via /mode ask
    app.event(key(KeyCode::Char('/')));
    for c in "mode ask".chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Enter));
    assert_eq!(
        app.entries[0]
            .session
            .as_ref()
            .unwrap()
            .permission_mode()
            .unwrap()
            .0,
        PermissionMode::Ask
    );

    // Wait until round 1 save is fully persisted before submitting round 2
    until(&mut app, |a| {
        a.entries[0].session.as_ref().is_some_and(|s| {
            matches!(
                s.status().save,
                tarz_core::application::SaveStatus::Saved { revision: 3, .. }
            )
        })
    })
    .await;

    // Send round 2
    app.entries[0].editor.insert_str("round 2 write");
    app.event(key(KeyCode::Enter));

    // Panel shows again!
    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;
    assert!(screen(&app).contains("wants to create sw3.txt"));
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| a.entries[a.selected].execution == "Completed").await;
    assert_eq!(
        std::fs::read_to_string(workspace.join("sw3.txt")).unwrap(),
        "sw3"
    );
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wm05_two_sessions_background_approval_hint_and_switching() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let responses_a = vec![wire::answer_with_text("openai-chat", "hello from A")];
    let fixture_a = wire::Fixture::with_responses("openai-chat", responses_a);

    let responses_b = vec![
        sse_single_tool(
            "write_file",
            json!({"path": "b_file.txt", "content": "from B"}),
            "c_b",
        ),
        wire::answer_with_text("openai-chat", "B finished"),
    ];
    let fixture_b = wire::Fixture::with_responses("openai-chat", responses_b);

    let mut app = app(dir.path());
    // Create Session A (index 0)
    create_test_session(&mut app, &fixture_a.endpoint, &workspace, "ask").await;
    // Create Session B (index 1)
    create_test_session(&mut app, &fixture_b.endpoint, &workspace, "ask").await;

    // Switch to B and submit work
    app.selected = 1;
    app.entries[1].editor.insert_str("start B");
    app.event(key(KeyCode::Enter));

    // Immediately switch back to A before B's approval arrives
    app.selected = 0;

    // Wait until B's approval arrives in B's queue
    until(&mut app, |a| !a.entries[1].approval_queue.is_empty()).await;

    // On Session A, confirmation panel is NOT shown!
    assert!(app.entries[0].approval_queue.is_empty());
    let view_a = screen(&app);
    assert!(!view_a.contains("wants to create b_file.txt"));
    // But waiting notice is visible on A!
    assert!(view_a.contains("is waiting for confirmation"));

    // Check sessions panel also marks B
    app.event(key(KeyCode::Char('/')));
    for c in "sessions".chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Enter));
    let sess_view = screen(&app);
    assert!(sess_view.contains("waiting for confirmation"));
    app.event(key(KeyCode::Esc));

    // Switch to B
    app.selected = 1;
    let view_b = screen(&app);
    assert!(view_b.contains("write_file wants to create b_file.txt"));

    // Answer on B
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| a.entries[1].execution == "Completed").await;
    assert_eq!(
        std::fs::read_to_string(workspace.join("b_file.txt")).unwrap(),
        "from B"
    );
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wm05_ctrl_q_and_close_with_panel_open_complete_in_bounded_time() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let responses = vec![sse_single_tool(
        "write_file",
        json!({"path": "will_not_write.txt", "content": "test"}),
        "c_close",
    )];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);

    let mut app = app(dir.path());
    create_test_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.entries[0].editor.insert_str("trigger write");
    app.event(key(KeyCode::Enter));

    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;

    // Test Ctrl-Q while panel is open: must complete within timeout!
    app.event(ctrl('q'));
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        until(&mut app, |a| a.done).await;
    })
    .await
    .expect("Ctrl-Q must exit cleanly within 5 seconds");

    assert!(!workspace.join("will_not_write.txt").exists());

    // Test /close while panel is open on reopened session
    let responses2 = vec![sse_single_tool(
        "write_file",
        json!({"path": "will_not_write2.txt", "content": "test2"}),
        "c_close2",
    )];
    let fixture2 = wire::Fixture::with_responses("openai-chat", responses2);
    let mut app2 = super::support::app(dir.path());
    create_test_session(&mut app2, &fixture2.endpoint, &workspace, "ask").await;
    app2.entries[0].editor.insert_str("trigger write 2");
    app2.event(key(KeyCode::Enter));
    until(&mut app2, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;

    // Close session directly via close action
    app2.close(0, ClosePolicy::Save);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        until(&mut app2, |a| a.entries[0].session.is_none()).await;
    })
    .await
    .expect("close must finish cleanly within 5 seconds");
    assert!(!workspace.join("will_not_write2.txt").exists());
}

#[tokio::test]
async fn wm17_permission_mode_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = wire::Fixture::with_responses("openai-chat", vec![]);

    let mut app1 = app(dir.path());
    create_test_session(&mut app1, &fixture.endpoint, dir.path(), "ask").await;

    // Switch to accept-edits
    app1.event(key(KeyCode::BackTab));
    assert_eq!(
        app1.entries[0]
            .session
            .as_ref()
            .unwrap()
            .permission_mode()
            .unwrap()
            .0,
        PermissionMode::AcceptEdits
    );

    // Save and close
    app1.close(0, ClosePolicy::Save);
    until(&mut app1, |a| a.entries[0].session.is_none()).await;

    // Reopen in same or new host
    let mut app2 = super::support::app(dir.path());
    app2.list();
    until(&mut app2, |a| !a.entries.is_empty()).await;
    app2.open();
    until(&mut app2, |a| a.entries[0].session.is_some()).await;

    // Must be accept-edits!
    assert_eq!(
        app2.entries[0]
            .session
            .as_ref()
            .unwrap()
            .permission_mode()
            .unwrap()
            .0,
        PermissionMode::AcceptEdits
    );

    // Now switch to full-access (initial was ask)
    app2.event(key(KeyCode::Char('/')));
    for c in "mode full-access".chars() {
        app2.event(key(KeyCode::Char(c)));
    }
    app2.event(key(KeyCode::Enter));
    assert_eq!(
        app2.entries[0]
            .session
            .as_ref()
            .unwrap()
            .permission_mode()
            .unwrap()
            .0,
        PermissionMode::FullAccess
    );

    // Close and reopen -> must revert to ask!
    app2.close(0, ClosePolicy::Save);
    until(&mut app2, |a| a.entries[0].session.is_none()).await;

    let mut app3 = app(dir.path());
    app3.list();
    until(&mut app3, |a| !a.entries.is_empty()).await;
    app3.open();
    until(&mut app3, |a| a.entries[0].session.is_some()).await;

    assert_eq!(
        app3.entries[0]
            .session
            .as_ref()
            .unwrap()
            .permission_mode()
            .unwrap()
            .0,
        PermissionMode::Ask
    );
    assert!(
        app3.entries[0]
            .notice
            .contains("permission mode reverted from full-access to ask"),
        "{}",
        app3.entries[0].notice
    );
}

#[test]
fn wm17_new_session_form_permission_mode_and_write_limits() {
    let mut preset_form = Form::default();
    preset_form.fields[1] = TextArea::from(["fixture-model"]);
    let mut preset = preset_form.definition().unwrap();
    preset.permission_mode = Some(PermissionMode::AcceptEdits);
    preset.write_limits = Some(WriteToolLimits {
        max_edit_file_bytes: 8192,
        max_write_bytes: 4096,
        max_edit_segments: 10,
    });

    let form = Form::new(
        std::sync::Arc::new(tarz_core::config::Config::default()),
        None,
        Some(&preset),
    );
    assert_eq!(form.fields.len(), 9);
    assert_eq!(form.fields[7].lines().join(""), "accept-edits");
    assert_eq!(form.write_limits.as_ref().unwrap().max_write_bytes, 4096);

    let def = form.definition().unwrap();
    assert_eq!(def.permission_mode, Some(PermissionMode::AcceptEdits));
    assert_eq!(def.write_limits.as_ref().unwrap().max_write_bytes, 4096);

    // Change to read-only
    let mut form2 = Form::new(
        std::sync::Arc::new(tarz_core::config::Config::default()),
        None,
        Some(&preset),
    );
    form2.fields[7] = TextArea::from(["read-only"]);
    let def2 = form2.definition().unwrap();
    assert_eq!(def2.permission_mode, Some(PermissionMode::ReadOnly));

    // Invalid mode name errors with all four names
    let mut form3 = Form::new(
        std::sync::Arc::new(tarz_core::config::Config::default()),
        None,
        Some(&preset),
    );
    form3.fields[7] = TextArea::from(["bogus"]);
    let err = form3.definition().unwrap_err();
    assert!(err.contains("read-only"));
    assert!(err.contains("ask"));
    assert!(err.contains("accept-edits"));
    assert!(err.contains("full-access"));

    // An empty mode is an error too, not a silent `ask`
    let mut form4 = Form::new(
        std::sync::Arc::new(tarz_core::config::Config::default()),
        None,
        Some(&preset),
    );
    form4.fields[7] = TextArea::from([""]);
    let err = form4.definition().unwrap_err();
    assert!(err.contains("accept-edits"));
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn wm13_mcp_approval_panel_display_and_actions() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();

    let fixture = wire::Fixture::with_responses("openai-chat", vec![]);
    let mut app = app(dir.path());
    create_test_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    // 1. Enqueue McpCall approval request
    let (tx1, mut rx1) = tokio::sync::oneshot::channel();
    app.entries[0]
        .approval_queue
        .push_back(crate::app::state::PendingApproval::mock(
            "mcp_demo_run".into(),
            PermissionMode::Ask,
            tarz_core::permission::ApprovalSubject::McpCall {
                server: "demo_server".into(),
                tool: "run_action".into(),
                arguments: "{\"param\": \"val\u{1b}[31m\"}".into(),
            },
            move |d| {
                let _ = tx1.send(d);
            },
        ));

    let view = screen(&app);
    assert!(view.contains(
        "mcp_demo_run wants to call MCP tool demo_server/run_action (permission mode: ask)"
    ));
    assert!(view.contains(r#"val\x1b[31m"#));
    assert!(!view.contains('\x1b'));
    assert!(view.contains("Enter / y allow"));

    // Press y to allow
    app.event(key(KeyCode::Char('y')));
    assert_eq!(
        rx1.try_recv().unwrap(),
        tarz_core::permission::ApprovalDecision::Allow
    );
    assert!(app.entries[0].approval_queue.is_empty());

    // 2. Enqueue second McpCall approval request
    let (tx2, mut rx2) = tokio::sync::oneshot::channel();
    app.entries[0]
        .approval_queue
        .push_back(crate::app::state::PendingApproval::mock(
            "mcp_demo_danger".into(),
            PermissionMode::Ask,
            tarz_core::permission::ApprovalSubject::McpCall {
                server: "danger_server".into(),
                tool: "delete_db".into(),
                arguments: "{}".into(),
            },
            move |d| {
                let _ = tx2.send(d);
            },
        ));

    // Press n to enter reason prompt, then type reason and press Enter to deny
    app.event(key(KeyCode::Char('n')));
    for c in "unsafe tool".chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Enter));

    let decision = rx2.try_recv().unwrap();
    assert_eq!(
        decision,
        tarz_core::permission::ApprovalDecision::Deny {
            reason: Some("unsafe tool".into()),
        }
    );
    assert!(app.entries[0].approval_queue.is_empty());

    // 3. Enqueue third McpCall approval request and test Esc key cancellation
    let (tx3, mut rx3) = tokio::sync::oneshot::channel();
    app.entries[0]
        .approval_queue
        .push_back(crate::app::state::PendingApproval::mock(
            "mcp_demo_cancel".into(),
            PermissionMode::Ask,
            tarz_core::permission::ApprovalSubject::McpCall {
                server: "cancel_server".into(),
                tool: "cancel_action".into(),
                arguments: "{}".into(),
            },
            move |d| {
                let _ = tx3.send(d);
            },
        ));
    assert_eq!(app.entries[0].approval_queue.len(), 1);

    // Press Esc to cancel turn
    app.event(key(KeyCode::Esc));

    // Approval queue must be cleared on Esc and responder channel was dropped without allow/deny
    assert!(app.entries[0].approval_queue.is_empty());
    assert!(app.entries[0].approval_reason.is_none());
    assert!(rx3.try_recv().is_err());
    // Without in-flight work (entry.work is None in this mock UI test), self.cancel()
    // safely no-ops, queue is cleared, and approval panel closes returning to editor.
    let view = screen(&app);
    assert!(
        !view.contains("wants to call MCP tool"),
        "approval modal must close after Esc"
    );
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn queued_approvals_reset_scroll_and_rendering_clamps_to_the_last_line() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let batch = sse_batch_tools(&[
        (
            "write_file",
            json!({"path": "scroll_one.txt", "content": "first replacement"}),
            "c_scroll_1",
        ),
        (
            "write_file",
            json!({"path": "scroll_two.txt", "content": "a\nb\nTARGET-LAST-LINE\n"}),
            "c_scroll_2",
        ),
    ]);
    let fixture = wire::Fixture::with_responses(
        "openai-chat",
        vec![batch, wire::answer_with_text("openai-chat", "scroll done")],
    );
    let mut app = app(dir.path());
    create_test_session(&mut app, &fixture.endpoint, &workspace, "ask").await;
    app.entries[0].editor.insert_str("two writes");
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;
    app.entries[0].approval_scroll = 7;
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| {
        a.entries[a.selected]
            .approval_queue
            .front()
            .is_some_and(|request| match &request.subject {
                tarz_core::permission::ApprovalSubject::FileWrite { path, .. } => {
                    path == "scroll_two.txt"
                }
                _ => false,
            })
    })
    .await;
    assert_eq!(app.entries[0].approval_scroll, 0);

    app.entries[0].approval_scroll = 10_000;
    let view = screen(&app);
    assert!(view.contains("TARGET-LAST-LINE"), "{view}");
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| a.entries[a.selected].execution == "Completed").await;
}

fn screen_at(app: &crate::app::App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

#[tokio::test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn hidden_approval_panel_ignores_decisions_but_keeps_cancel_and_quit() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let responses = vec![
        sse_single_tool(
            "write_file",
            json!({"path": "small_y.txt", "content": "allowed after resize\n"}),
            "c_small_y",
        ),
        wire::answer_with_text("openai-chat", "first done"),
        sse_single_tool(
            "write_file",
            json!({"path": "small_esc.txt", "content": "never written\n"}),
            "c_small_esc",
        ),
    ];
    let fixture = wire::Fixture::with_responses("openai-chat", responses);
    let mut app = app(dir.path());
    create_test_session(&mut app, &fixture.endpoint, &workspace, "ask").await;

    app.entries[0].editor.insert_str("first small write");
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;
    app.event(Event::Resize(29, 11));
    let hidden = screen_at(&app, 29, 11);
    assert!(hidden.contains("Resize to at least 30 x 19"), "{hidden}");
    app.event(key(KeyCode::Char('y')));
    app.event(key(KeyCode::Enter));
    assert!(!workspace.join("small_y.txt").exists());
    assert!(!app.entries[0].approval_queue.is_empty());

    // The nominal 30 x 12 minimum still cannot show the approval body.
    app.event(Event::Resize(30, 12));
    let boundary = screen_at(&app, 30, 12);
    assert!(
        boundary.contains("Resize to at least 30 x 19"),
        "{boundary}"
    );
    app.event(key(KeyCode::Char('y')));
    app.event(key(KeyCode::Enter));
    assert!(!workspace.join("small_y.txt").exists());
    assert!(!app.entries[0].approval_queue.is_empty());

    app.event(Event::Resize(30, 18));
    let too_short = screen_at(&app, 30, 18);
    assert!(
        too_short.contains("Resize to at least 30 x 19"),
        "{too_short}"
    );
    app.event(key(KeyCode::Char('y')));
    app.event(key(KeyCode::Enter));
    assert!(!workspace.join("small_y.txt").exists());
    assert!(!app.entries[0].approval_queue.is_empty());

    app.event(Event::Resize(30, 19));
    let shown = screen_at(&app, 30, 19);
    assert!(!shown.contains("Resize to at least 30 x 19"), "{shown}");
    app.event(key(KeyCode::Char('y')));
    until(&mut app, |a| a.entries[a.selected].execution == "Completed").await;
    assert_eq!(
        std::fs::read_to_string(workspace.join("small_y.txt")).unwrap(),
        "allowed after resize\n"
    );
    until(&mut app, |a| {
        a.entries[0]
            .session
            .as_ref()
            .is_some_and(|session| matches!(session.status().save, SaveStatus::Saved { .. }))
    })
    .await;

    app.entries[0].editor.insert_str("second small write");
    app.event(key(KeyCode::Enter));
    until(&mut app, |a| {
        !a.entries[a.selected].approval_queue.is_empty()
    })
    .await;
    app.event(Event::Resize(29, 11));
    app.event(key(KeyCode::Esc));
    assert!(app.entries[0].approval_queue.is_empty());
    until(&mut app, |a| a.entries[a.selected].execution == "Cancelled").await;
    assert!(!workspace.join("small_esc.txt").exists());
}

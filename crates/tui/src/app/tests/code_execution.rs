use super::support::*;
use crate::{app::state::Entry, options, render, transcript::Transcript};
use causa::kernel::{
    BlockContent, Context, ContextBlock, InvocationId, RoundId, ToolCallPayload, ToolOutput,
    ToolResultPayload, ToolResultStatus, TurnId,
};
use crossterm::event::KeyCode;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;
use std::sync::Arc;
use tars_core::definition::CodeExecutionSettings;
use tars_core::session::{ProgressUpdate, ToolPhase, ToolProgress, WorkProgress, WorkRef};
use tokio::sync::broadcast;

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

fn command(app: &mut crate::app::App, text: &str) {
    app.event(key(KeyCode::F(1)));
    for c in text.chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Enter));
}

#[tokio::test]
async fn ptc07_tui_cli_flags_and_code_execution_resolution() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        r#"
version = 1
store = "store"
default_profile = "work"
[profiles.work]
provider = "openai-chat"
model = "fixture-model"
api_key_env = "PATH"
[profiles.work.code_execution]
enabled = false
timeout_secs = 55
max_subcalls = 99
"#,
    )
    .unwrap();

    // 1. Mutually exclusive flags rejected.
    let err = options::parse(
        [
            "--no-config",
            "--store",
            "s",
            "--code-execution",
            "--no-code-execution",
        ]
        .into_iter()
        .map(str::to_owned),
    )
    .err()
    .expect("mutually exclusive flags must error");
    assert!(err.contains("mutually exclusive"), "{err}");

    // 2. Duplicate flags rejected.
    assert!(
        options::parse(
            [
                "--no-config",
                "--store",
                "s",
                "--code-execution",
                "--code-execution"
            ]
            .into_iter()
            .map(str::to_owned),
        )
        .is_err()
    );

    // 3. `--code-execution` overrides TOML `enabled = false` while keeping custom limits.
    let opts = options::parse(
        ["--config", cfg_path.to_str().unwrap(), "--code-execution"]
            .into_iter()
            .map(str::to_owned),
    )
    .unwrap()
    .unwrap();
    assert_eq!(opts.code_execution_enabled, Some(true));
    assert_eq!(
        opts.preset.as_ref().and_then(|p| p.code_execution),
        Some(CodeExecutionSettings {
            timeout_secs: 55,
            max_subcalls: 99,
            ..CodeExecutionSettings::default()
        })
    );

    let mut app = crate::app::App::new(opts);
    app.event(key(KeyCode::F(2)));
    until(&mut app, |a| {
        a.entries.len() == 1 && a.entries[0].session.is_some()
    })
    .await;
    assert_eq!(app.entries.len(), 1);
    let def_on = app.entries[0].session.as_ref().unwrap().definition();
    assert_eq!(
        def_on.code_execution,
        Some(CodeExecutionSettings {
            timeout_secs: 55,
            max_subcalls: 99,
            ..CodeExecutionSettings::default()
        })
    );

    // 4. Disabling code_execution via DefinitionOverrides turns it off.
    let def_off = app
        .options
        .config
        .definition(
            app.options.profile.as_deref(),
            tars_core::config::DefinitionOverrides {
                code_execution_enabled: Some(false),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(def_off.code_execution, None);
}

#[test]
fn ptc07_tui_status_and_transcript_render_run_code_summary_and_progress() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());

    let call_id = causa::runtime::new_block_id();
    let long_output = format!("HEAD_OUTPUT_{}", "x".repeat(240));
    let context = Context::from_blocks(vec![
        ContextBlock::new(
            call_id,
            BlockContent::ToolCall(ToolCallPayload::new(
                "run_code",
                json!({ "code": "return await read_file('a.txt');" }),
            )),
            Default::default(),
        ),
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::ToolResult(ToolResultPayload::new(
                call_id,
                ToolResultStatus::Succeeded,
                ToolOutput::new(json!({
                    "output": long_output,
                    "truncated": false,
                    "stats": {
                        "subcalls": 7,
                        "by_tool": { "read_file": 7 },
                        "max_in_flight": 3,
                        "duration_ms": 42,
                    }
                })),
            )),
            Default::default(),
        ),
    ])
    .unwrap();
    let transcript = Arc::new(Transcript::from_materials(&context, &[]));
    assert!(
        transcript
            .text
            .contains("subcalls=7 · max_in_flight=3 · 42ms · HEAD_OUTPUT_"),
        "{}",
        transcript.text
    );

    let mut entry = Entry::new("ptc-tui-status".into(), "fixture-model".into());
    entry.code_execution = Some(CodeExecutionSettings {
        timeout_secs: 45,
        max_subcalls: 128,
        max_concurrency: 4,
        ..CodeExecutionSettings::default()
    });
    entry.transcript = transcript.clone();
    entry.rows = Some(crate::transcript::Rows::wrap(transcript, 90));
    let work = WorkRef {
        conversation_id: "ptc-tui-status".into(),
        turn_id: TurnId::new("w1"),
    };
    entry.work = Some(work.clone());
    let (tx, rx) = broadcast::channel(8);
    entry.bind_progress(rx);
    app.entries.push(entry);

    tx.send(Arc::new(WorkProgress {
        work: work.clone(),
        invocation_id: InvocationId {
            turn_id: work.turn_id,
            round_id: RoundId(0),
        },
        update: ProgressUpdate::Tool(ToolProgress {
            invocation_id: InvocationId {
                turn_id: TurnId::new("w1"),
                round_id: RoundId(0),
            },
            call_block_id: call_id,
            tool_name: "run_code".into(),
            arguments_summary: "{\"code\":\"return 1;\"}".into(),
            phase: ToolPhase::Committed,
            status: Some(ToolResultStatus::Succeeded),
            operation_not_started: false,
            permission_decision: None,
        }),
    }))
    .unwrap();
    app.poll_progress();

    let main_screen = screen(&app);
    assert!(main_screen.contains("✓ Code"), "{main_screen}");
    assert!(
        main_screen.contains("subcalls=7 · max_in_flight=3 · 42ms"),
        "{main_screen}"
    );

    command(&mut app, "status");
    let status_screen = screen(&app);
    assert!(
        status_screen
            .contains("Code execution: on (timeout 45s, max_subcalls 128, max_concurrency 4)"),
        "{status_screen}"
    );

    app.event(key(KeyCode::Esc));
    app.entries[0].code_execution = None;
    command(&mut app, "status");
    let status_off = screen(&app);
    assert!(status_off.contains("Code execution: off"), "{status_off}");
}

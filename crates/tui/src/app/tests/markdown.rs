use super::support::{
    Server, app, create, create_in_view, ctrl, history_definition, key, open_in_view, until,
    write_history_session,
};
use crate::{
    app::{InfoKind, Panel, jobs::Message, state::Entry},
    render::{self, ACCENT, MUTED},
    transcript::{Rows, Transcript},
};
use causa::kernel::{
    BlockContent, BlockMeta, Context, ContextBlock, Role, ToolCallPayload, ToolOutput,
    ToolResultPayload, ToolResultStatus,
};
use crossterm::event::{Event, KeyCode};
use ratatui::{
    Terminal,
    backend::TestBackend,
    buffer::{Buffer, Cell},
    style::{Color, Modifier},
};
use std::sync::Arc;
use tars_core::application::{ClosePolicy, SaveStatus, SpawnAgentOutcome, SpawnAgentRequest};
use tars_core::definition::ChildNarrowingRequest;
use tars_core::events::{AgentInteraction, OnParentComplete, OperationExecutionMode};

fn render_buffer(app: &crate::app::App, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    terminal.backend().buffer().clone()
}

fn buffer_text(buf: &Buffer) -> String {
    let area = buf.area();
    let mut lines = Vec::with_capacity(usize::from(area.height));
    for y in 0..area.height {
        let mut row = String::new();
        for x in 0..area.width {
            row.push_str(buf.cell((x, y)).unwrap().symbol());
        }
        lines.push(row);
    }
    lines.join("\n")
}

fn has_styled_text(buf: &Buffer, needle: &str, pred: impl Fn(&Cell) -> bool) -> bool {
    let area = buf.area();
    for y in 0..area.height {
        let mut row = String::new();
        let mut map = Vec::new();
        for x in 0..area.width {
            let cell = buf.cell((x, y)).unwrap();
            let sym = cell.symbol();
            if !sym.is_empty() {
                let start = row.len();
                row.push_str(sym);
                map.push((start, row.len(), x));
            }
        }
        let mut search_from = 0;
        while let Some(rel) = row[search_from..].find(needle) {
            let abs_start = search_from + rel;
            let abs_end = abs_start + needle.len();
            let matching_cells: Vec<&Cell> = map
                .iter()
                .filter(|&&(s, e, _)| s >= abs_start && e <= abs_end)
                .map(|&(_, _, x)| buf.cell((x, y)).unwrap())
                .collect();
            if !matching_cells.is_empty() && matching_cells.iter().all(|c| pred(c)) {
                return true;
            }
            search_from = abs_start + 1;
        }
    }
    false
}

fn command(app: &mut crate::app::App, text: &str) {
    app.event(key(KeyCode::F(1)));
    for c in text.chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Enter));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn md06_markdown_renders_styled_cells_in_test_backend_and_rewraps_on_resize() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let def = history_definition(None);

    let user_block = ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::text(Role::User, "# RawUserHeading\n**raw_user_bold**"),
        BlockMeta::from_source("user"),
    );
    let asst_md = "\
# AssistantTitle\n\n\
Here is `inline_code` and **bold_word**.\n\n\
```rust\n\
let value = 42;\n\
```\n\n\
| Name | Kind |\n\
| --- | --- |\n\
| alpha | beta |";
    let assistant_block = ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::text(Role::Assistant, asst_md),
        BlockMeta::from_source("assistant"),
    );
    let ctx = Context::from_blocks(vec![user_block, assistant_block]).unwrap();
    create_in_view(&mut app, def, ctx).await;
    until(&mut app, |a| {
        a.entries[0].rows.is_some() && !a.entries[0].wrapping
    })
    .await;

    let buf = render_buffer(&app, 100, 32);
    let screen = buffer_text(&buf);
    // User Markdown syntax remains literal and unstyled.
    assert!(screen.contains("# RawUserHeading"), "{screen}");
    assert!(screen.contains("**raw_user_bold**"), "{screen}");
    assert!(has_styled_text(&buf, "# RawUserHeading", |c| {
        c.fg == Color::Reset && !c.modifier.contains(Modifier::BOLD)
    }));

    // Assistant Markdown elements render with proper styles.
    assert!(has_styled_text(&buf, "AssistantTitle", |c| {
        c.fg == ACCENT && c.modifier.contains(Modifier::BOLD | Modifier::UNDERLINED)
    }));
    assert!(has_styled_text(&buf, "inline_code", |c| {
        c.fg == Color::Cyan
    }));
    assert!(has_styled_text(&buf, "bold_word", |c| {
        c.modifier.contains(Modifier::BOLD)
    }));
    assert!(has_styled_text(&buf, "let", |c| c.fg == Color::Magenta));
    assert!(screen.contains('┌'), "boxed table expected:\n{screen}");
    assert!(screen.contains("alpha"), "{screen}");

    // Resize to a narrow width -> background re-wrap updates rows cleanly.
    app.event(Event::Resize(30, 22));
    app.poll();
    until(&mut app, |a| {
        !a.entries[0].wrapping
            && a.entries[0]
                .rows
                .as_ref()
                .is_some_and(|r| r.width == render::transcript_width(30))
    })
    .await;

    let narrow_buf = render_buffer(&app, 30, 22);
    let narrow_screen = buffer_text(&narrow_buf);
    assert!(narrow_screen.contains("alpha"), "{narrow_screen}");
    assert!(narrow_screen.contains("let"), "{narrow_screen}");

    app.event(ctrl('q'));
    until(&mut app, |a| a.done).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn md06_large_context_repeated_draws_trigger_zero_reparses_or_rewraps() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let def = history_definition(None);

    let chunk = "## Section\n\n- item **one** with `code`\n\n```rust\nfn work() -> usize { 42 }\n```\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n";
    let large_body = chunk.repeat(4_000); // ~480 KiB Markdown body
    let assistant_block = ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::text(Role::Assistant, large_body),
        BlockMeta::from_source("assistant"),
    );
    let ctx = Context::from_blocks(vec![assistant_block]).unwrap();
    create_in_view(&mut app, def, ctx).await;
    until(&mut app, |a| {
        a.entries[0].rows.is_some() && !a.entries[0].wrapping
    })
    .await;
    assert!(app.entries[0].rows.as_ref().unwrap().len() > 10_000);

    let reads_before = app.entries[0].material_reads;
    let ptr_before = Arc::as_ptr(&app.entries[0].rows.as_ref().unwrap().transcript);
    for step in 0..20 {
        app.entries[0].scroll = Some(step * 113);
        app.poll();
        let _ = render_buffer(&app, 100, 32);
    }

    assert_eq!(
        Arc::as_ptr(&app.entries[0].rows.as_ref().unwrap().transcript),
        ptr_before
    );
    assert!(!app.entries[0].wrapping);
    assert_eq!(app.entries[0].material_reads, reads_before);
    assert!(app.jobs.is_empty());

    app.event(ctrl('q'));
    until(&mut app, |a| a.done).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn md06_scroll_anchor_preserves_markdown_row_across_history_load_and_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::with_summary("## Compacted Brief\n\n- **Status**: done").await;
    let mut def = history_definition(Some(4_000));
    def.endpoint = Some(server.endpoint.clone());
    def.auto_compact = true;
    def.compact_threshold = 50;

    let old_user = ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::text(Role::User, "existing conversation line. ".repeat(70)),
        BlockMeta::from_source("user"),
    );
    let md_lines: String = (0..18)
        .map(|i| {
            if i == 5 {
                "### ANCHOR_MD_HEADING\n\n".to_owned()
            } else {
                format!("- list row {i} with **bold_{i}**\n")
            }
        })
        .collect();
    let md_reply = ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::text(Role::Assistant, md_lines),
        BlockMeta::from_source("assistant"),
    );
    let ctx = Context::from_blocks(vec![old_user, md_reply]).unwrap();
    create_in_view(&mut app, def, ctx).await;

    let anchor_idx = {
        let rows = app.entries[0].rows.as_ref().unwrap();
        (0..rows.len())
            .position(|i| rows.row_text(i).is_some_and(|t| t == "ANCHOR_MD_HEADING"))
            .expect("markdown heading row")
    };
    app.event(Event::Paste(
        "long follow up prompt to cross threshold ".repeat(50),
    ));
    app.event(ctrl('s'));
    app.entries[0].scroll = Some(anchor_idx);
    until(&mut app, |_| server.requests.lock().unwrap().len() >= 2).await;
    until(&mut app, |a| {
        a.entries[0].session.as_ref().unwrap().history().len() == 1 && !a.entries[0].wrapping
    })
    .await;

    let new_scroll = app.entries[0].scroll.expect("scroll preserved");
    let new_history_len = app.entries[0].history_rows.as_ref().map_or(0, Rows::len);
    let row_after = if new_scroll < new_history_len {
        app.entries[0]
            .history_rows
            .as_ref()
            .unwrap()
            .row_text(new_scroll)
            .unwrap()
    } else {
        app.entries[0]
            .rows
            .as_ref()
            .unwrap()
            .row_text(new_scroll - new_history_len)
            .unwrap()
    };
    assert_eq!(row_after, "ANCHOR_MD_HEADING");

    server.release.add_permits(1);
    until(&mut app, |a| a.entries[0].execution == "Completed").await;
    app.event(ctrl('q'));
    until(&mut app, |a| a.done).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn md07_history_expansion_and_readonly_child_agent_render_markdown() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");

    let hist_asst = ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::text(
            Role::Assistant,
            "# HistHeading\n\n```python\ndef hi():\n    return 1\n```",
        ),
        BlockMeta::from_source("assistant"),
    );
    let hist_ctx = Context::from_blocks(vec![hist_asst]).unwrap();
    let current_ctx = Context::from_blocks(vec![ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::text(Role::User, "current filler\n".repeat(45)),
        BlockMeta::from_source("user"),
    )])
    .unwrap();
    let id = write_history_session(&store, &current_ctx, &[hist_ctx]);

    let mut app1 = app(dir.path());
    open_in_view(&mut app1, &id).await;
    // Zero record or artifact reads on open.
    assert_eq!(app1.entries[0].record_reads, 0);
    assert_eq!(app1.entries[0].artifact_reads, 0);

    // Scroll to top to load history lazily.
    app1.entries[0].scroll = Some(0);
    app1.event(key(KeyCode::PageUp));
    until(&mut app1, |a| {
        a.entries[0].record_reads == 1 && !a.entries[0].history.loading() && !a.entries[0].wrapping
    })
    .await;

    app1.entries[0].scroll = Some(0);
    let buf = render_buffer(&app1, 100, 32);
    let screen = buffer_text(&buf);
    assert!(has_styled_text(&buf, "HistHeading", |c| {
        c.fg == ACCENT && c.modifier.contains(Modifier::BOLD | Modifier::UNDERLINED)
    }));
    assert!(has_styled_text(&buf, "def", |c| c.fg == Color::Magenta));
    assert!(has_styled_text(&buf, "Compacted", |c| c.fg == MUTED));
    assert!(screen.contains("start of conversation"), "{screen}");

    app1.event(ctrl('q'));
    until(&mut app1, |a| a.done).await;

    // Now verify read-only child agent view renders Markdown replies while staying read-only.
    let dir2 = tempfile::tempdir().unwrap();
    let server = Server::with_streaming_rounds(vec![
        vec![
            format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": "c1",
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "content": "## ChildReply\n\n**child_bold**\n\n```rust\nlet child = 42;\n```"
                        },
                        "finish_reason": null
                    }]
                })
            ),
            "data: {\"id\":\"c1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_owned(),
        ],
        vec![
            r#"data: {"id":"r1","choices":[{"index":0,"delta":{"content":"root ack"},"finish_reason":null}]}"#.to_owned() + "\n\n",
            "data: {\"id\":\"r1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_owned(),
        ],
    ])
    .await;
    let mut app2 = app(dir2.path());
    let mut def = history_definition(None);
    def.endpoint = Some(server.endpoint.clone());
    create_in_view(&mut app2, def, Context::from_blocks(vec![]).unwrap()).await;
    let session = app2.entries[0].session.clone().unwrap();
    let root_id = app2.entries[0].id().to_owned();

    let SpawnAgentOutcome::Background(spawn) = session
        .spawn_agent(SpawnAgentRequest {
            caller_agent_id: root_id,
            caller_operation_id: None,
            source_block_id: None,
            name: "md-child".into(),
            preset_name: None,
            role_prompt: None,
            prompt: "report in markdown".into(),
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
        panic!("expected background spawn");
    };
    session.wait_settled().await.unwrap();

    command(&mut app2, &format!("agents {}", spawn.agent_id));
    until(&mut app2, |a| {
        a.entries[0].transcript.text.contains("ChildReply")
            && a.entries[0].rows.is_some()
            && !a.entries[0].wrapping
    })
    .await;

    let child_buf = render_buffer(&app2, 100, 32);
    assert!(has_styled_text(&child_buf, "ChildReply", |c| {
        c.fg == ACCENT && c.modifier.contains(Modifier::BOLD)
    }));
    assert!(has_styled_text(&child_buf, "child_bold", |c| {
        c.modifier.contains(Modifier::BOLD)
    }));
    assert!(has_styled_text(&child_buf, "let", |c| {
        c.fg == Color::Magenta
    }));

    // Read-only enforcement remains intact.
    app2.event(Event::Paste("blocked input".into()));
    app2.event(key(KeyCode::Enter));
    assert_eq!(
        app2.entries[0].notice,
        "Viewing child agent (read-only); run /agents main to send input"
    );
    assert_eq!(app2.entries[0].editor.lines(), &["blocked input"]);
    assert!(app2.entries[0].work.is_none());

    let target = session.target();
    app2.host.close(&target, ClosePolicy::Save).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn md08_tools_diff_expansion_and_summary_markdown_panel_render_and_scroll() {
    // 1. Committed write_file/edit_file diff expansion + spilled artifact diff expansion in /tools
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.view_width = render::transcript_width(100);
    app.view_height = 32;

    let call_id = causa::runtime::new_block_id();
    let call_block = ContextBlock::new(
        call_id,
        BlockContent::ToolCall(ToolCallPayload::new(
            "edit_file",
            serde_json::json!({ "path": "src/demo.rs" }),
        )),
        BlockMeta::default(),
    );
    let result_block = ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::ToolResult(ToolResultPayload::new(
            call_id,
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::json!({
                "path": "src/demo.rs",
                "action": "updated",
                "diff": "--- a/src/demo.rs\n+++ b/src/demo.rs\n@@ -1,2 +1,2 @@\n-old_value\n+new_value\n unchanged\n",
            })),
        )),
        BlockMeta::default(),
    );
    let ctx = Context::from_blocks(vec![call_block, result_block]).unwrap();
    let mut entry = Entry::new("tools-diff".into(), "fixture-model".into());
    entry.transcript = Arc::new(Transcript::from_materials(&ctx, &[]));
    app.entries.push(entry);

    app.event(key(KeyCode::F(6)));
    let tools_buf = render_buffer(&app, 100, 32);
    let tools_screen = buffer_text(&tools_buf);
    assert!(
        tools_screen.contains("Diff (src/demo.rs):"),
        "{tools_screen}"
    );
    assert!(!tools_screen.contains("\\n+new_value"), "{tools_screen}");
    assert!(has_styled_text(&tools_buf, "Declaration", |c| {
        c.fg == ACCENT && c.modifier.contains(Modifier::BOLD)
    }));
    assert!(has_styled_text(&tools_buf, "Result for", |c| {
        c.fg == ACCENT && c.modifier.contains(Modifier::BOLD)
    }));
    assert!(has_styled_text(&tools_buf, "Diff (src/demo.rs):", |c| {
        c.fg == Color::Cyan && c.modifier.contains(Modifier::BOLD)
    }));
    assert!(has_styled_text(&tools_buf, "@@ -1,2 +1,2 @@", |c| {
        c.fg == Color::Cyan
    }));
    assert!(has_styled_text(&tools_buf, "-old_value", |c| {
        c.fg == Color::Red
    }));
    assert!(has_styled_text(&tools_buf, "+new_value", |c| {
        c.fg == Color::Green
    }));
    assert!(app.entries[0].work.is_none());
    app.event(key(KeyCode::Esc));

    // Also verify ToolArtifactsLoaded path with JSON `"diff"` in spilled output
    app.panel = Some(Panel::Info {
        title: "Recorded tool materials · viewing never executes".into(),
        kind: InfoKind::Tools(vec![]),
        lines: vec![],
        scroll: 0,
    });
    let key_now = app.entries[0].key.clone();
    let spilled_json = serde_json::json!({
        "path": "src/spilled.rs",
        "diff": "@@ -1 +1 @@\n-spilled_old\n+spilled_new\n"
    });
    let mut builder = crate::transcript::ToolDetailBuilder::new();
    builder.push_header(&format!(
        "Result for {} · Succeeded · artifact:1",
        call_id.0
    ));
    builder.push_note(
        "Model sees spilled preview only · full output from artifact:1 (90 bytes):",
        false,
    );
    builder.push_formatted_json(&crate::transcript::format_tool_output_json(&spilled_json));
    let (formatted_spilled, spilled_meta) = builder.finish();
    app.complete(Message::ToolArtifactsLoaded(
        key_now,
        None,
        vec![formatted_spilled],
        vec![spilled_meta],
        1,
    ));
    let spilled_buf = render_buffer(&app, 100, 32);
    assert!(has_styled_text(
        &spilled_buf,
        "Model sees spilled preview only",
        |c| c.fg == MUTED
    ));
    assert!(has_styled_text(&spilled_buf, "+spilled_new", |c| {
        c.fg == Color::Green
    }));
    assert!(has_styled_text(&spilled_buf, "-spilled_old", |c| {
        c.fg == Color::Red
    }));
    app.event(key(KeyCode::Esc));

    // 2. /summary panel renders Markdown and scrolls accurately to the bottom
    let dir2 = tempfile::tempdir().unwrap();
    let mut app2 = super::support::app(dir2.path());
    let summary_md = format!(
        "# SummaryHeading\n\n- **KeyPoint**: retained\n\n```rust\nlet summary_ok = true;\n```\n\n{}\n\nSUMMARY_TAIL_MARKER",
        (0..35)
            .map(|i| format!("- detail item {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let server = Server::with_summary(summary_md).await;
    let mut def = history_definition(None);
    def.endpoint = Some(server.endpoint.clone());
    create(&mut app2, &server.endpoint).await;
    app2.event(Event::Paste("trigger summary ".repeat(250)));
    app2.event(ctrl('s'));
    until(&mut app2, |_| server.requests.lock().unwrap().len() == 1).await;
    server.release.add_permits(1);
    until(&mut app2, |a| {
        a.entries[0].execution == "Completed"
            && matches!(
                a.entries[0].session.as_ref().unwrap().status().save,
                SaveStatus::Saved { .. }
            )
    })
    .await;

    command(&mut app2, "compact");
    until(&mut app2, |a| {
        a.entries[0].busy.is_none()
            && a.entries[0].transcript.summary.is_some()
            && !a.entries[0].wrapping
    })
    .await;

    app2.event(Event::Resize(100, 32));
    command(&mut app2, "summary");
    let sum_buf = render_buffer(&app2, 100, 32);
    assert!(has_styled_text(&sum_buf, "SummaryHeading", |c| {
        c.fg == ACCENT && c.modifier.contains(Modifier::BOLD | Modifier::UNDERLINED)
    }));
    assert!(has_styled_text(&sum_buf, "KeyPoint", |c| {
        c.modifier.contains(Modifier::BOLD)
    }));
    assert!(has_styled_text(&sum_buf, "let", |c| {
        c.fg == Color::Magenta
    }));

    // Scroll to the bottom: verify SUMMARY_TAIL_MARKER is visible during scrolling and
    // reaching max lands cleanly on the true last line without overshooting.
    let mut saw_tail_marker = false;
    for _ in 0..30 {
        app2.event(key(KeyCode::PageDown));
        let page_screen = buffer_text(&render_buffer(&app2, 100, 32));
        if page_screen.contains("SUMMARY_TAIL_MARKER") {
            saw_tail_marker = true;
        }
    }
    assert!(
        saw_tail_marker,
        "SUMMARY_TAIL_MARKER should appear while scrolling"
    );
    let bottom_buf = render_buffer(&app2, 100, 32);
    let bottom_screen = buffer_text(&bottom_buf);
    assert!(
        bottom_screen.contains("Original messages: recall id="),
        "{bottom_screen}"
    );
    if let Some(Panel::Info {
        scroll,
        lines,
        kind,
        ..
    }) = &app2.panel
    {
        let max =
            crate::transcript::info_panel_row_count(kind, lines, app2.view_width).saturating_sub(1);
        assert_eq!(*scroll, max);
    } else {
        panic!("expected summary Info panel");
    }

    app2.event(key(KeyCode::Esc));
    app2.event(ctrl('q'));
    until(&mut app2, |a| a.done).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn md09_streaming_preview_stays_plain_and_hands_off_to_markdown_on_commit() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.view_width = render::transcript_width(100);
    app.view_height = 32;
    app.viewport_height = render::conversation_height(32, 100, &app);

    let (mut entry, sender) = {
        let mut entry = Entry::new("stream-handoff".into(), "fixture-model".into());
        let turn_id = causa::kernel::TurnId::new("t-md09");
        entry.work = Some(tars_core::session::WorkRef {
            conversation_id: "stream-handoff".into(),
            turn_id: turn_id.clone(),
        });
        entry.execution = "Running".into();
        let (tx, rx) = tokio::sync::broadcast::channel(16);
        entry.bind_progress(rx);
        (entry, (tx, turn_id))
    };
    let initial_ctx = Context::from_blocks(vec![ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::text(Role::User, "write code"),
        BlockMeta::from_source("user"),
    )])
    .unwrap();
    entry.transcript = Arc::new(Transcript::from_materials(&initial_ctx, &[]));
    entry.rows = Some(Rows::wrap(entry.transcript.clone(), app.view_width));
    let key_now = entry.key.clone();
    app.entries.push(entry);

    let md_stream = "# StreamHeading\n\n**stream_bold**\n\n```rust\nlet x = 7;\n```";
    sender
        .0
        .send(Arc::new(tars_core::session::WorkProgress {
            work: app.entries[0].work.clone().unwrap(),
            invocation_id: causa::kernel::InvocationId {
                turn_id: sender.1.clone(),
                round_id: causa::kernel::RoundId(0),
            },
            update: tars_core::session::ProgressUpdate::Text {
                delta: md_stream.into(),
                truncated: false,
            },
        }))
        .unwrap();
    app.poll_progress();

    // While streaming, preview holds plain wrapped text (no Markdown parsing) and label is visible.
    assert!(app.entries[0].preview.is_active());
    let streaming_buf = render_buffer(&app, 100, 32);
    let streaming_screen = buffer_text(&streaming_buf);
    assert!(
        streaming_screen.contains("Generating · temporary"),
        "{streaming_screen}"
    );
    assert!(
        streaming_screen.contains("# StreamHeading"),
        "{streaming_screen}"
    );
    assert!(has_styled_text(&streaming_buf, "# StreamHeading", |c| {
        c.fg == Color::Reset && !c.modifier.contains(Modifier::UNDERLINED)
    }));

    // Turn finishes and commits materials -> preview clears and Markdown rich text renders.
    let result = Arc::new(tars_core::session::WorkResult::Run(
        causa::runtime::TurnResult::Completed {
            final_output: causa::kernel::ModelOutput::new(
                causa::kernel::ModelResponse {
                    items: vec![causa::kernel::ModelItem::text(md_stream)],
                },
                causa::kernel::ModelStopReason::EndTurn,
            ),
        },
    ));
    app.complete(Message::Finished(
        key_now,
        app.entries[0].work.clone().unwrap(),
        Ok((result, Arc::default())),
    ));
    assert!(!app.entries[0].preview.is_active());

    let committed_ctx = Context::from_blocks(vec![
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::User, "write code"),
            BlockMeta::from_source("user"),
        ),
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::Assistant, md_stream),
            BlockMeta::from_source("assistant"),
        ),
    ])
    .unwrap();
    app.entries[0].transcript = Arc::new(Transcript::from_materials(&committed_ctx, &[]));
    app.entries[0].rows = Some(Rows::wrap(
        app.entries[0].transcript.clone(),
        app.view_width,
    ));

    let final_buf = render_buffer(&app, 100, 32);
    let final_screen = buffer_text(&final_buf);
    assert!(
        !final_screen.contains("Generating · temporary"),
        "{final_screen}"
    );
    assert!(!final_screen.contains("# StreamHeading"), "{final_screen}");
    assert!(has_styled_text(&final_buf, "StreamHeading", |c| {
        c.fg == ACCENT && c.modifier.contains(Modifier::BOLD | Modifier::UNDERLINED)
    }));
    assert!(has_styled_text(&final_buf, "stream_bold", |c| {
        c.modifier.contains(Modifier::BOLD)
    }));
    assert!(has_styled_text(&final_buf, "let", |c| {
        c.fg == Color::Magenta
    }));
}

#[test]
fn md08_tool_artifacts_loaded_clamps_scroll_when_result_is_shorter() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.view_width = render::transcript_width(100);
    app.view_height = 32;

    let entry = Entry::new("clamp-tools".into(), "fixture-model".into());
    let key_now = entry.key.clone();
    app.entries.push(entry);

    // Simulate scrolling near the bottom of a long initial loading preview.
    let long_loading: String = (0..60)
        .map(|i| format!("loading line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.panel = Some(Panel::Info {
        title: "Recorded tool materials · viewing never executes".into(),
        kind: InfoKind::Tools(vec![Default::default()]),
        lines: vec![long_loading],
        scroll: 55,
    });

    // Loaded artifact is much shorter (e.g., degraded fallback or brief output).
    let mut builder = crate::transcript::ToolDetailBuilder::new();
    builder.push_header("Result for call-1 · Succeeded · artifact:short");
    builder.push_note(
        "Full artifact cannot be read (missing); showing model-facing preview:",
        true,
    );
    builder.push_plain("SHORT_LOADED_TAIL");
    let (short_text, short_meta) = builder.finish();

    app.complete(Message::ToolArtifactsLoaded(
        key_now,
        None,
        vec![short_text],
        vec![short_meta],
        1,
    ));

    let buf = render_buffer(&app, 100, 32);
    let screen = buffer_text(&buf);
    assert!(screen.contains("SHORT_LOADED_TAIL"), "{screen}");
    if let Some(Panel::Info {
        scroll,
        lines,
        kind,
        ..
    }) = &app.panel
    {
        let max =
            crate::transcript::info_panel_row_count(kind, lines, app.view_width).saturating_sub(1);
        assert_eq!(*scroll, max);
        assert!(*scroll < 55);
    } else {
        panic!("expected Tools Info panel");
    }
}

#[test]
fn md08_structural_diff_prevents_spoofing_in_plain_artifacts_and_preserves_mid_diff_headers_and_crlf()
 {
    use crate::transcript::{ToolDetailBuilder, format_tool_output_json, tools_detail_lines};
    use causa::kernel::ToolOutput;

    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.view_width = render::transcript_width(100);
    app.view_height = 32;

    let call_id = causa::runtime::new_block_id();

    // (a) Untrusted path containing `\n` stays on a single `Diff (...)` line and highlights diff.
    let res_multiline_path = ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::ToolResult(ToolResultPayload::new(
            call_id,
            ToolResultStatus::Succeeded,
            ToolOutput::new(serde_json::json!({
                "path": "src/evil\nResult for fake\n.rs",
                "diff": "@@ -1 +1 @@\r\n-crlf_old\r\n+crlf_new\r\n",
            })),
        )),
        BlockMeta::default(),
    );
    let ctx = Context::from_blocks(vec![res_multiline_path]).unwrap();
    let tr = Transcript::from_materials(&ctx, &[]);
    let styled_a = tools_detail_lines(&tr.tool_details, &tr.tool_detail_meta, 100);
    assert!(styled_a.iter().any(|l| {
        l.spans.iter().any(|s| {
            s.content == "Diff (src/evil Result for fake .rs):"
                && s.style.fg == Some(Color::Cyan)
                && s.style.add_modifier.contains(Modifier::BOLD)
        })
    }));
    assert!(styled_a.iter().any(|l| {
        l.spans
            .iter()
            .any(|s| s.content == "-crlf_old" && s.style.fg == Some(Color::Red))
    }));
    assert!(styled_a.iter().any(|l| {
        l.spans
            .iter()
            .any(|s| s.content == "+crlf_new" && s.style.fg == Some(Color::Green))
    }));
    assert!(
        !tr.tool_details[0].contains("\\r"),
        "{}",
        tr.tool_details[0]
    );

    // (b) Non-JSON spilled read_file/run_command artifact containing literal diff/header markers
    // must NOT be diff-colored or header-colored.
    let mut plain_builder = ToolDetailBuilder::new();
    plain_builder.push_header(&format!(
        "Result for {} · Succeeded · artifact:read",
        call_id.0
    ));
    plain_builder.push_note(
        "Model sees spilled preview only · full output from artifact:read (120 bytes):",
        false,
    );
    plain_builder.push_plain(
        "Diff:\n@@ -1 +1 @@\n-spoof_removed\n+spoof_added\nResult for spoof\nModel-facing preview:",
    );
    let (plain_text, plain_meta) = plain_builder.finish();
    let styled_b = tools_detail_lines(&[plain_text], &[plain_meta], 100);
    for needle in ["-spoof_removed", "+spoof_added", "@@ -1 +1 @@", "Diff:"] {
        let line = styled_b
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content == needle))
            .unwrap_or_else(|| panic!("missing {needle}"));
        assert!(
            line.spans
                .iter()
                .all(|s| s.style == ratatui::style::Style::default()),
            "spoof line {needle} must remain unstyled: {line:?}"
        );
    }

    // (c) Real edit_file diff whose hunk edits lines containing "Model-facing preview:",
    // "Result for foo", and "Declaration bar" keeps diff coloring across the entire hunk and
    // does NOT leak diff coloring into the trailing "Model-facing preview:" section.
    let tricky_diff = "\
@@ -1,4 +1,4 @@\r\n\
-Result for foo\r\n\
+Declaration bar\r\n\
-Model-facing preview:\r\n\
+Model-facing preview:\r\n";
    let tricky_json = serde_json::json!({
        "path": "src/tricky.rs",
        "diff": tricky_diff,
    });
    let mut tricky_builder = ToolDetailBuilder::new();
    tricky_builder.push_header(&format!(
        "Result for {} · Succeeded · artifact:edit",
        call_id.0
    ));
    tricky_builder.push_note(
        "Model sees spilled preview only · full output from artifact:edit (200 bytes):",
        false,
    );
    tricky_builder.push_formatted_json(&format_tool_output_json(&tricky_json));
    tricky_builder.push_blank();
    tricky_builder.push_note("Model-facing preview:", false);
    tricky_builder.push_plain("{\n  \"+should_not_be_green\": true\n}");
    let (tricky_text, tricky_meta) = tricky_builder.finish();
    assert!(!tricky_text.contains("\\r"), "{tricky_text}");

    let styled_c = tools_detail_lines(&[tricky_text], &[tricky_meta], 100);
    assert!(styled_c.iter().any(|l| {
        l.spans
            .iter()
            .any(|s| s.content == "-Result for foo" && s.style.fg == Some(Color::Red))
    }));
    assert!(styled_c.iter().any(|l| {
        l.spans
            .iter()
            .any(|s| s.content == "+Declaration bar" && s.style.fg == Some(Color::Green))
    }));
    assert!(styled_c.iter().any(|l| {
        l.spans
            .iter()
            .any(|s| s.content == "-Model-facing preview:" && s.style.fg == Some(Color::Red))
    }));
    assert!(styled_c.iter().any(|l| {
        l.spans
            .iter()
            .any(|s| s.content == "+Model-facing preview:" && s.style.fg == Some(Color::Green))
    }));
    // The real trailing note line is MUTED, and following JSON is plain.
    assert!(styled_c.iter().any(|l| {
        l.spans
            .iter()
            .any(|s| s.content == "Model-facing preview:" && s.style.fg == Some(MUTED))
    }));
    let trailing_json = styled_c
        .iter()
        .find(|l| {
            l.spans
                .iter()
                .any(|s| s.content.contains("+should_not_be_green"))
        })
        .expect("trailing preview json line");
    assert!(
        trailing_json
            .spans
            .iter()
            .all(|s| s.style == ratatui::style::Style::default()),
        "{trailing_json:?}"
    );
}

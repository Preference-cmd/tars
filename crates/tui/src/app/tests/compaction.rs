//! Compaction commands, the status bar estimate and the summary panel.

use super::support::*;
use crate::{app::Panel, render};
use crossterm::event::{Event, KeyCode};
use ratatui::{Terminal, backend::TestBackend};
use tars_core::application::SaveStatus;

fn screen(app: &crate::app::App) -> String {
    let mut terminal = Terminal::new(TestBackend::new(100, 50)).unwrap();
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
    for character in text.chars() {
        app.event(key(KeyCode::Char(character)));
    }
    app.event(key(KeyCode::Enter));
}

fn info_lines(app: &crate::app::App) -> Vec<String> {
    match &app.panel {
        Some(Panel::Info { lines, .. }) => lines.clone(),
        other => panic!(
            "expected an information panel, present: {}",
            other.is_some()
        ),
    }
}

fn settings(app: &crate::app::App) -> tars_core::application::CompactionSettings {
    app.entries[0]
        .session
        .as_ref()
        .unwrap()
        .compaction_settings()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_compact_command_reports_changes_and_rejects_invalid_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;
    let initial = settings(&app);
    assert!(initial.auto_compact);
    assert_eq!(initial.compact_threshold, 70);
    assert_eq!(initial.context_window, None);

    command(&mut app, "auto_compact off");
    assert!(!settings(&app).auto_compact);
    assert!(app.notice.contains("off"), "{}", app.notice);
    // An unknown window is named together with how to set it.
    assert!(app.notice.contains("context_window"), "{}", app.notice);
    let view = screen(&app);
    assert!(view.contains("auto off"), "{view}");

    command(&mut app, "auto_compact 50");
    let current = settings(&app);
    assert!(current.auto_compact);
    assert_eq!(current.compact_threshold, 50);
    assert!(app.notice.contains("50%"), "{}", app.notice);

    command(&mut app, "auto_compact 5");
    let unchanged = settings(&app);
    assert!(unchanged.auto_compact);
    assert_eq!(unchanged.compact_threshold, 50);
    assert!(app.notice.contains("10 and 95"), "{}", app.notice);

    command(&mut app, "auto_compact");
    let lines = info_lines(&app);
    let joined = lines.join("\n");
    assert!(joined.contains("Switch: on"), "{joined}");
    assert!(joined.contains("Threshold: 50%"), "{joined}");
    assert!(joined.contains("Context window: unknown"), "{joined}");
    assert!(joined.contains("`context_window`"), "{joined}");
    app.event(key(KeyCode::Esc));
    assert!(app.panel.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_bar_reports_the_window_and_auto_off_and_status_lists_work_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    create_in_view(
        &mut app,
        history_definition(Some(200_000)),
        text_context("hello context"),
    )
    .await;

    let view = screen(&app);
    assert!(view.contains("ctx "), "{view}");
    assert!(view.contains("/ 200k"), "{view}");
    assert!(!view.contains("auto off"), "{view}");

    let session = app.entries[0].session.as_ref().unwrap().clone();
    session.set_auto_compact(false).unwrap();
    app.poll();
    let view = screen(&app);
    assert!(view.contains("auto off"), "{view}");

    command(&mut app, "auto_compact");
    let joined = info_lines(&app).join("\n");
    assert!(joined.contains("Switch: off"), "{joined}");
    assert!(
        joined.contains("Context window: 200000 tokens (200k)"),
        "{joined}"
    );
    app.event(key(KeyCode::Esc));

    command(&mut app, "status");
    let joined = info_lines(&app).join("\n");
    assert!(joined.contains("Works: 0 / 8"), "{joined}");
    app.event(key(KeyCode::Esc));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_job_keeps_the_replaced_conversation_and_summary_panel() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;
    app.event(Event::Paste("recorded before compaction ".repeat(200)));
    app.event(ctrl('s'));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;
    server.release.add_permits(1);
    until(&mut app, |app| {
        app.entries[0].execution == "Completed" && app.entries[0].transcript.text.contains("你好")
    })
    .await;
    until(&mut app, |app| {
        matches!(
            app.entries[0].session.as_ref().unwrap().status().save,
            SaveStatus::Saved { .. }
        )
    })
    .await;

    let session = app.entries[0].session.as_ref().unwrap().clone();
    let usage_before = session.context_usage().unwrap().estimate_tokens;
    assert!(usage_before > 1_000, "{usage_before}");
    assert_eq!(app.entries[0].record_reads, 0);

    command(&mut app, "compact");
    assert_eq!(app.entries[0].busy, Some("compacting"));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 2).await;
    // The history view settles in steps; wait until it has reached the start.
    until(&mut app, |app| {
        app.entries[0].busy.is_none()
            && app.entries[0].rows.is_some()
            && app.entries[0].history_rows.is_some()
            && !app.entries[0].history.loading()
            && !app.entries[0].history.has_earlier()
            && !app.entries[0].wrapping
    })
    .await;

    let session = app.entries[0].session.as_ref().unwrap().clone();
    assert_eq!(session.history().len(), 1);
    // The replaced conversation was already on screen, so its file is not read.
    assert_eq!(app.entries[0].record_reads, 0);
    let transcript = &app.entries[0].transcript;
    assert!(
        transcript
            .summary
            .as_deref()
            .is_some_and(|summary| summary.contains("fixture summary: continue the task")),
        "summary text should be captured for /summary"
    );
    assert!(transcript.text.contains("Context summary · collapsed"));
    assert!(
        !transcript
            .text
            .contains("fixture summary: continue the task")
    );
    let usage_after = session.context_usage().unwrap().estimate_tokens;
    assert!(usage_after < usage_before, "{usage_after} < {usage_before}");

    // The head of the conversation keeps the replaced user request and the
    // divider that uses the record's estimates.
    app.entries[0].scroll = Some(0);
    let view = screen(&app);
    assert!(view.contains("recorded before compaction"), "{view}");
    assert!(view.contains("start of conversation"), "{view}");
    let history_rows = app.entries[0].history_rows.as_ref().unwrap();
    assert!(
        history_rows
            .transcript
            .text
            .contains("── Compacted (manual"),
        "the divider uses the record's estimates"
    );
    // Following the bottom shows the folded summary instead of its body.
    app.entries[0].scroll = None;
    let view = screen(&app);
    assert!(view.contains("Context summary · collapsed"), "{view}");

    command(&mut app, "summary");
    let joined = info_lines(&app).join("\n");
    assert!(
        joined.contains("fixture summary: continue the task"),
        "{joined}"
    );
    assert!(
        joined.contains("The conversation before this point was compacted"),
        "{joined}"
    );
    app.event(key(KeyCode::Esc));
    assert!(app.panel.is_none());

    // Reopening reads the record once and shows the same history the live
    // view adopted from the rows on screen.
    let live = app.entries[0]
        .history_rows
        .as_ref()
        .unwrap()
        .transcript
        .text
        .clone();
    assert!(live.starts_with("start of conversation"), "{live}");
    let id = app.entries[0].id().to_owned();
    app.event(ctrl('w'));
    until(&mut app, |app| app.entries[0].session.is_none()).await;
    let mut reopened = crate::app::tests::support::app(dir.path());
    open_in_view(&mut reopened, &id).await;
    until(&mut reopened, |app| {
        app.entries[0].record_reads == 1
            && !app.entries[0].history.loading()
            && !app.entries[0].wrapping
            && app.entries[0]
                .history_rows
                .as_ref()
                .is_some_and(|rows| rows.transcript.text.starts_with("start of conversation"))
    })
    .await;
    let again = &reopened.entries[0]
        .history_rows
        .as_ref()
        .unwrap()
        .transcript
        .text;
    assert_eq!(again, &live);
}

#[test]
fn compact_command_is_offered_before_auto_compact_for_the_same_word() {
    // The exact name wins over `auto_compact`, which contains it.
    let names = crate::app::matching_commands("compact");
    assert_eq!(crate::app::COMMANDS[names[0]].0, "compact");
    let long = crate::app::matching_commands("auto_compact off");
    assert_eq!(
        crate::app::COMMANDS[long[0]].0,
        "auto_compact",
        "an argument must select the command it follows"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_compaction_inside_a_work_keeps_the_old_conversation_visible() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    // A 2,000-token trigger line, above twice the `recall` declaration every
    // persistent session carries. The earlier conversation (about 1,000
    // tokens) and the request (about 850) cross it together; the request is
    // protected, so the earlier conversation is what gets summarized.
    let mut definition = history_definition(Some(4_000));
    definition.endpoint = Some(server.endpoint.clone());
    definition.auto_compact = true;
    definition.compact_threshold = 50;
    create_in_view(
        &mut app,
        definition,
        text_context(&"existing conversation before the work ".repeat(80)),
    )
    .await;

    app.event(Event::Paste(
        "this request is long enough to trigger compaction ".repeat(50),
    ));
    app.event(ctrl('s'));
    // The summary call is a non-streaming request and answers immediately; the
    // streaming main request follows it. Waiting for the exact intermediate
    // count would race with the second request, so wait until both were read.
    until(&mut app, |_| server.requests.lock().unwrap().len() >= 2).await;
    server.release.add_permits(1);
    until(&mut app, |app| app.entries[0].execution == "Completed").await;

    // The material reload after the work commits the new record; the rows
    // that were already on screen are adopted instead of being read again.
    until(&mut app, |app| {
        app.entries[0].session.as_ref().unwrap().history().len() == 1
            && app.entries[0].rows.is_some()
            && app.entries[0].history_rows.is_some()
            && !app.entries[0].wrapping
    })
    .await;
    assert_eq!(app.entries[0].record_reads, 0);
    let history_rows = app.entries[0].history_rows.as_ref().unwrap();
    assert!(
        history_rows
            .transcript
            .text
            .contains("existing conversation before the work"),
        "old content must stay visible after an automatic compaction"
    );
    assert!(
        history_rows.transcript.text.contains("── Compacted (auto"),
        "the separator uses the progress estimate"
    );
    assert!(
        app.entries[0]
            .transcript
            .summary
            .as_deref()
            .is_some_and(|summary| summary.contains("fixture summary")),
        "the new current Context is the folded summary"
    );
    // The status bar reports the estimate against the frozen 4k window.
    let usage = app.entries[0]
        .session
        .as_ref()
        .unwrap()
        .context_usage()
        .unwrap();
    assert!(usage.estimate_tokens < 2_000, "{usage:?}");
    let view = screen(&app);
    assert!(view.contains("ctx "), "{view}");
    assert!(view.contains("/ 4k"), "{view}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_materials_view_after_successful_compacted_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;
    app.event(Event::Paste("recorded before compaction ".repeat(200)));
    app.event(ctrl('s'));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;
    server.release.add_permits(1);
    until(&mut app, |app| app.entries[0].execution == "Completed").await;
    until(&mut app, |app| {
        matches!(
            app.entries[0].session.as_ref().unwrap().status().save,
            SaveStatus::Saved { .. }
        )
    })
    .await;

    let key = app.entries[0].key.clone();
    let session = app.entries[0].session.as_ref().unwrap().clone();

    let old_view = crate::app::jobs::MaterialView::read(&session).unwrap();
    let old_version = old_view.version.clone();
    // The app loads the saved materials on its own; wait until it shows them.
    until(&mut app, |app| {
        app.entries[0].material_version.as_ref() == Some(&old_version) && !app.entries[0].refreshing
    })
    .await;

    let outcome = session.compact().await.unwrap();
    let compacted_view = crate::app::jobs::MaterialView::read(&session).unwrap();
    let compacted_version = compacted_view.version.clone();
    assert_ne!(old_version, compacted_version);

    app.complete(crate::app::jobs::Message::Compacted(
        key.clone(),
        Ok(crate::app::jobs::CompactedReload {
            outcome,
            view: compacted_view,
        }),
    ));

    assert_eq!(
        app.entries[0].material_version.as_ref(),
        Some(&compacted_version)
    );
    assert_eq!(
        app.entries[0].requested_material_version.as_ref(),
        Some(&compacted_version)
    );
    assert!(
        app.entries[0]
            .transcript
            .text
            .contains("Context summary · collapsed")
    );

    app.complete(crate::app::jobs::Message::Materials(
        key.clone(),
        Ok(old_view),
    ));

    assert_eq!(
        app.entries[0].material_version.as_ref(),
        Some(&compacted_version)
    );
    assert!(
        app.entries[0]
            .transcript
            .text
            .contains("Context summary · collapsed")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn materials_view_delivered_while_compacting_is_ignored_and_failed_compaction_reloads() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;

    until(&mut app, |app| {
        app.entries[0].material_version.is_some()
            && app.entries[0].requested_material_version.is_some()
    })
    .await;

    let key = app.entries[0].key.clone();
    let session = app.entries[0].session.as_ref().unwrap().clone();
    let initial_version = app.entries[0].material_version.clone().unwrap();

    app.entries[0].busy = Some("compacting");

    let view = crate::app::jobs::MaterialView::read(&session).unwrap();
    app.complete(crate::app::jobs::Message::Materials(key.clone(), Ok(view)));

    assert_eq!(app.entries[0].busy, Some("compacting"));
    assert_eq!(app.entries[0].requested_material_version, None);
    assert!(!app.entries[0].refreshing);

    let reads_before = app.entries[0].material_reads;

    app.complete(crate::app::jobs::Message::Compacted(
        key.clone(),
        Err("compaction failed".into()),
    ));

    assert_eq!(app.entries[0].busy, None);
    assert_eq!(app.entries[0].requested_material_version, None);

    app.poll();
    assert_eq!(
        app.entries[0].requested_material_version,
        Some(initial_version.clone())
    );
    assert_eq!(app.entries[0].material_reads, reads_before + 1);
    assert!(app.entries[0].refreshing);

    // Also verify panic path resets requested_material_version so subsequent polls read again
    let view = crate::app::jobs::MaterialView::read(&session).unwrap();
    app.entries[0].busy = Some("compacting");
    app.complete(crate::app::jobs::Message::Materials(key.clone(), Ok(view)));
    assert_eq!(app.entries[0].requested_material_version, None);

    let reads_before_panic = app.entries[0].material_reads;
    app.complete(crate::app::jobs::Message::JobPanicked(
        Some(key.clone()),
        crate::app::jobs::JobKind::Compact,
        "controlled compact panic".into(),
    ));

    assert_eq!(app.entries[0].busy, None);
    assert_eq!(app.entries[0].requested_material_version, None);

    app.poll();
    assert_eq!(
        app.entries[0].requested_material_version,
        Some(initial_version)
    );
    assert_eq!(app.entries[0].material_reads, reads_before_panic + 1);
}

fn conversation_text_without_header(entry: &crate::app::Entry) -> String {
    let mut text = String::new();
    if let Some(history) = &entry.history_rows {
        let history_text = &history.transcript.text;
        if let Some((_header, rest)) = history_text.split_once("\n\n") {
            text.push_str(rest);
        } else {
            text.push_str(history_text);
        }
    }
    text.push_str(&entry.transcript.text);
    text
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_compact_keeping_tail_displays_all_blocks_once_with_no_reads_and_reopens_identical()
{
    use causa::kernel::{BlockContent, BlockMeta, Context, ContextBlock, Role};

    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;

    // Window 4,000 and threshold 50% give line 2,000 and tail budget 500 tokens.
    let mut definition = history_definition(Some(4_000));
    definition.endpoint = Some(server.endpoint.clone());
    definition.auto_compact = false;
    definition.compact_threshold = 50;

    // Head blocks take ~800 tokens (> 500), tail blocks take ~40 tokens (<= 500).
    let m1 = format!("MARKER_USER_HEAD_ONE: {}", "head1 ".repeat(200));
    let m2 = format!("MARKER_ASST_HEAD_TWO: {}", "head2 ".repeat(200));
    let m3 = "MARKER_USER_TAIL_THREE: tail question".to_string();
    let m4 = "MARKER_ASST_TAIL_FOUR: tail answer".to_string();

    let context = Context::from_blocks(vec![
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::User, m1),
            BlockMeta::default(),
        ),
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::Assistant, m2),
            BlockMeta::default(),
        ),
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::User, m3),
            BlockMeta::default(),
        ),
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::Assistant, m4),
            BlockMeta::default(),
        ),
    ])
    .unwrap();

    create_in_view(&mut app, definition, context).await;

    command(&mut app, "compact");
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;
    until(&mut app, |app| {
        app.entries[0].busy.is_none()
            && app.entries[0].rows.is_some()
            && app.entries[0].history_rows.is_some()
            && !app.entries[0].history.loading()
            && !app.entries[0].history.has_earlier()
            && !app.entries[0].wrapping
    })
    .await;

    // 0 TUI record reads and 0 core file reads.
    assert_eq!(app.entries[0].record_reads, 0);

    // Each marker appears exactly once across history + transcript.
    let live_text = conversation_text_without_header(&app.entries[0]);
    assert_eq!(count_occurrences(&live_text, "MARKER_USER_HEAD_ONE"), 1);
    assert_eq!(count_occurrences(&live_text, "MARKER_ASST_HEAD_TWO"), 1);
    assert_eq!(count_occurrences(&live_text, "MARKER_USER_TAIL_THREE"), 1);
    assert_eq!(count_occurrences(&live_text, "MARKER_ASST_TAIL_FOUR"), 1);

    // Divider is between summarized part and tail.
    let history_text = &app.entries[0]
        .history_rows
        .as_ref()
        .unwrap()
        .transcript
        .text;
    assert!(history_text.contains("MARKER_USER_HEAD_ONE"));
    assert!(history_text.contains("MARKER_ASST_HEAD_TWO"));
    assert!(history_text.contains("── Compacted (manual"));
    assert!(!history_text.contains("MARKER_USER_TAIL_THREE"));
    assert!(!history_text.contains("MARKER_ASST_TAIL_FOUR"));

    let current_text = &app.entries[0].transcript.text;
    assert!(!current_text.contains("MARKER_USER_HEAD_ONE"));
    assert!(!current_text.contains("MARKER_ASST_HEAD_TWO"));
    assert!(current_text.contains("MARKER_USER_TAIL_THREE"));
    assert!(current_text.contains("MARKER_ASST_TAIL_FOUR"));

    // Reopen in a fresh instance, scroll up to top, assert exact match.
    let id = app.entries[0].id().to_owned();
    app.event(ctrl('w'));
    until(&mut app, |app| app.entries[0].session.is_none()).await;

    let mut reopened = crate::app::tests::support::app(dir.path());
    open_in_view(&mut reopened, &id).await;
    reopened.entries[0].scroll = Some(0);
    until(&mut reopened, |app| {
        !app.entries[0].history.loading()
            && !app.entries[0].wrapping
            && app.entries[0]
                .history_rows
                .as_ref()
                .is_some_and(|rows| rows.transcript.text.starts_with("start of conversation"))
    })
    .await;

    let reopened_text = conversation_text_without_header(&reopened.entries[0]);
    assert_eq!(reopened_text, live_text);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_compaction_inside_work_with_tool_round_protects_u_with_no_reads_and_reopens_identical()
 {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let query_arg = format!("MARKER_TOOL_QUERY: {}", "arg ".repeat(150));
    let delta = serde_json::json!({
        "role": "assistant",
        "tool_calls": [{
            "index": 0,
            "id": "call-1",
            "type": "function",
            "function": {
                "name": "recall",
                "arguments": serde_json::json!({"query": query_arg}).to_string(),
            }
        }]
    });
    let round0 = vec![
        format!(
            "data: {}\n\n",
            serde_json::json!({"id":"fixture","choices":[{"index":0,"delta":delta,"finish_reason":null}]})
        ),
        format!(
            "data: {}\n\n",
            serde_json::json!({"id":"fixture","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})
        ),
        "data: [DONE]\n\n".to_string(),
    ];
    let round1 = vec![
        format!(
            "data: {}\n\n",
            serde_json::json!({
                "id": "fixture",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "content": "MARKER_ANSWER_DONE: finished answer after tool call"
                    },
                    "finish_reason": null
                }]
            })
        ),
        "data: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_string(),
    ];
    let server = Server::with_streaming_rounds(vec![round0, round1]).await;

    let mut definition = history_definition(Some(4_000));
    definition.endpoint = Some(server.endpoint.clone());
    definition.auto_compact = true;
    definition.compact_threshold = 50;
    definition.max_model_rounds = 4;
    definition.max_tool_calls = 2;

    // Earlier context (~1,000 tokens) + request U (~200 tokens) stay under 2,000 threshold before work.
    // The tool round adds ~450 tokens, crossing the threshold and triggering auto-compaction before round 1.
    let m_earlier = format!(
        "MARKER_EARLIER_CONTEXT: {}",
        "existing conversation before the work ".repeat(80)
    );
    create_in_view(&mut app, definition, text_context(&m_earlier)).await;

    let m_req = format!(
        "MARKER_USER_REQUEST_U: {}",
        "this is user request in work ".repeat(25)
    );
    app.event(Event::Paste(m_req));
    app.event(ctrl('s'));

    // Wait until work completes
    until(&mut app, |app| app.entries[0].execution == "Completed").await;
    until(&mut app, |app| {
        app.entries[0].session.as_ref().unwrap().history().len() == 1
            && app.entries[0].rows.is_some()
            && app.entries[0].history_rows.is_some()
            && !app.entries[0].history.loading()
            && !app.entries[0].wrapping
    })
    .await;

    // 0 reads
    assert_eq!(app.entries[0].record_reads, 0);

    // Full conversation has all elements exactly once:
    // User request U, tool call, tool result, and assistant reply.
    let live_text = conversation_text_without_header(&app.entries[0]);
    assert_eq!(count_occurrences(&live_text, "MARKER_EARLIER_CONTEXT"), 1);
    assert_eq!(count_occurrences(&live_text, "MARKER_USER_REQUEST_U"), 1);
    assert_eq!(
        count_occurrences(&live_text, "Tool recall · recorded declaration"),
        1
    );
    assert_eq!(count_occurrences(&live_text, "Tool result · Succeeded"), 1);
    assert_eq!(count_occurrences(&live_text, "MARKER_ANSWER_DONE"), 1);

    // Reopen in a fresh instance, scroll up to top, assert exact match.
    let id = app.entries[0].id().to_owned();
    app.event(ctrl('w'));
    until(&mut app, |app| app.entries[0].session.is_none()).await;

    let mut reopened = crate::app::tests::support::app(dir.path());
    open_in_view(&mut reopened, &id).await;
    reopened.entries[0].scroll = Some(0);
    while reopened.entries[0].history.has_earlier() {
        reopened.entries[0].request_earlier_history();
        until(&mut reopened, |app| {
            !app.entries[0].history.loading() && !app.entries[0].wrapping
        })
        .await;
    }
    until(&mut reopened, |app| {
        app.entries[0]
            .history_rows
            .as_ref()
            .is_some_and(|rows| rows.transcript.text.starts_with("start of conversation"))
    })
    .await;

    let reopened_text = conversation_text_without_header(&reopened.entries[0]);
    assert_eq!(reopened_text, live_text);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compaction_summarizing_and_merging_displays_all_blocks_once_with_no_reads_and_reopens_identical()
 {
    use causa::kernel::{BlockContent, BlockMeta, Context, ContextBlock, Role};

    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;

    // Window 3,000 and threshold 40% gives line 1,200 and chain budget 95 tokens.
    // Fixed prefix is recall (~440 tokens) < line / 2 (600 tokens).
    // Two summaries will cause a merge because two briefs (~200 tokens) exceed 95 tokens.
    let mut definition = history_definition(Some(3_000));
    definition.endpoint = Some(server.endpoint.clone());
    definition.auto_compact = false;
    definition.compact_threshold = 40;

    let m1 = format!("MARKER_ONE: {}", "first ".repeat(100));
    let m2 = format!("MARKER_TWO: {}", "reply1 ".repeat(100));

    let context = Context::from_blocks(vec![
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::User, m1),
            BlockMeta::default(),
        ),
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::Assistant, m2),
            BlockMeta::default(),
        ),
    ])
    .unwrap();

    create_in_view(&mut app, definition, context).await;

    // First compact: produces 1 summary entry.
    command(&mut app, "compact");
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;
    until(&mut app, |app| {
        app.entries[0].busy.is_none()
            && matches!(
                app.entries[0].session.as_ref().unwrap().status().save,
                SaveStatus::Saved { .. }
            )
            && app.entries[0].session.as_ref().unwrap().history().len() == 1
            && !app.entries[0].history.loading()
            && !app.entries[0].wrapping
    })
    .await;

    // Paste additional material and run work.
    let m3 = format!("MARKER_THREE: {}", "second ".repeat(50));
    app.event(Event::Paste(m3));
    app.event(ctrl('s'));
    until(&mut app, |_| server.requests.lock().unwrap().len() >= 2).await;
    server.release.add_permits(1);
    until(&mut app, |app| app.entries[0].execution == "Completed").await;
    until(&mut app, |app| {
        matches!(
            app.entries[0].session.as_ref().unwrap().status().save,
            SaveStatus::Saved { .. }
        )
    })
    .await;

    // Second compact: produces summary and merge (2 new entries at once).
    command(&mut app, "compact");
    until(&mut app, |_| server.requests.lock().unwrap().len() >= 4).await;
    until(&mut app, |app| {
        app.entries[0].busy.is_none()
            && app.entries[0].session.as_ref().unwrap().history().len() == 3
            && !app.entries[0].history.loading()
            && !app.entries[0].wrapping
    })
    .await;

    // 0 reads
    assert_eq!(app.entries[0].record_reads, 0);

    // Full conversation has all elements exactly once
    let live_text = conversation_text_without_header(&app.entries[0]);
    assert_eq!(count_occurrences(&live_text, "MARKER_ONE"), 1);
    assert_eq!(count_occurrences(&live_text, "MARKER_TWO"), 1);
    assert_eq!(count_occurrences(&live_text, "MARKER_THREE"), 1);

    // Reopen in a fresh instance, scroll up to top, assert exact match.
    let id = app.entries[0].id().to_owned();
    app.event(ctrl('w'));
    until(&mut app, |app| app.entries[0].session.is_none()).await;

    let mut reopened = crate::app::tests::support::app(dir.path());
    open_in_view(&mut reopened, &id).await;
    reopened.entries[0].scroll = Some(0);
    while reopened.entries[0].history.has_earlier() {
        reopened.entries[0].request_earlier_history();
        until(&mut reopened, |app| {
            !app.entries[0].history.loading() && !app.entries[0].wrapping
        })
        .await;
    }
    until(&mut reopened, |app| {
        app.entries[0]
            .history_rows
            .as_ref()
            .is_some_and(|rows| rows.transcript.text.starts_with("start of conversation"))
    })
    .await;

    let reopened_text = conversation_text_without_header(&reopened.entries[0]);
    assert_eq!(reopened_text, live_text);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn summary_command_shows_all_briefs_in_chain_oldest_first() {
    use causa::kernel::{BlockContent, BlockMeta, Context, ContextBlock, Role};

    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());

    // Create session whose context directly has a chain of two briefs.
    let b0 = ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::text(Role::User, "brief 0: first summary"),
        BlockMeta::from_source(tars_core::compact::SUMMARY_SOURCE),
    );
    let b1 = ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::text(Role::User, "brief 1: second summary"),
        BlockMeta::from_source(tars_core::compact::SUMMARY_SOURCE),
    );
    let current = text_context("current active text");
    let mut blocks = vec![b0, b1];
    blocks.extend(current.blocks().iter().cloned());
    let context = Context::from_blocks(blocks).unwrap();

    create_in_view(&mut app, history_definition(None), context).await;
    command(&mut app, "summary");

    let joined = info_lines(&app).join("\n");
    assert!(
        joined.contains("brief 0: first summary\n\nbrief 1: second summary"),
        "summary panel must show full chain oldest first: {joined}"
    );
    app.event(key(KeyCode::Esc));
    assert!(app.panel.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn automatic_compaction_during_work_preserves_scroll_position_on_live_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;

    // Window 4,000 and threshold 50% gives line 2,000.
    let mut definition = history_definition(Some(4_000));
    definition.endpoint = Some(server.endpoint.clone());
    definition.auto_compact = true;
    definition.compact_threshold = 50;

    // Earlier conversation (~1,000 tokens) + request (~850 tokens) cross 2,000 line together.
    create_in_view(
        &mut app,
        definition,
        text_context(&"existing conversation line to keep in view. ".repeat(80)),
    )
    .await;

    // Submit a request that crosses threshold and triggers automatic compaction during work.
    app.event(Event::Paste(
        "this request is long enough to trigger compaction ".repeat(50),
    ));
    app.event(ctrl('s'));

    // User is scrolled up to a specific row during active work.
    let target_row = 10;
    app.entries[0].scroll = Some(target_row);

    // Get the exact line of text the user is viewing at target_row.
    let viewed_text_before = {
        let rows = app.entries[0].rows.as_ref().unwrap();
        let range = rows.ranges[target_row].clone();
        rows.transcript.text[range].to_string()
    };
    assert!(!viewed_text_before.trim().is_empty());

    until(&mut app, |_| server.requests.lock().unwrap().len() >= 2).await;

    // After compaction's rebuild has wrapped (history index grew and no re-wrap is pending)
    // while the work is still running.
    until(&mut app, |app| {
        app.entries[0].session.as_ref().unwrap().history().len() == 1 && !app.entries[0].wrapping
    })
    .await;

    // After compaction and rebuild during active work, scroll is adjusted so the viewed line remains in place.
    assert_ne!(app.entries[0].execution, "Completed");
    assert!(app.entries[0].scroll.is_some());
    let new_scroll = app.entries[0].scroll.unwrap();

    let new_history_len = app.entries[0]
        .history_rows
        .as_ref()
        .map_or(0, |r| r.ranges.len());
    let viewed_text_after = if new_scroll < new_history_len {
        let rows = app.entries[0].history_rows.as_ref().unwrap();
        let range = rows.ranges[new_scroll].clone();
        rows.transcript.text[range].to_string()
    } else {
        let rows = app.entries[0].rows.as_ref().unwrap();
        let range = rows.ranges[new_scroll - new_history_len].clone();
        rows.transcript.text[range].to_string()
    };

    assert_eq!(viewed_text_after, viewed_text_before);

    // Let work finish and assert the view returns to the bottom.
    server.release.add_permits(1);
    until(&mut app, |app| app.entries[0].execution == "Completed").await;
    assert_eq!(app.entries[0].scroll, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn material_view_with_same_index_does_not_drop_in_flight_history_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let id = write_history_session(
        &dir.path().join("store"),
        &text_context(&"current line\n".repeat(40)),
        &[
            text_context(&"history span 0\n".repeat(10)),
            text_context(&"history span 1\n".repeat(10)),
        ],
    );
    open_in_view(&mut app, &id).await;

    // Load the first earlier record to satisfy has_loaded()
    app.entries[0].scroll = Some(0);
    app.event(key(KeyCode::PageUp));
    until(&mut app, |app| {
        app.entries[0].record_reads == 1
            && !app.entries[0].history.loading()
            && !app.entries[0].wrapping
    })
    .await;

    // Set scroll position in the visible history
    app.entries[0].scroll = Some(5);
    let viewed_text_before = {
        let history_len = app.entries[0]
            .history_rows
            .as_ref()
            .map_or(0, |r| r.ranges.len());
        if 5 < history_len {
            let hr = app.entries[0].history_rows.as_ref().unwrap();
            hr.transcript.text[hr.ranges[5].clone()].to_string()
        } else {
            let r = app.entries[0].rows.as_ref().unwrap();
            r.transcript.text[r.ranges[5 - history_len].clone()].to_string()
        }
    };

    // Request earlier history again. During poll, check_history_read will set history_anchor.
    app.entries[0].history.request_earlier();
    app.poll();
    assert_eq!(app.entries[0].record_reads, 2);
    assert_eq!(app.entries[0].history_anchor, Some(Some(5)));

    // While the second read is in flight, a routine material update arrives with the same history index.
    let key = app.entries[0].key.clone();
    let session = app.entries[0].session.as_ref().unwrap().clone();
    let view = crate::app::jobs::MaterialView::read(&session).unwrap();
    app.complete(crate::app::jobs::Message::Materials(key.clone(), Ok(view)));

    // history_anchor must not have been dropped because the history index was unchanged.
    assert_eq!(app.entries[0].history_anchor, Some(Some(5)));

    // Now let the earlier read and re-wrap land.
    until(&mut app, |app| {
        app.entries[0].record_reads == 2
            && !app.entries[0].history.loading()
            && !app.entries[0].wrapping
    })
    .await;

    // The scroll position was adjusted by the prepended rows, keeping viewed rows in place.
    assert!(app.entries[0].scroll.is_some());
    let scroll_after = app.entries[0].scroll.unwrap();
    assert!(scroll_after > 5);

    let history_len_after = app.entries[0]
        .history_rows
        .as_ref()
        .map_or(0, |r| r.ranges.len());
    let viewed_text_after = if scroll_after < history_len_after {
        let hr = app.entries[0].history_rows.as_ref().unwrap();
        hr.transcript.text[hr.ranges[scroll_after].clone()].to_string()
    } else {
        let r = app.entries[0].rows.as_ref().unwrap();
        r.transcript.text[r.ranges[scroll_after - history_len_after].clone()].to_string()
    };
    assert_eq!(viewed_text_after, viewed_text_before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_wrap_after_rebuild_does_not_consume_or_apply_scroll_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    let mut def = history_definition(None);
    def.endpoint = Some(server.endpoint.clone());
    create_in_view(
        &mut app,
        def,
        text_context(&"test line 0\ntest line 1\ntest line 2\ntest line 3\n".repeat(10)),
    )
    .await;
    until(&mut app, |app| {
        !app.entries[0].wrapping && app.entries[0].rows.is_some()
    })
    .await;

    // View row 2.
    app.entries[0].scroll = Some(2);
    app.entries[0].capture_scroll_anchor();
    assert!(app.entries[0].scroll_anchor.is_some());

    // Simulate a rebuild updating the entry's transcript to a new version with prepended rows.
    let old_transcript = app.entries[0].transcript.clone();
    let old_rows = crate::transcript::Rows::wrap(old_transcript, app.view_width);
    let new_transcript = std::sync::Arc::new(crate::transcript::Transcript::from_materials(
        &text_context(
            &("prefix\n".repeat(5)
                + &"test line 0\ntest line 1\ntest line 2\ntest line 3\n".repeat(10)),
        ),
        &[],
    ));
    app.entries[0].transcript = new_transcript.clone();
    app.entries[0].rows = None;

    // A stale wrap from the previous transcript arrives.
    let key = app.entries[0].key.clone();
    app.complete(crate::app::jobs::Message::Wrapped(
        key.clone(),
        old_rows,
        None,
    ));

    // Stale wrap must NOT be accepted, and must NOT consume the scroll anchor.
    assert!(app.entries[0].rows.is_none());
    assert!(app.entries[0].scroll_anchor.is_some());

    // Now the matching wrap arrives.
    let matching_rows = crate::transcript::Rows::wrap(new_transcript, app.view_width);
    app.complete(crate::app::jobs::Message::Wrapped(
        key.clone(),
        matching_rows,
        None,
    ));

    // Matching wrap is accepted and consumes/applies the scroll anchor.
    assert!(app.entries[0].rows.is_some());
    assert!(app.entries[0].scroll_anchor.is_none());
    // Scroll was adjusted to follow the anchored rows past the prepended prefix.
    assert_eq!(app.entries[0].scroll, Some(7));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn manual_scroll_clears_pending_scroll_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    let mut def = history_definition(None);
    def.endpoint = Some(server.endpoint.clone());
    create_in_view(&mut app, def, text_context(&"test line\n".repeat(100))).await;
    until(&mut app, |app| {
        !app.entries[0].wrapping && app.entries[0].rows.is_some()
    })
    .await;

    // Test PageUp clears scroll_anchor
    app.entries[0].scroll = Some(50);
    app.entries[0].capture_scroll_anchor();
    assert!(app.entries[0].scroll_anchor.is_some());

    app.event(key(KeyCode::PageUp));
    assert!(app.entries[0].scroll_anchor.is_none());
    assert!(app.entries[0].scroll.unwrap() < 50);

    // Test PageDown clears scroll_anchor
    app.entries[0].scroll = Some(20);
    app.entries[0].capture_scroll_anchor();
    assert!(app.entries[0].scroll_anchor.is_some());

    app.event(key(KeyCode::PageDown));
    assert!(app.entries[0].scroll_anchor.is_none());
    assert!(app.entries[0].scroll.unwrap() > 20);
}

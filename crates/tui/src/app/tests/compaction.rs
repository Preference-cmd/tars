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
    // A small window and threshold make the first submitted request compact.
    let mut definition = history_definition(Some(1_000));
    definition.endpoint = Some(server.endpoint.clone());
    definition.auto_compact = true;
    definition.compact_threshold = 10;
    create_in_view(
        &mut app,
        definition,
        text_context("existing conversation before the work"),
    )
    .await;

    app.event(Event::Paste(
        "this request is long enough to trigger compaction ".repeat(100),
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
    // The status bar reports the estimate against the frozen 1k window.
    let usage = app.entries[0]
        .session
        .as_ref()
        .unwrap()
        .context_usage()
        .unwrap();
    assert!(usage.estimate_tokens < 1_000, "{usage:?}");
    let view = screen(&app);
    assert!(view.contains("ctx "), "{view}");
    assert!(view.contains("/ 1k"), "{view}");
}

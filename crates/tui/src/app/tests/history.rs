//! Incremental history loading: open, fill-screen, top-scroll and cache.

use super::support::*;
use crate::render;
use crossterm::event::KeyCode;
use ratatui::{Terminal, backend::TestBackend};

fn screen_rows(app: &crate::app::App) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(100, 50)).unwrap();
    terminal.draw(|frame| render::draw(frame, app)).unwrap();
    let buffer = terminal.backend().buffer();
    buffer
        .content()
        .chunks(usize::from(buffer.area().width))
        .map(|cells| cells.iter().map(|cell| cell.symbol()).collect())
        .collect()
}

fn screen(app: &crate::app::App) -> String {
    screen_rows(app).join("\n")
}

/// One user block whose text has `lines` short rows.
fn lines_context(prefix: &str, lines: usize) -> causa::kernel::Context {
    let mut text = String::new();
    for index in 0..lines {
        text.push_str(&format!("{prefix} {index}\n"));
    }
    text_context(&text)
}

fn history_spans() -> Vec<causa::kernel::Context> {
    (0..5)
        .map(|index| lines_context(&format!("history span {index}"), 8))
        .collect()
}

async fn settle_load(app: &mut crate::app::App, reads: usize) {
    until(app, |app| {
        app.entries[0].record_reads == reads
            && !app.entries[0].history.loading()
            && !app.entries[0].wrapping
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn opening_reads_no_record_and_top_scroll_loads_one_without_shifting_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let id = write_history_session(
        &dir.path().join("store"),
        &lines_context("current line", 40),
        &history_spans(),
    );
    open_in_view(&mut app, &id).await;

    // The current Context fills the screen, so no record file was read.
    assert_eq!(app.entries[0].record_reads, 0);
    assert_eq!(app.entries[0].history.earlier(), 5);
    assert!(screen(&app).contains("current line 39"));

    // At the top the header counts every record that is still unread.
    app.entries[0].scroll = Some(0);
    let top = screen(&app);
    assert!(top.contains("↑ 5 earlier records"), "{top}");

    // The newest record is expanded above the current Context.
    app.event(key(KeyCode::PageUp));
    settle_load(&mut app, 1).await;
    assert_eq!(app.entries[0].history.earlier(), 4);
    let after_first = screen(&app);
    assert!(after_first.contains("↑ 4 earlier records"), "{after_first}");
    assert!(after_first.contains("history span 4 7"), "{after_first}");

    // One more earlier record keeps the row being read in place: the
    // previously visible rows stay at the same screen offset.
    app.entries[0].scroll = Some(0);
    let before_rows = screen_rows(&app);
    app.event(key(KeyCode::PageUp));
    settle_load(&mut app, 2).await;
    let after_rows = screen_rows(&app);
    assert_eq!(app.entries[0].history.earlier(), 3);
    let after = after_rows.join("\n");
    // The new record plus its divider sits above the viewport, and every
    // row that was being read keeps its screen position.
    assert!(
        after.contains("── Compacted (auto, ~130k → ~8k) ──"),
        "{after}"
    );
    assert_eq!(
        &before_rows[6..],
        &after_rows[6..],
        "a loaded record must not shift rows"
    );

    // Read the rest from the top, one record per scroll.
    for reads in 3..=5 {
        app.entries[0].scroll = Some(0);
        app.event(key(KeyCode::PageUp));
        settle_load(&mut app, reads).await;
    }
    assert_eq!(app.entries[0].history.earlier(), 0);
    app.entries[0].scroll = Some(0);
    let all = screen(&app);
    assert!(all.contains("start of conversation"), "{all}");
    // The oldest span is displayed first, and every loaded span stays cached.
    assert!(all.contains("history span 0 0"), "{all}");
    let first = all.find("history span 0 0").unwrap();
    let second = all.find("history span 1 0").unwrap();
    assert!(first < second, "spans must stay in creation order");
    let cached = app.entries[0].history_rows.as_ref().unwrap();
    assert!(cached.transcript.text.contains("history span 4 0"));
    assert!(cached.transcript.text.contains("history span 2 0"));

    // Loaded records are cached: scrolling over them never reads again.
    app.entries[0].scroll = Some(200);
    let _ = screen(&app);
    app.entries[0].scroll = Some(0);
    let _ = screen(&app);
    app.event(key(KeyCode::PageUp));
    app.poll();
    assert_eq!(app.entries[0].record_reads, 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_short_current_context_loads_records_only_until_the_screen_is_full() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let id = write_history_session(
        &dir.path().join("store"),
        // One line plus the collapsed brief row of the current Context.
        &lines_context("current line", 1),
        &history_spans(),
    );
    open_in_view(&mut app, &id).await;
    until(&mut app, |app| {
        !app.entries[0].history.loading()
            && !app.entries[0].wrapping
            && app.entries[0].history.earlier() == 2
    })
    .await;

    // Three spans fill the 39-row conversation area; the other two stay unread.
    assert_eq!(app.entries[0].record_reads, 3);
    assert_eq!(app.entries[0].history.earlier(), 2);
    assert!(screen(&app).contains("current line 0"));

    // A top scroll loads only one more, and the full screen stops auto-fill.
    app.entries[0].scroll = Some(0);
    app.event(key(KeyCode::PageUp));
    settle_load(&mut app, 4).await;
    assert_eq!(app.entries[0].history.earlier(), 1);
    app.entries[0].scroll = Some(0);
    let view = screen(&app);
    assert!(view.contains("↑ 1 earlier record"), "{view}");

    app.event(key(KeyCode::PageUp));
    settle_load(&mut app, 5).await;
    assert_eq!(app.entries[0].history.earlier(), 0);
    app.entries[0].scroll = Some(0);
    let view = screen(&app);
    assert!(view.contains("start of conversation"), "{view}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_record_is_marked_and_earlier_loading_continues() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let store = dir.path().join("store");
    let id = write_history_session(&store, &lines_context("current line", 40), &history_spans());
    // The record still lists the middle record; its file is gone.
    delete_history_record(&store, &id, 2);
    open_in_view(&mut app, &id).await;
    assert_eq!(app.entries[0].record_reads, 0);

    for reads in 1..=5 {
        app.entries[0].scroll = Some(0);
        app.event(key(KeyCode::PageUp));
        settle_load(&mut app, reads).await;
        if reads == 3 {
            app.entries[0].scroll = Some(0);
            let view = screen(&app);
            assert!(
                view.contains("this history record cannot be read"),
                "{view}"
            );
            assert!(!view.contains("history span 2 0"), "{view}");
        }
    }
    assert_eq!(app.entries[0].history.earlier(), 0);
    assert_eq!(app.entries[0].record_reads, 5);
    app.entries[0].scroll = Some(0);
    let view = screen(&app);
    assert!(view.contains("start of conversation"), "{view}");
    // Segments on both sides of the unreadable one are still displayed.
    assert!(view.contains("history span 1 0"), "{view}");
    assert!(view.contains("history span 3 0"), "{view}");
}

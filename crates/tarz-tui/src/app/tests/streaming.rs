//! UI-only injected progress tests; real provider integration has a separate suite.
use super::support::{app, ctrl, key};
use crate::{
    app::{jobs::Message, state::Entry},
    preview::MAX_PREVIEW_BYTES,
    render,
    transcript::{Rows, Transcript},
};
use causa::kernel::{InvocationId, RoundId, TurnId};
use crossterm::event::{Event, KeyCode};
use ratatui::{Terminal, backend::TestBackend};
use std::sync::Arc;
use tarz_core::session::{ProgressUpdate, WorkProgress, WorkRef};
use tokio::sync::broadcast;

type Sender = broadcast::Sender<Arc<WorkProgress>>;
fn work(turn: &str) -> WorkRef {
    WorkRef {
        conversation_id: "ui-fixture".into(),
        turn_id: TurnId::new(turn),
    }
}
fn event(work: &WorkRef, round: u32, update: ProgressUpdate) -> Arc<WorkProgress> {
    Arc::new(WorkProgress {
        work: work.clone(),
        invocation_id: InvocationId {
            turn_id: work.turn_id.clone(),
            round_id: RoundId(round),
        },
        update,
    })
}
fn text(value: &str) -> ProgressUpdate {
    ProgressUpdate::Text {
        delta: value.into(),
        truncated: false,
    }
}
fn entry(turn: &str, width: u16, capacity: usize) -> (Entry, Sender) {
    let mut entry = Entry::new("ui-fixture".into(), "fixture-model".into());
    entry.work = Some(work(turn));
    entry.execution = "Running".into();
    entry.transcript = Arc::new(Transcript {
        text: "❯ You\ncommitted history\n\n".into(),
    });
    entry.rows = Some(Rows::wrap(entry.transcript.clone(), width));
    let (sender, receiver) = broadcast::channel(capacity);
    entry.bind_progress(receiver);
    (entry, sender)
}
fn screen(app: &crate::app::App, width: u16, height: u16) -> String {
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

#[test]
fn provisional_unicode_is_visible_before_completion_without_mutating_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.view_width = render::transcript_width(80);
    let (entry, sender) = entry("a", app.view_width, 128);
    let history = entry.transcript.clone();
    app.entries.push(entry);
    sender
        .send(event(&work("a"), 0, ProgressUpdate::ModelStarted))
        .unwrap();
    sender
        .send(event(
            &work("a"),
            0,
            text("你好 👩‍💻 e\u{301}\nstream\u{1b}\u{7}\r\ttext"),
        ))
        .unwrap();
    app.poll_progress();
    let rendered = screen(&app, 80, 24);
    assert!(rendered.contains("Generating · temporary"), "{rendered}");
    assert!(
        rendered.contains("你") && rendered.contains("👩‍💻"),
        "{rendered}"
    );
    assert!(
        !app.entries[0]
            .preview
            .text
            .chars()
            .any(|c| c.is_control() && c != '\n')
    );
    assert!(Arc::ptr_eq(&history, &app.entries[0].transcript));
    assert!(!history.text.contains("stream"));
    assert_eq!(app.entries[0].material_reads, 0);
    assert_eq!(app.jobs.len(), 0);
}

#[test]
fn multiple_rounds_reset_preview_and_ready_does_not_claim_commit() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let (entry, sender) = entry("a", app.view_width, 128);
    app.entries.push(entry);
    sender
        .send(event(&work("a"), 0, text("round zero")))
        .unwrap();
    sender
        .send(event(&work("a"), 0, ProgressUpdate::ModelOutputReady))
        .unwrap();
    app.poll_progress();
    assert!(app.entries[0].preview.ready);
    let rendered = screen(&app, 100, 24);
    assert!(
        rendered.contains("Temporary output · awaiting result"),
        "{rendered}"
    );
    assert_eq!(app.entries[0].execution, "Running");
    sender
        .send(event(&work("a"), 1, ProgressUpdate::ModelStarted))
        .unwrap();
    sender
        .send(event(&work("a"), 1, text("round one")))
        .unwrap();
    sender
        .send(event(&work("a"), 0, text("stale prior round")))
        .unwrap();
    app.poll_progress();
    assert_eq!(app.entries[0].preview.text, "round one");
    assert_eq!(
        app.entries[0].preview.invocation.as_ref().unwrap().round_id,
        RoundId(1)
    );
    assert!(!app.entries[0].preview.ready);
}

#[test]
fn stale_work_instance_and_completion_never_touch_current_preview() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let (entry, sender) = entry("new-work", app.view_width, 128);
    app.entries.push(entry);
    let old_key = app.entries[0].key.clone();
    sender
        .send(event(&work("old-work"), 9, text("old")))
        .unwrap();
    sender
        .send(event(&work("new-work"), 0, text("current")))
        .unwrap();
    app.poll_progress();
    assert_eq!(app.entries[0].preview.text, "current");
    app.complete(Message::Finished(
        old_key.clone(),
        work("old-work"),
        Err("old failure".into()),
    ));
    assert_eq!(app.entries[0].preview.text, "current");
    app.entries[0].key.generation += 1;
    sender
        .send(event(&work("new-work"), 0, text("old instance")))
        .unwrap();
    app.poll_progress();
    assert!(!app.entries[0].preview.is_active());
    assert!(app.entries[0].progress.is_none());
    let (new_sender, receiver) = broadcast::channel(128);
    app.entries[0].bind_progress(receiver);
    new_sender
        .send(event(&work("new-work"), 1, text("new instance")))
        .unwrap();
    app.poll_progress();
    app.complete(Message::Finished(
        old_key,
        work("new-work"),
        Err("old observation".into()),
    ));
    assert_eq!(app.entries[0].preview.text, "new instance");
}

#[test]
fn lag_event_and_preview_limits_are_visible_and_utf8_safe() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.view_width = render::transcript_width(80);
    let (entry, sender) = entry("a", app.view_width, 128);
    app.entries.push(entry);
    for _ in 0..200 {
        sender.send(event(&work("a"), 0, text("🙂"))).unwrap();
    }
    app.poll_progress();
    assert_eq!(app.entries[0].preview.lost, 72);
    assert_eq!(app.entries[0].preview.text.len(), 127 * 4);
    app.poll_progress();
    assert_eq!(app.entries[0].preview.text.len(), 128 * 4);
    for _ in 0..20 {
        sender
            .send(event(
                &work("a"),
                0,
                ProgressUpdate::Text {
                    delta: "🙂".repeat(4096),
                    truncated: true,
                },
            ))
            .unwrap();
    }
    app.poll_progress();
    let preview = &app.entries[0].preview;
    assert!(preview.truncated);
    assert!(preview.text.len() <= MAX_PREVIEW_BYTES);
    assert!(preview.text.is_char_boundary(preview.text.len()));
    let rendered = screen(&app, 80, 24);
    assert!(rendered.contains("Generating · temporary"), "{rendered}");
    assert!(rendered.contains("Preview incomplete"), "{rendered}");
    assert!(
        rendered.contains("lost") && rendered.contains("truncated"),
        "{rendered}"
    );
}

#[test]
fn one_tick_has_a_global_128_item_budget_and_rotates_between_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let (first, first_sender) = entry("a", app.view_width, 256);
    let (second, second_sender) = entry("b", app.view_width, 256);
    app.entries.extend([first, second]);
    for _ in 0..200 {
        first_sender.send(event(&work("a"), 0, text("a"))).unwrap();
        second_sender.send(event(&work("b"), 0, text("b"))).unwrap();
    }
    for expected in [128, 256, 384, 400] {
        app.poll_progress();
        assert_eq!(
            app.entries
                .iter()
                .map(|e| e.preview.text.len())
                .sum::<usize>(),
            expected
        );
    }
    assert_eq!(app.entries[1].preview.text.len(), 200);
}

#[test]
fn finished_or_closed_observation_clears_temporary_content_and_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let (entry, sender) = entry("a", app.view_width, 128);
    app.entries.push(entry);
    sender
        .send(event(&work("a"), 0, text("must disappear")))
        .unwrap();
    app.poll_progress();
    app.complete(Message::Finished(
        app.entries[0].key.clone(),
        work("a"),
        Err("cancelled fixture".into()),
    ));
    assert!(app.entries[0].preview.text.is_empty());
    assert!(app.entries[0].progress.is_none());
    assert!(!screen(&app, 80, 24).contains("must disappear"));
    // Final materials are independent of the provisional event text.
    app.entries[0].transcript = Arc::new(Transcript {
        text: "●\nactual final response\n".into(),
    });
    app.entries[0].rows = Some(Rows::wrap(
        app.entries[0].transcript.clone(),
        app.view_width,
    ));
    assert!(screen(&app, 80, 24).contains("actual final response"));
    let (sender, receiver) = broadcast::channel(128);
    app.entries[0].bind_progress(receiver);
    sender
        .send(event(&work("a"), 1, text("later preview")))
        .unwrap();
    app.poll_progress();
    drop(sender);
    app.poll_progress();
    assert!(app.entries[0].preview.text.is_empty());
    assert!(app.entries[0].progress.is_none());
}

#[test]
fn resize_scroll_and_switch_keep_draft_progress_and_do_not_submit() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.view_width = render::transcript_width(80);
    let (entry, sender) = entry("a", app.view_width, 128);
    app.entries.push(entry);
    app.entries
        .push(Entry::new("second".into(), "second-model".into()));
    app.entries[0].editor.insert_str("unsubmitted draft");
    sender
        .send(event(
            &work("a"),
            0,
            text(&"中国🙂 long streamed line ".repeat(200)),
        ))
        .unwrap();
    app.poll_progress();
    app.event(Event::Resize(40, 18));
    app.poll_progress();
    let before = app.entries[0].preview.text.clone();
    let narrow_rows = app.entries[0].conversation_len();
    app.event(key(KeyCode::PageUp));
    assert!(app.entries[0].scroll.is_some());
    assert!(!screen(&app, 40, 18).is_empty());
    app.event(key(KeyCode::F(4)));
    sender
        .send(event(&work("a"), 0, text("while hidden")))
        .unwrap();
    app.poll_progress();
    app.event(key(KeyCode::F(4)));
    app.event(Event::Resize(120, 40));
    app.poll_progress();
    assert!(app.entries[0].conversation_len() < narrow_rows);
    assert_eq!(app.entries[0].preview.text, before + "while hidden");
    assert_eq!(app.entries[0].editor.lines(), &["unsubmitted draft"]);
    assert_eq!(app.jobs.len(), 0);
    assert_eq!(app.entries[0].work.as_ref(), Some(&work("a")));
    app.event(ctrl('c'));
    assert_eq!(app.entries[0].work.as_ref(), Some(&work("a")));
}

#[test]
fn incremental_wrap_matches_full_grapheme_layout_across_split_unicode() {
    let mut preview = crate::preview::Preview::default();
    preview.start(InvocationId {
        turn_id: TurnId::new("unicode"),
        round_id: RoundId(0),
    });
    for delta in [
        "你", "好 ", "👩", "\u{200d}", "💻 ", "e", "\u{301}", "\n", "🙂xyz",
    ] {
        preview.append(delta, false);
        preview.layout(6);
        let full = Rows::wrap(
            Arc::new(Transcript {
                text: preview.text.clone(),
            }),
            6,
        );
        let visible = full.ranges.len()
            - usize::from(
                full.ranges.len() > 1 && full.ranges.last().is_some_and(std::ops::Range::is_empty),
            );
        for (index, range) in full.ranges.iter().take(visible).enumerate() {
            assert_eq!(
                preview.line(index + 1),
                &full.transcript.text[range.clone()]
            );
        }
        assert_eq!(preview.line_count(), visible + 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_submit_keeps_the_existing_progress_receiver_and_draft() {
    use super::support::{Server, create, until};
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;
    app.event(Event::Paste("first accepted work".into()));
    app.event(ctrl('s'));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;
    let work = app.entries[0].work.clone().unwrap();
    let (sender, receiver) = broadcast::channel(128);
    app.entries[0].bind_progress(receiver);
    sender
        .send(event(&work, 0, text("existing observation")))
        .unwrap();
    app.poll_progress();
    app.event(Event::Paste("second retained input".into()));
    app.event(ctrl('s'));
    assert!(app.entries[0].notice.starts_with("Input retained"));
    sender
        .send(event(&work, 0, text(" still subscribed")))
        .unwrap();
    app.poll_progress();
    assert_eq!(
        app.entries[0].preview.text,
        "existing observation still subscribed"
    );
    assert_eq!(app.entries[0].editor.lines(), &["second retained input"]);
    assert_eq!(app.entries[0].work.as_ref(), Some(&work));
    assert_eq!(server.requests.lock().unwrap().len(), 1);
    server.release.add_permits(1);
    until(&mut app, |a| a.entries[0].execution == "Completed").await;
    assert!(app.entries[0].preview.text.is_empty());
    app.quitting = true;
    until(&mut app, |a| a.done).await;
}

#[test]
fn long_combining_sequence_has_bounded_incremental_lookback() {
    let mut preview = crate::preview::Preview::default();
    preview.start(InvocationId {
        turn_id: TurnId::new("combining"),
        round_id: RoundId(0),
    });
    preview.append("e", false);
    for _ in 0..100 {
        preview.append(&"\u{301}".repeat(128), false);
        let before = preview.reflow_bytes;
        preview.layout(80);
        assert!(preview.reflow_bytes - before <= 4096 + 256 + 2);
    }
    assert_eq!(preview.text.len(), 25_601);
}

#[tokio::test]
#[ignore = "explicit isolated local streaming resource measurement"]
async fn streaming_resource_probe() {
    use std::time::Instant;
    let bytes: usize = std::env::var("TARZ_STREAMING_PROBE_BYTES")
        .unwrap()
        .parse()
        .unwrap();
    let dense = std::env::var("TARZ_STREAMING_PROBE_DENSE").is_ok_and(|v| v == "1");
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.view_width = render::transcript_width(100);
    let (mut entry, sender) = entry("probe", app.view_width, 128);
    // A large committed history should not be cloned, reread or rewrapped for
    // any preview token. Its initial layout is outside the streaming sample.
    entry.transcript = Arc::new(Transcript {
        text: "recorded history\n".repeat(1_048_576 / 17),
    });
    entry.rows = Some(Rows::wrap(entry.transcript.clone(), app.view_width));
    let history = entry.transcript.clone();
    app.entries.push(entry);
    let mut terminal = Terminal::new(TestBackend::new(100, 32)).unwrap();
    let content = if dense {
        "x".repeat(bytes)
    } else {
        let seed = "中文🙂 e\u{301} 👩‍💻 stream sample.\n";
        let mut text = seed.repeat(bytes.div_ceil(seed.len()));
        let mut end = bytes.min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text
    };
    let mut offset = 0;
    let fragment = if dense { 1 } else { 64 };
    let mut drain_ms = Vec::new();
    let mut draw_ms = Vec::new();
    let mut events = 0;
    while offset < content.len() {
        for _ in 0..128 {
            if offset == content.len() {
                break;
            }
            let mut end = (offset + fragment).min(content.len());
            while !content.is_char_boundary(end) {
                end += 1;
            }
            sender
                .send(event(&work("probe"), 0, text(&content[offset..end])))
                .unwrap();
            offset = end;
            events += 1;
        }
        let start = Instant::now();
        app.poll_progress();
        drain_ms.push(start.elapsed().as_secs_f64() * 1000.0);
        let start = Instant::now();
        terminal.draw(|frame| render::draw(frame, &app)).unwrap();
        draw_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    assert_eq!(app.entries[0].preview.text, content);
    assert!(Arc::ptr_eq(&history, &app.entries[0].transcript));
    assert_eq!(app.entries[0].material_reads, 0);
    assert!(app.jobs.is_empty());
    drain_ms.sort_by(f64::total_cmp);
    draw_ms.sort_by(f64::total_cmp);
    let p95 = (drain_ms.len() * 95 / 100).min(drain_ms.len() - 1);
    let start = Instant::now();
    app.event(Event::Resize(70, 24));
    app.poll_progress();
    let resize_ms = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "STREAMING_UI_PROBE {}",
        serde_json::json!({
            "requested_bytes": bytes, "preview_bytes": content.len(), "dense_one_byte": dense,
            "events": events, "batches":drain_ms.len(), "history_bytes":history.text.len(),
            "drain_layout_p95_ms":drain_ms[p95], "draw_p95_ms":draw_ms[p95],
            "drain_layout_max_ms":drain_ms.last(), "draw_max_ms":draw_ms.last(),
            "resize_ms":resize_ms, "cumulative_reflow_bytes":app.entries[0].preview.reflow_bytes,
            "text_capacity":app.entries[0].preview.text.capacity(), "row_index_bytes":app.entries[0].preview.index_bytes(),
            "history_additional_reads":app.entries[0].material_reads, "history_pointer_preserved": true,
        })
    );
}

#[test]
fn minimum_height_keeps_temporary_label_and_latest_preview_text_visible() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.view_width = render::transcript_width(30);
    let (entry, sender) = entry("a", app.view_width, 128);
    app.entries.push(entry);
    sender
        .send(event(
            &work("a"),
            0,
            text("a long provisional answer ending 你好🙂"),
        ))
        .unwrap();
    app.poll_progress();
    let rendered = screen(&app, 30, 12);
    assert!(rendered.contains("Generating temp"), "{rendered}");
    assert!(
        rendered.contains("你") || rendered.contains("🙂"),
        "{rendered}"
    );
}

#[test]
fn one_row_viewport_warns_about_loss_and_truncation_and_scrolls_earlier_text() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    app.view_width = render::transcript_width(30);
    let (entry, sender) = entry("a", app.view_width, 128);
    app.entries.push(entry);
    sender
        .send(event(
            &work("a"),
            0,
            ProgressUpdate::Text {
                delta: "earlier你好\nlatest🙂\n".into(),
                truncated: true,
            },
        ))
        .unwrap();
    app.poll_progress();
    app.entries[0].preview.lost = 1;
    app.event(Event::Resize(30, 12));
    let rendered = screen(&app, 30, 12);
    assert!(rendered.contains("Temp lost/cut"), "{rendered}");
    assert!(rendered.contains("🙂"), "{rendered}");
    app.event(key(KeyCode::PageUp));
    let rendered = screen(&app, 30, 12);
    assert!(rendered.contains("Temp lost/cut"), "{rendered}");
    assert!(
        rendered.contains("你") || rendered.contains("好"),
        "{rendered}"
    );
    assert!(!rendered.contains("🙂"), "{rendered}");
    app.event(key(KeyCode::PageDown));
    assert!(screen(&app, 30, 12).contains("🙂"));
}

#[test]
fn warning_headers_share_scroll_geometry_and_pages_never_skip_body_rows() {
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let (entry, sender) = entry("a", app.view_width, 128);
    app.entries.push(entry);
    sender
        .send(event(
            &work("a"),
            0,
            ProgressUpdate::Text {
                delta: (0..50).map(|i| format!("row-{i:02}\n")).collect(),
                truncated: true,
            },
        ))
        .unwrap();
    app.poll_progress();
    app.entries[0].preview.lost = 2;
    app.viewport_height = 12;
    let history = app.entries[0].rows.as_ref().unwrap().ranges.len();
    let auto = app.entries[0].viewport(12);
    assert!(auto.sticky);
    assert_eq!(auto.reserved, 2);
    assert_eq!(auto.start, app.entries[0].conversation_len() - 10);
    app.event(key(KeyCode::PageUp));
    assert_eq!(app.entries[0].scroll, Some(auto.start - 9));
    // Start at a known preview body row and walk pages to the actual bottom.
    app.entries[0].scroll = Some(history + 2);
    let mut last_end = history + 2;
    loop {
        let view = app.entries[0].viewport(12);
        assert!(
            view.start <= last_end,
            "a body row was skipped between pages"
        );
        assert_eq!(view.page, 9);
        last_end = view.start + 12 - view.reserved;
        if app.entries[0].scroll.is_none() {
            break;
        }
        app.event(key(KeyCode::PageDown));
    }
    assert!(last_end >= app.entries[0].conversation_len());
}

use super::support::{app, until};
use crate::{app::state::Entry, input::Form, render};
use causa::kernel::Context;
use ratatui::{Terminal, backend::TestBackend};
use std::time::Instant;
use tarz_core::application::{PreparedCreate, text_input};
use tarz_core::definition::CredentialRef;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "local resource measurement; explicit isolated process only"]
async fn tui_resource_probe() {
    let bytes: usize = std::env::var("TARZ_TUI_PROBE_BYTES")
        .unwrap()
        .parse()
        .unwrap();
    let count: usize = std::env::var("TARZ_TUI_PROBE_SESSIONS")
        .unwrap()
        .parse()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut app = app(dir.path());
    let mut form = Form::default();
    form.fields[1].insert_str("probe-model");
    let mut definition = form.definition().unwrap();
    definition.credential = CredentialRef::Environment("PATH".into());
    let text = "中文 👩‍💻 e\u{301} text for long terminal materials.\n";
    let start = Instant::now();
    for _ in 0..count {
        let context =
            Context::from_blocks(vec![text_input(text.repeat(bytes.div_ceil(text.len())))])
                .unwrap();
        let prepared = PreparedCreate::persistent(definition.clone(), context);
        let session = app.host.create(prepared).await.unwrap();
        let mut entry = Entry::new(
            session.target().conversation_id().as_str().to_owned(),
            "probe-model".into(),
        );
        entry.session = Some(session);
        app.entries.push(entry);
    }
    app.view_width = render::transcript_width(120);
    until(&mut app, |a| {
        a.entries[0].rows.is_some() && a.entries.iter().all(|e| e.material_version.is_some())
    })
    .await;
    let load_ms = start.elapsed().as_secs_f64() * 1000.0;
    let reads_before: usize = app.entries.iter().map(|e| e.material_reads).sum();
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    let mut draw_ms = Vec::new();
    for step in 0..30 {
        app.entries[0].scroll = Some(step * 7);
        let start = Instant::now();
        app.poll();
        terminal.draw(|frame| render::draw(frame, &app)).unwrap();
        draw_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    draw_ms.sort_by(f64::total_cmp);
    assert_eq!(
        app.entries.iter().map(|e| e.material_reads).sum::<usize>(),
        reads_before
    );
    let start = Instant::now();
    app.view_width = render::transcript_width(90);
    until(&mut app, |a| {
        a.entries[0]
            .rows
            .as_ref()
            .is_some_and(|r| r.width == a.view_width)
    })
    .await;
    let resize_ms = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "TUI_PROBE {}",
        serde_json::json!({"input_bytes_per_session":bytes,"sessions":count,"load_ms":load_ms,"cached_draw_p95_ms":draw_ms[28],"resize_layout_ms":resize_ms,"material_reads":reads_before,"cached_draw_additional_reads":0})
    );
    assert!(draw_ms[28] < 50.0, "local cached draw target exceeded");
    app.quitting = true;
    until(&mut app, |a| a.done).await;
}

use super::support::{Server, app, create, ctrl, key, until};
use crate::{
    app::{Confirmation, Panel, QueuedPrompt, state::MAX_QUEUED_PROMPTS},
    input::Form,
    render,
};
use causa::kernel::{BlockContent, BlockMeta, Context, ContextBlock, ToolCallPayload};
use crossterm::event::{Event, KeyCode};
use ratatui::{Terminal, backend::TestBackend};
use ratatui_textarea::TextArea;
use std::path::Path;
use tars_core::{
    application::{ClosePolicy, PreparedCreate, SaveStatus},
    definition::{CredentialRef, Provider, SessionDefinition},
    media::MAX_IMAGES_PER_MESSAGE,
};

fn minimal_png() -> Vec<u8> {
    let mut bytes = Vec::from(*b"\x89PNG\r\n\x1a\n");
    bytes.extend_from_slice(&[0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D', b'R']);
    bytes.extend_from_slice(&[0x00; 17]);
    bytes
}

fn minimal_jpeg() -> Vec<u8> {
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00,
    ]
}

fn minimal_webp() -> Vec<u8> {
    let mut bytes = Vec::from(*b"RIFF");
    bytes.extend_from_slice(&12u32.to_le_bytes());
    bytes.extend_from_slice(b"WEBP");
    bytes.extend_from_slice(b"VP8 ");
    bytes.extend_from_slice(&[0u8; 8]);
    bytes
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

fn command(app: &mut crate::app::App, text: &str) {
    app.event(key(KeyCode::F(1)));
    for c in text.chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Enter));
}

async fn create_with_workspace(app: &mut crate::app::App, endpoint: &str, workspace: &Path) {
    let ws = workspace.display().to_string();
    app.form = Some(Form {
        fields: [
            "openai-chat",
            "fixture-model",
            endpoint,
            "env:PATH",
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
        ..Form::default()
    });
    app.event(ctrl('s'));
    until(app, |a| a.entries[a.selected].session.is_some()).await;
}

async fn wait_idle_saved(app: &mut crate::app::App, index: usize) {
    until(app, |a| {
        a.entries.get(index).is_some_and(|entry| {
            !entry.turn_in_flight
                && entry.session.as_ref().is_some_and(|s| {
                    s.status().active_work.is_none()
                        && matches!(s.status().save, SaveStatus::Saved { .. })
                })
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mm07_slash_image_commands_and_tab_completion() {
    let dir = tempfile::tempdir().unwrap();
    let ws_dir = tempfile::tempdir().unwrap();
    let ws = ws_dir.path().canonicalize().unwrap();
    let assets = ws.join("assets");
    std::fs::create_dir(&assets).unwrap();

    let png_path = assets.join("shot.png");
    let spaced_jpg = assets.join("my photo.jpg");
    let second_jpg = assets.join("my diagram.webp");
    let fake_png = assets.join("fake.png");
    let svg_path = assets.join("vector.svg");

    std::fs::write(&png_path, minimal_png()).unwrap();
    std::fs::write(&spaced_jpg, minimal_jpeg()).unwrap();
    std::fs::write(&second_jpg, minimal_webp()).unwrap();
    std::fs::write(&fake_png, b"not actually a png").unwrap();
    std::fs::write(&svg_path, b"<svg></svg>").unwrap();

    let server = Server::new().await;
    let mut app = app(dir.path());
    create_with_workspace(&mut app, &server.endpoint, &ws).await;

    // 1. Bare `/image` when empty informs user cleanly.
    command(&mut app, "image");
    assert!(
        app.entries[0].notice.contains("No pending images attached"),
        "{}",
        app.entries[0].notice
    );
    app.event(key(KeyCode::Esc));

    // 2. Tab completion: `/im` -> `/image `, `/image r` -> `/image remove `, `/image c` -> `/image clear`.
    app.event(Event::Paste("/im".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[0].editor.lines(), &["/image "]);

    app.event(key(KeyCode::Char('r')));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[0].editor.lines(), &["/image remove "]);

    app.entries[0].editor = crate::input::editor();
    app.event(Event::Paste("/image c".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[0].editor.lines(), &["/image clear"]);

    // Counterexample: plain chat prompts starting with `image ` must NOT hijack Tab away from @path completion.
    app.entries[0].editor = crate::input::editor();
    app.event(Event::Paste("image r".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[0].editor.lines(), &["image r"]);

    app.entries[0].editor = crate::input::editor();
    app.event(Event::Paste("image @ass".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[0].editor.lines(), &["image @assets/"]);

    // Panel::Commands without leading `/` still completes `image r` -> `image remove `.
    app.entries[0].editor = crate::input::editor();
    app.event(key(KeyCode::Char('/')));
    for c in "image r".chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Tab));
    match &app.panel {
        Some(Panel::Commands { query, .. }) => {
            assert_eq!(query, "image remove ");
        }
        _ => panic!("expected Commands panel"),
    }
    app.event(key(KeyCode::Esc));

    // 3. Tab completion for `/image <partial-path>`:
    //    - `ass` -> `assets/`
    //    - `my ` -> LCP `assets/my\ ` + notice
    //    - `p` -> `assets/my\ photo.jpg` (escaped space!)
    app.entries[0].editor = crate::input::editor();
    app.event(Event::Paste("/image ass".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[0].editor.lines(), &["/image assets/"]);

    app.event(Event::Paste("my".into()));
    app.event(key(KeyCode::Tab));
    assert_eq!(app.entries[0].editor.lines(), &["/image assets/my\\ "]);
    assert!(app.entries[0].notice.contains("my photo.jpg"));

    app.event(key(KeyCode::Char('p')));
    app.event(key(KeyCode::Tab));
    assert_eq!(
        app.entries[0].editor.lines(),
        &["/image assets/my\\ photo.jpg"]
    );

    // Pressing Enter in editor after Tab-completing executes `/image` and clears editor!
    app.event(key(KeyCode::Enter));
    assert!(!app.entries[0].has_text_draft());
    assert_eq!(app.entries[0].pending_images.len(), 1);
    assert_eq!(
        app.entries[0].pending_images[0].name.as_deref(),
        Some("my photo.jpg")
    );
    assert_eq!(app.entries[0].pending_images[0].media_type, "image/jpeg");

    // 4. `/image <relative-path>` via command palette resolves relative to workspace_root.
    command(&mut app, "image assets/shot.png");
    assert_eq!(app.entries[0].pending_images.len(), 2);

    // 5. Quoted path via `/image '<path>'`.
    command(&mut app, &format!("image '{}'", second_jpg.display()));
    assert_eq!(app.entries[0].pending_images.len(), 3);

    // 6. Tab completion inside Panel::Commands also works!
    app.event(key(KeyCode::Char('/')));
    for c in "image assets/sh".chars() {
        app.event(key(KeyCode::Char(c)));
    }
    app.event(key(KeyCode::Tab));
    match &app.panel {
        Some(Panel::Commands { query, .. }) => {
            assert_eq!(query, "image assets/shot.png");
        }
        _ => panic!("expected Commands panel"),
    }
    app.event(key(KeyCode::Esc));

    // 7. `/image` (no args) lists 1-based index, name, MIME, and human size.
    command(&mut app, "image");
    match &app.panel {
        Some(Panel::Info { title, lines, .. }) => {
            assert_eq!(title, "Pending images (3)");
            let joined = lines.join("\n");
            assert!(joined.contains("1. my photo.jpg (image/jpeg,"), "{joined}");
            assert!(joined.contains("2. shot.png (image/png,"), "{joined}");
            assert!(
                joined.contains("3. my diagram.webp (image/webp,"),
                "{joined}"
            );
        }
        _ => panic!("expected Pending images panel"),
    }
    app.event(key(KeyCode::Esc));

    // 8. Counterexamples: fake `.png`, SVG, missing file, and read-only child view are rejected cleanly.
    command(&mut app, "image assets/fake.png");
    assert_eq!(app.entries[0].pending_images.len(), 3);
    assert!(app.entries[0].notice.contains("Failed to attach image"));

    command(&mut app, "image assets/vector.svg");
    assert_eq!(app.entries[0].pending_images.len(), 3);
    assert!(app.entries[0].notice.contains("Failed to attach image"));

    command(&mut app, "image assets/missing.png");
    assert_eq!(app.entries[0].pending_images.len(), 3);

    // 9. `/image remove <n>` validates 1-based index and removes cleanly.
    command(&mut app, "image remove 0");
    assert_eq!(app.entries[0].pending_images.len(), 3);
    assert!(app.entries[0].notice.contains("Usage: /image remove"));

    command(&mut app, "image remove 99");
    assert_eq!(app.entries[0].pending_images.len(), 3);
    assert!(app.entries[0].notice.contains("No pending image #99"));

    command(&mut app, "image remove 2");
    assert_eq!(app.entries[0].pending_images.len(), 2);
    assert_eq!(
        app.entries[0].pending_images[0].name.as_deref(),
        Some("my photo.jpg")
    );
    assert_eq!(
        app.entries[0].pending_images[1].name.as_deref(),
        Some("my diagram.webp")
    );

    // 10. Enforce MAX_IMAGES_PER_MESSAGE (8).
    while app.entries[0].pending_images.len() < MAX_IMAGES_PER_MESSAGE {
        command(&mut app, "image assets/shot.png");
    }
    assert_eq!(app.entries[0].pending_images.len(), MAX_IMAGES_PER_MESSAGE);
    command(&mut app, "image assets/shot.png");
    assert_eq!(app.entries[0].pending_images.len(), MAX_IMAGES_PER_MESSAGE);
    assert!(
        app.entries[0].notice.contains("max 8 images"),
        "{}",
        app.entries[0].notice
    );

    // 11. `/image clear` empties pending_images.
    command(&mut app, "image clear");
    assert!(app.entries[0].pending_images.is_empty());
    assert!(app.entries[0].notice.contains("Cleared 8"));

    let target = app.entries[0].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mm07_event_paste_image_detection_and_lossless_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let img_dir = tempfile::tempdir().unwrap();
    let spaced = img_dir.path().join("screen shot.png");
    let second = img_dir.path().join("photo two.jpg");
    let third = img_dir.path().join("clip.webp");
    let txt_file = img_dir.path().join("notes.txt");
    let svg_file = img_dir.path().join("icon.svg");

    std::fs::write(&spaced, minimal_png()).unwrap();
    std::fs::write(&second, minimal_jpeg()).unwrap();
    std::fs::write(&third, minimal_webp()).unwrap();
    std::fs::write(&txt_file, b"plain text file").unwrap();
    std::fs::write(&svg_file, b"<svg/>").unwrap();

    let server = Server::new().await;
    let mut app = app(dir.path());
    create(&mut app, &server.endpoint).await;

    let mut expected_count = 0usize;
    app.event(Event::Paste("see attached: ".into()));

    if cfg!(unix) {
        // 1. Backslash-escaped path with trailing space (macOS drag-and-drop style) attaches without modifying editor.
        let escaped = format!("{} ", spaced.display().to_string().replace(' ', "\\ "));
        app.event(Event::Paste(escaped));
        expected_count += 1;
        assert_eq!(app.entries[0].editor.lines(), &["see attached: "]);
        assert_eq!(app.entries[0].pending_images.len(), expected_count);
        assert_eq!(
            app.entries[0].pending_images[0].name.as_deref(),
            Some("screen shot.png")
        );
    }

    // 2. Multi-path drag-and-drop with single quotes and double quotes across newlines.
    let multi = format!("'{}'\n\"{}\"", second.display(), third.display());
    app.event(Event::Paste(multi));
    expected_count += 2;
    assert_eq!(app.entries[0].pending_images.len(), expected_count);
    assert_eq!(app.entries[0].editor.lines(), &["see attached: "]);

    if cfg!(unix) {
        // 3. `file://` URL with percent-encoded space attaches cleanly.
        let file_url = format!(
            "file://{}",
            spaced.display().to_string().replace(' ', "%20")
        );
        app.event(Event::Paste(file_url));
        expected_count += 1;
        assert_eq!(app.entries[0].pending_images.len(), expected_count);
    }

    // 4. Exceeding MAX_IMAGES_PER_MESSAGE when ALL pasted tokens are valid images warns without corrupting state or inserting raw paths.
    let overflow_needed = MAX_IMAGES_PER_MESSAGE + 1 - expected_count;
    let extra_images = vec![format!("'{}'", third.display()); overflow_needed].join(" ");
    app.event(Event::Paste(extra_images));
    assert_eq!(app.entries[0].pending_images.len(), expected_count);
    assert_eq!(app.entries[0].editor.lines(), &["see attached: "]);
    assert!(
        app.entries[0].notice.contains("exceeds max 8"),
        "{}",
        app.entries[0].notice
    );

    // 5. Lossless plain-text fallbacks:
    app.entries[0].editor = crate::input::editor();
    app.entries[0].pending_images.clear();

    // 5a. Mixed image path + non-image file path falls back losslessly to editor text.
    let mixed_files = format!("'{}' '{}'", spaced.display(), txt_file.display());
    app.event(Event::Paste(mixed_files.clone()));
    assert!(app.entries[0].pending_images.is_empty());
    assert_eq!(app.entries[0].editor.lines(), &[mixed_files]);

    // 5b. Mixed prose + image path falls back losslessly.
    app.entries[0].editor = crate::input::editor();
    let mixed_prose = format!("please inspect '{}'", spaced.display());
    app.event(Event::Paste(mixed_prose.clone()));
    assert!(app.entries[0].pending_images.is_empty());
    assert_eq!(app.entries[0].editor.lines(), &[mixed_prose]);

    // 5c. HTTPS URL and SVG path fall back losslessly.
    app.entries[0].editor = crate::input::editor();
    app.event(Event::Paste("https://example.com/a.png".into()));
    assert!(app.entries[0].pending_images.is_empty());
    assert_eq!(
        app.entries[0].editor.lines(),
        &["https://example.com/a.png"]
    );

    app.entries[0].editor = crate::input::editor();
    let svg_raw = svg_file.display().to_string();
    app.event(Event::Paste(svg_raw.clone()));
    assert!(app.entries[0].pending_images.is_empty());
    assert_eq!(app.entries[0].editor.lines(), &[svg_raw]);

    let target = app.entries[0].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mm07_attachment_bar_layout_height_and_render() {
    let dir = tempfile::tempdir().unwrap();
    let img_dir = tempfile::tempdir().unwrap();
    let shot = img_dir.path().join("diagram.png");
    let second = img_dir.path().join("chart.webp");
    std::fs::write(&shot, minimal_png()).unwrap();
    std::fs::write(&second, minimal_webp()).unwrap();

    let server = Server::new().await;
    let mut app = app(dir.path());
    create(&mut app, &server.endpoint).await;

    let baseline_height = render::conversation_height(24, 80, &app);

    app.event(Event::Paste(format!("'{}'", shot.display())));
    assert_eq!(app.entries[0].pending_images.len(), 1);
    assert_eq!(
        render::conversation_height(24, 80, &app),
        baseline_height - 1
    );

    app.event(Event::Paste(format!("'{}'", second.display())));
    let full = screen(&app, 100, 24);
    assert!(
        full.contains("Images: [1: diagram.png (33 B)] [2: chart.webp (24 B)]"),
        "{full}"
    );
    assert!(
        full.contains("Add a message or press Enter to send image(s)"),
        "{full}"
    );

    let narrow_bar = render::format_attachment_bar(&app.entries[0].pending_images, 20);
    assert!(narrow_bar.ends_with('…'), "{narrow_bar}");

    // Narrow small terminal with wrapped multiline draft still keeps attachment bar, input, hint, and status visible.
    app.entries[0].execution = "Completed".into();
    app.entries[0]
        .editor
        .insert_str("line one\nline two\nvisible tail");
    let narrow = screen(&app, 30, 15);
    assert!(narrow.contains("Images: [1:"), "{narrow}");
    assert!(narrow.contains("visible tail"), "{narrow}");
    assert!(narrow.contains("Enter send"), "{narrow}");
    assert!(narrow.contains("Completed · Open"), "{narrow}");

    // Clearing pending images restores original conversation height (once editor is reset to 1 line).
    command(&mut app, "image clear");
    assert_eq!(
        render::conversation_height(24, 80, &app),
        baseline_height - 2
    );
    app.entries[0].editor = crate::input::editor();
    assert_eq!(render::conversation_height(24, 80, &app), baseline_height);

    let target = app.entries[0].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mm08_text_image_and_image_only_submit_live_echo_and_wire_payload() {
    let dir = tempfile::tempdir().unwrap();
    let img_dir = tempfile::tempdir().unwrap();
    let png_path = img_dir.path().join("capture.png");
    let jpg_path = img_dir.path().join("photo.jpg");
    std::fs::write(&png_path, minimal_png()).unwrap();
    std::fs::write(&jpg_path, minimal_jpeg()).unwrap();

    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;

    // 1. Image-only turn (empty text editor, 1 pending image).
    app.event(Event::Paste(format!("'{}'", png_path.display())));
    assert!(!app.entries[0].has_text_draft());
    assert!(app.entries[0].has_draft());
    app.event(key(KeyCode::Enter));

    // Draft and pending images cleared immediately on accept.
    assert!(!app.entries[0].has_draft());
    assert!(app.entries[0].pending_images.is_empty());

    // Live transcript immediately echoes the user image badge BEFORE the model finishes!
    until(&mut app, |a| {
        server.requests.lock().unwrap().len() == 1
            && a.entries[0]
                .transcript
                .text
                .contains("[media image/png capture.png (media:sha256:")
    })
    .await;

    {
        let reqs = server.requests.lock().unwrap();
        let req_str = reqs[0].to_string();
        assert!(
            req_str.contains("data:image/png;base64,iVBORw0KGgo"),
            "{req_str}"
        );
    }

    server.release.add_permits(1);
    wait_idle_saved(&mut app, 0).await;

    // 2. Text + image turn.
    app.event(Event::Paste(format!("'{}'", jpg_path.display())));
    app.event(Event::Paste("compare with capture".into()));
    app.event(key(KeyCode::Enter));
    assert!(!app.entries[0].has_draft());
    assert!(app.entries[0].pending_images.is_empty());

    until(&mut app, |a| {
        server.requests.lock().unwrap().len() == 2
            && a.entries[0]
                .transcript
                .text
                .contains("compare with capture\n[media image/jpeg photo.jpg (media:sha256:")
    })
    .await;

    {
        let reqs = server.requests.lock().unwrap();
        let req_str = reqs[1].to_string();
        assert!(req_str.contains("compare with capture"), "{req_str}");
        assert!(req_str.contains("data:image/jpeg;base64,/9j/"), "{req_str}");
    }

    server.release.add_permits(1);
    wait_idle_saved(&mut app, 0).await;

    let target = app.entries[0].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mm08_busy_queue_with_images_preview_pop_auto_drain_and_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let img_dir = tempfile::tempdir().unwrap();
    let img_a = img_dir.path().join("a.png");
    let img_b = img_dir.path().join("b.jpg");
    std::fs::write(&img_a, minimal_png()).unwrap();
    std::fs::write(&img_b, minimal_jpeg()).unwrap();

    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;

    // Start turn 1 so the session is busy.
    app.event(Event::Paste("turn 1".into()));
    app.event(key(KeyCode::Enter));
    until(&mut app, |_| server.requests.lock().unwrap().len() == 1).await;

    // Queue 1: image-only prompt.
    app.event(Event::Paste(format!("'{}'", img_a.display())));
    app.event(key(KeyCode::Enter));
    assert!(!app.entries[0].has_draft());
    assert!(app.entries[0].pending_images.is_empty());

    // Queue 2: text + 2 images prompt.
    app.event(Event::Paste(format!(
        "'{}' '{}'",
        img_a.display(),
        img_b.display()
    )));
    app.event(Event::Paste("analyze both".into()));
    app.event(key(KeyCode::Enter));
    assert!(!app.entries[0].has_draft());
    assert!(app.entries[0].pending_images.is_empty());
    assert_eq!(app.entries[0].queued_prompts.len(), 2);

    // Queue bar in footer shows [1 image], and `/queue list` shows both previews.
    let rendered = screen(&app, 120, 40);
    assert!(rendered.contains("Queued (2): [1 image]"), "{rendered}");

    command(&mut app, "queue list");
    match &app.panel {
        Some(Panel::Info { title, lines, .. }) => {
            assert_eq!(title, "Queued prompts (2)");
            let joined = lines.join("\n");
            assert!(joined.contains("1. [1 image]"), "{joined}");
            assert!(joined.contains("2. [2 images] analyze both"), "{joined}");
        }
        _ => panic!("expected queue list panel"),
    }
    app.event(key(KeyCode::Esc));

    // Queue capacity limit protects BOTH text and pending_images when full.
    while app.entries[0].queued_prompts.len() < MAX_QUEUED_PROMPTS {
        app.entries[0]
            .queued_prompts
            .push_back(QueuedPrompt::from("filler"));
    }
    app.event(Event::Paste(format!("'{}'", img_a.display())));
    app.event(Event::Paste("overflow text".into()));
    app.event(key(KeyCode::Enter));
    assert_eq!(app.entries[0].queued_prompts.len(), MAX_QUEUED_PROMPTS);
    assert_eq!(app.entries[0].pending_images.len(), 1);
    assert_eq!(app.entries[0].editor.lines(), &["overflow text"]);

    // `/queue pop` refuses while pending_images is non-empty (`has_draft()`).
    app.entries[0].editor = crate::input::editor();
    command(&mut app, "queue pop");
    assert_eq!(app.entries[0].queued_prompts.len(), MAX_QUEUED_PROMPTS);
    assert_eq!(app.entries[0].pending_images.len(), 1);
    assert!(app.entries[0].notice.contains("clear it before /queue pop"));

    // Clear pending_images and shrink queue back to the 2 real items; pop front and re-queue.
    command(&mut app, "image clear");
    app.entries[0].queued_prompts.truncate(2);
    command(&mut app, "queue pop");
    assert!(app.entries[0].editor.lines().join("").is_empty());
    assert_eq!(app.entries[0].pending_images.len(), 1);
    assert_eq!(
        app.entries[0].pending_images[0].name.as_deref(),
        Some("a.png")
    );
    assert_eq!(app.entries[0].queued_prompts.len(), 1);

    // Pop second item fails because pending_images has 1 image, so clear and pop second item too!
    command(&mut app, "image clear");
    command(&mut app, "queue pop");
    assert_eq!(app.entries[0].editor.lines(), &["analyze both"]);
    assert_eq!(app.entries[0].pending_images.len(), 2);
    assert!(app.entries[0].queued_prompts.is_empty());

    // Re-queue it while turn 1 is still running and verify auto-drain submits BOTH images!
    app.event(key(KeyCode::Enter));
    assert_eq!(app.entries[0].queued_prompts.len(), 1);
    assert!(app.entries[0].pending_images.is_empty());

    server.release.add_permits(1);
    until(&mut app, |_| server.requests.lock().unwrap().len() == 2).await;
    assert!(app.entries[0].queued_prompts.is_empty());
    {
        let reqs = server.requests.lock().unwrap();
        let req_str = reqs[1].to_string();
        assert!(req_str.contains("analyze both"), "{req_str}");
        assert!(
            req_str.contains("data:image/png;base64,iVBORw0KGgo"),
            "{req_str}"
        );
        assert!(req_str.contains("data:image/jpeg;base64,/9j/"), "{req_str}");
    }

    server.release.add_permits(1);
    wait_idle_saved(&mut app, 0).await;

    // Rollback check: if `submit()` fails on a session needing repair, BOTH text and pending_images are preserved!
    let target = app.entries[0].session.as_ref().unwrap().target();
    app.host.close(&target, ClosePolicy::Save).await.unwrap();

    let dir2 = tempfile::tempdir().unwrap();
    let mut app2 = super::support::app(dir2.path());
    let def = SessionDefinition {
        provider: Provider::OpenAiChat,
        model: "fixture-model".into(),
        endpoint: Some(server.endpoint.clone()),
        credential: CredentialRef::Environment("PATH".into()),
        work_timeout_secs: Some(30),
        retained_work_capacity: 8,
        max_model_rounds: 1,
        max_tool_calls: 1,
        max_tokens: None,
        reasoning_effort: None,
        temperature: None,
        extra_body: Default::default(),
        max_model_attempts: None,
        render_options: Default::default(),
        workspace_root: None,
        tools: Vec::new(),
        tool_limits: Default::default(),
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
    };
    let broken_ctx = Context::from_blocks(vec![
        tars_core::application::text_input("prior"),
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::ToolCall(ToolCallPayload::new("read_file", serde_json::json!({}))),
            BlockMeta::default().with_provider_call_id("unpaired-call-1"),
        ),
    ])
    .unwrap();
    let broken_session = app2
        .host
        .create(PreparedCreate::persistent(def, broken_ctx))
        .await
        .unwrap();
    let mut broken_entry = crate::app::state::Entry::new(
        broken_session.target().conversation_id().as_str().into(),
        "fixture-model".into(),
    );
    broken_entry.session = Some(broken_session);
    app2.entries.push(broken_entry);

    app2.event(Event::Paste(format!("'{}'", img_a.display())));
    app2.event(Event::Paste("must survive failure".into()));
    app2.event(key(KeyCode::Enter));
    assert_eq!(app2.entries[0].editor.lines(), &["must survive failure"]);
    assert_eq!(app2.entries[0].pending_images.len(), 1);
    assert!(
        app2.entries[0].notice.contains("Input retained"),
        "{}",
        app2.entries[0].notice
    );

    // Also verify `try_dequeue_ready` rollback restores `QueuedPrompt` with its images intact!
    let popped_img = app2.entries[0].pending_images.pop().unwrap();
    app2.entries[0].editor = crate::input::editor();
    app2.entries[0]
        .queued_prompts
        .push_back(QueuedPrompt::new("queued rollback", vec![popped_img]));
    app2.try_dequeue_ready();
    assert!(app2.entries[0].queue_paused);
    assert_eq!(app2.entries[0].queued_prompts.len(), 1);
    assert_eq!(
        app2.entries[0].queued_prompts.front().unwrap().text,
        "queued rollback"
    );
    assert_eq!(
        app2.entries[0].queued_prompts.front().unwrap().images.len(),
        1
    );

    let target2 = app2.entries[0].session.as_ref().unwrap().target();
    app2.host.close(&target2, ClosePolicy::Save).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mm08_multi_session_isolation_and_draft_guards_for_pending_images() {
    let dir = tempfile::tempdir().unwrap();
    let img_dir = tempfile::tempdir().unwrap();
    let img = img_dir.path().join("isolated.png");
    std::fs::write(&img, minimal_png()).unwrap();

    let mut app = app(dir.path());
    let server = Server::new().await;
    create(&mut app, &server.endpoint).await;
    create(&mut app, &server.endpoint).await;

    // Attach pending image ONLY on session 0 (editor text empty).
    app.selected = 0;
    app.event(Event::Paste(format!("'{}'", img.display())));
    assert_eq!(app.entries[0].pending_images.len(), 1);
    assert!(!app.entries[0].has_text_draft());
    assert!(app.entries[0].has_draft());

    // Pressing `/` with empty text editor and non-empty pending_images STILL opens command menu!
    app.event(key(KeyCode::Char('/')));
    assert!(matches!(app.panel, Some(Panel::Commands { .. })));
    app.event(key(KeyCode::Esc));

    // Switch to session 1 via F4: session 1 has NO pending images and NO draft.
    app.event(key(KeyCode::F(4)));
    assert_eq!(app.selected, 1);
    assert!(app.entries[1].pending_images.is_empty());
    assert!(!app.entries[1].has_draft());

    // Ctrl-Q from session 1 MUST still confirm because session 0 has pending_images!
    app.event(ctrl('q'));
    assert!(matches!(
        app.confirmation,
        Some(Confirmation::Quit {
            has_drafts: true,
            has_queue: false
        })
    ));
    app.event(key(KeyCode::Char('n')));

    // Switch back to session 0: Ctrl-W triggers Close confirmation because of pending_images.
    app.event(key(KeyCode::F(4)));
    assert_eq!(app.selected, 0);
    app.event(ctrl('w'));
    assert!(matches!(
        app.confirmation,
        Some(Confirmation::Close {
            has_draft: true,
            has_queue: false
        })
    ));
    // Confirming close clears session 0's pending_images upon close completion.
    app.event(key(KeyCode::Char('y')));
    until(&mut app, |a| {
        a.entries[0].session.is_none() && a.entries[0].busy.is_none()
    })
    .await;
    assert!(app.entries[0].pending_images.is_empty());

    // Remaining session 1 closes cleanly on Ctrl-Q without confirmation.
    app.event(ctrl('q'));
    assert!(app.confirmation.is_none());
    until(&mut app, |a| a.done).await;
}

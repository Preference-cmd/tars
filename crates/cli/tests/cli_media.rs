//! Integration tests for CLI multimodal `--image` input, history media display,
//! and sidecar `export-materials` (`MM06`).

use causa::kernel::{
    BlockContent, BlockMeta, ContentPart, ContextBlock, MediaRef, Role, TextPayload, ToolOutput,
    ToolResultPayload, ToolResultStatus,
};
use causa::runtime::new_block_id;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use tars_core::media::{MAX_IMAGE_BYTES, MAX_IMAGES_PER_MESSAGE};

#[path = "common/sse.rs"]
mod sse;

const KEY: &str = "fixture-secret";

fn minimal_png() -> Vec<u8> {
    let mut bytes = vec![0x2a; 33];
    bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    bytes
}

fn minimal_jpeg() -> Vec<u8> {
    vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01,
    ]
}

fn encode_base64(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(triple >> 18) & 0x3F] as char);
        out.push(TABLE[(triple >> 12) & 0x3F] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(triple >> 6) & 0x3F] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[triple & 0x3F] as char
        } else {
            '='
        });
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

fn read_request(socket: &mut TcpStream) -> String {
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut bytes = Vec::new();
    let split = loop {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            break index + 4;
        }
    };
    let headers = String::from_utf8(bytes[..split].to_vec()).unwrap();
    let length: usize = headers
        .lines()
        .find_map(|line| {
            line.to_lowercase()
                .strip_prefix("content-length: ")
                .map(str::to_owned)
        })
        .unwrap()
        .parse()
        .unwrap();
    while bytes.len() < split + length {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
    String::from_utf8(bytes).unwrap()
}

struct Fixture {
    endpoint: String,
    requests: mpsc::Receiver<Value>,
}

impl Fixture {
    fn new(answers: Vec<&'static str>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/fixture", listener.local_addr().unwrap());
        let (tx, requests) = mpsc::channel();
        std::thread::spawn(move || {
            for answer in answers {
                let Ok(mut socket) = sse::accept_post(&listener) else {
                    return;
                };
                let raw = read_request(&mut socket);
                let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
                let json_body: Value = serde_json::from_str(body).unwrap();
                if tx.send(json_body).is_err() {
                    return;
                }
                let (content_type, payload) = sse::reply(
                    200,
                    json!({"choices":[{"message":{"role":"assistant","content":answer},"finish_reason":"stop"}]}),
                );
                let _ = write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            }
        });
        Self { endpoint, requests }
    }

    fn next_request(&self) -> Value {
        self.requests.recv_timeout(Duration::from_secs(10)).unwrap()
    }
}

fn tars_cmd() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tars"));
    cmd.env_remove("TARS_CONFIG")
        .env_remove("TARS_HOME")
        .env("TARS_TEST_KEY", KEY);
    cmd
}

fn run_cmd(endpoint: &str) -> Command {
    let mut cmd = tars_cmd();
    cmd.args([
        "run",
        "--no-config",
        "--provider",
        "openai-chat",
        "--model",
        "fixture-model",
        "--api-key-env",
        "TARS_TEST_KEY",
        "--endpoint",
        endpoint,
    ]);
    cmd
}

fn json_out(output: &Output) -> Value {
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    serde_json::from_str(stdout).unwrap_or_else(|e| panic!("invalid JSON stdout `{stdout}`: {e}"))
}

fn collect_image_urls(body: &Value) -> Vec<String> {
    let mut urls = Vec::new();
    let Some(messages) = body["messages"].as_array() else {
        return urls;
    };
    for msg in messages {
        if let Some(parts) = msg["content"].as_array() {
            for part in parts {
                if part["type"] == "image_url"
                    && let Some(url) = part["image_url"]["url"].as_str()
                {
                    urls.push(url.to_owned());
                }
            }
        }
    }
    urls
}

#[test]
fn mm06_tars_run_with_image_and_text_sends_base64_payload_and_outputs_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let png = minimal_png();
    let png_path = dir.path().join("shot.png");
    fs::write(&png_path, &png).unwrap();

    let fixture = Fixture::new(vec!["saw png json", "saw png text"]);

    // 1. JSON mode with text + image.
    let out_json = run_cmd(&fixture.endpoint)
        .args(["--input", "describe this image", "--image"])
        .arg(&png_path)
        .arg("--json")
        .output()
        .unwrap();
    assert!(out_json.status.success(), "{out_json:?}");
    let report = json_out(&out_json);
    assert_eq!(report["status"], "completed");
    assert_eq!(report["text"], "saw png json");

    let req1 = fixture.next_request();
    let expected_url = format!("data:image/png;base64,{}", encode_base64(&png));
    assert_eq!(collect_image_urls(&req1), vec![expected_url.clone()]);
    assert!(req1.to_string().contains("describe this image"));

    // 2. Standard text output mode with text + image.
    let out_text = run_cmd(&fixture.endpoint)
        .args(["--input", "plain output", "--image"])
        .arg(&png_path)
        .output()
        .unwrap();
    assert!(out_text.status.success(), "{out_text:?}");
    assert_eq!(
        std::str::from_utf8(&out_text.stdout).unwrap(),
        "saw png text\n"
    );
    let req2 = fixture.next_request();
    assert_eq!(collect_image_urls(&req2), vec![expected_url]);
}

#[test]
fn mm06_tars_run_image_only_without_text_or_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let png = minimal_png();
    let png_path = dir.path().join("only.png");
    fs::write(&png_path, &png).unwrap();

    let fixture = Fixture::new(vec![
        "image only direct",
        "image only empty stdin",
        "image and stdin text",
        "image only blank input",
    ]);
    let expected_url = format!("data:image/png;base64,{}", encode_base64(&png));

    // 1. Omit both --input and --stdin when --image is provided.
    let out1 = run_cmd(&fixture.endpoint)
        .arg("--image")
        .arg(&png_path)
        .arg("--json")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(out1.status.success(), "{out1:?}");
    assert_eq!(json_out(&out1)["text"], "image only direct");
    assert_eq!(
        collect_image_urls(&fixture.next_request()),
        vec![expected_url.clone()]
    );

    // 2. --stdin with empty EOF when --image is provided.
    let mut child = run_cmd(&fixture.endpoint)
        .args(["--stdin", "--json", "--image"])
        .arg(&png_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    let out2 = child.wait_with_output().unwrap();
    assert!(out2.status.success(), "{out2:?}");
    assert_eq!(json_out(&out2)["text"], "image only empty stdin");
    assert_eq!(
        collect_image_urls(&fixture.next_request()),
        vec![expected_url.clone()]
    );

    // 3. --stdin with non-empty text + --image.
    let mut child_text = run_cmd(&fixture.endpoint)
        .args(["--stdin", "--json", "--image"])
        .arg(&png_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child_text
        .stdin
        .take()
        .unwrap()
        .write_all(b"stdin text with image")
        .unwrap();
    let out3 = child_text.wait_with_output().unwrap();
    assert!(out3.status.success(), "{out3:?}");
    assert_eq!(json_out(&out3)["text"], "image and stdin text");
    let req3 = fixture.next_request();
    assert_eq!(collect_image_urls(&req3), vec![expected_url.clone()]);
    assert!(req3.to_string().contains("stdin text with image"));

    // 4. Blank whitespace --input with --image degrades cleanly to image-only.
    let out4 = run_cmd(&fixture.endpoint)
        .args(["--input", "   ", "--json", "--image"])
        .arg(&png_path)
        .output()
        .unwrap();
    assert!(out4.status.success(), "{out4:?}");
    assert_eq!(json_out(&out4)["text"], "image only blank input");
    assert_eq!(
        collect_image_urls(&fixture.next_request()),
        vec![expected_url]
    );
}

#[test]
fn mm06_session_run_multi_image_followup_history_and_export_materials() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let png = minimal_png();
    let jpeg = minimal_jpeg();
    let png_path = dir.path().join("diagram.png");
    let jpeg_path = dir.path().join("photo.jpg");
    fs::write(&png_path, &png).unwrap();
    fs::write(&jpeg_path, &jpeg).unwrap();

    let fixture = Fixture::new(vec!["turn 1 ok", "turn 2 ok", "turn 3 ok"]);

    // Create persistent session.
    let created = tars_cmd()
        .args([
            "session",
            "create",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            &fixture.endpoint,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(created.status.success(), "{created:?}");
    let id = json_out(&created)["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Text-only session for text-only export check.
    let text_created = tars_cmd()
        .args([
            "session",
            "create",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            &fixture.endpoint,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(text_created.status.success(), "{text_created:?}");
    let text_id = json_out(&text_created)["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Turn 1: text-only.
    let t1 = tars_cmd()
        .args([
            "session",
            "run",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--input",
            "first text turn",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(t1.status.success(), "{t1:?}");
    assert_eq!(json_out(&t1)["text"], "turn 1 ok");
    let _ = fixture.next_request();

    // Turn 2: multi-image follow-up (PNG + JPEG) with text.
    let t2 = tars_cmd()
        .args([
            "session",
            "run",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--image",
        ])
        .arg(&png_path)
        .arg("--image")
        .arg(&jpeg_path)
        .args(["--input", "compare these two", "--json"])
        .output()
        .unwrap();
    assert!(t2.status.success(), "{t2:?}");
    let t2_json = json_out(&t2);
    assert_eq!(t2_json["status"], "completed");
    assert_eq!(t2_json["saved"], true);
    assert_eq!(t2_json["text"], "turn 2 ok");

    let req2 = fixture.next_request();
    assert!(req2.to_string().contains("first text turn"));
    assert!(req2.to_string().contains("compare these two"));
    let expected_png_url = format!("data:image/png;base64,{}", encode_base64(&png));
    let expected_jpeg_url = format!("data:image/jpeg;base64,{}", encode_base64(&jpeg));
    assert_eq!(
        collect_image_urls(&req2),
        vec![expected_png_url.clone(), expected_jpeg_url.clone()]
    );

    // Turn 3: image-only follow-up on saved session.
    let t3 = tars_cmd()
        .args([
            "session",
            "run",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--image",
        ])
        .arg(&png_path)
        .arg("--json")
        .output()
        .unwrap();
    assert!(t3.status.success(), "{t3:?}");
    assert_eq!(json_out(&t3)["text"], "turn 3 ok");
    let req3 = fixture.next_request();
    assert_eq!(
        collect_image_urls(&req3),
        vec![
            expected_png_url.clone(),
            expected_jpeg_url,
            expected_png_url
        ]
    );

    // `tars session history` human-readable output displays media summaries.
    let png_ref = format!("media:sha256:{}", sha256_hex(&png));
    let jpeg_ref = format!("media:sha256:{}", sha256_hex(&jpeg));
    let hist_text = tars_cmd()
        .args([
            "session",
            "history",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(hist_text.status.success(), "{hist_text:?}");
    let hist_stdout = std::str::from_utf8(&hist_text.stdout).unwrap();
    assert!(
        hist_stdout.contains(&format!("[media image/png diagram.png ({png_ref})]")),
        "{hist_stdout}"
    );
    assert!(
        hist_stdout.contains(&format!("[media image/jpeg photo.jpg ({jpeg_ref})]")),
        "{hist_stdout}"
    );

    // `tars session history --json` preserves ContentPart::Media natively.
    let hist_json = tars_cmd()
        .args([
            "session",
            "history",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(hist_json.status.success(), "{hist_json:?}");
    let hist_json_str = std::str::from_utf8(&hist_json.stdout).unwrap();
    assert!(hist_json_str.contains(&png_ref), "{hist_json_str}");
    assert!(hist_json_str.contains(&jpeg_ref), "{hist_json_str}");
    for line in hist_json_str.lines() {
        let _: tars_core::compact::ExpandedBlock = serde_json::from_str(line).unwrap();
    }

    // `tars session export-materials <id> <out.json>` exports JSON + .media/ sidecar blobs.
    let out_pos = dir.path().join("bundle-pos.json");
    let export_pos = tars_cmd()
        .args([
            "session",
            "export-materials",
            &id,
            out_pos.to_str().unwrap(),
            "--no-config",
            "--store",
            store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(export_pos.status.success(), "{export_pos:?}");
    let export_stdout = std::str::from_utf8(&export_pos.stdout).unwrap();
    assert!(
        export_stdout.contains(out_pos.to_str().unwrap()),
        "{export_stdout}"
    );
    assert!(export_stdout.contains("2 media file(s)"), "{export_stdout}");
    let sidecar_pos = tars_core::material::media_sidecar_dir(&out_pos);
    assert!(
        export_stdout.contains(sidecar_pos.to_str().unwrap()),
        "{export_stdout}"
    );
    assert!(sidecar_pos.is_dir());
    let (loaded_ctx, loaded_blobs) = tars_core::material::load_with_media(&out_pos).unwrap();
    assert_eq!(loaded_blobs.len(), 2);
    assert_eq!(loaded_blobs.get(&png_ref).unwrap(), &png);
    assert_eq!(loaded_blobs.get(&jpeg_ref).unwrap(), &jpeg);
    assert!(!loaded_ctx.blocks().is_empty());

    // `tars session export-materials <id> --store ... --output <out2.json> --json`
    let out_flag = dir.path().join("bundle-flag.json");
    let export_flag = tars_cmd()
        .args([
            "session",
            "export-materials",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--output",
            out_flag.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(export_flag.status.success(), "{export_flag:?}");
    let export_json = json_out(&export_flag);
    assert_eq!(export_json["status"], "exported");
    assert_eq!(export_json["conversation_id"], id);
    assert_eq!(export_json["media_count"], 2);
    assert_eq!(
        export_json["media_dir"],
        tars_core::material::media_sidecar_dir(&out_flag)
            .to_str()
            .unwrap()
    );

    // Text-only session export creates no empty .media/ directory and reports 0 media.
    let text_out = dir.path().join("text-only.json");
    let export_text = tars_cmd()
        .args([
            "session",
            "export-materials",
            &text_id,
            text_out.to_str().unwrap(),
            "--no-config",
            "--store",
            store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(export_text.status.success(), "{export_text:?}");
    assert_eq!(
        std::str::from_utf8(&export_text.stdout).unwrap(),
        format!("{}\n", text_out.display())
    );
    assert!(!tars_core::material::media_sidecar_dir(&text_out).exists());

    // Refusing overwrite on existing export target leaves target and sidecar untouched.
    let before_bytes = fs::read(&out_pos).unwrap();
    let overwrite_fail = tars_cmd()
        .args([
            "session",
            "export-materials",
            &id,
            out_pos.to_str().unwrap(),
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(overwrite_fail.status.code(), Some(1), "{overwrite_fail:?}");
    assert_eq!(json_out(&overwrite_fail)["status"], "not_started");
    assert_eq!(fs::read(&out_pos).unwrap(), before_bytes);

    // Conflicting --record + conversation ID is rejected with exit code 2.
    let conflict_export = tars_cmd()
        .args([
            "session",
            "export-materials",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--record",
            out_pos.to_str().unwrap(),
            "--output",
            dir.path().join("never.json").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(conflict_export.status.code(), Some(2));
}

#[test]
fn mm06_counterexamples_invalid_spoofed_empty_and_over_limit_images() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");

    // Unused TCP listener: none of the failing invocations may send an HTTP request.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/unused", listener.local_addr().unwrap());

    let valid_png = dir.path().join("valid.png");
    fs::write(&valid_png, minimal_png()).unwrap();

    let spoofed_png = dir.path().join("spoofed.png");
    fs::write(&spoofed_png, b"<svg xmlns='http://www.w3.org/2000/svg'/>").unwrap();

    let empty_png = dir.path().join("empty.png");
    fs::write(&empty_png, b"").unwrap();

    let oversized_png = dir.path().join("oversized.png");
    let mut big = vec![0u8; MAX_IMAGE_BYTES + 1];
    big[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    fs::write(&oversized_png, &big).unwrap();

    let subdir = dir.path().join("subdir");
    fs::create_dir(&subdir).unwrap();

    let missing_png = dir.path().join("does-not-exist.png");

    // Create a persistent session for `session run` counterexamples.
    let created = tars_cmd()
        .args([
            "session",
            "create",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            &endpoint,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(created.status.success(), "{created:?}");
    let id = json_out(&created)["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // 1. Invalid / spoofed / empty / oversized / missing / directory images fail before HTTP.
    for bad_path in [
        &spoofed_png,
        &empty_png,
        &oversized_png,
        &subdir,
        &missing_png,
    ] {
        let out_run = run_cmd(&endpoint)
            .args(["--image", bad_path.to_str().unwrap(), "--json"])
            .output()
            .unwrap();
        assert!(!out_run.status.success(), "{out_run:?}");
        assert_eq!(json_out(&out_run)["status"], "not_started");

        let out_session = tars_cmd()
            .args([
                "session",
                "run",
                &id,
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "--image",
                bad_path.to_str().unwrap(),
                "--json",
            ])
            .output()
            .unwrap();
        assert!(!out_session.status.success(), "{out_session:?}");
        assert_eq!(json_out(&out_session)["status"], "not_started");
    }

    // 2. More than MAX_IMAGES_PER_MESSAGE (9 images) fails clearly before HTTP.
    let mut too_many_run = run_cmd(&endpoint);
    let mut too_many_session = tars_cmd();
    too_many_session.args([
        "session",
        "run",
        &id,
        "--no-config",
        "--store",
        store.to_str().unwrap(),
    ]);
    for _ in 0..=MAX_IMAGES_PER_MESSAGE {
        too_many_run.arg("--image").arg(&valid_png);
        too_many_session.arg("--image").arg(&valid_png);
    }
    let out_many_run = too_many_run.arg("--json").output().unwrap();
    assert!(!out_many_run.status.success(), "{out_many_run:?}");
    let stderr_run = std::str::from_utf8(&out_many_run.stderr).unwrap();
    assert!(stderr_run.contains("too many images"), "{stderr_run}");

    let out_many_session = too_many_session.arg("--json").output().unwrap();
    assert!(!out_many_session.status.success(), "{out_many_session:?}");
    let stderr_session = std::str::from_utf8(&out_many_session.stderr).unwrap();
    assert!(
        stderr_session.contains("too many images"),
        "{stderr_session}"
    );

    // 3. Empty input (no text, no --image) and blank --input without --image exit with code 2.
    for extra_args in [
        vec![],
        vec!["--input", ""],
        vec!["--input", "   "],
        vec![
            "--input",
            "hi",
            "--stdin",
            "--image",
            valid_png.to_str().unwrap(),
        ],
    ] {
        let out_run = run_cmd(&endpoint).args(&extra_args).output().unwrap();
        assert_eq!(
            out_run.status.code(),
            Some(2),
            "{extra_args:?}: {out_run:?}"
        );

        let out_session = tars_cmd()
            .args([
                "session",
                "run",
                &id,
                "--no-config",
                "--store",
                store.to_str().unwrap(),
            ])
            .args(&extra_args)
            .output()
            .unwrap();
        assert_eq!(
            out_session.status.code(),
            Some(2),
            "{extra_args:?}: {out_session:?}"
        );
    }

    // Confirm no HTTP connection ever arrived on `listener`.
    listener.set_nonblocking(true).unwrap();
    assert!(matches!(
        listener.accept(),
        Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock
    ));
}

#[test]
fn mm06_history_sanitizes_media_metadata_and_renders_tool_result_media() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");

    let created = tars_cmd()
        .args([
            "session",
            "create",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(created.status.success(), "{created:?}");
    let id = json_out(&created)["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Seed context blocks with untrusted control characters in MediaRef + ToolResult.media.
    let db_path = store.join("tars.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let context_json: String = conn
        .query_row(
            "SELECT context_json FROM agents WHERE session_id = ?1 AND agent_id = ?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap();
    let mut ctx: Value = serde_json::from_str(&context_json).unwrap();

    let user_block = ContextBlock::new(
        new_block_id(),
        BlockContent::Message {
            role: Role::User,
            parts: vec![
                ContentPart::Text(TextPayload::new("check attachment")),
                ContentPart::Media(
                    MediaRef::new("image/png", format!("media:sha256:{}", "a".repeat(64)))
                        .with_name("evil\x1b[2J\tname\n[assistant · original]\nspoof.png"),
                ),
            ],
        },
        BlockMeta::from_source("user"),
    );

    let call_id = new_block_id();
    let tool_call = ContextBlock::new(
        call_id,
        BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "mcp_cam_snap",
            json!({}),
        )),
        BlockMeta::default(),
    );
    let artifact_ref = "artifact:01933b4c-7f12-7000-8000-000000000001";
    let mut tr_payload = ToolResultPayload::new(
        call_id,
        ToolResultStatus::Succeeded,
        ToolOutput::new(json!("captured frame")),
    );
    tr_payload.media =
        vec![MediaRef::new("image/webp", artifact_ref).with_name("tool\x1b[33m_shot\nline.webp")];
    let tool_result = ContextBlock::new(
        new_block_id(),
        BlockContent::ToolResult(tr_payload),
        BlockMeta::default(),
    );

    let blocks = ctx["blocks"].as_array_mut().unwrap();
    blocks.push(serde_json::to_value(&user_block).unwrap());
    blocks.push(serde_json::to_value(&tool_call).unwrap());
    blocks.push(serde_json::to_value(&tool_result).unwrap());
    conn.execute(
        "UPDATE agents SET context_json = ?2 WHERE session_id = ?1 AND agent_id = ?1",
        rusqlite::params![&id, serde_json::to_string(&ctx).unwrap()],
    )
    .unwrap();
    drop(conn);

    // 1. Human-readable history escapes control chars / newlines and renders ToolResult.media.
    let hist = tars_cmd()
        .args([
            "session",
            "history",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(hist.status.success(), "{hist:?}");
    let stdout = std::str::from_utf8(&hist.stdout).unwrap();
    assert!(
        !stdout.contains('\x1b') && !stdout.contains('\r'),
        "raw control bytes must be escaped: {stdout:?}"
    );
    assert!(
        !stdout.lines().any(|line| line.starts_with("[assistant")),
        "newline in media_type must not inject a fake message header: {stdout}"
    );
    assert!(
        stdout.contains("evil\\x1b[2J    name\\n[assistant · original]\\nspoof.png"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "[media image/webp tool\\x1b[33m_shot\\nline.webp ({artifact_ref})]"
        )),
        "{stdout}"
    );

    // 2. JSON history remains valid and keeps native ToolResult.media.
    let hist_json = tars_cmd()
        .args([
            "session",
            "history",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(hist_json.status.success(), "{hist_json:?}");
    let json_stdout = std::str::from_utf8(&hist_json.stdout).unwrap();
    assert!(json_stdout.contains(artifact_ref), "{json_stdout}");
    for line in json_stdout.lines() {
        let _: tars_core::compact::ExpandedBlock = serde_json::from_str(line).unwrap();
    }
}

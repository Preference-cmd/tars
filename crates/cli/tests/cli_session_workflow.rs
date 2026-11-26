//! CLI integration tests for `tars session rename`, `delete`, and `fork` (`WF04`).
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

#[path = "common/sse.rs"]
mod sse;

const KEY: &str = "fixture-secret";

fn tars_cmd() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tars"));
    cmd.env_remove("TARS_CONFIG")
        .env_remove("TARS_HOME")
        .env("TARS_TEST_KEY", KEY);
    cmd
}

fn json_out(output: &Output) -> Value {
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    serde_json::from_str(stdout).unwrap_or_else(|e| panic!("invalid JSON stdout `{stdout}`: {e}"))
}

fn create_session(store: &Path, endpoint: Option<&str>, context_window: Option<&str>) -> String {
    let mut cmd = tars_cmd();
    cmd.args([
        "session",
        "create",
        "--no-config",
        "--store",
        store.to_str().unwrap(),
        "--json",
        "--provider",
        "openai-chat",
        "--model",
        "fixture-model",
        "--api-key-env",
        "TARS_TEST_KEY",
    ]);
    if let Some(ep) = endpoint {
        cmd.args(["--endpoint", ep]);
    }
    if let Some(win) = context_window {
        cmd.args(["--context-window", win]);
    }
    cmd.args(["--auto-compact", "off"]);
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{out:?}");
    json_out(&out)["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

struct Fixture {
    endpoint: String,
    requests: mpsc::Receiver<Value>,
    replies: mpsc::Sender<Reply>,
}

enum Reply {
    Stream(String),
    Summary(String),
}

impl Fixture {
    fn new(count: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/fixture", listener.local_addr().unwrap());
        let (request_tx, requests) = mpsc::channel();
        let (replies, reply_rx) = mpsc::channel::<Reply>();
        std::thread::spawn(move || {
            for _ in 0..count {
                let Ok(mut socket) = sse::accept_post(&listener) else {
                    return;
                };
                let raw = read_request(&mut socket);
                let body = raw.split_once("\r\n\r\n").map_or("", |(_, b)| b);
                if request_tx
                    .send(serde_json::from_str(body).unwrap())
                    .is_err()
                {
                    return;
                }
                let Ok(reply) = reply_rx.recv_timeout(Duration::from_secs(15)) else {
                    return;
                };
                let (content_type, payload) = match reply {
                    Reply::Stream(text) => sse::reply(
                        200,
                        json!({"choices":[{"message":{"content":text},"finish_reason":"stop"}]}),
                    ),
                    Reply::Summary(text) => (
                        "application/json",
                        json!({"choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}]})
                            .to_string(),
                    ),
                };
                let _ = write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            }
        });
        Self {
            endpoint,
            requests,
            replies,
        }
    }

    fn request(&self) -> Value {
        self.requests.recv_timeout(Duration::from_secs(15)).unwrap()
    }

    fn stream(&self, text: &str) {
        self.replies.send(Reply::Stream(text.to_owned())).unwrap();
    }

    fn summary(&self, text: &str) {
        self.replies.send(Reply::Summary(text.to_owned())).unwrap();
    }
}

fn read_request(socket: &mut TcpStream) -> String {
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
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
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap();
    while bytes.len() < split + length {
        let mut chunk = [0; 4096];
        let count = socket.read(&mut chunk).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
    String::from_utf8(bytes[..split + length].to_vec()).unwrap()
}

fn spawn_cmd(cmd: &mut Command) -> Child {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn wait_child(mut child: Child) -> Output {
    let deadline = Instant::now() + Duration::from_secs(15);
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < deadline, "child timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().unwrap()
}

#[test]
fn cli_session_rename_set_clear_json_text_and_rejections() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let id = create_session(&store, None, None);

    // 1. Set title with --json
    let out = tars_cmd()
        .args([
            "session",
            "rename",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--title",
            "  Alpha Branch  ",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let rep = json_out(&out);
    assert_eq!(rep["conversation_id"], id);
    assert_eq!(rep["title"], "Alpha Branch");
    assert_eq!(rep["revision"], 2);

    // Verify `session show --json` reflects the updated title and revision.
    let show = tars_cmd()
        .args([
            "session",
            "show",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(show.status.success(), "{show:?}");
    let show_json = json_out(&show);
    assert_eq!(show_json["title"], "Alpha Branch");
    assert_eq!(show_json["revision"], 2);

    // 2. Clear title with --clear --json
    let out = tars_cmd()
        .args([
            "session",
            "rename",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--clear",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let rep = json_out(&out);
    assert_eq!(rep["conversation_id"], id);
    assert_eq!(rep["title"], Value::Null);
    assert_eq!(rep["revision"], 3);

    // 3. Plain text output
    let out = tars_cmd()
        .args([
            "session",
            "rename",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--title",
            "Plain Text Title",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = std::str::from_utf8(&out.stdout).unwrap();
    assert!(stdout.contains("Plain Text Title"), "{stdout}");
    assert!(stdout.contains("revision 4"), "{stdout}");

    // 4. Argument errors: conflict (--title + --clear) and missing both -> exit 2
    let conflict = tars_cmd()
        .args([
            "session",
            "rename",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--title",
            "X",
            "--clear",
        ])
        .output()
        .unwrap();
    assert_eq!(conflict.status.code(), Some(2));

    let missing_flag = tars_cmd()
        .args([
            "session",
            "rename",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(missing_flag.status.code(), Some(2));

    // 5. Invalid titles, nonexistent UUID, and invalid UUID -> exit 1
    let too_long = "x".repeat(161);
    for bad in ["", "   ", "bad\ntitle", "bad\x1btitle", &too_long] {
        let res = tars_cmd()
            .args([
                "session",
                "rename",
                &id,
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "--title",
                bad,
                "--json",
            ])
            .output()
            .unwrap();
        assert_eq!(res.status.code(), Some(1), "bad title `{bad:?}`: {res:?}");
    }

    let missing_id = "00000000-0000-0000-0000-000000000099";
    let res = tars_cmd()
        .args([
            "session",
            "rename",
            missing_id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--title",
            "Nope",
        ])
        .output()
        .unwrap();
    assert_eq!(res.status.code(), Some(1));

    let bad_uuid = tars_cmd()
        .args([
            "session",
            "rename",
            "not-a-uuid",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--title",
            "Nope",
        ])
        .output()
        .unwrap();
    assert_eq!(bad_uuid.status.code(), Some(1));

    // 6. `session fork` argument & title error paths
    let fork_missing_id = tars_cmd().args(["session", "fork"]).output().unwrap();
    assert_eq!(fork_missing_id.status.code(), Some(2));

    let fork_missing_positional_with_flags = tars_cmd()
        .args([
            "session",
            "fork",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!fork_missing_positional_with_flags.status.success());

    let fork_bad_uuid = tars_cmd()
        .args([
            "session",
            "fork",
            "not-a-uuid",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(fork_bad_uuid.status.code(), Some(1));

    for blank in ["", "   "] {
        let fork_blank_title = tars_cmd()
            .args([
                "session",
                "fork",
                &id,
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "--title",
                blank,
                "--json",
            ])
            .output()
            .unwrap();
        assert_eq!(fork_blank_title.status.code(), Some(1));
    }
}

#[test]
fn cli_session_fork_with_compacted_history_survives_source_delete() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let fixture = Fixture::new(2);
    let id = create_session(&store, Some(&fixture.endpoint), Some("8192"));

    // Run one turn large enough that manual compaction replaces it with a brief.
    let long_input = format!("first turn message {}", "m".repeat(3_000));
    let child = spawn_cmd(tars_cmd().args([
        "session",
        "run",
        &id,
        "--no-config",
        "--store",
        store.to_str().unwrap(),
        "--input",
        &long_input,
        "--json",
    ]));
    let _ = fixture.request();
    fixture.stream("model reply");
    let out = wait_child(child);
    assert!(out.status.success(), "{out:?}");

    // Compact the session into a history record.
    let compact_child = spawn_cmd(tars_cmd().args([
        "session",
        "compact",
        &id,
        "--no-config",
        "--store",
        store.to_str().unwrap(),
        "--json",
    ]));
    let _ = fixture.request();
    fixture.summary("Compacted goal and progress summary.");
    let compact_out = wait_child(compact_child);
    assert!(compact_out.status.success(), "{compact_out:?}");
    assert_eq!(json_out(&compact_out)["status"], "compacted");

    // Rename source session so derived fork title is predictable.
    let ren = tars_cmd()
        .args([
            "session",
            "rename",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--title",
            "Parent",
        ])
        .output()
        .unwrap();
    assert!(ren.status.success(), "{ren:?}");

    // Fork with derived title (--json).
    let fork_out = tars_cmd()
        .args([
            "session",
            "fork",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(fork_out.status.success(), "{fork_out:?}");
    let fork_json = json_out(&fork_out);
    assert_eq!(fork_json["source_conversation_id"], id);
    let forked_id = fork_json["conversation_id"].as_str().unwrap().to_owned();
    assert_ne!(forked_id, id);
    assert_eq!(fork_json["title"], "Parent (fork)");
    assert_eq!(fork_json["revision"], 1);

    // Fork with explicit title (text output).
    let fork_text = tars_cmd()
        .args([
            "session",
            "fork",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--title",
            "Custom Fork",
        ])
        .output()
        .unwrap();
    assert!(fork_text.status.success(), "{fork_text:?}");
    let stdout = std::str::from_utf8(&fork_text.stdout).unwrap();
    assert!(stdout.contains("Custom Fork"), "{stdout}");
    assert!(stdout.contains("revision 1"), "{stdout}");

    // Delete the source session.
    let del = tars_cmd()
        .args([
            "session",
            "delete",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(del.status.success(), "{del:?}");
    let del_json = json_out(&del);
    assert_eq!(del_json["conversation_id"], id);
    assert_eq!(del_json["deleted"], true);

    // `session show` and `session history` on the forked session must succeed after source deletion!
    let show = tars_cmd()
        .args([
            "session",
            "show",
            &forked_id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(show.status.success(), "{show:?}");
    let show_json = json_out(&show);
    assert_eq!(show_json["title"], "Parent (fork)");
    assert_eq!(show_json["revision"], 1);
    assert_eq!(show_json["history_records"], 1);

    let history = tars_cmd()
        .args([
            "session",
            "history",
            &forked_id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(history.status.success(), "{history:?}");
    let hist_stdout = std::str::from_utf8(&history.stdout).unwrap();
    assert!(hist_stdout.contains("first turn message"), "{hist_stdout}");
}

#[test]
fn cli_session_delete_and_cross_process_lock_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let fixture = Fixture::new(1);
    let id = create_session(&store, Some(&fixture.endpoint), None);

    // Hold the session OS lock in a separate `tars session run` process.
    let child = spawn_cmd(tars_cmd().args([
        "session",
        "run",
        &id,
        "--no-config",
        "--store",
        store.to_str().unwrap(),
        "--input",
        "hold lock",
        "--json",
    ]));
    let _ = fixture.request();

    for sub in [
        vec!["delete", &id],
        vec!["rename", &id, "--title", "Locked"],
        vec!["fork", &id],
    ] {
        let mut cmd = tars_cmd();
        cmd.arg("session");
        cmd.args(&sub);
        cmd.args(["--no-config", "--store", store.to_str().unwrap(), "--json"]);
        let res = cmd.output().unwrap();
        assert_eq!(
            res.status.code(),
            Some(1),
            "expected exit 1 for {sub:?}: {res:?}"
        );
    }

    // Finish the locked run cleanly.
    fixture.stream("finished");
    let run_out = wait_child(child);
    assert!(run_out.status.success(), "{run_out:?}");

    // Delete in plain text succeeds now that lock is released.
    let del = tars_cmd()
        .args([
            "session",
            "delete",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(del.status.success(), "{del:?}");
    let del_stdout = std::str::from_utf8(&del.stdout).unwrap();
    assert!(
        del_stdout.contains(&format!("Deleted session {id}")),
        "{del_stdout}"
    );

    // Deleting again (nonexistent) or invalid UUID exits 1.
    let missing = tars_cmd()
        .args([
            "session",
            "delete",
            &id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));

    let invalid = tars_cmd()
        .args([
            "session",
            "delete",
            "not-a-uuid",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(1));
}

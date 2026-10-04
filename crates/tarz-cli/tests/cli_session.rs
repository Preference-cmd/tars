//! Actual persistent CLI processes, with local HTTP and request barriers.
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

const KEY: &str = "fixture-secret";

#[path = "common/sse.rs"]
mod sse;

#[test]
fn global_profile_changes_do_not_replace_a_reopened_definition() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(1);
    let config_path = dir.path().join("config.toml");
    let config = format!(
        "version = 1\ndefault_profile = 'local'\n[profiles.local]\nprovider = 'openai-chat'\nmodel = 'original-profile-model'\napi_key_env = 'TARZ_TEST_KEY'\nendpoint = '{}'\n",
        fixture.endpoint
    );
    fs::write(&config_path, &config).unwrap();
    let output = command(dir.path(), "create")
        .arg("--config")
        .arg(&config_path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let id = report(&output)["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    fs::write(
        &config_path,
        config.replace("original-profile-model", "changed-profile-model"),
    )
    .unwrap();
    let child = spawn(
        run(dir.path(), &id, "use saved definition")
            .arg("--config")
            .arg(&config_path),
    );
    let request = fixture.request();
    assert!(request.contains("original-profile-model"));
    assert!(!request.contains("changed-profile-model"));
    fixture.answer("saved definition result");
    let output = finished(child);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(report(&output)["text"], "saved definition result");
}

struct Fixture {
    endpoint: String,
    requests: mpsc::Receiver<String>,
    responses: mpsc::Sender<(u16, Value)>,
}
impl Fixture {
    fn new(count: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/fixture", listener.local_addr().unwrap());
        let (request_tx, requests) = mpsc::channel();
        let (responses, response_rx) = mpsc::channel::<(u16, Value)>();
        std::thread::spawn(move || {
            for _ in 0..count {
                let (mut socket, _) = listener.accept().unwrap();
                let request = read_request(&mut socket);
                if request_tx.send(request).is_err() {
                    return;
                }
                let Ok((status, body)) = response_rx.recv_timeout(Duration::from_secs(10)) else {
                    return;
                };
                let (content_type, body) = sse::reply(status, body);
                let _ = write!(
                    socket,
                    "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self {
            endpoint,
            requests,
            responses,
        }
    }
    fn request(&self) -> String {
        self.requests.recv_timeout(Duration::from_secs(5)).unwrap()
    }
    fn answer(&self, text: &str) {
        self.responses.send((200, json!({"choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}]}))).unwrap();
    }
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
fn command(store: &Path, operation: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tarz"));
    command.args(["session", operation]);
    if operation != "run" {
        command.arg("--store").arg(store).arg("--json");
    }
    command.env("TARZ_TEST_KEY", KEY);
    command
}
fn create(store: &Path, endpoint: &str, timeout: &str) -> String {
    let output = command(store, "create")
        .args([
            "--provider",
            "openai-chat",
            "--model",
            "saved-fixture-model",
            "--api-key-env",
            "TARZ_TEST_KEY",
            "--endpoint",
            endpoint,
            "--timeout-secs",
            timeout,
            "--capacity",
            "7",
            "--max-model-rounds",
            "3",
            "--max-tool-calls",
            "0",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value = report(&output);
    assert_eq!(value["status"], "created");
    assert_eq!(value["saved"], true);
    assert_eq!(value["revision"], 1);
    value["conversation_id"].as_str().unwrap().to_owned()
}
fn run(store: &Path, id: &str, input: &str) -> Command {
    let mut command = command(store, "run");
    command
        .arg(id)
        .arg("--store")
        .arg(store)
        .args(["--input", input, "--json"]);
    command
}
fn spawn(command: &mut Command) -> Child {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}
fn finished(mut child: Child) -> Output {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("CLI did not finish within 10 seconds: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn report(output: &Output) -> Value {
    assert!(!String::from_utf8_lossy(&output.stdout).contains(KEY));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(KEY));
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("{output:?}"))
}
fn record(store: &Path, id: &str) -> Value {
    let bytes = fs::read(store.join("sessions").join(format!("{id}.json"))).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains(KEY));
    serde_json::from_slice(&bytes).unwrap()
}
fn body(request: &str) -> Value {
    serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
}

#[test]
fn create_t1_close_reopen_t2_preserve_definition_identity_and_block_ids() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(2);
    // Responses are queued so an unexpected create-time request cannot hang.
    fixture.answer("first answer");
    let id = create(&store, &fixture.endpoint, "10");
    assert!(
        fixture
            .requests
            .recv_timeout(Duration::from_millis(100))
            .is_err(),
        "create called model"
    );
    let original = record(&store, &id);
    assert!(original["context"]["blocks"].as_array().unwrap().is_empty());
    assert_eq!(
        original["definition"]["credential"],
        json!({"environment":"TARZ_TEST_KEY"})
    );
    let first = finished(spawn(&mut run(&store, &id, "T1 input")));
    assert!(first.status.success(), "{first:?}");
    let first_report = report(&first);
    assert_eq!(first_report["conversation_id"], id);
    assert_eq!(first_report["status"], "completed");
    assert_eq!(first_report["text"], "first answer");
    assert_eq!(first_report["saved"], true);
    assert_eq!(first_report["revision"], 2);
    let first_request = fixture.request();
    assert!(first_request.contains("authorization: Bearer fixture-secret"));
    assert_eq!(body(&first_request)["model"], "saved-fixture-model");
    let first_record = record(&store, &id);
    assert_eq!(first_record["definition"], original["definition"]);
    let first_blocks = first_record["context"]["blocks"].as_array().unwrap();
    assert_eq!(first_blocks.len(), 2);
    let invalid = run(&store, &id, "attempt override")
        .args(["--model", "different"])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(fixture.requests.try_recv().is_err());
    fixture.answer("second answer");
    let second = finished(spawn(
        run(&store, &id, "T2 input").env("TARZ_TEST_KEY", "reopened-fixture-key"),
    ));
    assert!(second.status.success(), "{second:?}");
    let second_report = report(&second);
    assert_eq!(second_report["conversation_id"], id);
    assert_eq!(second_report["revision"], 3);
    assert_eq!(second_report["saved"], true);
    assert_ne!(first_report["work_id"], second_report["work_id"]);
    let second_request = fixture.request();
    assert!(second_request.contains("authorization: Bearer reopened-fixture-key"));
    let second_body = body(&second_request);
    assert_eq!(second_body["model"], "saved-fixture-model");
    assert!(second_body.to_string().contains("T1 input"));
    assert!(second_body.to_string().contains("first answer"));
    assert!(second_body.to_string().contains("T2 input"));
    let second_record = record(&store, &id);
    assert_eq!(second_record["definition"], original["definition"]);
    let second_blocks = second_record["context"]["blocks"].as_array().unwrap();
    assert_eq!(second_blocks.len(), 4);
    assert_eq!(&second_blocks[..2], first_blocks);
    assert_eq!(second_record.as_object().unwrap().len(), 6); // No request keys/results/execution archive.
    let listed = command(&store, "list").output().unwrap();
    assert!(listed.status.success());
    let listed = report(&listed);
    assert_eq!(listed["sessions"][0]["active"], false);
    assert_eq!(listed["sessions"][0]["revision"], 3);
}

#[test]
fn list_keeps_good_record_and_reports_corruption_without_counting_pending_files() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let id = create(&store, "http://127.0.0.1:1/unused", "10");
    let bad = "00000000-0000-0000-0000-000000000001";
    fs::write(
        store.join("sessions").join(format!("{bad}.json")),
        b"broken JSON",
    )
    .unwrap();
    fs::write(
        store
            .join("sessions")
            .join(format!(".{id}.pending-half.json")),
        b"half JSON",
    )
    .unwrap();
    let output = command(&store, "list").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let listing = report(&output);
    let entries = listing["sessions"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let good = entries
        .iter()
        .find(|entry| entry["conversation_id"] == id)
        .unwrap();
    assert_eq!(good["revision"], 1);
    assert_eq!(good["active"], false);
    assert!(good["record_error"].is_null());
    let bad = entries
        .iter()
        .find(|entry| entry["conversation_id"] == bad)
        .unwrap();
    assert!(
        bad["record_error"]
            .as_str()
            .unwrap()
            .contains("invalid session JSON")
    );
    assert!(bad["revision"].is_null());
    let rejected = run(
        &store,
        bad["conversation_id"].as_str().unwrap(),
        "must not execute",
    )
    .output()
    .unwrap();
    assert_eq!(rejected.status.code(), Some(1));
    assert_eq!(report(&rejected)["status"], "not_started");
}

#[test]
fn real_running_process_excludes_second_process_and_list_reports_active() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(1);
    let id = create(&store, &fixture.endpoint, "10");
    let child = spawn(&mut run(&store, &id, "owner input"));
    fixture.request();
    let listed = command(&store, "list").output().unwrap();
    assert!(listed.status.success());
    assert_eq!(report(&listed)["sessions"][0]["active"], true);
    let rejected = run(&store, &id, "second process input").output().unwrap();
    assert_eq!(rejected.status.code(), Some(1), "{rejected:?}");
    let rejected = report(&rejected);
    assert_eq!(rejected["status"], "not_started");
    assert!(rejected["work_id"].is_null());
    assert!(rejected["error"].as_str().unwrap().contains("occupied"));
    assert!(fixture.requests.try_recv().is_err());
    fixture.answer("owner answer");
    let output = finished(child);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(report(&output)["saved"], true);
    assert_eq!(
        record(&store, &id)["context"]["blocks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn failed_model_and_timeout_keep_input_and_confirm_saved_revision() {
    for (timeout, reply, code, status) in [
        ("10", Some((401, json!({"error":KEY}))), 1, "failed"),
        ("1", None, 124, "timed_out"),
    ] {
        let folder = tempfile::tempdir().unwrap();
        let store = folder.path().join("store");
        let fixture = Fixture::new(1);
        let id = create(&store, &fixture.endpoint, timeout);
        let child = spawn(&mut run(&store, &id, "keep actual input"));
        fixture.request();
        if let Some(reply) = reply {
            fixture.responses.send(reply).unwrap();
        }
        let output = finished(child);
        assert_eq!(output.status.code(), Some(code), "{output:?}");
        let result = report(&output);
        assert_eq!(result["status"], status);
        assert_eq!(result["saved"], true);
        assert_eq!(result["revision"], 2);
        assert!(result["save_error"].is_null());
        let saved = record(&store, &id);
        assert_eq!(saved["context"]["blocks"].as_array().unwrap().len(), 1);
        assert!(saved.to_string().contains("keep actual input"));
    }
}

#[cfg(unix)]
#[test]
fn ctrl_c_still_saves_materials_and_closes_before_process_exit() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(1);
    let id = create(&store, &fixture.endpoint, "10");
    let child = spawn(&mut run(&store, &id, "cancel actual input"));
    fixture.request();
    assert!(
        Command::new("kill")
            .args(["-s", "INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let output = finished(child);
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    let result = report(&output);
    assert_eq!(result["status"], "cancelled");
    assert_eq!(result["saved"], true);
    assert_eq!(result["revision"], 2);
    assert_eq!(
        record(&store, &id)["context"]["blocks"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let listed = command(&store, "list").output().unwrap();
    assert_eq!(report(&listed)["sessions"][0]["active"], false);
}

#[test]
fn completed_work_save_failure_exports_actual_recovery_and_reports_export_failure_separately() {
    for export_succeeds in [true, false] {
        let folder = tempfile::tempdir().unwrap();
        let store = folder.path().join("store");
        let backup = folder.path().join("store-before-loss");
        let fixture = Fixture::new(1);
        let id = create(&store, &fixture.endpoint, "10");
        let baseline = record(&store, &id);
        let recovery = folder.path().join(if export_succeeds {
            "recovery"
        } else {
            "absent/recovery"
        });
        if export_succeeds {
            fs::create_dir(&recovery).unwrap();
        }
        let child = spawn(
            run(&store, &id, "unsaved actual input")
                .arg("--recovery-dir")
                .arg(&recovery),
        );
        fixture.request();
        // No other owner is installed at the old path. The live Writer now
        // points at a missing record/directory and cannot confirm its baseline.
        fs::rename(&store, &backup).unwrap();
        fixture.answer("completed but save failed");
        let output = finished(child);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        let result = report(&output);
        assert_eq!(result["status"], "completed");
        assert_eq!(result["text"], "completed but save failed");
        assert_eq!(result["saved"], false);
        assert_eq!(result["error"], "session save failed");
        assert!(result["execution_error"].is_null());
        assert!(result["save_error"].as_str().is_some());
        assert!(result["revision"].is_null());
        let close = &result["close_outcome"];
        assert_eq!(close["policy"], "discard_local");
        assert_eq!(close["pending_commit"], "unresolved");
        assert_eq!(close["discarded_unconfirmed"], true);
        assert!(close["saved_revision"].is_null());
        assert!(close["pending_revision"].is_null());
        assert!(close["confirmation_error"].as_str().is_some());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unconfirmed"));
        assert_eq!(record(&backup, &id), baseline); // No fabricated successful save or rollback.
        assert!(!store.exists());
        if export_succeeds {
            assert!(result["recovery_error"].is_null());
            let path = Path::new(result["recovery_file"].as_str().unwrap());
            assert_eq!(path.parent().unwrap(), recovery);
            let material = tarz_core::material::load(path).unwrap();
            assert_eq!(material.blocks().len(), 2);
            let serialized = serde_json::to_string(&material).unwrap();
            assert!(serialized.contains("unsaved actual input"));
            assert!(serialized.contains("completed but save failed"));
            assert!(!serialized.contains(KEY));
            assert_eq!(fs::read_dir(&recovery).unwrap().count(), 1);
        } else {
            assert!(result["recovery_file"].is_null());
            assert_eq!(result["recovery_error"], "recovery material export failed");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("recovery material export failed")
            );
            assert!(!recovery.exists());
        }
    }
}

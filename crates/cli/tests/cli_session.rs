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
        "version = 1\ndefault_profile = 'local'\n[profiles.local]\nprovider = 'openai-chat'\nmodel = 'original-profile-model'\napi_key_env = 'TARS_TEST_KEY'\nendpoint = '{}'\n",
        fixture.endpoint
    );
    fs::write(&config_path, &config).unwrap();
    let output = tars_bin()
        .args(["session", "create", "--store"])
        .arg(dir.path())
        .args(["--json", "--config"])
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
        tars_bin()
            .args(["session", "run", &id, "--store"])
            .arg(dir.path())
            .args(["--input", "use saved definition", "--json", "--config"])
            .arg(&config_path),
    );
    let request = fixture.request();
    let (_, body) = request.split_once("\r\n\r\n").unwrap();
    let body_json: Value = serde_json::from_str(body).unwrap();
    assert_eq!(body_json["model"], "original-profile-model");
    assert_ne!(body_json["model"], "changed-profile-model");
    fixture.answer("saved definition result");
    let output = finished(child);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(report(&output)["text"], "saved definition result");
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn session_create_reports_the_full_command_sandbox_line_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let store = dir.path().join("store");
    let output = command(&store, "create")
        .args([
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--tools",
            "run_command",
            "--workspace",
        ])
        .arg(&workspace)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let expected = if cfg!(target_os = "macos") {
        "TARS: command sandbox: seatbelt: file read full; file write full; network full; \
         signal isolation full; descendant lifecycle partial ("
    } else {
        "TARS: command sandbox: bwrap: file read full; file write full; network full; \
         signal isolation full; descendant lifecycle full"
    };
    assert!(stderr.contains(expected), "{stderr}");
    assert_eq!(report(&output)["status"], "created");
}

/// A failed probe at session creation must still open the session fail-closed
/// and report the unavailable sandbox through the same `session create` path.
/// Removing `HOME` makes the probe fail (credential denials need a home
/// directory) without a production injection seam.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn session_create_reports_an_unavailable_command_sandbox_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let store = dir.path().join("store");
    let output = command(&store, "create")
        .args([
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--tools",
            "run_command",
            "--workspace",
        ])
        .arg(&workspace)
        .env_remove("HOME")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let backend = if cfg!(target_os = "macos") {
        "seatbelt"
    } else {
        "bwrap"
    };
    let expected = format!(
        "TARS: command sandbox: {backend} sandbox is unavailable: HOME is not set; \
         credential denials require a home directory"
    );
    assert!(stderr.contains(&expected), "{stderr}");
    assert_eq!(report(&output)["status"], "created");
}

struct Fixture {
    endpoint: String,
    requests: mpsc::Receiver<String>,
    responses: mpsc::Sender<(u16, Value)>,
    answered: mpsc::Receiver<()>,
}
impl Fixture {
    fn new(count: usize) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/fixture", listener.local_addr().unwrap());
        let (request_tx, requests) = mpsc::channel();
        let (responses, response_rx) = mpsc::channel::<(u16, Value)>();
        let (answered_tx, answered) = mpsc::channel();
        std::thread::spawn(move || {
            for _ in 0..count {
                let mut socket = sse::accept_post(&listener).unwrap();
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
                let _ = socket.flush();
                drop(socket);
                let _ = answered_tx.send(());
            }
        });
        Self {
            endpoint,
            requests,
            responses,
            answered,
        }
    }
    fn request(&self) -> String {
        self.requests.recv_timeout(Duration::from_secs(5)).unwrap()
    }
    fn answer(&self, text: &str) {
        self.responses.send((200, json!({"choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}]}))).unwrap();
    }
    #[cfg(unix)]
    fn wait_answered(&self) {
        self.answered.recv_timeout(Duration::from_secs(5)).unwrap();
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
fn tars_bin() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tars"));
    command
        .env_remove("TARS_CONFIG")
        .env_remove("TARS_HOME")
        .env("TARS_TEST_KEY", KEY);
    command
}

fn command(store: &Path, operation: &str) -> Command {
    let mut command = tars_bin();
    command.args(["session", operation]);
    if operation != "run" {
        command
            .arg("--no-config")
            .arg("--store")
            .arg(store)
            .arg("--json");
    }
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
            "TARS_TEST_KEY",
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
        .arg("--no-config")
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
    struct DbRow {
        revision: i64,
        last_commit_id: String,
        policy_json: String,
        title: Option<String>,
        permission_mode: Option<String>,
        auto_compact: Option<i64>,
        compact_threshold: Option<i64>,
        usage_json: Option<String>,
        updated_at_ms: i64,
        def_json: String,
        ctx_json: String,
        history_json: String,
    }
    let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
    let row = conn
        .query_row(
            "SELECT
                s.revision,
                s.last_commit_id,
                s.policy_json,
                s.title,
                s.permission_mode,
                s.auto_compact,
                s.compact_threshold,
                s.usage_json,
                s.updated_at_ms,
                a.definition_json,
                a.context_json,
                a.history_index_json
             FROM sessions s
             JOIN agents a
               ON s.session_id = a.session_id
              AND s.entry_agent_id = a.agent_id
             WHERE s.session_id = ?1",
            rusqlite::params![id],
            |row| {
                Ok(DbRow {
                    revision: row.get(0)?,
                    last_commit_id: row.get(1)?,
                    policy_json: row.get(2)?,
                    title: row.get(3)?,
                    permission_mode: row.get(4)?,
                    auto_compact: row.get(5)?,
                    compact_threshold: row.get(6)?,
                    usage_json: row.get(7)?,
                    updated_at_ms: row.get(8)?,
                    def_json: row.get(9)?,
                    ctx_json: row.get(10)?,
                    history_json: row.get(11)?,
                })
            },
        )
        .unwrap();
    assert!(!row.def_json.contains(KEY) && !row.ctx_json.contains(KEY));
    let policy: Value = serde_json::from_str(&row.policy_json).unwrap();
    let mut obj = serde_json::Map::new();
    obj.insert("format".into(), policy["format"].clone());
    obj.insert("conversation_id".into(), json!(id));
    obj.insert("revision".into(), json!(row.revision));
    obj.insert("last_commit_id".into(), json!(row.last_commit_id));
    obj.insert(
        "definition".into(),
        serde_json::from_str(&row.def_json).unwrap(),
    );
    obj.insert(
        "context".into(),
        serde_json::from_str(&row.ctx_json).unwrap(),
    );
    if let Some(title) = row.title {
        obj.insert("title".into(), json!(title));
    }
    obj.insert("updated_at_ms".into(), json!(row.updated_at_ms));
    if let Some(mode) = row.permission_mode {
        obj.insert("permission_mode".into(), json!(mode));
    }
    if let Some(auto) = row.auto_compact {
        obj.insert("auto_compact".into(), json!(auto != 0));
    }
    if let Some(thresh) = row.compact_threshold {
        obj.insert("compact_threshold".into(), json!(thresh));
    }
    let history: Vec<Value> = serde_json::from_str(&row.history_json).unwrap();
    if !history.is_empty() {
        obj.insert("history".into(), Value::Array(history));
    }
    if let Some(usage) = row.usage_json {
        obj.insert("usage".into(), serde_json::from_str(&usage).unwrap());
    }
    Value::Object(obj)
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
    let original_blocks = original["context"]["blocks"].as_array().unwrap();
    assert_eq!(original_blocks.len(), 3);
    assert_eq!(original_blocks[0]["meta"]["source"], "tars:prelude:base");
    assert_eq!(original_blocks[1]["meta"]["source"], "tars:prelude:agents");
    assert_eq!(original_blocks[2]["meta"]["source"], "tars:prelude:env");
    assert_eq!(
        original["definition"]["credential"],
        json!({"environment":"TARS_TEST_KEY"})
    );
    let first = finished(spawn(&mut run(&store, &id, "T1 input")));
    assert!(first.status.success(), "{first:?}");
    let first_report = report(&first);
    assert_eq!(first_report["conversation_id"], id);
    assert_eq!(first_report["status"], "completed");
    assert_eq!(first_report["kind"], "completed");
    assert_eq!(first_report["text"], "first answer");
    assert_eq!(first_report["saved"], true);
    assert_eq!(first_report["revision"], 2);
    let first_request = fixture.request();
    assert!(first_request.contains("authorization: Bearer fixture-secret"));
    assert_eq!(body(&first_request)["model"], "saved-fixture-model");
    let first_record = record(&store, &id);
    assert_eq!(first_record["definition"], original["definition"]);
    let first_blocks = first_record["context"]["blocks"].as_array().unwrap();
    assert_eq!(first_blocks.len(), 6);
    assert_eq!(&first_blocks[..3], original_blocks);
    let invalid = run(&store, &id, "attempt override")
        .args(["--model", "different"])
        .output()
        .unwrap();
    assert_eq!(invalid.status.code(), Some(2));
    assert!(fixture.requests.try_recv().is_err());
    fixture.answer("second answer");
    let second = finished(spawn(
        run(&store, &id, "T2 input").env("TARS_TEST_KEY", "reopened-fixture-key"),
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
    assert_eq!(second_blocks.len(), 8);
    assert_eq!(&second_blocks[..6], first_blocks);
    assert!(second_record.get("usage").is_some());
    assert_eq!(second_record.as_object().unwrap().len(), 9); // No request keys/results/execution archive.
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
    let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
    conn.execute(
        "INSERT INTO sessions (session_id, revision, last_commit_id, policy_json, entry_agent_id, created_at_ms, updated_at_ms)
         VALUES (?1, 1, ?1, '{\"format\":\"tars-session-v2.4\"}', ?1, 1000, 1000)",
        rusqlite::params![bad],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO agents (session_id, agent_id, name, preset_name, interaction, definition_json, material_revision, context_json, history_index_json, created_at_ms, updated_at_ms)
         VALUES (?1, ?1, 'main', NULL, 'reusable', '{}', 1, 'broken JSON', '[]', 1000, 1000)",
        rusqlite::params![bad],
    )
    .unwrap();
    drop(conn);
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
        6
    );
}

#[test]
fn failed_model_and_timeout_keep_input_and_confirm_saved_revision() {
    for (timeout, reply, code, status, kind) in [
        (
            "10",
            Some((401, json!({"error":KEY}))),
            1,
            "failed",
            "model_failed",
        ),
        ("1", None, 124, "timed_out", "timed_out"),
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
        assert_eq!(result["kind"], kind);
        assert_eq!(result["saved"], true);
        assert_eq!(result["revision"], 2);
        assert!(result["save_error"].is_null());
        let saved = record(&store, &id);
        assert_eq!(saved["context"]["blocks"].as_array().unwrap().len(), 5);
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
    assert_eq!(result["kind"], "cancelled");
    assert_eq!(result["saved"], true);
    assert_eq!(result["revision"], 2);
    assert_eq!(
        record(&store, &id)["context"]["blocks"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    let listed = command(&store, "list").output().unwrap();
    assert_eq!(report(&listed)["sessions"][0]["active"], false);
}

#[test]
fn completed_work_save_failure_exports_actual_recovery_and_reports_export_failure_separately() {
    for export_succeeds in [true, false] {
        let folder = tempfile::tempdir().unwrap();
        let store = folder.path().join("store");
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
        // Corrupt policy_json in tars.db so both pre-write read and DiscardLocal
        // confirmation fail with StoreError without losing the baseline row.
        let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
        let orig_policy: String = conn
            .query_row(
                "SELECT policy_json FROM sessions WHERE session_id = ?1",
                rusqlite::params![&id],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "UPDATE sessions SET policy_json = 'corrupt' WHERE session_id = ?1",
            rusqlite::params![&id],
        )
        .unwrap();
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
        conn.execute(
            "UPDATE sessions SET policy_json = ?1 WHERE session_id = ?2",
            rusqlite::params![orig_policy, &id],
        )
        .unwrap();
        drop(conn);
        assert_eq!(record(&store, &id), baseline); // No fabricated successful save or rollback.
        if export_succeeds {
            assert!(result["recovery_error"].is_null());
            let path = Path::new(result["recovery_file"].as_str().unwrap());
            assert_eq!(path.parent().unwrap(), recovery);
            let material = tars_core::material::load(path).unwrap();
            assert_eq!(material.blocks().len(), 6);
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

#[test]
fn reopening_a_saved_full_access_mode_reports_the_downgrade() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(1);
    let id = create(&store, &fixture.endpoint, "10");
    fixture.answer("full access answer");
    let run = finished(spawn(
        run(&store, &id, "set full access")
            .arg("--permission-mode")
            .arg("full-access"),
    ));
    assert!(run.status.success(), "{run:?}");
    let saved = record(&store, &id);
    assert_eq!(saved["permission_mode"], "full-access");

    let repaired = repair(&store, &id);
    assert!(repaired.status.success(), "{repaired:?}");
    assert!(
        String::from_utf8_lossy(&repaired.stderr).contains("downgraded from full-access to ask")
    );
    let repaired_report = report(&repaired);
    assert!(repaired_report.get("permission_downgraded").is_none());

    // A definition created with full-access as its initial mode is not a downgrade.
    let initial = tars_bin()
        .args([
            "session",
            "create",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--json",
            "--provider",
            "openai-chat",
            "--model",
            "initial-full-access",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--permission-mode",
            "full-access",
        ])
        .output()
        .unwrap();
    assert!(initial.status.success(), "{initial:?}");
    let initial_id = report(&initial)["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let repaired = repair(&store, &initial_id);
    assert!(repaired.status.success(), "{repaired:?}");
    assert!(
        !String::from_utf8_lossy(&repaired.stderr).contains("downgraded from full-access to ask")
    );
}

fn repair(store: &Path, id: &str) -> Output {
    tars_bin()
        .args(["session", "repair", id, "--no-config", "--store"])
        .arg(store)
        .args(["--dry-run", "--json"])
        .output()
        .unwrap()
}

#[cfg(unix)]
#[test]
fn second_sigint_abandons_a_blocked_save_and_exports_importable_recovery() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let recovery = folder.path().join("recovery");
    std::fs::create_dir(&recovery).unwrap();
    let fixture = Fixture::new(1);
    let id = create(&store, &fixture.endpoint, "10");
    let (mut child, stderr) = capture_stderr(spawn(
        run(&store, &id, "blocked save input")
            .arg("--recovery-dir")
            .arg(&recovery),
    ));
    fixture.request();
    let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
    fixture.answer("blocked but returned answer");
    fixture.wait_answered();
    // Allow the child to parse the completed HTTP response and enter the blocked
    // SQLite commit before sending the first SIGINT, so SIGINT tests blocked-save
    // handling rather than cancelling the in-flight model turn.
    std::thread::sleep(Duration::from_millis(250));
    interrupt_until_prompt(&child, &stderr);
    assert!(
        child.try_wait().unwrap().is_none(),
        "first SIGINT must not exit"
    );
    interrupt(&child);
    let output = finished(child);
    let _ = conn.execute_batch("ROLLBACK;");
    drop(conn);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let result = report(&output);
    assert_eq!(result["save_wait_abandoned"], true);
    assert_eq!(result["saved"], false);
    assert_eq!(result["recovery_importable"], true);
    let path = Path::new(result["recovery_file"].as_str().unwrap());
    assert_eq!(path.parent().unwrap(), recovery);
    let materials = tars_core::material::load(path).unwrap();
    assert_eq!(materials.blocks().len(), 6);

    let imported_store = folder.path().join("imported");
    let imported = tars_bin()
        .args([
            "session",
            "create",
            "--no-config",
            "--store",
            imported_store.to_str().unwrap(),
            "--json",
            "--provider",
            "openai-chat",
            "--model",
            "imported-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--load",
        ])
        .arg(path)
        .output()
        .unwrap();
    assert!(imported.status.success(), "{imported:?}");
}

#[cfg(unix)]
#[test]
fn one_sigint_during_a_blocked_save_waits_and_normal_completion_creates_no_recovery() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let recovery = folder.path().join("recovery");
    std::fs::create_dir(&recovery).unwrap();
    let fixture = Fixture::new(1);
    let id = create(&store, &fixture.endpoint, "10");
    let (mut child, stderr) = capture_stderr(spawn(
        run(&store, &id, "normally saved input")
            .arg("--recovery-dir")
            .arg(&recovery),
    ));
    fixture.request();
    let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE;").unwrap();
    fixture.answer("normally saved answer");
    fixture.wait_answered();
    std::thread::sleep(Duration::from_millis(250));
    interrupt_until_prompt(&child, &stderr);
    assert!(
        child.try_wait().unwrap().is_none(),
        "one SIGINT must not exit"
    );
    // Release SQLite's write lock only after the prompt proves the wait is live.
    conn.execute_batch("ROLLBACK;").unwrap();
    drop(conn);

    let output = finished(child);
    assert!(output.status.success(), "{output:?}");
    let result = report(&output);
    assert!(result["recovery_file"].is_null());
    assert_eq!(std::fs::read_dir(&recovery).unwrap().count(), 0);
    assert_eq!(record(&store, &id)["revision"], 2);
}

#[cfg(unix)]
fn capture_stderr(mut child: Child) -> (Child, mpsc::Receiver<String>) {
    use std::io::BufRead;
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (child, rx)
}

#[cfg(unix)]
fn interrupt_until_prompt(child: &Child, stderr: &mpsc::Receiver<String>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        interrupt(child);
        if let Ok(line) = stderr.recv_timeout(Duration::from_secs(1))
            && line.contains("press Ctrl-C again")
        {
            return;
        }
    }
    panic!("saving prompt did not appear");
}

#[cfg(unix)]
fn interrupt(child: &Child) {
    assert!(
        Command::new("kill")
            .args(["-s", "INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn create_initial_save_failure_exports_recovery_to_the_selected_directory() {
    use causa::kernel::{BlockContent, Context, ContextBlock, Role};

    let folder = tempfile::tempdir().unwrap();
    let material = folder.path().join("large-material.json");
    let context = Context::from_blocks(vec![ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::text(
            Role::User,
            "x".repeat(tars_core::material::MAX_IMPORT_BYTES - 64 * 1024),
        ),
        Default::default(),
    )])
    .unwrap();
    let mut file = std::fs::File::create(&material).unwrap();
    serde_json::to_writer(&mut file, &context).unwrap();
    drop(file);
    drop(context);
    assert!(
        std::fs::metadata(&material).unwrap().len() <= tars_core::material::MAX_IMPORT_BYTES as u64
    );

    let recovery = folder.path().join("initial-recovery");
    std::fs::create_dir(&recovery).unwrap();
    // The material is below the record limit; the configured definition
    // padding pushes the complete session record over it at commit time.
    // Use a config file rather than a 128 KiB argv entry, which exceeds
    // Linux MAX_ARG_STRLEN and the Windows command-line limit.
    let config_path = folder.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "version = 1\ndefault_profile = 'p'\n[profiles.p]\nprovider = 'openai-chat'\nmodel = 'oversized-record'\napi_key_env = 'TARS_TEST_KEY'\ntimeout_secs = 10\n[profiles.p.extra_body]\npad = \"{}\"\n",
            "p".repeat(128 * 1024)
        ),
    )
    .unwrap();
    let output = tars_bin()
        .args([
            "session",
            "create",
            "--store",
            folder.path().join("store").to_str().unwrap(),
            "--json",
            "--config",
        ])
        .arg(&config_path)
        .arg("--load")
        .arg(&material)
        .arg("--recovery-dir")
        .arg(&recovery)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let value = report(&output);
    assert_eq!(value["status"], "not_started", "{value}");
    assert_eq!(value["error"], "initial session save failed", "{value}");
    assert_eq!(value["recovery_importable"], true, "{value}");
    let path = Path::new(value["recovery_file"].as_str().unwrap());
    assert_eq!(path.parent().unwrap(), recovery);
    let restored = tars_core::material::load(path).unwrap();
    assert_eq!(restored.blocks().len(), 4);
}

#[test]
fn session_run_reports_required_mcp_assembly_failure_with_server_and_hint() {
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("store");
    let config = directory.path().join("config.toml");
    std::fs::write(&config, "version = 1\nrecovery_dir = '.'\n").unwrap();
    let mcp = directory.path().join("mcp.json");
    std::fs::write(
        &mcp,
        r#"{"mcpServers": {"needed": {"command": "/nonexistent/tars-mcp-needed", "required": true}}}"#,
    )
    .unwrap();

    let created = tars_bin()
        .env("TARS_HOME", directory.path().join("tars-home"))
        .current_dir(directory.path())
        .args(["session", "create", "--store"])
        .arg(&store)
        .args(["--config"])
        .arg(&config)
        .args([
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--mcp",
            "needed",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(created.status.success(), "{created:?}");
    let id = report(&created)["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let failed = tars_bin()
        .env("TARS_HOME", directory.path().join("tars-home"))
        .current_dir(directory.path())
        .args(["session", "run", &id, "--store"])
        .arg(&store)
        .args(["--config"])
        .arg(&config)
        .args(["--input", "run against failing required mcp"])
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(1), "{failed:?}");
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(stderr.contains("needed"), "{stderr}");
    assert!(stderr.contains("mcp.json"), "{stderr}");
    assert!(!stderr.contains("work failed"), "{stderr}");
}

#[test]
fn cli_import_legacy_imports_v2_4_history_artifacts_and_rejects_duplicates_or_missing_files() {
    let folder = tempfile::tempdir().unwrap();
    let legacy_dir = folder.path().join("legacy");
    fs::create_dir_all(&legacy_dir).unwrap();

    let conv_id = "0190b000-0000-7000-8000-000000000101";
    let commit_id = "0190b000-0000-7000-8000-000000000102";
    let hist_id = "0190b000-0000-7000-8000-000000000103";
    let art_uuid = "0190b000-0000-7000-8000-000000000104";
    let art_id = format!("artifact:{art_uuid}");

    let call_id = causa::runtime::new_block_id();
    let res_id = causa::runtime::new_block_id();
    let mut out = causa::kernel::ToolOutput::new(json!("spilled preview"));
    out.artifact = Some(causa::kernel::ArtifactRef::new(
        art_id.clone(),
        18,
        causa::kernel::ArtifactKind::FullOutput,
        true,
    ));
    let orig_call = causa::kernel::ContextBlock::new(
        call_id,
        causa::kernel::BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "read_file",
            json!({}),
        )),
        causa::kernel::BlockMeta::default(),
    );
    let orig_res = causa::kernel::ContextBlock::new(
        res_id,
        causa::kernel::BlockContent::ToolResult(causa::kernel::ToolResultPayload::new(
            call_id,
            causa::kernel::ToolResultStatus::Succeeded,
            out,
        )),
        causa::kernel::BlockMeta::default(),
    );
    let brief_block = causa::kernel::ContextBlock::new(
        causa::runtime::new_block_id(),
        causa::kernel::BlockContent::text(causa::kernel::Role::User, "imported brief"),
        causa::kernel::BlockMeta::from_source(tars_core::compact::SUMMARY_SOURCE),
    );
    let user_block = causa::kernel::ContextBlock::new(
        causa::runtime::new_block_id(),
        causa::kernel::BlockContent::text(causa::kernel::Role::User, "follow up"),
        causa::kernel::BlockMeta::default(),
    );

    let history_doc = json!({
        "format": "tars-history-v1.2",
        "conversation_id": conv_id,
        "record_id": hist_id,
        "created_at_ms": 500,
        "trigger": "auto",
        "kind": "summarize",
        "estimate_before": 1000,
        "estimate_after": 100,
        "replacements": [{
            "range": { "start": 0, "end": 2 },
            "originals": [orig_call, orig_res],
            "inserted_ids": [brief_block.id()],
        }],
    });
    let legacy_json = json!({
        "format": "tars-session-v2.4",
        "conversation_id": conv_id,
        "revision": 4,
        "last_commit_id": commit_id,
        "title": "Imported Session",
        "updated_at_ms": 123456,
        "permission_mode": "accept-edits",
        "definition": {
            "provider": "openai-chat",
            "model": "legacy-model",
            "endpoint": null,
            "credential": { "environment": "TARS_TEST_KEY" },
            "work_timeout_secs": null,
            "retained_work_capacity": 16,
            "max_model_rounds": 8,
            "max_tool_calls": 0,
            "workspace_root": null,
            "tools": [],
            "tool_limits": {
                "max_concurrent_operations": 4,
                "call_timeout_secs": 10,
                "max_output_bytes": 65536,
                "max_directory_entries": 200,
                "max_file_scan_bytes": 2097152,
                "max_search_files": 1000,
                "max_search_depth": 32,
                "max_search_scan_bytes": 8388608,
                "max_search_matches": 200
            }
        },
        "context": { "blocks": [brief_block, user_block] },
        "history": [{
            "record_id": hist_id,
            "created_at_ms": 500,
            "trigger": "auto",
            "kind": "summarize",
            "removed_block_count": 2,
            "inserted_block_ids": [brief_block.id()]
        }]
    });

    let legacy_record_path = legacy_dir.join(format!("{conv_id}.json"));
    let legacy_history_dir = legacy_dir.join(format!("{conv_id}.history"));
    let legacy_artifacts_dir = legacy_dir.join(format!("{conv_id}.artifacts"));
    fs::create_dir_all(&legacy_history_dir).unwrap();
    fs::create_dir_all(&legacy_artifacts_dir).unwrap();
    let legacy_hist_path = legacy_history_dir.join(format!("{hist_id}.json"));
    let legacy_art_path = legacy_artifacts_dir.join(art_uuid);
    let record_bytes = serde_json::to_vec(&legacy_json).unwrap();
    let hist_bytes = serde_json::to_vec(&history_doc).unwrap();
    let art_bytes = b"full artifact data";
    fs::write(&legacy_record_path, &record_bytes).unwrap();
    fs::write(&legacy_hist_path, &hist_bytes).unwrap();
    fs::write(&legacy_art_path, art_bytes).unwrap();

    // Missing artifact file fails closed before touching target store.
    fs::remove_file(&legacy_art_path).unwrap();
    let broken_store = folder.path().join("broken-store");
    let fail_out = tars_bin()
        .args(["session", "import-legacy", "--no-config", "--record"])
        .arg(&legacy_record_path)
        .arg("--store")
        .arg(&broken_store)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(fail_out.status.code(), Some(1), "{fail_out:?}");
    assert!(!broken_store.join("tars.db").exists());

    // Restore artifact and import cleanly.
    fs::write(&legacy_art_path, art_bytes).unwrap();
    let store = folder.path().join("store");
    let ok_out = tars_bin()
        .args(["session", "import-legacy", "--no-config", "--record"])
        .arg(&legacy_record_path)
        .arg("--store")
        .arg(&store)
        .arg("--json")
        .output()
        .unwrap();
    assert!(ok_out.status.success(), "{ok_out:?}");
    let rep = report(&ok_out);
    assert_eq!(rep["status"], "imported");
    assert_eq!(rep["conversation_id"], conv_id);
    assert_eq!(rep["revision"], 4);
    assert_eq!(rep["block_count"], 2);
    assert_eq!(rep["history_records"], 1);
    assert_eq!(rep["artifacts"], 1);

    // Source files remain byte-for-byte untouched.
    assert_eq!(fs::read(&legacy_record_path).unwrap(), record_bytes);
    assert_eq!(fs::read(&legacy_hist_path).unwrap(), hist_bytes);
    assert_eq!(fs::read(&legacy_art_path).unwrap(), art_bytes);

    // Duplicate import is refused cleanly.
    let dup_out = tars_bin()
        .args(["session", "import-legacy", "--no-config", "--record"])
        .arg(&legacy_record_path)
        .arg("--store")
        .arg(&store)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(dup_out.status.code(), Some(1), "{dup_out:?}");

    // Show & history commands read the imported SQLite session accurately.
    let show_out = tars_bin()
        .args(["session", "show", conv_id, "--no-config", "--store"])
        .arg(&store)
        .arg("--json")
        .output()
        .unwrap();
    assert!(show_out.status.success(), "{show_out:?}");
    let shown = report(&show_out);
    assert_eq!(shown["revision"], 4);
    assert_eq!(shown["history_records"], 1);
    assert_eq!(shown["title"], "Imported Session");

    let hist_out = tars_bin()
        .args(["session", "history", conv_id, "--no-config", "--store"])
        .arg(&store)
        .arg("--json")
        .output()
        .unwrap();
    assert!(hist_out.status.success(), "{hist_out:?}");
    let hist_stdout = String::from_utf8(hist_out.stdout).unwrap();
    assert!(hist_stdout.contains(&art_id), "{hist_stdout}");

    let stored = record(&store, conv_id);
    assert_eq!(stored["permission_mode"], "accept-edits");
}

#[test]
fn cli_help_lists_import_legacy_and_session_import_legacy_help_succeeds() {
    let top_help = tars_bin().args(["--help"]).output().unwrap();
    assert!(top_help.status.success(), "{top_help:?}");
    let top_stdout = String::from_utf8(top_help.stdout).unwrap();
    assert!(top_stdout.contains("import-legacy"), "{top_stdout}");

    for flag in ["--help", "-h"] {
        let sub_help = tars_bin()
            .args(["session", "import-legacy", flag])
            .output()
            .unwrap();
        assert!(sub_help.status.success(), "{sub_help:?}");
        let sub_stdout = String::from_utf8(sub_help.stdout).unwrap();
        assert!(sub_stdout.contains("import-legacy"), "{sub_stdout}");
        assert!(sub_stdout.contains("--record"), "{sub_stdout}");
    }
}

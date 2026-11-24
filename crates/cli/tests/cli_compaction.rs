//! Actual CLI processes for context compaction, with local scripted models.
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

#[path = "common/sse.rs"]
mod sse;

const KEY: &str = "fixture-secret";

/// A scripted OpenAI Chat endpoint: the test decides the next response, so a
/// summary request can answer differently from an ordinary streamed round.
struct Fixture {
    endpoint: String,
    requests: mpsc::Receiver<Value>,
    replies: mpsc::Sender<Reply>,
}

enum Reply {
    /// An ordinary streamed chat round.
    Stream(String),
    /// A non-streamed summary request answer.
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
                let body = raw.split_once("\r\n\r\n").map_or("", |(_, body)| body);
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

/// The CLI environment every invocation shares; configuration stays isolated.
fn cli() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tars"));
    command.env_remove("TARS_CONFIG").env("TARS_TEST_KEY", KEY);
    command
}

fn create(store: &Path, endpoint: &str, extra: &[&str]) -> String {
    let output = cli()
        .args(["session", "create", "--no-config", "--json", "--store"])
        .arg(store)
        .args([
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            endpoint,
        ])
        .args(extra)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value = report(&output);
    assert_eq!(value["status"], "created");
    assert_eq!(value["saved"], true);
    value["conversation_id"].as_str().unwrap().to_owned()
}

fn run(store: &Path, id: &str, input: &str) -> Command {
    let mut command = cli();
    command
        .args(["session", "run", id, "--no-config", "--json", "--store"])
        .arg(store)
        .args(["--input", input]);
    command
}

fn compact(store: &Path, id: &str, json_output: bool) -> Command {
    let mut command = cli();
    command
        .args(["session", "compact", id, "--no-config", "--store"])
        .arg(store);
    if json_output {
        command.arg("--json");
    }
    command
}

fn spawn(command: Command) -> Child {
    let mut command = command;
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

fn finished(mut child: Child) -> Output {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!("CLI did not finish within 20 seconds: {output:?}");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("{output:?}"))
}

fn record(store: &Path, id: &str) -> Value {
    let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
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
    let DbRow {
        revision,
        last_commit_id,
        policy_json,
        title,
        permission_mode,
        auto_compact,
        compact_threshold,
        usage_json,
        updated_at_ms,
        def_json,
        ctx_json,
        history_json,
    } = conn
        .query_row(
            "SELECT
                s.revision, s.last_commit_id, s.policy_json, s.title, s.permission_mode,
                s.auto_compact, s.compact_threshold, s.usage_json, s.updated_at_ms,
                a.definition_json, a.context_json, a.history_index_json
             FROM sessions s
             JOIN agents a ON s.session_id = a.session_id AND s.entry_agent_id = a.agent_id
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
    let policy: Value = serde_json::from_str(&policy_json).unwrap();
    let mut obj = serde_json::Map::new();
    obj.insert("format".into(), policy["format"].clone());
    obj.insert("conversation_id".into(), json!(id));
    obj.insert("revision".into(), json!(revision));
    obj.insert("last_commit_id".into(), json!(last_commit_id));
    obj.insert(
        "definition".into(),
        serde_json::from_str(&def_json).unwrap(),
    );
    obj.insert("context".into(), serde_json::from_str(&ctx_json).unwrap());
    if let Some(t) = title {
        obj.insert("title".into(), json!(t));
    }
    obj.insert("updated_at_ms".into(), json!(updated_at_ms));
    if let Some(m) = permission_mode {
        obj.insert("permission_mode".into(), json!(m));
    }
    if let Some(a) = auto_compact {
        obj.insert("auto_compact".into(), json!(a != 0));
    }
    if let Some(ct) = compact_threshold {
        obj.insert("compact_threshold".into(), json!(ct));
    }
    let history: Vec<Value> = serde_json::from_str(&history_json).unwrap();
    if !history.is_empty() {
        obj.insert("history".into(), Value::Array(history));
    }
    if let Some(u) = usage_json {
        obj.insert("usage".into(), serde_json::from_str(&u).unwrap());
    }
    Value::Object(obj)
}

fn write_context_to_db(store: &Path, id: &str, context: &Value) {
    let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
    conn.execute(
        "UPDATE agents SET context_json = ?1 WHERE session_id = ?2 AND agent_id = ?2",
        rusqlite::params![serde_json::to_string(context).unwrap(), id],
    )
    .unwrap();
}

fn read_history_doc(store: &Path, id: &str, record_id: &str) -> Value {
    let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
    let doc: String = conn
        .query_row(
            "SELECT document_json FROM history_records
             WHERE session_id = ?1 AND agent_id = ?1 AND record_id = ?2",
            rusqlite::params![id, record_id],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str(&doc).unwrap()
}

fn history_row_count(store: &Path, id: &str) -> usize {
    let db_path = store.join("tars.db");
    if !db_path.exists() {
        return 0;
    }
    let conn = rusqlite::Connection::open(db_path).unwrap();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM history_records WHERE session_id = ?1 AND agent_id = ?1",
            rusqlite::params![id],
            |row| row.get(0),
        )
        .unwrap();
    usize::try_from(count).unwrap()
}

fn insert_artifact_row(store: &Path, id: &str, artifact_uuid: &str, content: &[u8]) {
    let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
    conn.execute(
        "INSERT INTO artifacts (session_id, agent_id, artifact_id, content) VALUES (?1, ?1, ?2, ?3)",
        rusqlite::params![id, artifact_uuid, content],
    )
    .unwrap();
}

fn write_snapshot_json(store: &Path, id: &str, dest: &Path) {
    let rec = record(store, id);
    fs::write(dest, serde_json::to_vec(&rec).unwrap()).unwrap();
}

fn history_blocks(output: &Output) -> Vec<tars_core::compact::ExpandedBlock> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("history line {line}: {error}"))
        })
        .collect()
}

/// Flags for sessions that compact: a 6,000-token window with a 3,000-token
/// trigger line, well above twice the initial prelude (~780 tokens) plus
/// `recall` declaration (~440 tokens) that every persistent session carries.
const COMPACTING: [&str; 4] = ["--context-window", "6000", "--auto-compact", "50"];

/// Run one work that stays under the trigger line and answer it.
///
/// A work's own input is protected from its own compaction, so a later work
/// needs this earlier material to summarize.
fn seed(fixture: &Fixture, store: &Path, id: &str, input: &str) {
    let child = spawn(run(store, id, input));
    let round = fixture.request();
    assert!(!round.to_string().contains("Prepare a continuation brief"));
    fixture.stream("seeded answer");
    let output = finished(child);
    assert!(output.status.success(), "{output:?}");
}

/// Input that brings a seeded session over its trigger line, about 800 tokens.
fn crossing_input() -> String {
    "y".repeat(2_400)
}

#[test]
fn compaction_flags_resolve_into_the_definition_and_help_lists_them() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let id = create(
        &store,
        "http://127.0.0.1:1/unused",
        &["--context-window", "4096", "--auto-compact", "55"],
    );
    let definition = record(&store, &id)["definition"].clone();
    assert_eq!(definition["context_window"], 4096);
    assert_eq!(definition["auto_compact"], true);
    assert_eq!(definition["compact_threshold"], 55);

    let off = create(
        &store,
        "http://127.0.0.1:1/unused",
        &["--auto-compact", "off"],
    );
    let definition = record(&store, &off)["definition"].clone();
    assert_eq!(definition["auto_compact"], false);
    assert_eq!(definition["compact_threshold"], 70);
    assert!(definition.get("context_window").is_none());

    let on = create(
        &store,
        "http://127.0.0.1:1/unused",
        &["--auto-compact", "on", "--context-window", "1000"],
    );
    let definition = record(&store, &on)["definition"].clone();
    assert_eq!(definition["auto_compact"], true);
    assert_eq!(definition["compact_threshold"], 70);
    assert_eq!(definition["context_window"], 1000);

    for value in ["5", "96", "0", "maybe"] {
        let output = cli()
            .args(["session", "create", "--no-config", "--json", "--store"])
            .arg(&store)
            .args([
                "--provider",
                "openai-chat",
                "--model",
                "fixture-model",
                "--api-key-env",
                "TARS_TEST_KEY",
                "--auto-compact",
                value,
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{value}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(
                "--auto-compact must be on, off, or an integer percentage between 10 and 95"
            ),
            "{value}: {stderr}"
        );
        assert!(
            stderr.contains("Use tars session --help."),
            "{value}: {stderr}"
        );
    }
    for value in ["0", "abc", "-1"] {
        let output = cli()
            .args(["session", "create", "--no-config", "--json", "--store"])
            .arg(&store)
            .args([
                "--provider",
                "openai-chat",
                "--model",
                "fixture-model",
                "--api-key-env",
                "TARS_TEST_KEY",
                "--context-window",
                value,
            ])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{value}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--context-window must be a positive")
                || stderr.contains("invalid numeric option: --context-window"),
            "{value}: {stderr}"
        );
        assert!(
            stderr.contains("Use tars session --help."),
            "{value}: {stderr}"
        );
    }

    let session_help = cli().args(["session", "--help"]).output().unwrap();
    assert!(session_help.status.success(), "{session_help:?}");
    let help = String::from_utf8_lossy(&session_help.stdout);
    for expected in [
        "--context-window",
        "--auto-compact",
        "session show",
        "session compact",
        "session history",
        "prints the expanded full conversation",
        "one block per line",
    ] {
        assert!(
            help.contains(expected),
            "session help lacks {expected}: {help}"
        );
    }
    for command in ["create", "history", "compact", "show"] {
        let output = cli().args(["session", command, "--help"]).output().unwrap();
        assert!(output.status.success(), "{command}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("--context-window"));
    }
    let run_help = cli().args(["run", "--help"]).output().unwrap();
    let help = String::from_utf8_lossy(&run_help.stdout);
    assert!(help.contains("--context-window"), "{help}");
    assert!(help.contains("--auto-compact"), "{help}");

    let rejected = cli()
        .args([
            "run",
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--auto-compact",
            "maybe",
            "--input",
            "x",
        ])
        .output()
        .unwrap();
    assert_eq!(rejected.status.code(), Some(2), "{rejected:?}");
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("Use tars run --help."));

    // Transient runs accept the flags even though they keep no history.
    let fixture = Fixture::new(1);
    let child = spawn({
        let mut command = cli();
        command
            .args(["run", "--no-config", "--json"])
            .args(["--provider", "openai-chat", "--model", "fixture-model"])
            .args([
                "--api-key-env",
                "TARS_TEST_KEY",
                "--endpoint",
                &fixture.endpoint,
            ])
            .args(["--context-window", "4000", "--auto-compact", "off"])
            .args(["--input", "flag run"]);
        command
    });
    let request = fixture.request();
    assert_eq!(request["model"], "fixture-model");
    fixture.stream("flag answer");
    let output = finished(child);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(report(&output)["text"], "flag answer");
}

#[test]
fn small_context_window_compacts_automatically_and_keeps_a_history_record() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(3);
    let id = create(&store, &fixture.endpoint, &COMPACTING);
    seed(&fixture, &store, &id, &"x".repeat(3_000));
    let crossing = crossing_input();
    let child = spawn(run(&store, &id, &crossing));
    let summary_request = fixture.request();
    assert!(
        summary_request
            .to_string()
            .contains("Prepare a continuation brief"),
        "the first request must be the summary call: {summary_request}"
    );
    assert!(summary_request.to_string().contains("xxxxxxxxxx"));
    assert!(
        !summary_request.to_string().contains(&crossing),
        "round-0 current input must be excluded from the brief request"
    );
    fixture.summary("Goal: finish the compaction fixture. Next steps: report the result.");
    let round = fixture.request();
    assert!(!round.to_string().contains("Prepare a continuation brief"));
    assert!(
        round
            .to_string()
            .contains("Goal: finish the compaction fixture")
    );
    fixture.stream("compacted answer");
    let output = finished(child);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(report(&output)["text"], "compacted answer");
    assert_eq!(report(&output)["saved"], true);
    assert_eq!(report(&output)["revision"], 3);

    let saved = record(&store, &id);
    let history = saved["history"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["trigger"], "auto");
    // The seeded request, its tool declaration and answer; the round-0
    // current request stays untouched in the retained tail.
    assert_eq!(history[0]["removed_block_count"], 3);
    let inserted = history[0]["inserted_block_ids"].as_array().unwrap();
    assert_eq!(inserted.len(), 2);
    let summary_block = inserted[1].clone();
    let blocks = saved["context"]["blocks"].as_array().unwrap();
    assert_eq!(blocks.len(), 6);
    assert_eq!(blocks[0]["meta"]["source"], "tars:prelude:base");
    assert_eq!(blocks[1]["meta"]["source"], "tars:prelude:env");
    assert_eq!(blocks[2]["content"]["shape"], "tool_change");
    assert_eq!(blocks[3]["meta"]["source"], "tars:compaction");
    assert_eq!(blocks[3]["id"], summary_block);
    assert_eq!(blocks[4]["content"]["value"]["role"], "user");
    assert!(!inserted.contains(&blocks[4]["id"]));
    assert_eq!(blocks[5]["content"]["value"]["role"], "assistant");

    let history_output = cli()
        .args([
            "session",
            "history",
            &id,
            "--no-config",
            "--json",
            "--store",
        ])
        .arg(&store)
        .output()
        .unwrap();
    assert!(history_output.status.success(), "{history_output:?}");
    let blocks = history_blocks(&history_output);
    // Initial prelude (2 original), then seeded request, declaration and answer
    // summarized; the current request and its answer as they are.
    assert_eq!(blocks.len(), 7);
    let record_id = history[0]["id"]
        .as_str()
        .or_else(|| history[0]["record_id"].as_str())
        .unwrap();
    assert_eq!(blocks[0].status, tars_core::compact::BlockStatus::Original);
    assert_eq!(blocks[1].status, tars_core::compact::BlockStatus::Original);
    assert_eq!(
        blocks[2].status,
        tars_core::compact::BlockStatus::Summarized(record_id.to_owned())
    );
    assert_eq!(
        blocks[3].status,
        tars_core::compact::BlockStatus::Summarized(record_id.to_owned())
    );
    assert_eq!(
        blocks[4].status,
        tars_core::compact::BlockStatus::Summarized(record_id.to_owned())
    );
    assert_eq!(blocks[5].status, tars_core::compact::BlockStatus::Original);
    assert_eq!(blocks[6].status, tars_core::compact::BlockStatus::Original);

    let last = cli()
        .args([
            "session",
            "history",
            &id,
            "--no-config",
            "--last",
            "1",
            "--json",
            "--store",
        ])
        .arg(&store)
        .output()
        .unwrap();
    assert!(last.status.success(), "{last:?}");
    let last_blocks = history_blocks(&last);
    assert_eq!(last_blocks.len(), 1);
    assert_eq!(
        last_blocks[0].status,
        tars_core::compact::BlockStatus::Original
    );

    let human = cli()
        .args(["session", "history", &id, "--no-config", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(human.status.success(), "{human:?}");
    let text = String::from_utf8_lossy(&human.stdout);
    for expected in [
        "[user · summarized (",
        "[assistant · original]",
        &"x".repeat(20),
    ] {
        assert!(
            text.contains(expected),
            "history output lacks {expected}: {text}"
        );
    }

    let show = cli()
        .args(["session", "show", &id, "--no-config", "--json", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(show.status.success(), "{show:?}");
    let shown = report(&show);
    assert_eq!(shown["history_records"], 1);
    assert_eq!(shown["block_count"], 6);
    assert_eq!(shown["context_window"], 6000);
    assert_eq!(shown["auto_compact"], true);
    assert_eq!(shown["compact_threshold"], 50);
    assert!(shown["context_estimate_tokens"].as_u64().unwrap() > 0);
    assert_eq!(shown["revision"], 3);
    let shown_text = cli()
        .args(["session", "show", &id, "--no-config", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(shown_text.status.success(), "{shown_text:?}");
    let shown_text = String::from_utf8_lossy(&shown_text.stdout);
    assert!(shown_text.contains("1 history record(s)"), "{shown_text}");
    assert!(shown_text.contains("auto on"), "{shown_text}");
}

/// Export and `--load` import are defined to carry only the current Context,
/// never the replaced history records.
#[test]
fn export_and_import_carry_only_the_current_context_after_compaction() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(3);
    let id = create(&store, &fixture.endpoint, &COMPACTING);
    seed(&fixture, &store, &id, &"x".repeat(3_000));
    let child = spawn(run(&store, &id, &crossing_input()));
    fixture.request();
    fixture.summary("Goal: finish the export fixture.");
    fixture.request();
    fixture.stream("exported answer");
    let output = finished(child);
    assert!(output.status.success(), "{output:?}");

    let snapshot = folder.path().join("snapshot.json");
    write_snapshot_json(&store, &id, &snapshot);
    let exported = folder.path().join("exported.json");
    let export = cli()
        .args(["session", "export-materials", "--record"])
        .arg(&snapshot)
        .arg("--output")
        .arg(&exported)
        .output()
        .unwrap();
    assert!(export.status.success(), "{export:?}");
    let material = tars_core::material::load(&exported).unwrap();
    // Prelude (2), declaration, brief, the kept request and its answer.
    assert_eq!(material.blocks().len(), 6);
    let serialized = serde_json::to_string(&material).unwrap();
    assert!(serialized.contains("Goal: finish the export fixture"));
    assert!(!serialized.contains(&"x".repeat(20)));
    let imported_store = folder.path().join("imported");
    let imported = create(
        &imported_store,
        "http://127.0.0.1:1/unused",
        &["--load", exported.to_str().unwrap()],
    );
    let imported_record = record(&imported_store, &imported);
    assert_eq!(
        imported_record["context"]["blocks"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    assert!(
        imported_record
            .get("history")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    );
}

#[test]
fn manual_compaction_reports_trigger_estimates_and_record_and_last_limits_history() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(4);
    let id = create(&store, &fixture.endpoint, &["--auto-compact", "off"]);
    // Large enough that a brief saves more than restating the `recall`
    // declaration it summarizes costs.
    let long_input = "m".repeat(3_000);
    let child = spawn(run(&store, &id, &long_input));
    let round = fixture.request();
    assert!(!round.to_string().contains("Prepare a continuation brief"));
    fixture.stream("first manual answer");
    let output = finished(child);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(report(&output)["revision"], 2);

    let child = spawn(compact(&store, &id, true));
    let request = fixture.request();
    assert!(
        request.to_string().contains("Prepare a continuation brief"),
        "manual compaction must ask the model: {request}"
    );
    fixture.summary("Goal: first manual brief.");
    let output = finished(child);
    assert!(output.status.success(), "{output:?}");
    let first = report(&output);
    assert_eq!(first["status"], "compacted");
    assert_eq!(first["trigger"], "manual");
    assert_eq!(first["saved"], true);
    assert_eq!(first["revision"], 3);
    assert_eq!(first["blocks"], 3);
    assert!(first["estimate_before"].as_u64() > first["estimate_after"].as_u64());
    let first_record = first["record_id"].as_str().unwrap().to_owned();
    let first_summary = first["summary_block_id"].as_str().unwrap().to_owned();
    assert_eq!(first["merge_record_id"], json!(null));
    let saved = record(&store, &id);
    assert_eq!(saved["context"]["blocks"].as_array().unwrap().len(), 4);
    assert_eq!(saved["context"]["blocks"][3]["id"], json!(first_summary));
    assert_eq!(saved["history"][0]["trigger"], "manual");

    // Material added after one compaction keeps the next replacement smaller.
    let second_input = format!("second manual question {}", "n".repeat(1_500));
    let child = spawn(run(&store, &id, &second_input));
    let round = fixture.request();
    assert!(!round.to_string().contains("Prepare a continuation brief"));
    fixture.stream("second manual answer");
    let output = finished(child);
    assert!(output.status.success(), "{output:?}");

    let child = spawn(compact(&store, &id, false));
    let request = fixture.request();
    assert!(request.to_string().contains("Prepare a continuation brief"));
    fixture.summary("Goal: second manual brief.");
    let output = finished(child);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Compacted (manual)"), "{text}");
    assert!(text.contains("->"), "{text}");

    let saved = record(&store, &id);
    assert_eq!(saved["revision"], 5);
    let history = saved["history"].as_array().unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0]["record_id"], first_record.as_str());
    assert_eq!(history[0]["trigger"], "manual");
    assert_eq!(history[1]["trigger"], "manual");
    // Only what came after the first brief; the brief itself stays.
    assert_eq!(history[1]["removed_block_count"], 2);
    let second_record = history[1]["record_id"].as_str().unwrap().to_owned();
    assert!(text.contains(&second_record), "{text}");
    let blocks = saved["context"]["blocks"].as_array().unwrap();
    assert_eq!(blocks.len(), 5, "prelude, declaration and the two briefs");
    assert_eq!(blocks[3]["id"], json!(first_summary));
    let second_summary = blocks[4]["id"].as_str().unwrap();
    assert!(
        text.contains(&format!("summary block {second_summary}")),
        "the newest brief is reported: {text}"
    );
    assert!(!text.contains("briefs merged"), "{text}");

    let full = cli()
        .args([
            "session",
            "history",
            &id,
            "--no-config",
            "--json",
            "--store",
        ])
        .arg(&store)
        .output()
        .unwrap();
    assert!(full.status.success(), "{full:?}");
    let blocks = history_blocks(&full);
    assert_eq!(blocks.len(), 7);
    assert_eq!(blocks[0].status, tars_core::compact::BlockStatus::Original);
    assert_eq!(blocks[1].status, tars_core::compact::BlockStatus::Original);
    assert_eq!(
        blocks[2].status,
        tars_core::compact::BlockStatus::Summarized(first_record.clone())
    );
    assert_eq!(
        blocks[3].status,
        tars_core::compact::BlockStatus::Summarized(first_record.clone())
    );
    assert_eq!(
        blocks[4].status,
        tars_core::compact::BlockStatus::Summarized(first_record.clone())
    );
    assert_eq!(
        blocks[5].status,
        tars_core::compact::BlockStatus::Summarized(second_record.clone())
    );
    assert_eq!(
        blocks[6].status,
        tars_core::compact::BlockStatus::Summarized(second_record.clone())
    );

    let last = cli()
        .args([
            "session",
            "history",
            &id,
            "--no-config",
            "--last",
            "1",
            "--json",
            "--store",
        ])
        .arg(&store)
        .output()
        .unwrap();
    let last_blocks = history_blocks(&last);
    assert_eq!(last_blocks.len(), 1);
    assert_eq!(
        last_blocks[0].status,
        tars_core::compact::BlockStatus::Summarized(second_record.clone())
    );
    let human_last = cli()
        .args([
            "session",
            "history",
            &id,
            "--no-config",
            "--last",
            "1",
            "--store",
        ])
        .arg(&store)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&human_last.stdout);
    assert!(text.contains("second manual answer"), "{text}");
    assert!(text.contains(&second_record), "{text}");

    let show = cli()
        .args(["session", "show", &id, "--no-config", "--json", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    let shown = report(&show);
    assert_eq!(shown["history_records"], 2);
    assert_eq!(shown["auto_compact"], false);
    assert_eq!(shown["compact_threshold"], 70);
    assert!(shown["context_window"].is_null());
    assert_eq!(shown["block_count"], 5);

    // An unreadable record is reported in both output modes and fails the
    // command, while the readable record is still expanded.
    let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
    conn.execute(
        "UPDATE history_records SET document_json = 'not json'
         WHERE session_id = ?1 AND agent_id = ?1 AND record_id = ?2",
        rusqlite::params![&id, &first_record],
    )
    .unwrap();
    drop(conn);
    let broken_json = cli()
        .args([
            "session",
            "history",
            &id,
            "--no-config",
            "--json",
            "--store",
        ])
        .arg(&store)
        .output()
        .unwrap();
    assert_eq!(broken_json.status.code(), Some(1), "{broken_json:?}");
    let broken_stderr = String::from_utf8_lossy(&broken_json.stderr);
    assert!(broken_stderr.contains(&first_record), "{broken_stderr}");
    let documents = history_blocks(&broken_json);
    assert!(
        documents.iter().any(|block| block.status
            == tars_core::compact::BlockStatus::Unreadable(first_record.clone()))
    );
    let broken_human = cli()
        .args(["session", "history", &id, "--no-config", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert_eq!(broken_human.status.code(), Some(1), "{broken_human:?}");
    let human_stderr = String::from_utf8_lossy(&broken_human.stderr);
    assert!(human_stderr.contains(&first_record), "{human_stderr}");
    let text = String::from_utf8_lossy(&broken_human.stdout);
    assert!(text.contains(&second_record), "{text}");
}

#[test]
fn a_busy_session_is_refused_and_unpaired_materials_need_repair() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(1);
    let id = create(&store, &fixture.endpoint, &["--auto-compact", "off"]);
    let child = spawn(run(&store, &id, "busy input"));
    fixture.request();
    let busy = compact(&store, &id, true).output().unwrap();
    assert_eq!(busy.status.code(), Some(1), "{busy:?}");
    let refused = report(&busy);
    assert_eq!(refused["status"], "not_started");
    assert!(
        refused["error"].as_str().unwrap().contains("occupied"),
        "{refused}"
    );
    fixture.stream("busy answer");
    let output = finished(child);
    assert!(output.status.success(), "{output:?}");
    assert!(record(&store, &id).get("history").is_none());

    use causa::kernel::{BlockContent, BlockMeta, Context, ContextBlock, ToolCallPayload};
    let repair_folder = tempfile::tempdir().unwrap();
    let repair_store = repair_folder.path().join("store");
    let material = Context::from_blocks(vec![
        tars_core::application::text_input("old unpaired input"),
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::ToolCall(ToolCallPayload::new(
                "read_file",
                json!({"path": "missing.txt"}),
            )),
            BlockMeta::default().with_provider_call_id("old-unpaired-call"),
        ),
    ])
    .unwrap();
    let input = repair_folder.path().join("input.json");
    tars_core::material::save_new(&input, &material).unwrap();
    let unpaired = create(
        &repair_store,
        "http://127.0.0.1:1/unused",
        &["--load", input.to_str().unwrap()],
    );
    let before = record(&repair_store, &unpaired);
    let output = compact(&repair_store, &unpaired, true).output().unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let value = report(&output);
    assert_eq!(value["status"], "rejected");
    let error = value["error"].as_str().unwrap();
    assert!(
        error.contains("request materials need attention"),
        "{error}"
    );
    assert!(
        error.contains("inspect repair before sending new work"),
        "{error}"
    );
    assert_eq!(record(&repair_store, &unpaired), before);
    assert_eq!(history_row_count(&repair_store, &unpaired), 0);
    let human = compact(&repair_store, &unpaired, false).output().unwrap();
    assert_eq!(human.status.code(), Some(1), "{human:?}");
    assert!(
        String::from_utf8_lossy(&human.stderr).contains("inspect repair before sending new work")
    );

    let missing = compact(&repair_store, "01a197b5-0000-7000-8000-000000000000", true)
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1), "{missing:?}");
    assert!(report(&missing)["error"].as_str().is_some());

    let missing_id = cli().args(["session", "history"]).output().unwrap();
    assert_eq!(missing_id.status.code(), Some(2), "{missing_id:?}");
    let stderr = String::from_utf8_lossy(&missing_id.stderr);
    assert!(stderr.contains("conversation ID is required"), "{stderr}");
    assert!(stderr.contains("Use tars session --help."), "{stderr}");

    let bad_last = cli()
        .args([
            "session",
            "history",
            unpaired.as_str(),
            "--no-config",
            "--last",
            "0",
            "--store",
        ])
        .arg(&repair_store)
        .output()
        .unwrap();
    assert_eq!(bad_last.status.code(), Some(2), "{bad_last:?}");
    let stderr = String::from_utf8_lossy(&bad_last.stderr);
    assert!(
        stderr.contains("--last must be a positive integer"),
        "{stderr}"
    );
    assert!(stderr.contains("Use tars session --help."), "{stderr}");
}

#[test]
fn show_reports_retained_work_capacity() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(5);
    let id = create(&store, &fixture.endpoint, &["--capacity", "2"]);
    for index in 0..5 {
        let child = spawn(run(&store, &id, &format!("capacity work {index}")));
        let request = fixture.request();
        assert!(!request.to_string().contains("Prepare a continuation brief"));
        fixture.stream(&format!("capacity answer {index}"));
        let output = finished(child);
        assert!(output.status.success(), "{index}: {output:?}");
        assert_eq!(report(&output)["saved"], true);
    }
    let show = cli()
        .args(["session", "show", &id, "--no-config", "--json", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(show.status.success(), "{show:?}");
    let shown = report(&show);
    assert_eq!(shown["work_capacity"]["capacity"], 2);
    assert!(
        shown["work_capacity"]["used"].as_u64().unwrap() <= 2,
        "{shown}"
    );
    assert_eq!(shown["block_count"], 13);
    assert_eq!(shown["history_records"], 0);

    let human = cli()
        .args(["session", "show", &id, "--no-config", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(human.status.success(), "{human:?}");
    let text = String::from_utf8_lossy(&human.stdout);
    assert!(text.contains("Works: "), "{text}");
    assert!(text.contains("/ 2"), "{text}");
}

#[test]
fn test_be05_cli_history_prints_expanded_conversation_and_deleting_record_exits_1() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(3);
    let id = create(&store, &fixture.endpoint, &COMPACTING);
    seed(&fixture, &store, &id, &"first long user input ".repeat(150));
    let child = spawn(run(&store, &id, &crossing_input()));
    let summary_request = fixture.request();
    assert!(
        summary_request
            .to_string()
            .contains("Prepare a continuation brief")
    );
    fixture.summary("Goal: continue task from brief.");
    fixture.request();
    fixture.stream("final assistant answer");
    let output = finished(child);
    assert!(output.status.success());

    // 1. CLI history prints expanded conversation with block status
    let human = cli()
        .args(["session", "history", &id, "--no-config", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(human.status.success(), "{human:?}");
    let human_text = String::from_utf8_lossy(&human.stdout);
    assert!(human_text.contains("[user · summarized ("), "{human_text}");
    assert!(
        human_text.contains("[assistant · original]"),
        "{human_text}"
    );

    // 2. --json prints one block per line
    let json_output = cli()
        .args([
            "session",
            "history",
            &id,
            "--no-config",
            "--json",
            "--store",
        ])
        .arg(&store)
        .output()
        .unwrap();
    assert!(json_output.status.success(), "{json_output:?}");
    let lines: Vec<&str> = std::str::from_utf8(&json_output.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    assert_eq!(lines.len(), 7);
    for line in &lines {
        let block_json: Value = serde_json::from_str(line).expect("valid JSON line");
        assert!(block_json.get("status").is_some());
        assert!(block_json.get("block").is_some());
    }

    // 3. Deleting one record row reports it and exits 1 after printing what it could
    let saved = record(&store, &id);
    let rec_id = saved["history"][0]["record_id"].as_str().unwrap();
    let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
    let deleted = conn
        .execute(
            "DELETE FROM history_records WHERE session_id = ?1 AND agent_id = ?1 AND record_id = ?2",
            rusqlite::params![&id, rec_id],
        )
        .unwrap();
    assert_eq!(deleted, 1);
    drop(conn);

    let broken = cli()
        .args([
            "session",
            "history",
            &id,
            "--no-config",
            "--json",
            "--store",
        ])
        .arg(&store)
        .output()
        .unwrap();
    assert_eq!(broken.status.code(), Some(1), "{broken:?}");
    let stderr = String::from_utf8_lossy(&broken.stderr);
    assert!(stderr.contains(rec_id), "{stderr}");
    // Prints what it could (e.g. the unsummarized original block):
    let broken_lines: Vec<&str> = std::str::from_utf8(&broken.stdout)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    // The inserted blocks of the missing record stand in for its originals:
    // the declaration and the brief. The untouched round-0 request and
    // assistant reply remain original.
    assert_eq!(broken_lines.len(), 6, "{broken_lines:?}");
    for line in &broken_lines[..2] {
        let block: Value = serde_json::from_str(line).unwrap();
        assert_eq!(block["status"], "original");
    }
    for line in &broken_lines[2..4] {
        let block: Value = serde_json::from_str(line).unwrap();
        assert_eq!(block["status"], "unreadable");
        assert_eq!(block["record_id"], rec_id);
    }
    for line in &broken_lines[4..6] {
        let block: Value = serde_json::from_str(line).unwrap();
        assert_eq!(block["status"], "original");
    }
}

#[test]
fn test_be09_cli_export_materials_drops_own_reference_and_create_accepts() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(3);
    let id = create(&store, &fixture.endpoint, &COMPACTING);
    seed(&fixture, &store, &id, &"x".repeat(3_000));
    let child = spawn(run(&store, &id, &crossing_input()));
    fixture.request();
    fixture.summary("Goal: test export drops own reference.");
    fixture.request();
    fixture.stream("done");
    let output = finished(child);
    assert!(output.status.success(), "{output:?}");

    let mut rec_json = record(&store, &id);
    let record_id = rec_json["history"][0]["record_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let hist_json = read_history_doc(&store, &id, &record_id);
    let orig_block_id = hist_json["replacements"][0]["originals"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let call_id = causa::runtime::new_block_id();
    let res_id = causa::runtime::new_block_id();
    let tool_call_block = causa::kernel::ContextBlock::new(
        call_id,
        causa::kernel::BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "read_file",
            serde_json::json!({}),
        )),
        causa::kernel::BlockMeta::default(),
    );
    let mut out = causa::kernel::ToolOutput::new(serde_json::json!("preview of data"));
    out.artifact = Some(causa::kernel::ArtifactRef::new(
        format!("{record_id}/{orig_block_id}"),
        50,
        causa::kernel::ArtifactKind::FullOutput,
        true,
    ));
    let tool_res_payload = causa::kernel::ToolResultPayload::new(
        call_id,
        causa::kernel::ToolResultStatus::Succeeded,
        out,
    )
    .with_notes(vec![causa::kernel::TextPayload::new("original note")]);
    let tool_res_block = causa::kernel::ContextBlock::new(
        res_id,
        causa::kernel::BlockContent::ToolResult(tool_res_payload),
        causa::kernel::BlockMeta::default(),
    );

    rec_json["context"]["blocks"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::to_value(&tool_call_block).unwrap());
    rec_json["context"]["blocks"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::to_value(&tool_res_block).unwrap());
    let rec_file = folder.path().join("export-source.json");
    fs::write(&rec_file, serde_json::to_vec(&rec_json).unwrap()).unwrap();

    let exported = folder.path().join("exported.json");
    let export = cli()
        .args(["session", "export-materials", "--record"])
        .arg(&rec_file)
        .arg("--output")
        .arg(&exported)
        .output()
        .unwrap();
    assert!(export.status.success(), "{export:?}");

    let material = tars_core::material::load(&exported).unwrap();
    let exported_res = material
        .blocks()
        .iter()
        .find_map(|b| match b.content() {
            causa::kernel::BlockContent::ToolResult(r) => Some(r),
            _ => None,
        })
        .expect("exported tool result present");
    assert!(
        exported_res.output.artifact.is_none(),
        "own reference must be dropped"
    );
    assert!(
        exported_res
            .notes
            .iter()
            .any(|n| n.0 == tars_core::material::DETACHED_HISTORY_NOTE)
    );

    let imported_store = folder.path().join("imported");
    let imported_id = create(
        &imported_store,
        "http://127.0.0.1:1/unused",
        &["--load", exported.to_str().unwrap()],
    );
    let imported_record = record(&imported_store, &imported_id);
    assert_eq!(
        imported_record["context"]["blocks"]
            .as_array()
            .unwrap()
            .len(),
        material.blocks().len()
    );
}

#[test]
fn test_be09_cli_recovery_export_drops_own_reference_and_is_importable() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(4);
    let id = create(&store, &fixture.endpoint, &COMPACTING);
    seed(&fixture, &store, &id, &"x".repeat(3_000));
    let child = spawn(run(&store, &id, &crossing_input()));
    fixture.request();
    fixture.summary("Goal: test recovery drops own reference.");
    fixture.request();
    fixture.stream("done");
    let output = finished(child);
    assert!(output.status.success(), "{output:?}");

    let mut rec_json = record(&store, &id);
    let record_id = rec_json["history"][0]["record_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let hist_json = read_history_doc(&store, &id, &record_id);
    let orig_block_id = hist_json["replacements"][0]["originals"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    let call_id = causa::runtime::new_block_id();
    let res_id = causa::runtime::new_block_id();
    let tool_call_block = causa::kernel::ContextBlock::new(
        call_id,
        causa::kernel::BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "read_file",
            serde_json::json!({}),
        )),
        causa::kernel::BlockMeta::default(),
    );
    let mut out = causa::kernel::ToolOutput::new(serde_json::json!("preview of data"));
    out.artifact = Some(causa::kernel::ArtifactRef::new(
        format!("{record_id}/{orig_block_id}"),
        50,
        causa::kernel::ArtifactKind::FullOutput,
        true,
    ));
    let tool_res_payload = causa::kernel::ToolResultPayload::new(
        call_id,
        causa::kernel::ToolResultStatus::Succeeded,
        out,
    )
    .with_notes(vec![causa::kernel::TextPayload::new("pre-recovery note")]);
    let tool_res_block = causa::kernel::ContextBlock::new(
        res_id,
        causa::kernel::BlockContent::ToolResult(tool_res_payload),
        causa::kernel::BlockMeta::default(),
    );

    rec_json["context"]["blocks"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::to_value(&tool_call_block).unwrap());
    rec_json["context"]["blocks"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::to_value(&tool_res_block).unwrap());
    write_context_to_db(&store, &id, &rec_json["context"]);

    let recovery_dir = folder.path().join("recovery");
    std::fs::create_dir_all(&recovery_dir).unwrap();

    let mut cmd = run(&store, &id, "trigger save failure");
    cmd.arg("--recovery-dir").arg(&recovery_dir);
    let child = spawn(cmd);
    fixture.request();
    let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER test_block_save BEFORE UPDATE ON sessions
         BEGIN SELECT RAISE(ABORT, 'injected save failure'); END;",
    )
    .unwrap();
    drop(conn);
    fixture.stream("completed turn");
    let run_output = finished(child);

    assert_eq!(run_output.status.code(), Some(1), "{run_output:?}");
    let val = report(&run_output);
    assert_eq!(val["saved"], false);
    assert!(val["recovery_file"].is_string());
    let recovery_path = PathBuf::from(val["recovery_file"].as_str().unwrap());

    let recovered_context = tars_core::material::load(&recovery_path).unwrap();
    let res = recovered_context
        .blocks()
        .iter()
        .find_map(|b| match b.content() {
            causa::kernel::BlockContent::ToolResult(r) => Some(r),
            _ => None,
        })
        .expect("tool result in recovery file");
    assert!(res.output.artifact.is_none());
    assert!(
        res.notes
            .iter()
            .any(|n| n.0 == tars_core::material::DETACHED_HISTORY_NOTE)
    );

    let import_store = folder.path().join("imported_from_recovery");
    let imported_id = create(
        &import_store,
        "http://127.0.0.1:1/unused",
        &["--load", recovery_path.to_str().unwrap()],
    );
    let imported_record = record(&import_store, &imported_id);
    assert_eq!(
        imported_record["context"]["blocks"]
            .as_array()
            .unwrap()
            .len(),
        recovered_context.blocks().len()
    );
}

#[test]
fn test_be17_cli_session_history_marks_spilled_artifact_id_text_and_json() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(2);
    let id = create(&store, &fixture.endpoint, &COMPACTING);
    seed(&fixture, &store, &id, "initial message");

    let mut rec_json = record(&store, &id);
    let art_uuid = causa::runtime::new_block_id().0.to_string();
    let art_id = format!("artifact:{art_uuid}");
    insert_artifact_row(&store, &id, &art_uuid, b"full spilled output");

    let call_id = causa::runtime::new_block_id();
    let res_id = causa::runtime::new_block_id();
    let call_block = causa::kernel::ContextBlock::new(
        call_id,
        causa::kernel::BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "read_file",
            serde_json::json!({"path": "big.rs"}),
        )),
        causa::kernel::BlockMeta::default(),
    );
    let mut out = causa::kernel::ToolOutput::new(serde_json::Value::String(format!(
        "head\n[... output spilled: 19 bytes; full output: recall id={art_id} ...]\ntail"
    )));
    out.truncation = causa::kernel::Truncation::Middle;
    out.artifact = Some(causa::kernel::ArtifactRef::new(
        art_id.clone(),
        19,
        causa::kernel::ArtifactKind::FullOutput,
        true,
    ));
    let res_block = causa::kernel::ContextBlock::new(
        res_id,
        causa::kernel::BlockContent::ToolResult(causa::kernel::ToolResultPayload::new(
            call_id,
            causa::kernel::ToolResultStatus::Succeeded,
            out,
        )),
        causa::kernel::BlockMeta::default(),
    );
    let blocks = rec_json["context"]["blocks"].as_array_mut().unwrap();
    blocks.push(serde_json::to_value(&call_block).unwrap());
    blocks.push(serde_json::to_value(&res_block).unwrap());
    write_context_to_db(&store, &id, &rec_json["context"]);

    // 1. Human text mode marks the spilled artifact id on the tool result line
    let text_out = cli()
        .args(["session", "history", &id, "--no-config", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(text_out.status.success(), "{text_out:?}");
    let text = String::from_utf8_lossy(&text_out.stdout);
    assert!(
        text.contains(&format!(
            "[tool result · original] Succeeded · declaration {} · {art_id}",
            call_id.0
        )),
        "{text}"
    );

    // 2. --json mode carries artifact_id and preserves ExpandedBlock deserialization
    let json_out = cli()
        .args([
            "session",
            "history",
            &id,
            "--no-config",
            "--json",
            "--store",
        ])
        .arg(&store)
        .output()
        .unwrap();
    assert!(json_out.status.success(), "{json_out:?}");
    let expanded = history_blocks(&json_out);
    assert!(!expanded.is_empty());
    let json_lines: Vec<Value> = String::from_utf8(json_out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        json_lines
            .iter()
            .any(|v| v.get("artifact_id").and_then(|s| s.as_str()) == Some(art_id.as_str())),
        "JSON history must include artifact_id: {json_lines:?}"
    );
}

#[test]
fn test_be16_cli_export_materials_drops_spilled_artifact_and_imports() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(1);
    let id = create(&store, &fixture.endpoint, &COMPACTING);
    seed(&fixture, &store, &id, "initial message");

    let mut rec_json = record(&store, &id);
    let art_uuid = causa::runtime::new_block_id().0.to_string();
    let art_id = format!("artifact:{art_uuid}");
    insert_artifact_row(&store, &id, &art_uuid, b"full spilled output");

    let call_id = causa::runtime::new_block_id();
    let res_id = causa::runtime::new_block_id();
    let call_block = causa::kernel::ContextBlock::new(
        call_id,
        causa::kernel::BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
            "read_file",
            serde_json::json!({}),
        )),
        causa::kernel::BlockMeta::default(),
    );
    let mut out = causa::kernel::ToolOutput::new(serde_json::Value::String(format!(
        "head\n[... output spilled; recall id={art_id} ...]\ntail"
    )));
    out.truncation = causa::kernel::Truncation::Middle;
    out.artifact = Some(causa::kernel::ArtifactRef::new(
        art_id,
        19,
        causa::kernel::ArtifactKind::FullOutput,
        true,
    ));
    let res_block = causa::kernel::ContextBlock::new(
        res_id,
        causa::kernel::BlockContent::ToolResult(causa::kernel::ToolResultPayload::new(
            call_id,
            causa::kernel::ToolResultStatus::Succeeded,
            out,
        )),
        causa::kernel::BlockMeta::default(),
    );
    let blocks = rec_json["context"]["blocks"].as_array_mut().unwrap();
    blocks.push(serde_json::to_value(&call_block).unwrap());
    blocks.push(serde_json::to_value(&res_block).unwrap());
    let rec_file = folder.path().join("spilled-source.json");
    fs::write(&rec_file, serde_json::to_vec(&rec_json).unwrap()).unwrap();

    let exported = folder.path().join("exported_spilled.json");
    let export = cli()
        .args(["session", "export-materials", "--record"])
        .arg(&rec_file)
        .arg("--output")
        .arg(&exported)
        .output()
        .unwrap();
    assert!(export.status.success(), "{export:?}");

    let material = tars_core::material::load(&exported).unwrap();
    let exported_res = material
        .blocks()
        .iter()
        .find_map(|b| match b.content() {
            causa::kernel::BlockContent::ToolResult(r) => Some(r),
            _ => None,
        })
        .expect("exported tool result present");
    assert!(exported_res.output.artifact.is_none());
    assert!(
        exported_res
            .notes
            .iter()
            .any(|n| n.0 == tars_core::material::DETACHED_HISTORY_NOTE)
    );

    let imported_store = folder.path().join("imported_spilled");
    let imported_id = create(
        &imported_store,
        "http://127.0.0.1:1/unused",
        &["--load", exported.to_str().unwrap()],
    );
    let imported_record = record(&imported_store, &imported_id);
    assert_eq!(
        imported_record["context"]["blocks"]
            .as_array()
            .unwrap()
            .len(),
        material.blocks().len()
    );
}

#[test]
fn test_be19_cli_transient_run_with_window_trims_over_window_request_and_reports_stderr() {
    let folder = tempfile::tempdir().unwrap();
    let fixture = Fixture::new(1);
    fixture.stream("transient answer");

    let earlier_marker = "transient-earlier-marker ";
    let material_path = folder.path().join("material.json");
    let material_context = causa::kernel::Context::from_blocks(vec![
        causa::kernel::ContextBlock::text(
            causa::runtime::new_block_id(),
            causa::kernel::Role::User,
            earlier_marker.repeat(400),
        ),
        causa::kernel::ContextBlock::text(
            causa::runtime::new_block_id(),
            causa::kernel::Role::Assistant,
            "earlier transient reply",
        ),
    ])
    .unwrap();
    tars_core::material::save(&material_path, &material_context).unwrap();

    let output = cli()
        .args([
            "run",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            &fixture.endpoint,
            "--context-window",
            "1600",
            "--max-tokens",
            "200",
            "--load",
        ])
        .arg(&material_path)
        .args(["--input", "current transient question", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let value = report(&output);
    assert_eq!(value["status"], "completed");
    assert_eq!(value["text"], "transient answer");

    let sent = fixture.request().to_string();
    assert!(
        !sent.contains(earlier_marker),
        "earlier interval trimmed: {sent}"
    );
    assert!(sent.contains("You are running inside the TARS agent host."));
    assert!(sent.contains("current transient question"));

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("TARS: request frame temporarily trimmed"),
        "expected trim notice on stderr, got: {stderr}"
    );
}

#[test]
fn test_mc08_cli_session_history_and_show_with_micro_compact_and_missing_record() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(1);
    let id = create(
        &store,
        &fixture.endpoint,
        &["--context-window", "10000", "--auto-compact", "70"],
    );

    let mut rec_json = record(&store, &id);
    let blocks = rec_json["context"]["blocks"].as_array_mut().unwrap();
    blocks.push(
        serde_json::to_value(causa::kernel::ContextBlock::text(
            causa::runtime::new_block_id(),
            causa::kernel::Role::User,
            "read four files",
        ))
        .unwrap(),
    );
    let mut orig_result_ids = Vec::new();
    for idx in 0..4 {
        let call_id = causa::runtime::new_block_id();
        let res_id = causa::runtime::new_block_id();
        orig_result_ids.push(res_id);
        let call_block = causa::kernel::ContextBlock::new(
            call_id,
            causa::kernel::BlockContent::ToolCall(causa::kernel::ToolCallPayload::new(
                "read_file",
                json!({ "path": format!("src/mc08_{idx}.rs"), "start_line": 1, "max_lines": 100 }),
            )),
            causa::kernel::BlockMeta::default(),
        );
        let res_block = causa::kernel::ContextBlock::new(
            res_id,
            causa::kernel::BlockContent::ToolResult(causa::kernel::ToolResultPayload::new(
                call_id,
                causa::kernel::ToolResultStatus::Succeeded,
                causa::kernel::ToolOutput::new(json!({
                    "path": format!("src/mc08_{idx}.rs"),
                    "start_line": 1,
                    "end_line": 100,
                    "text": format!("MC08_ORIGINAL_TOOL_OUTPUT_{idx} {}", "m".repeat(4_800)),
                })),
            )),
            causa::kernel::BlockMeta::default(),
        );
        blocks.push(serde_json::to_value(&call_block).unwrap());
        blocks.push(serde_json::to_value(&res_block).unwrap());
    }
    blocks.push(
        serde_json::to_value(causa::kernel::ContextBlock::text(
            causa::runtime::new_block_id(),
            causa::kernel::Role::Assistant,
            "done reading files",
        ))
        .unwrap(),
    );
    write_context_to_db(&store, &id, &rec_json["context"]);

    // Run one turn: hot-cache Path A micro-compacts without any summary model call.
    let child = spawn(run(&store, &id, "continue after reads"));
    let req = fixture.request();
    assert!(!req.to_string().contains("Prepare a continuation brief"));
    fixture.stream("done after micro-compact");
    let run_out = finished(child);
    assert!(run_out.status.success(), "{run_out:?}");
    let run_stderr = String::from_utf8_lossy(&run_out.stderr);
    assert!(
        !run_stderr.contains("Micro-compacted"),
        "CLI stderr stays quiet for MicroCompacted: {run_stderr}"
    );

    let saved = record(&store, &id);
    let history = saved["history"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["kind"], "micro-compact");
    let mc_record_id = history[0]["record_id"].as_str().unwrap().to_owned();
    let expected_artifact_id = format!("{mc_record_id}/{}", orig_result_ids[0].0);

    // 1. `session show` counts the MicroCompact rewrite record.
    let show_json = cli()
        .args(["session", "show", &id, "--no-config", "--json", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(show_json.status.success(), "{show_json:?}");
    assert_eq!(report(&show_json)["history_records"], 1);
    let show_text = cli()
        .args(["session", "show", &id, "--no-config", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(show_text.status.success(), "{show_text:?}");
    assert!(String::from_utf8_lossy(&show_text.stdout).contains("1 history record(s)"));

    // 2. `session history --json` expands originals with micro_compacted status & artifact_id.
    let hist_json = cli()
        .args([
            "session",
            "history",
            &id,
            "--no-config",
            "--json",
            "--store",
        ])
        .arg(&store)
        .output()
        .unwrap();
    assert!(hist_json.status.success(), "{hist_json:?}");
    let expanded = history_blocks(&hist_json);
    assert!(
        expanded
            .iter()
            .any(|b| b.status
                == tars_core::compact::BlockStatus::MicroCompacted(mc_record_id.clone())),
        "{expanded:?}"
    );
    let json_lines: Vec<Value> = String::from_utf8(hist_json.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert!(
        json_lines.iter().any(|v| {
            v.get("status").and_then(Value::as_str) == Some("micro_compacted")
                && v.get("record_id").and_then(Value::as_str) == Some(mc_record_id.as_str())
                && v.get("artifact_id").and_then(Value::as_str)
                    == Some(expected_artifact_id.as_str())
        }),
        "JSON history must include micro_compacted status, record_id and artifact_id: {json_lines:?}"
    );
    assert!(
        json_lines
            .iter()
            .any(|v| v.to_string().contains("MC08_ORIGINAL_TOOL_OUTPUT_0")),
        "expanded JSON must restore original output"
    );

    // 3. `session history` text mode renders micro-compacted (<id>), artifact id, and full original.
    let hist_text = cli()
        .args(["session", "history", &id, "--no-config", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(hist_text.status.success(), "{hist_text:?}");
    let text = String::from_utf8_lossy(&hist_text.stdout);
    assert!(
        text.contains(&format!("[tool result · micro-compacted ({mc_record_id})]")),
        "{text}"
    );
    assert!(text.contains(&expected_artifact_id), "{text}");
    assert!(text.contains("MC08_ORIGINAL_TOOL_OUTPUT_0"), "{text}");

    // 4. Deleting the MicroCompact record row reports on stderr and exits 1.
    let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
    conn.execute(
        "DELETE FROM history_records WHERE session_id = ?1 AND agent_id = ?1 AND record_id = ?2",
        rusqlite::params![&id, &mc_record_id],
    )
    .unwrap();
    drop(conn);
    let broken = cli()
        .args([
            "session",
            "history",
            &id,
            "--no-config",
            "--json",
            "--store",
        ])
        .arg(&store)
        .output()
        .unwrap();
    assert_eq!(broken.status.code(), Some(1), "{broken:?}");
    let stderr = String::from_utf8_lossy(&broken.stderr);
    assert!(stderr.contains(&mc_record_id), "{stderr}");
    let broken_blocks = history_blocks(&broken);
    assert!(
        broken_blocks
            .iter()
            .any(|b| b.status == tars_core::compact::BlockStatus::Unreadable(mc_record_id.clone())),
        "{broken_blocks:?}"
    );
}

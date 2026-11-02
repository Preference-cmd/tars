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

fn record_path(store: &Path, id: &str) -> PathBuf {
    store.join("sessions").join(format!("{id}.json"))
}

fn record(store: &Path, id: &str) -> Value {
    serde_json::from_slice(&fs::read(record_path(store, id)).unwrap()).unwrap()
}

fn history_documents(output: &Output) -> Vec<tars_core::compact::HistorySegmentDocument> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| {
            serde_json::from_str(line)
                .unwrap_or_else(|error| panic!("history line {line}: {error}"))
        })
        .collect()
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
        "replaced history segments",
        "one tars-history-v1 document per line",
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
fn small_context_window_compacts_automatically_and_keeps_a_history_segment() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(2);
    let id = create(
        &store,
        &fixture.endpoint,
        &["--context-window", "500", "--auto-compact", "50"],
    );
    let long_input = "x".repeat(1_200);
    let child = spawn(run(&store, &id, &long_input));
    let summary_request = fixture.request();
    assert!(
        summary_request
            .to_string()
            .contains("Prepare a continuation brief"),
        "the first request must be the summary call: {summary_request}"
    );
    assert!(summary_request.to_string().contains("xxxxxxxxxx"));
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
    assert_eq!(report(&output)["revision"], 2);

    let saved = record(&store, &id);
    let history = saved["history"].as_array().unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["trigger"], "auto");
    assert_eq!(history[0]["blocks"], 1);
    let summary_block = history[0]["summary_block_id"].clone();
    let blocks = saved["context"]["blocks"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["meta"]["source"], "tars:compaction");
    assert_eq!(blocks[0]["id"], summary_block);
    assert_eq!(blocks[1]["content"]["value"]["role"], "assistant");

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
    let documents = history_documents(&history_output);
    assert_eq!(documents.len(), 1);
    let document = &documents[0];
    assert_eq!(document.format, "tars-history-v1");
    assert_eq!(document.conversation_id, id);
    assert_eq!(document.segment_id, history[0]["id"].as_str().unwrap());
    assert_eq!(document.trigger, tars_core::compact::HistoryTrigger::Auto);
    assert_eq!(document.context.blocks().len(), 1);
    assert!(document.estimate_before > document.estimate_after);
    assert_eq!(json!(document.summary_block_id.0), summary_block);
    assert_eq!(history[0]["created_at_ms"], document.created_at_ms);

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
    assert_eq!(history_documents(&last).len(), 1);

    let human = cli()
        .args(["session", "history", &id, "--no-config", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(human.status.success(), "{human:?}");
    let text = String::from_utf8_lossy(&human.stdout);
    for expected in [
        "Segment 1 of 1",
        "auto",
        "->",
        "Current material",
        "2 block(s)",
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
    assert_eq!(shown["history_segments"], 1);
    assert_eq!(shown["block_count"], 2);
    assert_eq!(shown["context_window"], 500);
    assert_eq!(shown["auto_compact"], true);
    assert_eq!(shown["compact_threshold"], 50);
    assert!(shown["context_estimate_tokens"].as_u64().unwrap() > 0);
    assert_eq!(shown["revision"], 2);
    let shown_text = cli()
        .args(["session", "show", &id, "--no-config", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert!(shown_text.status.success(), "{shown_text:?}");
    let shown_text = String::from_utf8_lossy(&shown_text.stdout);
    assert!(shown_text.contains("1 history segment(s)"), "{shown_text}");
    assert!(shown_text.contains("auto on"), "{shown_text}");
}

/// Export and `--load` import are defined to carry only the current Context,
/// never the replaced history segments.
#[test]
fn export_and_import_carry_only_the_current_context_after_compaction() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(2);
    let id = create(
        &store,
        &fixture.endpoint,
        &["--context-window", "500", "--auto-compact", "50"],
    );
    let child = spawn(run(&store, &id, &"x".repeat(1_200)));
    fixture.request();
    fixture.summary("Goal: finish the export fixture.");
    fixture.request();
    fixture.stream("exported answer");
    let output = finished(child);
    assert!(output.status.success(), "{output:?}");

    let exported = folder.path().join("exported.json");
    let export = cli()
        .args(["session", "export-materials", "--record"])
        .arg(record_path(&store, &id))
        .arg("--output")
        .arg(&exported)
        .output()
        .unwrap();
    assert!(export.status.success(), "{export:?}");
    let material = tars_core::material::load(&exported).unwrap();
    assert_eq!(material.blocks().len(), 2);
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
        2
    );
    assert!(
        imported_record
            .get("history")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    );
}

#[test]
fn manual_compaction_reports_trigger_estimates_and_segment_and_last_limits_history() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");
    let fixture = Fixture::new(4);
    let id = create(&store, &fixture.endpoint, &["--auto-compact", "off"]);
    let long_input = "m".repeat(600);
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
    assert_eq!(first["blocks"], 2);
    assert!(first["estimate_before"].as_u64() > first["estimate_after"].as_u64());
    let first_segment = first["segment_id"].as_str().unwrap().to_owned();
    let first_summary = first["summary_block_id"].as_str().unwrap().to_owned();
    let saved = record(&store, &id);
    assert_eq!(saved["context"]["blocks"].as_array().unwrap().len(), 1);
    assert_eq!(saved["context"]["blocks"][0]["id"], json!(first_summary));
    assert_eq!(saved["history"][0]["trigger"], "manual");

    // Material added after one compaction keeps the next replacement smaller.
    let child = spawn(run(&store, &id, "second manual question"));
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
    assert_eq!(history[0]["id"], first_segment.as_str());
    assert_eq!(history[0]["trigger"], "manual");
    assert_eq!(history[1]["trigger"], "manual");
    assert_eq!(history[1]["blocks"], 3);
    let second_segment = history[1]["id"].as_str().unwrap().to_owned();
    assert!(text.contains(&second_segment), "{text}");
    assert_eq!(saved["context"]["blocks"].as_array().unwrap().len(), 1);

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
    let documents = history_documents(&full);
    assert_eq!(documents.len(), 2);
    assert_eq!(documents[0].segment_id, first_segment);
    assert_eq!(documents[1].segment_id, second_segment);
    assert_eq!(documents[0].context.blocks().len(), 2);

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
    let last_documents = history_documents(&last);
    assert_eq!(last_documents.len(), 1);
    assert_eq!(last_documents[0].segment_id, second_segment);
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
    assert!(text.contains("2 segment(s)"), "{text}");
    assert!(text.contains("1 earlier not shown"), "{text}");
    assert!(text.contains("Segment 2 of 2"), "{text}");

    let show = cli()
        .args(["session", "show", &id, "--no-config", "--json", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    let shown = report(&show);
    assert_eq!(shown["history_segments"], 2);
    assert_eq!(shown["auto_compact"], false);
    assert_eq!(shown["compact_threshold"], 70);
    assert!(shown["context_window"].is_null());
    assert_eq!(shown["block_count"], 1);

    // An unreadable segment is reported in both output modes and fails the
    // command, while the readable segment is still printed.
    fs::write(
        store
            .join("sessions")
            .join(format!("{id}.history"))
            .join(format!("{first_segment}.json")),
        b"not json",
    )
    .unwrap();
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
    let documents = history_documents(&broken_json);
    assert_eq!(documents.len(), 1);
    assert_eq!(documents[0].segment_id, second_segment);
    let broken_human = cli()
        .args(["session", "history", &id, "--no-config", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert_eq!(broken_human.status.code(), Some(1), "{broken_human:?}");
    let text = String::from_utf8_lossy(&broken_human.stdout);
    assert!(
        text.contains("this history segment cannot be read"),
        "{text}"
    );
    assert!(text.contains("Segment 2 of 2"), "{text}");
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
    let before = fs::read(record_path(&repair_store, &unpaired)).unwrap();
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
    assert_eq!(
        fs::read(record_path(&repair_store, &unpaired)).unwrap(),
        before
    );
    assert!(
        !repair_store
            .join("sessions")
            .join(format!("{unpaired}.history"))
            .exists()
    );
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
    assert_eq!(shown["block_count"], 10);
    assert_eq!(shown["history_segments"], 0);

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

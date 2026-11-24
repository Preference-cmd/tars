use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::time::Duration;

#[path = "common/sse.rs"]
mod sse;

const KEY: &str = "fixture-secret";

fn cli(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tars"));
    command
        .env_remove("TARS_CONFIG")
        .env("TARS_HOME", home.join("tars-home"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("TARS_PRELUDE_KEY", KEY);
    command
}

fn json_out(output: &Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("{output:?}"))
}

fn write_skill(root: &Path, name: &str, description: &str, body: &str) {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
    )
    .unwrap();
}

fn server_once(body: Value) -> (String, mpsc::Receiver<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/fixture", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut socket = sse::accept_post(&listener).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut bytes = vec![];
        let split = loop {
            let mut chunk = [0; 4096];
            let count = socket.read(&mut chunk).unwrap();
            assert!(count > 0);
            bytes.extend_from_slice(&chunk[..count]);
            if let Some(index) = bytes.windows(4).position(|s| s == b"\r\n\r\n") {
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
        let req_body: Value = serde_json::from_slice(&bytes[split..split + length]).unwrap();
        tx.send(req_body).unwrap();
        let (content_type, reply_body) = sse::reply(200, body);
        let _ = write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply_body}",
            reply_body.len()
        );
    });
    (endpoint, rx)
}

#[test]
fn cli_prelude_and_skills_flags_reject_conflicts_and_missing_explicit_paths() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let existing_file = dir.path().join("exists.txt");
    fs::write(&existing_file, "ok").unwrap();
    let existing_dir = dir.path().join("skills");
    fs::create_dir(&existing_dir).unwrap();

    let base_create: &[&str] = &[
        "session",
        "create",
        "--no-config",
        "--store",
        store.to_str().unwrap(),
        "--provider",
        "openai-chat",
        "--model",
        "m",
        "--api-key-env",
        "TARS_PRELUDE_KEY",
    ];
    let base_run: &[&str] = &[
        "run",
        "--no-config",
        "--provider",
        "openai-chat",
        "--model",
        "m",
        "--api-key-env",
        "TARS_PRELUDE_KEY",
        "--input",
        "hi",
    ];

    let missing_sys = dir.path().join("missing.txt");
    let missing_dyn = dir.path().join("missing-dyn.txt");
    let missing_dir = dir.path().join("missing-dir");

    let conflict_cases: Vec<Vec<&str>> = vec![
        vec![
            "--system",
            "a",
            "--system-file",
            existing_file.to_str().unwrap(),
        ],
        vec![
            "--dynamic-context",
            "a",
            "--dynamic-context-file",
            existing_file.to_str().unwrap(),
        ],
        vec!["--no-skills", "--skill-dir", existing_dir.to_str().unwrap()],
        vec!["--system-file", missing_sys.to_str().unwrap()],
        vec!["--dynamic-context-file", missing_dyn.to_str().unwrap()],
        vec!["--skill-dir", missing_dir.to_str().unwrap()],
        vec!["--skill-dir", existing_file.to_str().unwrap()],
    ];

    for extra in conflict_cases {
        for base in [base_create, base_run] {
            let output = cli(dir.path()).args(base).args(&extra).output().unwrap();
            assert_eq!(output.status.code(), Some(2), "{extra:?}: {output:?}");
        }
    }
}

#[test]
fn cli_overrides_config_supports_no_config_and_repeatable_skill_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("workspace");
    fs::create_dir(&ws).unwrap();
    let ws_skills = ws.join(".tars/skills");
    fs::create_dir_all(&ws_skills).unwrap();
    write_skill(&ws_skills, "ws-only", "workspace skill", "ws body");
    write_skill(&ws_skills, "shared", "from workspace", "ws shared body");

    let cfg_skills = dir.path().join("cfg-skills");
    write_skill(&cfg_skills, "cfg-only", "config skill", "cfg body");
    write_skill(&cfg_skills, "shared", "from config", "cfg shared body");

    let extra1 = dir.path().join("extra1");
    write_skill(&extra1, "e1", "first extra", "e1 body");
    write_skill(&extra1, "shared", "from extra1", "extra1 shared body");

    let extra2 = dir.path().join("extra2");
    write_skill(&extra2, "e2", "second extra", "e2 body");
    write_skill(&extra2, "shared", "from extra2", "extra2 shared body");

    let sys_file = dir.path().join("sys.md");
    fs::write(&sys_file, "CLI_STATIC_FROM_FILE").unwrap();
    let dyn_file = dir.path().join("dyn.md");
    fs::write(&dyn_file, "CLI_DYNAMIC_FROM_FILE").unwrap();

    let cfg_path = dir.path().join("config.toml");
    fs::write(
        &cfg_path,
        r#"
version = 1
store = "store"
default_profile = "p"

[prelude]
system_prompt = "TOML_STATIC"
dynamic_context = "TOML_DYNAMIC"

[skills]
dirs = ["cfg-skills"]
include_workspace = true

[profiles.p]
provider = "openai-chat"
model = "m"
api_key_env = "TARS_PRELUDE_KEY"
"#,
    )
    .unwrap();

    // 1. CLI overrides TOML slots and appends repeatable --skill-dir in order (extra1 < extra2 < ws).
    let created = json_out(
        &cli(dir.path())
            .current_dir(dir.path())
            .args([
                "session",
                "create",
                "--config",
                cfg_path.to_str().unwrap(),
                "--workspace",
                "workspace",
                "--system-file",
                "sys.md",
                "--dynamic-context-file",
                "dyn.md",
                "--skill-dir",
                "extra1",
                "--skill-dir",
                "extra2",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    let id = created["conversation_id"].as_str().unwrap();

    let record: Value = {
        let conn = rusqlite::Connection::open(dir.path().join("store/tars.db")).unwrap();
        let (def_json, ctx_json): (String, String) = conn
            .query_row(
                "SELECT definition_json, context_json FROM agents WHERE session_id = ?1 AND agent_id = ?1",
                rusqlite::params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        json!({
            "definition": serde_json::from_str::<Value>(&def_json).unwrap(),
            "context": serde_json::from_str::<Value>(&ctx_json).unwrap(),
        })
    };
    assert_eq!(
        record["definition"]["prelude"]["static_custom"],
        "CLI_STATIC_FROM_FILE"
    );
    assert_eq!(
        record["definition"]["prelude"]["dynamic_custom"]["kind"],
        "file"
    );
    assert_eq!(
        record["definition"]["prelude"]["dynamic_custom"]["value"],
        dyn_file.canonicalize().unwrap().to_str().unwrap()
    );
    let dirs = record["definition"]["prelude"]["skill_dirs"]
        .as_array()
        .unwrap();
    assert_eq!(dirs.len(), 3);
    assert_eq!(
        dirs[0],
        cfg_skills.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(dirs[1], extra1.canonicalize().unwrap().to_str().unwrap());
    assert_eq!(dirs[2], extra2.canonicalize().unwrap().to_str().unwrap());

    let blocks = record["context"]["blocks"].as_array().unwrap();
    let skills_block = blocks
        .iter()
        .find(|b| b["meta"]["source"] == "tars:prelude:skills")
        .unwrap();
    let skills_text = skills_block["content"]["value"]["parts"][0]["value"]
        .as_str()
        .unwrap();
    assert!(skills_text.contains("from workspace"), "{skills_text}");
    assert!(!skills_text.contains("from extra2"), "{skills_text}");
    assert!(skills_text.contains("<name>e1</name>"));
    assert!(skills_text.contains("<name>e2</name>"));
    assert!(skills_text.contains("<name>cfg-only</name>"));
    assert!(skills_text.contains("<name>ws-only</name>"));

    // 2. --no-skills overrides config dirs and workspace skills, and works in --no-config mode too.
    let no_skills_store = dir.path().join("no-skills-store");
    let no_skills_created = json_out(
        &cli(dir.path())
            .current_dir(dir.path())
            .args([
                "session",
                "create",
                "--no-config",
                "--store",
                no_skills_store.to_str().unwrap(),
                "--provider",
                "openai-chat",
                "--model",
                "m",
                "--api-key-env",
                "TARS_PRELUDE_KEY",
                "--workspace",
                "workspace",
                "--system",
                "NO_CONFIG_STATIC",
                "--dynamic-context",
                "NO_CONFIG_DYNAMIC",
                "--no-skills",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    let id2 = no_skills_created["conversation_id"].as_str().unwrap();
    let record2: Value = {
        let conn = rusqlite::Connection::open(no_skills_store.join("tars.db")).unwrap();
        let (def_json, ctx_json): (String, String) = conn
            .query_row(
                "SELECT definition_json, context_json FROM agents WHERE session_id = ?1 AND agent_id = ?1",
                rusqlite::params![id2],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        json!({
            "definition": serde_json::from_str::<Value>(&def_json).unwrap(),
            "context": serde_json::from_str::<Value>(&ctx_json).unwrap(),
        })
    };
    assert_eq!(record2["definition"]["prelude"]["skills_enabled"], false);
    assert_eq!(
        record2["definition"]["prelude"]["static_custom"],
        "NO_CONFIG_STATIC"
    );
    assert_eq!(
        record2["definition"]["prelude"]["dynamic_custom"],
        json!({"kind": "inline", "value": "NO_CONFIG_DYNAMIC"})
    );
    let blocks2 = record2["context"]["blocks"].as_array().unwrap();
    assert!(
        blocks2
            .iter()
            .all(|b| b["meta"]["source"] != "tars:prelude:skills")
    );
}

#[test]
fn non_utf8_agents_md_fails_cleanly_without_leaving_dirty_files() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir(&ws).unwrap();
    fs::write(ws.join("AGENTS.md"), [0xff, 0xfe, 0x00]).unwrap();
    let store = dir.path().join("store");

    let out = cli(dir.path())
        .args([
            "session",
            "create",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--provider",
            "openai-chat",
            "--model",
            "m",
            "--api-key-env",
            "TARS_PRELUDE_KEY",
            "--workspace",
            ws.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not valid UTF-8"), "{stderr}");
    assert!(!store.exists(), "must not leave dirty store directory");
}

#[test]
fn malformed_skill_diagnostic_writes_stderr_only_without_polluting_json_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    write_skill(&skills, "good-skill", "valid skill", "body");
    let bad_dir = skills.join("bad-skill");
    fs::create_dir_all(&bad_dir).unwrap();
    fs::write(
        bad_dir.join("SKILL.md"),
        "---\nname: bad-skill\n---\nmissing description\n",
    )
    .unwrap();

    let store = dir.path().join("store");
    let created = cli(dir.path())
        .args([
            "session",
            "create",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--provider",
            "openai-chat",
            "--model",
            "m",
            "--api-key-env",
            "TARS_PRELUDE_KEY",
            "--skill-dir",
            skills.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(created.status.success(), "{created:?}");
    let created_json: Value = serde_json::from_slice(&created.stdout).unwrap();
    assert_eq!(created_json["status"], "created");
    let created_stderr = String::from_utf8_lossy(&created.stderr);
    assert!(
        created_stderr.contains("prelude warning"),
        "{created_stderr}"
    );
    assert!(created_stderr.contains("bad-skill"), "{created_stderr}");

    let (endpoint, _rx) = server_once(json!({
        "choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]
    }));
    let run_out = cli(dir.path())
        .args([
            "run",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "m",
            "--api-key-env",
            "TARS_PRELUDE_KEY",
            "--endpoint",
            &endpoint,
            "--skill-dir",
            skills.to_str().unwrap(),
            "--input",
            "hi",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(run_out.status.success(), "{run_out:?}");
    let run_json: Value = serde_json::from_slice(&run_out.stdout).unwrap();
    assert_eq!(run_json["status"], "completed");
    let run_stderr = String::from_utf8_lossy(&run_out.stderr);
    assert!(run_stderr.contains("prelude warning"), "{run_stderr}");
    assert!(run_stderr.contains("bad-skill"), "{run_stderr}");
}

#[test]
fn session_show_reports_prelude_and_skills_summary_without_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    fs::create_dir(&ws).unwrap();
    write_skill(
        &ws.join(".tars/skills"),
        "ws-skill",
        "workspace skill",
        "body",
    );
    let extra = dir.path().join("extra-skills");
    write_skill(&extra, "ext-skill", "extra skill", "body");
    let dyn_file = dir.path().join("dyn.txt");
    fs::write(&dyn_file, "DYN_TEXT").unwrap();
    let store = dir.path().join("store");

    let created = json_out(
        &cli(dir.path())
            .args([
                "session",
                "create",
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "--provider",
                "openai-chat",
                "--model",
                "m",
                "--api-key-env",
                "TARS_PRELUDE_KEY",
                "--workspace",
                ws.to_str().unwrap(),
                "--system",
                "SYS_TEXT",
                "--dynamic-context-file",
                dyn_file.to_str().unwrap(),
                "--skill-dir",
                extra.to_str().unwrap(),
                "--json",
            ])
            .output()
            .unwrap(),
    );
    let id = created["conversation_id"].as_str().unwrap();
    let before_bytes: (i64, String, String, String) = {
        let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
        conn.query_row(
            "SELECT s.revision, s.last_commit_id, a.definition_json, a.context_json
             FROM sessions s
             JOIN agents a ON s.session_id = a.session_id AND s.entry_agent_id = a.agent_id
             WHERE s.session_id = ?1",
            rusqlite::params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
    };

    // Run `session show` WITHOUT setting TARS_PRELUDE_KEY!
    let show_json = json_out(
        &Command::new(env!("CARGO_BIN_EXE_tars"))
            .env_remove("TARS_PRELUDE_KEY")
            .args([
                "session",
                "show",
                id,
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "--json",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(show_json["prelude"]["static_custom"], true);
    assert_eq!(show_json["prelude"]["dynamic_custom_kind"], "file");
    assert_eq!(
        show_json["prelude"]["dynamic_custom_file"],
        dyn_file.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(show_json["prelude"]["dynamic_custom_file_exists"], true);
    assert_eq!(show_json["prelude"]["skills_enabled"], true);
    assert_eq!(show_json["prelude"]["skill_dirs_count"], 1);
    assert_eq!(show_json["prelude"]["include_workspace_skills"], true);
    assert_eq!(show_json["prelude"]["skills_count"], 2);

    let show_text = Command::new(env!("CARGO_BIN_EXE_tars"))
        .env_remove("TARS_PRELUDE_KEY")
        .args([
            "session",
            "show",
            id,
            "--no-config",
            "--store",
            store.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(show_text.status.success(), "{show_text:?}");
    let text = String::from_utf8_lossy(&show_text.stdout);
    assert!(
        text.contains("Prelude: static_custom configured · dynamic_custom file"),
        "{text}"
    );
    assert!(text.contains("present"), "{text}");
    assert!(
        text.contains("Skills: enabled · 1 dir(s) · workspace on · 2 available"),
        "{text}"
    );

    // Record in SQLite is strictly untouched.
    let after_bytes: (i64, String, String, String) = {
        let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
        conn.query_row(
            "SELECT s.revision, s.last_commit_id, a.definition_json, a.context_json
             FROM sessions s
             JOIN agents a ON s.session_id = a.session_id AND s.entry_agent_id = a.agent_id
             WHERE s.session_id = ?1",
            rusqlite::params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap()
    };
    assert_eq!(after_bytes, before_bytes);

    // Also verify `session show` with inline dynamic_custom, no static_custom, and --no-skills.
    let created_inline = json_out(
        &cli(dir.path())
            .args([
                "session",
                "create",
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "--provider",
                "openai-chat",
                "--model",
                "m",
                "--api-key-env",
                "TARS_PRELUDE_KEY",
                "--workspace",
                ws.to_str().unwrap(),
                "--dynamic-context",
                "INLINE_DYN",
                "--no-skills",
                "--json",
            ])
            .output()
            .unwrap(),
    );
    let id_inline = created_inline["conversation_id"].as_str().unwrap();
    let show_inline = json_out(
        &Command::new(env!("CARGO_BIN_EXE_tars"))
            .env_remove("TARS_PRELUDE_KEY")
            .args([
                "session",
                "show",
                id_inline,
                "--no-config",
                "--store",
                store.to_str().unwrap(),
                "--json",
            ])
            .output()
            .unwrap(),
    );
    assert_eq!(show_inline["prelude"]["static_custom"], false);
    assert_eq!(show_inline["prelude"]["dynamic_custom_kind"], "inline");
    assert!(show_inline["prelude"]["dynamic_custom_file"].is_null());
    assert_eq!(show_inline["prelude"]["skills_enabled"], false);
    assert_eq!(show_inline["prelude"]["skills_count"], 0);
}

#[test]
fn transient_tars_run_injects_prelude_skills_and_custom_slots_without_writing_files() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("workspace");
    fs::create_dir(&ws).unwrap();
    fs::write(ws.join("AGENTS.md"), "WS_RULE_FROM_AGENTS_MD").unwrap();
    let skills = dir.path().join("skills");
    write_skill(&skills, "run-skill", "transient run skill", "skill body");

    let (endpoint, requests) = server_once(json!({
        "choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]
    }));

    let workdir = dir.path().join("cwd");
    fs::create_dir(&workdir).unwrap();
    let output = cli(dir.path())
        .current_dir(&workdir)
        .args([
            "run",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--api-key-env",
            "TARS_PRELUDE_KEY",
            "--endpoint",
            &endpoint,
            "--workspace",
            ws.to_str().unwrap(),
            "--system",
            "TRANSIENT_STATIC_CUSTOM",
            "--dynamic-context",
            "TRANSIENT_DYNAMIC_CUSTOM",
            "--skill-dir",
            skills.to_str().unwrap(),
            "--input",
            "hi",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let req = requests.recv_timeout(Duration::from_secs(5)).unwrap();
    let serialized = req.to_string();
    assert!(
        serialized.contains("TRANSIENT_STATIC_CUSTOM"),
        "{serialized}"
    );
    assert!(
        serialized.contains("WS_RULE_FROM_AGENTS_MD"),
        "{serialized}"
    );
    assert!(
        serialized.contains("TRANSIENT_DYNAMIC_CUSTOM"),
        "{serialized}"
    );
    assert!(serialized.contains("run-skill"), "{serialized}");
    assert!(serialized.contains("load_skill"), "{serialized}");
    assert_eq!(fs::read_dir(&workdir).unwrap().count(), 0);
}

//! CLI integration tests for `tars doctor`, session short ID / `latest`
//! selector resolution, `tars session export-markdown`, `tars completions`,
//! and dedicated `--help` outputs.

use causa::kernel::{
    BlockContent, BlockMeta, ContentPart, Context, ContextBlock, Role, TextPayload,
    ToolCallPayload, ToolOutput, ToolResultPayload, ToolResultStatus,
};
use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::time::Duration;

#[path = "common/sse.rs"]
mod sse;

const SECRET_TOKEN: &str = "sk-doctor-super-secret-value-987654321";

fn tars_bin() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tars"));
    command
        .env_remove("TARS_CONFIG")
        .env_remove("TARS_HOME")
        .env("TARS_TEST_KEY", SECRET_TOKEN);
    command
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

fn assert_no_secret(output: &Output) {
    assert!(
        !text(&output.stdout).contains(SECRET_TOKEN),
        "stdout leaked secret: {}",
        text(&output.stdout)
    );
    assert!(
        !text(&output.stderr).contains(SECRET_TOKEN),
        "stderr leaked secret: {}",
        text(&output.stderr)
    );
}

fn json_out(output: &Output) -> Value {
    assert_no_secret(output);
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("{output:?}"))
}

fn create_session(store: &Path, endpoint: &str, model: &str) -> String {
    let output = tars_bin()
        .args([
            "session",
            "create",
            "--no-config",
            "--store",
            store.to_str().unwrap(),
            "--provider",
            "openai-chat",
            "--model",
            model,
            "--api-key-env",
            "TARS_TEST_KEY",
            "--endpoint",
            endpoint,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    json_out(&output)["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn spawn_single_answer_server(answer: &str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!(
        "http://{}/v1/chat/completions",
        listener.local_addr().unwrap()
    );
    let answer = answer.to_owned();
    std::thread::spawn(move || {
        if let Ok(mut socket) = sse::accept_post(&listener) {
            let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
            let mut bytes = Vec::new();
            let split = loop {
                let mut buf = [0u8; 4096];
                let count = socket.read(&mut buf).unwrap_or(0);
                if count == 0 {
                    return;
                }
                bytes.extend_from_slice(&buf[..count]);
                if let Some(pos) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break pos + 4;
                }
            };
            let headers = String::from_utf8_lossy(&bytes[..split]);
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, val) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| val.trim().parse().ok())
                        .flatten()
                })
                .unwrap_or(0);
            while bytes.len() < split + length {
                let mut buf = [0u8; 4096];
                let count = socket.read(&mut buf).unwrap_or(0);
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buf[..count]);
            }
            let (content_type, body) = sse::reply(
                200,
                json!({
                    "choices": [{
                        "message": { "role": "assistant", "content": answer },
                        "finish_reason": "stop"
                    }]
                }),
            );
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(resp.as_bytes());
        }
    });
    endpoint
}

#[test]
fn doctor_cli_reports_ok_fail_invalid_args_and_never_leaks_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let recovery = dir.path().join("recovery");
    fs::create_dir_all(&store).unwrap();
    fs::create_dir_all(&recovery).unwrap();

    let healthy_cfg = dir.path().join("healthy.toml");
    fs::write(
        &healthy_cfg,
        format!(
            "version = 1\nstore = '{}'\nrecovery_dir = '{}'\ndefault_profile = 'ok'\n[profiles.ok]\nprovider = 'openai-chat'\nmodel = 'gpt-4o'\napi_key_env = 'TARS_TEST_KEY'\n",
            store.display(),
            recovery.display(),
        ),
    )
    .unwrap();

    // Healthy config exits 0 in both text and --json modes and never echoes secret value.
    let ok_text = tars_bin()
        .args(["doctor", "--config"])
        .arg(&healthy_cfg)
        .output()
        .unwrap();
    assert!(ok_text.status.success(), "{ok_text:?}");
    assert_no_secret(&ok_text);
    let stdout = text(&ok_text.stdout);
    assert!(stdout.contains("[ok] [runtime] TARS:"), "{stdout}");
    assert!(stdout.contains("Summary:"), "{stdout}");
    assert!(stdout.contains("0 fail"), "{stdout}");

    let ok_json = tars_bin()
        .args(["doctor", "--config"])
        .arg(&healthy_cfg)
        .arg("--json")
        .output()
        .unwrap();
    assert!(ok_json.status.success(), "{ok_json:?}");
    let val = json_out(&ok_json);
    assert!(val["ok"].as_u64().unwrap() > 0);
    assert_eq!(val["fail"], 0);
    let items = val["items"].as_array().unwrap();
    assert!(
        items
            .iter()
            .any(|item| item["category"] == "credentials" && item["status"] == "ok")
    );

    // Fail condition: Anthropic profile missing max_tokens and missing env var -> exit 1.
    let bad_cfg = dir.path().join("bad.toml");
    fs::write(
        &bad_cfg,
        format!(
            "version = 1\nstore = '{}'\nrecovery_dir = '{}'\ndefault_profile = 'bad'\n[profiles.bad]\nprovider = 'anthropic'\nmodel = 'claude-sonnet-4'\napi_key_env = 'TARS_MISSING_CRED_FOR_DOCTOR'\n",
            store.display(),
            recovery.display(),
        ),
    )
    .unwrap();

    let fail_text = tars_bin()
        .env_remove("TARS_MISSING_CRED_FOR_DOCTOR")
        .args(["doctor", "--config"])
        .arg(&bad_cfg)
        .output()
        .unwrap();
    assert_eq!(fail_text.status.code(), Some(1), "{fail_text:?}");
    assert_no_secret(&fail_text);
    assert!(text(&fail_text.stdout).contains("[fail]"), "{fail_text:?}");

    let fail_json = tars_bin()
        .env_remove("TARS_MISSING_CRED_FOR_DOCTOR")
        .args(["doctor", "--config"])
        .arg(&bad_cfg)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(fail_json.status.code(), Some(1), "{fail_json:?}");
    let fail_val = json_out(&fail_json);
    assert!(fail_val["fail"].as_u64().unwrap() >= 2, "{fail_val}");

    // Non-existent config file exits 1.
    let missing_cfg = tars_bin()
        .args(["doctor", "--config", "/nonexistent/tars-doctor.toml"])
        .output()
        .unwrap();
    assert_eq!(missing_cfg.status.code(), Some(1));

    // Invalid CLI flags exit 2 with precise diagnostics.
    for (args, expected) in [
        (vec!["doctor", "--bogus"], "unknown option: --bogus"),
        (
            vec!["doctor", "--config", "a.toml", "--no-config"],
            "--config and --no-config are mutually exclusive",
        ),
        (
            vec!["doctor", "--profile"],
            "option --profile requires a value",
        ),
        (
            vec!["doctor", "--store", ""],
            "option --store must not be empty",
        ),
        (
            vec!["doctor", "extra"],
            "unexpected positional argument: extra",
        ),
    ] {
        let out = tars_bin().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty());
        assert!(
            text(&out.stderr).contains(expected),
            "{args:?}: stderr={}",
            text(&out.stderr)
        );
    }
}

#[test]
fn session_short_id_prefix_and_latest_resolve_across_all_session_subcommands() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");

    // Seed two sessions with distinct prefixes and deterministic updated_at_ms.
    let endpoint = spawn_single_answer_server("hello from latest");
    let id_old = create_session(&store, &endpoint, "old-model");
    let id_new = create_session(&store, &endpoint, "new-model");

    // Ensure id_new has strictly newer updated_at_ms and find distinct prefixes.
    let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
    conn.execute(
        "UPDATE sessions SET updated_at_ms = 1000 WHERE session_id = ?1",
        rusqlite::params![&id_old],
    )
    .unwrap();
    conn.execute(
        "UPDATE sessions SET updated_at_ms = 2000 WHERE session_id = ?1",
        rusqlite::params![&id_new],
    )
    .unwrap();

    // Also insert two sessions sharing prefix "deadbeef" to test Ambiguous failure.
    let ambig_a = "deadbeef-0000-7000-8000-000000000001";
    let ambig_b = "deadbeef-0000-7000-8000-000000000002";
    let (policy_json, def_json, ctx_json): (String, String, String) = conn
        .query_row(
            "SELECT s.policy_json, a.definition_json, a.context_json
             FROM sessions s JOIN agents a ON s.session_id = a.session_id AND s.entry_agent_id = a.agent_id
             WHERE s.session_id = ?1",
            rusqlite::params![&id_old],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    for (idx, aid) in [ambig_a, ambig_b].iter().enumerate() {
        let ts = 500 + idx as i64;
        conn.execute(
            "INSERT INTO sessions (session_id, revision, last_commit_id, policy_json, entry_agent_id, created_at_ms, updated_at_ms)
             VALUES (?1, 1, ?1, ?2, ?1, ?3, ?3)",
            rusqlite::params![aid, &policy_json, ts],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agents (session_id, agent_id, name, preset_name, interaction, definition_json, material_revision, context_json, history_index_json, created_at_ms, updated_at_ms)
             VALUES (?1, ?1, 'main', NULL, 'reusable', ?2, 1, ?3, '[]', ?4, ?4)",
            rusqlite::params![aid, &def_json, &ctx_json, ts],
        )
        .unwrap();
    }
    drop(conn);

    // Build an 8-char uppercase prefix unique to id_new (or full unique prefix if UUIDv7 shares first 8).
    let unique_len = (8..=id_new.len())
        .find(|&n| !id_old[..n].eq_ignore_ascii_case(&id_new[..n]))
        .unwrap();
    let upper_prefix = id_new[..unique_len].to_ascii_uppercase();

    // `show` via uppercase unique prefix & `latest`.
    let show_prefix = tars_bin()
        .args(["session", "show", &upper_prefix, "--no-config", "--store"])
        .arg(&store)
        .arg("--json")
        .output()
        .unwrap();
    assert!(show_prefix.status.success(), "{show_prefix:?}");
    assert_eq!(json_out(&show_prefix)["conversation_id"], id_new);

    let show_latest = tars_bin()
        .args(["session", "show", "latest", "--no-config", "--store"])
        .arg(&store)
        .arg("--json")
        .output()
        .unwrap();
    assert!(show_latest.status.success(), "{show_latest:?}");
    assert_eq!(json_out(&show_latest)["conversation_id"], id_new);

    // `run` via `latest`.
    let run_latest = tars_bin()
        .args(["session", "run", "latest", "--no-config", "--store"])
        .arg(&store)
        .args(["--input", "hi", "--json"])
        .output()
        .unwrap();
    assert!(run_latest.status.success(), "{run_latest:?}");
    let run_val = json_out(&run_latest);
    assert_eq!(run_val["conversation_id"], id_new);
    assert_eq!(run_val["text"], "hello from latest");

    // `history` via uppercase prefix.
    let hist_prefix = tars_bin()
        .args([
            "session",
            "history",
            &upper_prefix,
            "--no-config",
            "--store",
        ])
        .arg(&store)
        .arg("--json")
        .output()
        .unwrap();
    assert!(hist_prefix.status.success(), "{hist_prefix:?}");
    assert!(text(&hist_prefix.stdout).contains("hello from latest"));

    // `rename` via `latest`.
    let rename_latest = tars_bin()
        .args(["session", "rename", "latest", "--no-config", "--store"])
        .arg(&store)
        .args(["--title", "Latest Title", "--json"])
        .output()
        .unwrap();
    assert!(rename_latest.status.success(), "{rename_latest:?}");
    assert_eq!(json_out(&rename_latest)["conversation_id"], id_new);
    assert_eq!(json_out(&rename_latest)["title"], "Latest Title");

    // `export-materials` via `latest`.
    let mat_out = dir.path().join("latest-materials.json");
    let exp_mat = tars_bin()
        .args([
            "session",
            "export-materials",
            "latest",
            "--no-config",
            "--store",
        ])
        .arg(&store)
        .arg("--output")
        .arg(&mat_out)
        .arg("--json")
        .output()
        .unwrap();
    assert!(exp_mat.status.success(), "{exp_mat:?}");
    assert_eq!(json_out(&exp_mat)["conversation_id"], id_new);
    assert!(mat_out.exists());

    // `export-markdown` via `latest`.
    let md_out = dir.path().join("latest.md");
    let exp_md = tars_bin()
        .args([
            "session",
            "export-markdown",
            "latest",
            "--no-config",
            "--store",
        ])
        .arg(&store)
        .arg("--output")
        .arg(&md_out)
        .arg("--json")
        .output()
        .unwrap();
    assert!(exp_md.status.success(), "{exp_md:?}");
    assert_eq!(json_out(&exp_md)["conversation_id"], id_new);
    assert!(
        fs::read_to_string(&md_out)
            .unwrap()
            .contains("Latest Title")
    );

    // `fork` via uppercase prefix.
    let fork_prefix = tars_bin()
        .args(["session", "fork", &upper_prefix, "--no-config", "--store"])
        .arg(&store)
        .arg("--json")
        .output()
        .unwrap();
    assert!(fork_prefix.status.success(), "{fork_prefix:?}");
    let fork_val = json_out(&fork_prefix);
    assert_eq!(fork_val["source_conversation_id"], id_new);
    let forked_id = fork_val["conversation_id"].as_str().unwrap().to_owned();

    // `delete` via unique prefix of the forked session.
    let fork_unique_len = (8..=forked_id.len())
        .find(|&n| {
            !forked_id[..n].eq_ignore_ascii_case(&id_new[..n])
                && !forked_id[..n].eq_ignore_ascii_case(&id_old[..n])
        })
        .unwrap();
    let fork_prefix_short = &forked_id[..fork_unique_len];
    let del_fork = tars_bin()
        .args([
            "session",
            "delete",
            fork_prefix_short,
            "--no-config",
            "--store",
        ])
        .arg(&store)
        .arg("--json")
        .output()
        .unwrap();
    assert!(del_fork.status.success(), "{del_fork:?}");
    assert_eq!(json_out(&del_fork)["conversation_id"], forked_id);

    // Ambiguous prefix `deadbeef` fails with exit 1 and lists candidates.
    for subcmd in ["show", "history", "export-markdown", "delete"] {
        let ambig = tars_bin()
            .args(["session", subcmd, "deadbeef", "--no-config", "--store"])
            .arg(&store)
            .arg("--json")
            .output()
            .unwrap();
        assert_eq!(ambig.status.code(), Some(1), "{subcmd}: {ambig:?}");
        let err_str = json_out(&ambig)["error"].as_str().unwrap().to_owned();
        assert!(err_str.contains("ambiguous"), "{err_str}");
        assert!(err_str.contains("deadbeef"), "{err_str}");
    }

    // Ambiguous prefix in `session run --stream-json` emits NDJSON result with exit 1.
    let run_ambig = tars_bin()
        .args(["session", "run", "deadbeef", "--no-config", "--store"])
        .arg(&store)
        .args(["--input", "hi", "--stream-json"])
        .output()
        .unwrap();
    assert_eq!(run_ambig.status.code(), Some(1));
    let stream_val = json_out(&run_ambig);
    assert_eq!(stream_val["type"], "result");
    assert_eq!(stream_val["status"], "not_started");
    assert!(stream_val["error"].as_str().unwrap().contains("ambiguous"));

    // Empty selector "" exits 2 across subcommands.
    for args in [
        vec!["session", "show", ""],
        vec!["session", "history", ""],
        vec!["session", "rename", "", "--title", "x"],
        vec!["session", "delete", ""],
        vec!["session", "fork", ""],
        vec!["session", "export-markdown", ""],
        vec!["session", "run", "", "--input", "hi"],
    ] {
        let out = tars_bin()
            .args(&args)
            .args(["--no-config", "--store"])
            .arg(&store)
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty());
    }

    // Unknown prefix exits 1.
    let unknown = tars_bin()
        .args(["session", "show", "ffffffff", "--no-config", "--store"])
        .arg(&store)
        .output()
        .unwrap();
    assert_eq!(unknown.status.code(), Some(1));
    assert!(text(&unknown.stderr).contains("session not found"));
}

#[test]
fn export_markdown_supports_stdout_file_last_agent_dynamic_fences_and_unreadable_records() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let conv_id = "01940000-0000-7000-8000-000000000010";
    let child_id = "01940000-0000-7000-8000-000000000020";
    let hist_id = "01940000-0000-7000-8000-000000000030";

    // Import a legacy session with 1 history record so we can test both clean expansion
    // and unreadable history records.
    let call_id = causa::runtime::new_block_id();
    let res_id = causa::runtime::new_block_id();
    let orig_user = ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::Message {
            role: Role::User,
            parts: vec![ContentPart::Text(TextPayload::new("archived question"))],
        },
        BlockMeta::default(),
    );
    let brief_block = ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::Message {
            role: Role::Assistant,
            parts: vec![ContentPart::Text(TextPayload::new("summary brief"))],
        },
        BlockMeta::from_source(tars_core::compact::SUMMARY_SOURCE),
    );
    let prelude_block = ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::Message {
            role: Role::System,
            parts: vec![ContentPart::Text(TextPayload::new("INTERNAL PRELUDE"))],
        },
        BlockMeta::from_source(tars_core::prelude::PRELUDE_BASE_SOURCE),
    );
    let live_user = ContextBlock::new(
        causa::runtime::new_block_id(),
        BlockContent::Message {
            role: Role::User,
            parts: vec![ContentPart::Text(TextPayload::new("show fence test"))],
        },
        BlockMeta::default(),
    );
    let call_block = ContextBlock::new(
        call_id,
        BlockContent::ToolCall(ToolCallPayload::new(
            "read_file",
            json!({ "path": "demo.md" }),
        )),
        BlockMeta::default(),
    );
    let res_block = ContextBlock::new(
        res_id,
        BlockContent::ToolResult(ToolResultPayload::new(
            call_id,
            ToolResultStatus::Succeeded,
            ToolOutput::new(json!("before\n```\ninner\n```\nafter")),
        )),
        BlockMeta::default(),
    );

    let history_doc = json!({
        "format": "tars-history-v1.2",
        "conversation_id": conv_id,
        "record_id": hist_id,
        "created_at_ms": 500,
        "trigger": "auto",
        "kind": "summarize",
        "estimate_before": 500,
        "estimate_after": 100,
        "replacements": [{
            "range": { "start": 0, "end": 1 },
            "originals": [orig_user],
            "inserted_ids": [brief_block.id()],
        }],
    });
    let legacy_json = json!({
        "format": "tars-session-v2.4",
        "conversation_id": conv_id,
        "revision": 2,
        "last_commit_id": "01940000-0000-7000-8000-000000000011",
        "title": "Markdown Demo",
        "updated_at_ms": 1700000000000i64,
        "definition": {
            "provider": "openai-chat",
            "model": "root-model",
            "endpoint": null,
            "credential": { "environment": "TARS_TEST_KEY" },
            "work_timeout_secs": null,
            "retained_work_capacity": 16,
            "max_model_rounds": 8,
            "max_tool_calls": 4,
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
        "context": { "blocks": [prelude_block, brief_block, live_user, call_block, res_block] },
        "history": [{
            "record_id": hist_id,
            "created_at_ms": 500,
            "trigger": "auto",
            "kind": "summarize",
            "removed_block_count": 1,
            "inserted_block_ids": [brief_block.id()]
        }]
    });

    let legacy_dir = dir.path().join("legacy");
    let legacy_hist_dir = legacy_dir.join(format!("{conv_id}.history"));
    fs::create_dir_all(&legacy_hist_dir).unwrap();
    let legacy_record = legacy_dir.join(format!("{conv_id}.json"));
    fs::write(&legacy_record, serde_json::to_vec(&legacy_json).unwrap()).unwrap();
    fs::write(
        legacy_hist_dir.join(format!("{hist_id}.json")),
        serde_json::to_vec(&history_doc).unwrap(),
    )
    .unwrap();

    let import_out = tars_bin()
        .args(["session", "import-legacy", "--no-config", "--record"])
        .arg(&legacy_record)
        .arg("--store")
        .arg(&store)
        .output()
        .unwrap();
    assert!(import_out.status.success(), "{import_out:?}");

    // Insert a child agent with its own frozen model and conversation blocks.
    let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
    let root_def_json: String = conn
        .query_row(
            "SELECT definition_json FROM agents WHERE session_id = ?1 AND agent_id = ?1",
            rusqlite::params![conv_id],
            |r| r.get(0),
        )
        .unwrap();
    let mut child_def: Value = serde_json::from_str(&root_def_json).unwrap();
    child_def["model"] = json!("child-specialist-model");
    let child_ctx = Context::from_blocks(vec![
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::Message {
                role: Role::User,
                parts: vec![ContentPart::Text(TextPayload::new("child inspect prompt"))],
            },
            BlockMeta::default(),
        ),
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::Message {
                role: Role::Assistant,
                parts: vec![ContentPart::Text(TextPayload::new("child inspect answer"))],
            },
            BlockMeta::default(),
        ),
    ])
    .unwrap();
    conn.execute(
        "INSERT INTO agents (session_id, agent_id, name, preset_name, interaction, definition_json, material_revision, context_json, history_index_json, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, 'specialist', 'explore', 'reusable', ?3, 1, ?4, '[]', 1000, 1000)",
        rusqlite::params![
            conv_id,
            child_id,
            child_def.to_string(),
            serde_json::to_string(&child_ctx).unwrap(),
        ],
    )
    .unwrap();
    drop(conn);

    // 1. stdout text mode: skips prelude, expands history, uses dynamic 4-backtick fence for ``` in tool output.
    let md_stdout = tars_bin()
        .args([
            "session",
            "export-markdown",
            "01940000",
            "--no-config",
            "--store",
        ])
        .arg(&store)
        .output()
        .unwrap();
    assert!(md_stdout.status.success(), "{md_stdout:?}");
    let md = text(&md_stdout.stdout);
    assert!(md.starts_with("# Markdown Demo\n"), "{md}");
    assert!(md.contains("- **Model:** `root-model`\n"), "{md}");
    assert!(!md.contains("INTERNAL PRELUDE"), "{md}");
    assert!(md.contains("archived question"), "{md}");
    assert!(
        md.contains("````text\nbefore\n```\ninner\n```\nafter\n````\n"),
        "{md}"
    );

    // 2. stdout --json mode.
    let md_json = tars_bin()
        .args([
            "session",
            "export-markdown",
            "latest",
            "--no-config",
            "--store",
        ])
        .arg(&store)
        .arg("--json")
        .output()
        .unwrap();
    assert!(md_json.status.success(), "{md_json:?}");
    let json_val = json_out(&md_json);
    assert_eq!(json_val["status"], "exported");
    assert_eq!(json_val["conversation_id"], conv_id);
    assert!(json_val["agent"].is_null());
    assert_eq!(json_val["unreadable_records"], 0);
    assert!(
        json_val["markdown"]
            .as_str()
            .unwrap()
            .contains("Markdown Demo")
    );

    // 3. --last 1 restricts blocks exported.
    let md_last = tars_bin()
        .args([
            "session",
            "export-markdown",
            conv_id,
            "--last",
            "1",
            "--no-config",
            "--store",
        ])
        .arg(&store)
        .output()
        .unwrap();
    assert!(md_last.status.success(), "{md_last:?}");
    let last_text = text(&md_last.stdout);
    assert!(last_text.contains("### Tool Result (ok)"), "{last_text}");
    assert!(!last_text.contains("archived question"), "{last_text}");

    // Invalid --last 0 exits 2.
    let bad_last = tars_bin()
        .args([
            "session",
            "export-markdown",
            conv_id,
            "--last",
            "0",
            "--no-config",
            "--store",
        ])
        .arg(&store)
        .output()
        .unwrap();
    assert_eq!(bad_last.status.code(), Some(2));

    // 4. --agent <child> exports child conversation with child's model & agent label.
    let md_child = tars_bin()
        .args([
            "session",
            "export-markdown",
            conv_id,
            "--agent",
            "specialist",
            "--no-config",
            "--store",
        ])
        .arg(&store)
        .arg("--json")
        .output()
        .unwrap();
    assert!(md_child.status.success(), "{md_child:?}");
    let child_val = json_out(&md_child);
    assert_eq!(child_val["agent"], "specialist");
    let child_md = child_val["markdown"].as_str().unwrap();
    assert!(
        child_md.contains("- **Model:** `child-specialist-model`\n"),
        "{child_md}"
    );
    assert!(child_md.contains("- **Agent:** specialist\n"), "{child_md}");
    assert!(child_md.contains("child inspect prompt"), "{child_md}");
    assert!(child_md.contains("child inspect answer"), "{child_md}");

    // 5. --output <new-file> text & --json, plus refusing existing file without overwriting.
    let out_file_text = dir.path().join("exported.md");
    let file_text_res = tars_bin()
        .args([
            "session",
            "export-markdown",
            conv_id,
            "--no-config",
            "--store",
        ])
        .arg(&store)
        .arg("--output")
        .arg(&out_file_text)
        .output()
        .unwrap();
    assert!(file_text_res.status.success(), "{file_text_res:?}");
    assert_eq!(
        text(&file_text_res.stdout),
        format!("{}\n", out_file_text.display())
    );
    assert!(
        fs::read_to_string(&out_file_text)
            .unwrap()
            .contains("Markdown Demo")
    );

    let out_file_json = dir.path().join("exported-json.md");
    let file_json_res = tars_bin()
        .args([
            "session",
            "export-markdown",
            conv_id,
            "--no-config",
            "--store",
        ])
        .arg(&store)
        .arg("--output")
        .arg(&out_file_json)
        .arg("--json")
        .output()
        .unwrap();
    assert!(file_json_res.status.success(), "{file_json_res:?}");
    let fj_val = json_out(&file_json_res);
    assert_eq!(fj_val["status"], "exported");
    assert_eq!(fj_val["path"], out_file_json.to_str().unwrap());

    // Refuses overwrite of existing file and exits 1.
    fs::write(&out_file_text, "sentinel content").unwrap();
    let overwrite_err = tars_bin()
        .args([
            "session",
            "export-markdown",
            conv_id,
            "--no-config",
            "--store",
        ])
        .arg(&store)
        .arg("--output")
        .arg(&out_file_text)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(overwrite_err.status.code(), Some(1), "{overwrite_err:?}");
    assert!(
        json_out(&overwrite_err)["error"]
            .as_str()
            .unwrap()
            .contains("already exists")
    );
    assert_eq!(
        fs::read_to_string(&out_file_text).unwrap(),
        "sentinel content"
    );

    // 6. Corrupt history record exits 1 after outputting readable content and reporting unreadable record on stderr.
    let conn = rusqlite::Connection::open(store.join("tars.db")).unwrap();
    conn.execute(
        "UPDATE history_records SET document_json = 'corrupt-json' WHERE session_id = ?1 AND record_id = ?2",
        rusqlite::params![conv_id, hist_id],
    )
    .unwrap();
    drop(conn);

    let corrupt_out = tars_bin()
        .args([
            "session",
            "export-markdown",
            conv_id,
            "--no-config",
            "--store",
        ])
        .arg(&store)
        .arg("--json")
        .output()
        .unwrap();
    assert_eq!(corrupt_out.status.code(), Some(1), "{corrupt_out:?}");
    let corrupt_val = json_out(&corrupt_out);
    assert_eq!(corrupt_val["unreadable_records"], 1);
    assert!(
        corrupt_val["markdown"]
            .as_str()
            .unwrap()
            .contains("show fence test")
    );
    assert!(
        text(&corrupt_out.stderr).contains(hist_id),
        "stderr={}",
        text(&corrupt_out.stderr)
    );
}

#[test]
fn completions_and_dedicated_help_outputs_match_contract() {
    for (shell, expected_tokens) in [
        (
            "bash",
            vec![
                "complete -F _tars_completions tars",
                "doctor",
                "completions",
                "export-markdown",
                "--print",
                "-p",
            ],
        ),
        (
            "zsh",
            vec![
                "#compdef tars",
                "compdef _tars tars",
                "doctor",
                "completions",
                "export-markdown",
                "--print",
                "-p",
            ],
        ),
        (
            "fish",
            vec![
                "complete -c tars",
                "doctor",
                "completions",
                "export-markdown",
                "-l print",
                "-s p",
            ],
        ),
    ] {
        let out = tars_bin().args(["completions", shell]).output().unwrap();
        assert!(out.status.success(), "{shell}: {out:?}");
        assert!(out.stderr.is_empty(), "{shell}: {out:?}");
        let script = text(&out.stdout);
        for token in expected_tokens {
            assert!(
                script.contains(token),
                "{shell} completion missing '{token}'"
            );
        }
    }

    // Counterexamples for `tars completions` exit 2.
    for (args, expected) in [
        (vec!["completions"], "shell argument is required"),
        (
            vec!["completions", "powershell"],
            "unsupported shell: powershell",
        ),
        (vec!["completions", "--bogus"], "unknown option: --bogus"),
        (
            vec!["completions", "bash", "extra"],
            "unexpected positional argument: extra",
        ),
    ] {
        let out = tars_bin().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(out.stdout.is_empty());
        assert!(text(&out.stderr).contains(expected), "{out:?}");
    }

    // Dedicated `--help` and `-h` outputs.
    for (args, required) in [
        (
            vec!["doctor", "--help"],
            vec!["Usage: tars doctor", "--workspace", "--json"],
        ),
        (
            vec!["doctor", "-h"],
            vec!["Usage: tars doctor", "--recovery-dir"],
        ),
        (
            vec!["completions", "--help"],
            vec!["Usage: tars completions <bash | zsh | fish>"],
        ),
        (
            vec!["completions", "-h"],
            vec!["Usage: tars completions <bash | zsh | fish>"],
        ),
        (
            vec!["session", "export-markdown", "--help"],
            vec![
                "Usage: tars session export-markdown",
                "--agent",
                "--last",
                "--output",
            ],
        ),
        (
            vec!["session", "export-markdown", "-h"],
            vec!["Usage: tars session export-markdown"],
        ),
    ] {
        let out = tars_bin().args(&args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        let stdout = text(&out.stdout);
        for needle in required {
            assert!(stdout.contains(needle), "{args:?}: {stdout}");
        }
    }
}

#[test]
fn doctor_cli_inspects_sibling_credentials_store_and_flags_insecure_permissions() {
    use tars_core::config::CredentialsStore;

    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let recovery = dir.path().join("recovery");
    fs::create_dir_all(&store).unwrap();
    fs::create_dir_all(&recovery).unwrap();

    let cfg_path = dir.path().join("config.toml");
    let creds_path = dir.path().join("credentials.toml");
    fs::write(
        &cfg_path,
        format!(
            "version = 1\nstore = '{}'\nrecovery_dir = '{}'\ndefault_profile = 'work'\n[profiles.work]\nprovider = 'openai-chat'\nmodel = 'gpt-4o'\n",
            store.display(),
            recovery.display(),
        ),
    )
    .unwrap();

    CredentialsStore::set(&creds_path, "work", SECRET_TOKEN).unwrap();

    let ok_out = tars_bin()
        .args(["doctor", "--config"])
        .arg(&cfg_path)
        .arg("--json")
        .output()
        .unwrap();
    assert!(ok_out.status.success(), "{ok_out:?}");
    let ok_val = json_out(&ok_out);
    assert_eq!(ok_val["fail"], 0);
    let items = ok_val["items"].as_array().unwrap();
    assert!(
        items
            .iter()
            .any(|i| i["label"] == "Credentials store" && i["status"] == "ok"),
        "{items:?}"
    );
    assert!(
        items
            .iter()
            .any(|i| i["label"] == "Store work" && i["status"] == "ok"),
        "{items:?}"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&creds_path, fs::Permissions::from_mode(0o644)).unwrap();
        let fail_out = tars_bin()
            .args(["doctor", "--config"])
            .arg(&cfg_path)
            .output()
            .unwrap();
        assert_eq!(fail_out.status.code(), Some(1), "{fail_out:?}");
        assert_no_secret(&fail_out);
        let stdout = text(&fail_out.stdout);
        assert!(
            stdout.contains("[fail] [credentials] Credentials store"),
            "{stdout}"
        );
        assert!(stdout.contains("chmod 600"), "{stdout}");
    }
}

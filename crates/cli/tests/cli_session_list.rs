use serde_json::Value;
use std::fs;
use std::process::Command;

#[allow(dead_code)]
#[path = "common/tools.rs"]
mod tools;

const KEY: &str = "fixture-secret";

fn report(output: &std::process::Output) -> Value {
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    serde_json::from_str(stdout).unwrap()
}

fn create_session(
    store: &std::path::Path,
    model: &str,
    prompt_cache: Option<&str>,
    endpoint: Option<&str>,
) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tars"));
    cmd.args([
        "session",
        "create",
        "--store",
        store.to_str().unwrap(),
        "--json",
        "--provider",
        "openai-chat",
        "--model",
        model,
        "--api-key-env",
        "TARS_TEST_KEY",
    ]);
    if let Some(pc) = prompt_cache {
        cmd.args(["--prompt-cache", pc]);
    }
    if let Some(endpoint) = endpoint {
        cmd.args(["--endpoint", endpoint]);
    }
    let output = cmd.env("TARS_TEST_KEY", KEY).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    report(&output)["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn test_be23_cli_session_list_ordering_and_fields() {
    let folder = tempfile::tempdir().unwrap();
    let store = folder.path().join("store");

    // Create 3 sessions, oldest first; the first one talks to a model.
    let model = tools::Fixture::with_responses("openai-chat", vec![tools::answer("openai-chat")]);
    let id1 = create_session(&store, "model-1", None, Some(&model.endpoint));
    std::thread::sleep(std::time::Duration::from_millis(15));
    let id2 = create_session(&store, "model-2", Some("off"), None);
    std::thread::sleep(std::time::Duration::from_millis(15));
    let id3 = create_session(&store, "model-3", Some("on"), None);
    std::thread::sleep(std::time::Duration::from_millis(15));

    // A turn on the oldest session gives it a title and the latest activity.
    let output = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(["session", "run", &id1, "--store", store.to_str().unwrap()])
        .args([
            "--input",
            "\n   Fix the   login bug  \nwith details",
            "--json",
        ])
        .env("TARS_TEST_KEY", KEY)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    // Create an invalid session file
    let bad_id = "00000000-0000-0000-0000-000000000002";
    let bad_path = store.join("sessions").join(format!("{bad_id}.json"));
    fs::write(&bad_path, b"not valid json").unwrap();

    // 1. Test `session list --json`
    let output = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "list",
            "--store",
            store.to_str().unwrap(),
            "--json",
        ])
        .env("TARS_TEST_KEY", KEY)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let rep = report(&output);
    let sessions = rep["sessions"].as_array().unwrap();

    // 3 valid + 1 invalid = 4 sessions
    assert_eq!(sessions.len(), 4);

    // Valid sessions are sorted by last activity, newest first, not by
    // creation: the session that ran a turn comes first.
    assert_eq!(sessions[0]["conversation_id"], id1);
    assert_eq!(sessions[1]["conversation_id"], id3);
    assert_eq!(sessions[2]["conversation_id"], id2);

    // Invalid session is at the end
    assert_eq!(sessions[3]["conversation_id"], bad_id);
    assert!(sessions[3]["record_error"].is_string());

    // Check fields on valid sessions
    assert!(sessions[0]["created_at_ms"].as_u64().is_some());
    assert!(sessions[0]["updated_at_ms"].as_u64().is_some());
    assert_eq!(sessions[0]["title"], "Fix the login bug");
    assert!(sessions[1].get("title").is_none_or(Value::is_null));
    let created = sessions[0]["created_at_ms"].as_u64().unwrap();
    let u1 = sessions[0]["updated_at_ms"].as_u64().unwrap();
    let u3 = sessions[1]["updated_at_ms"].as_u64().unwrap();
    let u2 = sessions[2]["updated_at_ms"].as_u64().unwrap();
    assert!(u1 > u3 && u3 > u2, "{u1} > {u3} > {u2}");
    assert!(u1 > created, "a turn moves the time past creation");

    // 2. Test `session list` plain text
    let text_output = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(["session", "list", "--store", store.to_str().unwrap()])
        .env("TARS_TEST_KEY", KEY)
        .output()
        .unwrap();
    assert_eq!(text_output.status.code(), Some(1));
    let text = String::from_utf8(text_output.stdout).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 4);
    // Line order: id1, id3, id2, bad_id
    assert!(lines[0].starts_with(&id1));
    assert!(lines[0].contains("Fix the login bug"));
    assert!(
        lines[0].contains(&tars_core::format_timestamp_utc(u1)),
        "{}",
        lines[0]
    );
    assert!(lines[1].starts_with(&id3));
    assert!(lines[2].starts_with(&id2));
    assert!(lines[3].starts_with(bad_id));
    assert!(lines[3].contains("invalid"));

    // 3. Test `session show`
    let show_output = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(["session", "show", &id1, "--store", store.to_str().unwrap()])
        .env("TARS_TEST_KEY", KEY)
        .output()
        .unwrap();
    assert!(show_output.status.success(), "{show_output:?}");
    let show_text = String::from_utf8(show_output.stdout).unwrap();
    assert!(
        show_text.contains("Title: Fix the login bug"),
        "{show_text}"
    );
    assert!(show_text.contains(&format!(
        "Created: {}",
        tars_core::format_timestamp_utc(created)
    )));
    assert!(show_text.contains(&format!(
        "Last activity: {}",
        tars_core::format_timestamp_utc(u1)
    )));
    assert!(show_text.contains("Prompt cache: on"));

    // Test `session show --json`
    let show_json_output = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args([
            "session",
            "show",
            &id2,
            "--store",
            store.to_str().unwrap(),
            "--json",
        ])
        .env("TARS_TEST_KEY", KEY)
        .output()
        .unwrap();
    assert!(show_json_output.status.success(), "{show_json_output:?}");
    let show_rep = report(&show_json_output);
    assert!(show_rep.get("title").is_none_or(Value::is_null));
    assert_eq!(show_rep["prompt_cache"], false);
    assert!(show_rep["created_at_ms"].as_u64().is_some());
    assert!(show_rep["updated_at_ms"].as_u64().is_some());
}

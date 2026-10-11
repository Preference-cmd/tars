//! Real CLI processes preview/apply a saved incomplete exchange without model I/O.
use causa::kernel::{BlockContent, BlockMeta, Context, ContextBlock, ToolCallPayload};
use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};
#[path = "../../tarz-core/tests/common/workspace_http.rs"]
mod wire;

fn command(store: &Path, id: &str) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tarz"));
    command
        .args(["session", "repair", id, "--no-config", "--json", "--store"])
        .arg(store);
    command
}
fn report(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| panic!("{output:?}"))
}
fn create(dir: &Path, provider: &str, endpoint: &str) -> (String, std::path::PathBuf) {
    let store = dir.join("store");
    let workspace = dir.join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join("evidence.txt"),
        "new read returns this actual marker\n",
    )
    .unwrap();
    let context = Context::from_blocks(vec![
        tarz_core::application::text_input("old input"),
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::ToolCall(ToolCallPayload {
                tool_name: "read_file".into(),
                arguments: serde_json::json!({"path":"evidence.txt"}),
            }),
            BlockMeta {
                provider_call_id: Some("old-unpaired-wire-id".into()),
                ..Default::default()
            },
        ),
    ])
    .unwrap();
    let input = dir.join("input.json");
    tarz_core::material::save_new(&input, &context).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_tarz"))
        .args([
            "session",
            "create",
            "--no-config",
            "--json",
            "--provider",
            provider,
            "--model",
            "fixture",
            "--api-key-env",
            "PATH",
            "--endpoint",
            endpoint,
            "--tools",
            "read_file",
            "--workspace",
        ])
        .arg(&workspace)
        .arg("--store")
        .arg(&store)
        .arg("--load")
        .arg(input)
        .args(if provider == "anthropic" {
            vec!["--max-tokens", "1024"]
        } else {
            vec![]
        })
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    (
        report(&output)["conversation_id"]
            .as_str()
            .unwrap()
            .to_owned(),
        store,
    )
}

#[test]
fn repair_cli_preview_apply_revision_conflict_and_new_work_for_three_protocols() {
    for provider in ["openai-chat", "openai-responses", "anthropic"] {
        let dir = tempfile::tempdir().unwrap();
        let fixture = wire::Fixture::new(
            provider,
            vec![wire::Reply::Read("evidence.txt"), wire::Reply::Answer],
            None,
        );
        fixture.release();
        let (id, store) = create(dir.path(), provider, &fixture.endpoint);
        let path = store.join("sessions").join(format!("{id}.json"));
        let before = std::fs::read(&path).unwrap();
        let rejected = Command::new(env!("CARGO_BIN_EXE_tarz"))
            .args([
                "session",
                "run",
                &id,
                "--no-config",
                "--json",
                "--input",
                "keep this input",
                "--store",
            ])
            .arg(&store)
            .output()
            .unwrap();
        assert_eq!(rejected.status.code(), Some(1));
        assert_eq!(report(&rejected)["status"], "not_started");
        assert!(
            report(&rejected)["error"]
                .as_str()
                .unwrap()
                .contains("unpaired")
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        for explicit in [false, true] {
            let mut cmd = command(&store, &id);
            if explicit {
                cmd.arg("--dry-run");
            }
            let preview = cmd.output().unwrap();
            assert!(preview.status.success(), "{preview:?}");
            let value = report(&preview);
            assert_eq!(value["status"], "preview");
            assert_eq!(value["revision"], 1);
            assert_eq!(value["plan"]["removed_count"], 1);
            assert_eq!(value["uncommitted_results_available"], false);
            assert_eq!(std::fs::read(&path).unwrap(), before);
            assert_eq!(
                fixture.accepted.load(std::sync::atomic::Ordering::SeqCst),
                0
            );
        }
        let applied = command(&store, &id)
            .args(["--apply", "--expected-revision", "1"])
            .output()
            .unwrap();
        assert!(applied.status.success(), "{applied:?}");
        assert_eq!(report(&applied)["applied"], true);
        assert_eq!(report(&applied)["revision"], 2);
        assert_eq!(report(&applied)["saved"], true);
        let repaired = std::fs::read(&path).unwrap();
        let conflict = command(&store, &id)
            .args(["--apply", "--expected-revision", "1"])
            .output()
            .unwrap();
        assert_eq!(conflict.status.code(), Some(1));
        assert!(
            report(&conflict)["error"]
                .as_str()
                .unwrap()
                .contains("expected 1, current 2")
        );
        assert_eq!(std::fs::read(&path).unwrap(), repaired);
        let done = command(&store, &id).output().unwrap();
        assert_eq!(report(&done)["status"], "not_needed");
        assert_eq!(std::fs::read(&path).unwrap(), repaired);
        assert_eq!(
            fixture.accepted.load(std::sync::atomic::Ordering::SeqCst),
            0
        );

        let next = Command::new(env!("CARGO_BIN_EXE_tarz"))
            .args([
                "session",
                "run",
                &id,
                "--no-config",
                "--json",
                "--input",
                "new work",
                "--store",
            ])
            .arg(&store)
            .output()
            .unwrap();
        assert!(next.status.success(), "{next:?}");
        assert_eq!(report(&next)["status"], "completed");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(fixture.wait_for_requests(2));
        let requests = fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(!requests[0].to_string().contains("old-unpaired-wire-id"));
        assert!(
            requests[1]
                .to_string()
                .contains("new read returns this actual marker")
        );
        let context = tarz_core::material::load_session_materials(&path).unwrap();
        assert_eq!(
            context
                .blocks()
                .iter()
                .filter(|block| matches!(block.content(), BlockContent::ToolResult(_)))
                .count(),
            1
        );
    }
}

#[test]
fn repair_cli_requires_explicit_apply_revision_and_rejects_ambiguous_options() {
    for flags in [
        vec!["--apply"],
        vec!["--expected-revision", "1"],
        vec!["--apply", "--expected-revision", "0"],
        vec!["--apply", "--expected-revision", "1", "--dry-run"],
        vec!["--apply", "--apply", "--expected-revision", "1"],
        vec!["--provider", "anthropic"],
    ] {
        let dir = tempfile::tempdir().unwrap();
        let output = command(dir.path(), "not-opened")
            .args(flags)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn repair_cli_save_failure_exports_the_applied_candidate_without_repeating_work() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let fixture = wire::Fixture::new("anthropic", vec![], None);
    fixture.release();
    let (id, store) = create(dir.path(), "anthropic", &fixture.endpoint);
    let sessions = store.join("sessions");
    let path = sessions.join(format!("{id}.json"));
    let before = std::fs::read(&path).unwrap();
    struct Restore(std::path::PathBuf);
    impl Drop for Restore {
        fn drop(&mut self) {
            std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    let _restore = Restore(sessions.clone());
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o500)).unwrap();
    let probe = sessions.join("permission-probe");
    if std::fs::write(&probe, "").is_ok() {
        std::fs::remove_file(probe).unwrap();
        eprintln!("filesystem permissions are bypassed; save-failure fixture unavailable");
        return;
    }
    let output = command(&store, &id)
        .args(["--apply", "--expected-revision", "1", "--recovery-dir"])
        .arg(dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let value = report(&output);
    assert_eq!(value["applied"], true);
    assert_eq!(value["saved"], false);
    assert!(value["save_error"].is_string());
    assert!(value["recovery_error"].is_null());
    assert!(value["close_error"].is_null());
    let recovered = tarz_core::material::load(value["recovery_file"].as_str().unwrap()).unwrap();
    assert!(
        !recovered
            .blocks()
            .iter()
            .any(|block| matches!(block.content(), BlockContent::ToolCall(_)))
    );
    assert!(
        serde_json::to_string(&recovered)
            .unwrap()
            .contains("material_repair")
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(
        fixture.accepted.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

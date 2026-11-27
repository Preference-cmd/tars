use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};
use tars_core::application::SessionHost;

fn tars_bin() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tars"));
    command.env_remove("TARS_CONFIG").env_remove("TARS_HOME");
    command
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

fn run_with_home(args: &[&str], cwd: &Path, tars_home: &Path) -> Output {
    tars_bin()
        .args(args)
        .current_dir(cwd)
        .env("TARS_HOME", tars_home)
        .output()
        .expect("run tars")
}

#[test]
fn ft10_trust_help_and_completions_surface() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("tars-home");

    let top = run_with_home(&["--help"], dir.path(), &home);
    assert!(top.status.success());
    let top_stdout = text(&top.stdout);
    assert!(top_stdout.contains("tars trust"), "{top_stdout}");
    assert!(top_stdout.contains("/trust"), "{top_stdout}");

    for args in [
        vec!["trust"],
        vec!["trust", "--help"],
        vec!["trust", "-h"],
        vec!["trust", "list", "--help"],
        vec!["trust", "revoke", "--help"],
    ] {
        let out = run_with_home(&args, dir.path(), &home);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        let stdout = text(&out.stdout);
        assert!(stdout.contains("Usage: tars trust"), "{args:?}: {stdout}");
    }

    for (shell, expected) in [
        ("bash", &["trust", "list", "revoke"][..]),
        ("zsh", &["trust:", "list:", "revoke:"][..]),
        ("fish", &["-a trust", "-a list", "-a revoke"][..]),
    ] {
        let out = run_with_home(&["completions", shell], dir.path(), &home);
        assert!(out.status.success(), "{shell}: {}", text(&out.stderr));
        let script = text(&out.stdout);
        for token in expected {
            assert!(
                script.contains(token),
                "{shell} completion missing '{token}'"
            );
        }
    }
}

#[test]
fn ft10_trust_argument_validation_and_no_config_without_store_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("tars-home");

    for (args, expected) in [
        (vec!["trust", "list", "--no-config"], "--store"),
        (vec!["trust", "revoke", "--no-config"], "--store"),
        (
            vec!["trust", "revoke", "/tmp/some-folder", "--no-config"],
            "--store",
        ),
        (
            vec!["trust", "list", "--config", "a.toml", "--no-config"],
            "mutually exclusive",
        ),
        (
            vec!["trust", "revoke", "--config", "a.toml", "--no-config"],
            "mutually exclusive",
        ),
        (vec!["trust", "bogus"], "unknown trust command"),
        (vec!["trust", "list", "extra"], "unexpected positional"),
        (vec!["trust", "revoke", "a", "b"], "unexpected positional"),
        (vec!["trust", "revoke", "--json"], "not applicable"),
        (vec!["trust", "list", "--store"], "requires a value"),
        (vec!["trust", "list", "--store", ""], "must not be empty"),
    ] {
        let out = run_with_home(&args, dir.path(), &home);
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: stdout={} stderr={}",
            text(&out.stdout),
            text(&out.stderr)
        );
        assert!(
            text(&out.stderr).contains(expected),
            "{args:?}: expected '{expected}' in '{}'",
            text(&out.stderr)
        );
    }
}

#[test]
fn ft10_trust_missing_and_v1_store_are_never_created_or_migrated() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("tars-home");

    // 1. Missing store directory/DB: list and revoke succeed without creating store or tars.db.
    let missing_store = dir.path().join("missing-store");
    let ws = dir.path().join("ws");
    std::fs::create_dir(&ws).unwrap();

    let list_text = run_with_home(
        &[
            "trust",
            "list",
            "--no-config",
            "--store",
            missing_store.to_str().unwrap(),
        ],
        dir.path(),
        &home,
    );
    assert!(list_text.status.success(), "{}", text(&list_text.stderr));
    assert!(
        text(&list_text.stdout).contains("No trusted workspace folders"),
        "{}",
        text(&list_text.stdout)
    );
    assert!(!missing_store.exists());

    let list_json = run_with_home(
        &[
            "trust",
            "list",
            "--no-config",
            "--store",
            missing_store.to_str().unwrap(),
            "--json",
        ],
        dir.path(),
        &home,
    );
    assert!(list_json.status.success());
    assert_eq!(
        serde_json::from_slice::<Value>(&list_json.stdout).unwrap(),
        Value::Array(vec![])
    );
    assert!(!missing_store.exists());

    let revoke_missing = run_with_home(
        &[
            "trust",
            "revoke",
            ws.to_str().unwrap(),
            "--no-config",
            "--store",
            missing_store.to_str().unwrap(),
        ],
        dir.path(),
        &home,
    );
    assert!(
        revoke_missing.status.success(),
        "{}",
        text(&revoke_missing.stderr)
    );
    assert!(
        text(&revoke_missing.stdout).contains("Folder is not trusted"),
        "{}",
        text(&revoke_missing.stdout)
    );
    assert!(!missing_store.exists());

    // 2. Untouched v1 SQLite store: list and revoke succeed without migrating to v2.
    let v1_store = dir.path().join("v1-store");
    std::fs::create_dir_all(&v1_store).unwrap();
    let db_path = v1_store.join("tars.db");
    {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "PRAGMA user_version = 1;
             CREATE TABLE sessions (
                 session_id TEXT PRIMARY KEY,
                 revision INTEGER NOT NULL,
                 last_commit_id TEXT NOT NULL,
                 policy_json TEXT NOT NULL,
                 entry_agent_id TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             );",
        )
        .unwrap();
    }

    let v1_list = run_with_home(
        &[
            "trust",
            "list",
            "--no-config",
            "--store",
            v1_store.to_str().unwrap(),
            "--json",
        ],
        dir.path(),
        &home,
    );
    assert!(v1_list.status.success(), "{}", text(&v1_list.stderr));
    assert_eq!(
        serde_json::from_slice::<Value>(&v1_list.stdout).unwrap(),
        Value::Array(vec![])
    );

    let v1_revoke = run_with_home(
        &[
            "trust",
            "revoke",
            ws.to_str().unwrap(),
            "--no-config",
            "--store",
            v1_store.to_str().unwrap(),
        ],
        dir.path(),
        &home,
    );
    assert!(v1_revoke.status.success(), "{}", text(&v1_revoke.stderr));
    assert!(text(&v1_revoke.stdout).contains("Folder is not trusted"));

    let conn = rusqlite::Connection::open(&db_path).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version;", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 1, "v1 store must not be silently migrated");
    let has_table: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='trusted_workspaces'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(has_table, 0);
}

#[tokio::test]
async fn ft10_trust_list_json_store_precedence_and_revoke_including_deleted_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let tars_home = dir.path().join("tars-home");
    let default_store = tars_home.join("data");
    let config_store = dir.path().join("config-store");
    let explicit_store = dir.path().join("explicit-store");

    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        format!("version = 1\nstore = '{}'\n", config_store.display()),
    )
    .unwrap();

    let ws_default = dir.path().join("ws-default");
    let ws_config = dir.path().join("ws-config");
    let ws_a = dir.path().join("ws-a");
    let ws_b = dir.path().join("ws-b");
    let ws_deleted_rel = dir.path().join("ws-deleted-rel");
    for ws in [&ws_default, &ws_config, &ws_a, &ws_b, &ws_deleted_rel] {
        std::fs::create_dir_all(ws).unwrap();
    }

    // Seed trust in all 3 stores via SessionHost.
    SessionHost::new(Some(default_store.clone()))
        .trust_workspace_folder(&ws_default)
        .await
        .unwrap();
    SessionHost::new(Some(config_store.clone()))
        .trust_workspace_folder(&ws_config)
        .await
        .unwrap();
    let host_explicit = SessionHost::new(Some(explicit_store.clone()));
    host_explicit.trust_workspace_folder(&ws_a).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    host_explicit.trust_workspace_folder(&ws_b).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    host_explicit
        .trust_workspace_folder(&ws_deleted_rel)
        .await
        .unwrap();

    // 1. Store precedence: default (TARS_HOME/data) -> --config store -> --store override.
    let out_default = run_with_home(&["trust", "list", "--json"], dir.path(), &tars_home);
    assert!(
        out_default.status.success(),
        "{}",
        text(&out_default.stderr)
    );
    let arr_default: Vec<Value> = serde_json::from_slice(&out_default.stdout).unwrap();
    assert_eq!(arr_default.len(), 1);
    assert_eq!(
        arr_default[0]["workspace_root"],
        ws_default.canonicalize().unwrap().to_str().unwrap()
    );

    let out_cfg = run_with_home(
        &[
            "trust",
            "list",
            "--config",
            cfg_path.to_str().unwrap(),
            "--json",
        ],
        dir.path(),
        &tars_home,
    );
    assert!(out_cfg.status.success());
    let arr_cfg: Vec<Value> = serde_json::from_slice(&out_cfg.stdout).unwrap();
    assert_eq!(arr_cfg.len(), 1);
    assert_eq!(
        arr_cfg[0]["workspace_root"],
        ws_config.canonicalize().unwrap().to_str().unwrap()
    );

    let out_override = run_with_home(
        &[
            "trust",
            "list",
            "--config",
            cfg_path.to_str().unwrap(),
            "--store",
            explicit_store.to_str().unwrap(),
            "--json",
        ],
        dir.path(),
        &tars_home,
    );
    assert!(out_override.status.success());
    let arr_override: Vec<Value> = serde_json::from_slice(&out_override.stdout).unwrap();
    assert_eq!(arr_override.len(), 3);
    assert!(arr_override[0]["trusted_at_ms"].as_i64().unwrap() > 0);
    // Ordered newest first (trusted_at_ms DESC).
    assert_eq!(
        arr_override[0]["workspace_root"],
        ws_deleted_rel.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(
        arr_override[1]["workspace_root"],
        ws_b.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(
        arr_override[2]["workspace_root"],
        ws_a.canonicalize().unwrap().to_str().unwrap()
    );

    // 2. Revoke with omitted DIR defaults to cwd (ws_a).
    let revoke_cwd = run_with_home(
        &[
            "trust",
            "revoke",
            "--no-config",
            "--store",
            explicit_store.to_str().unwrap(),
        ],
        &ws_a,
        &tars_home,
    );
    assert!(revoke_cwd.status.success(), "{}", text(&revoke_cwd.stderr));
    assert!(text(&revoke_cwd.stdout).contains("Revoked folder trust"));

    // 3. Delete ws_deleted_rel from disk, then revoke via relative path `./ws-deleted-rel`.
    std::fs::remove_dir_all(&ws_deleted_rel).unwrap();
    let revoke_rel_deleted = run_with_home(
        &[
            "trust",
            "revoke",
            "./ws-deleted-rel",
            "--no-config",
            "--store",
            explicit_store.to_str().unwrap(),
        ],
        dir.path(),
        &tars_home,
    );
    assert!(
        revoke_rel_deleted.status.success(),
        "{}",
        text(&revoke_rel_deleted.stderr)
    );
    assert!(text(&revoke_rel_deleted.stdout).contains("Revoked folder trust"));

    // Only ws_b remains trusted.
    let after_list = run_with_home(
        &[
            "trust",
            "list",
            "--no-config",
            "--store",
            explicit_store.to_str().unwrap(),
            "--json",
        ],
        dir.path(),
        &tars_home,
    );
    assert!(after_list.status.success());
    let after_arr: Vec<Value> = serde_json::from_slice(&after_list.stdout).unwrap();
    assert_eq!(after_arr.len(), 1);
    assert_eq!(
        after_arr[0]["workspace_root"],
        ws_b.canonicalize().unwrap().to_str().unwrap()
    );

    // Text list prints the trusted folder cleanly.
    let text_list = run_with_home(
        &[
            "trust",
            "list",
            "--no-config",
            "--store",
            explicit_store.to_str().unwrap(),
        ],
        dir.path(),
        &tars_home,
    );
    assert!(text_list.status.success());
    assert!(
        text(&text_list.stdout).contains(ws_b.canonicalize().unwrap().to_str().unwrap()),
        "{}",
        text(&text_list.stdout)
    );
}

#[cfg(unix)]
#[tokio::test]
async fn ft10_trust_revoke_deleted_directory_via_symlinked_parent() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let home = dir.path().join("tars-home");
    let real_parent = dir.path().join("real-parent");
    let link_parent = dir.path().join("link-parent");
    std::fs::create_dir_all(&real_parent).unwrap();
    std::os::unix::fs::symlink(&real_parent, &link_parent).unwrap();

    let target = real_parent.join("deleted-ws");
    std::fs::create_dir_all(&target).unwrap();
    SessionHost::new(Some(store.clone()))
        .trust_workspace_folder(&target)
        .await
        .unwrap();
    std::fs::remove_dir_all(&target).unwrap();

    let out = run_with_home(
        &[
            "trust",
            "revoke",
            link_parent.join("deleted-ws").to_str().unwrap(),
            "--no-config",
            "--store",
            store.to_str().unwrap(),
        ],
        dir.path(),
        &home,
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("Revoked folder trust"),
        "{}",
        text(&out.stdout)
    );
    assert!(
        SessionHost::new(Some(store))
            .list_trusted_workspace_folders()
            .await
            .unwrap()
            .is_empty()
    );
}

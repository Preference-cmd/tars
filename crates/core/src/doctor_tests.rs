use super::*;
use crate::config::{Config, ConfigSource};
use crate::definition::WorkspaceTool;
use crate::external_tools::ToolEnvironment;
use crate::sandbox::{BackendId, BackendReport, Capability, SandboxStatus};
use crate::store::schema;

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

fn available_sandbox() -> SandboxStatus {
    SandboxStatus::Available(BackendReport::new(
        BackendId::SEATBELT,
        Capability::Full,
        Capability::Full,
        Capability::Full,
        Capability::Full,
        Capability::Full,
    ))
}

fn unavailable_sandbox() -> SandboxStatus {
    SandboxStatus::Unavailable(tars_sandbox::test_support::unavailable(
        BackendId::UNAVAILABLE,
        "no sandbox backend in test",
    ))
}

fn unavailable_external_tools(dir: &Path) -> ExternalTools {
    let cfg_path = dir.join("ext-rg.toml");
    write_file(
        &cfg_path,
        "version = 1\n[external_tools]\nrg = \"/nonexistent/tars-doctor-rg\"\n",
    );
    let cfg = Config::load(ConfigSource::File(cfg_path)).unwrap();
    ExternalTools::resolve_with(&cfg, &ToolEnvironment::default())
}

#[test]
fn credential_check_detects_set_and_unset_without_leaking_values() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join("config.toml");
    write_file(
        &cfg_path,
        r#"
version = 1
store = "data"
recovery_dir = "."
default_profile = "set_env"

[profiles.set_env]
provider = "openai-responses"
model = "gpt-5"
api_key_env = "PATH"

[profiles.unset_env]
provider = "openai-chat"
model = "gpt-4o"
api_key_env = "TARS_DOCTOR_UNSET_KEY_999999"
"#,
    );
    let config = Config::load(ConfigSource::File(cfg_path.clone())).unwrap();
    let store_dir = dir.path().join("data");
    let ext = unavailable_external_tools(dir.path());
    let sandbox = available_sandbox();

    let path_value = std::env::var("PATH").unwrap();

    // Case 1: set env var (`PATH`) -> Ok, and value NEVER appears anywhere in output.
    let ok_report = run_diagnostics(&DoctorInput {
        version: "0.1.0-test",
        config: &config,
        config_path: Some(&cfg_path),
        config_disabled: false,
        profile: Some("set_env"),
        store_dir: &store_dir,
        recovery_dir: dir.path(),
        workspace_root: Some(dir.path()),
        active_tools: Some(&[WorkspaceTool::ReadFile]),
        sandbox: Some(&sandbox),
        external_tools: Some(&ext),
        user_rules_path: None,
    });

    let cred_item = ok_report
        .items
        .iter()
        .find(|item| item.category == "credentials" && item.label == "Env PATH")
        .expect("PATH credential item");
    assert_eq!(cred_item.status, DoctorStatus::Ok);
    assert_eq!(cred_item.detail, "set");
    assert!(cred_item.hint.is_none());

    let joined = ok_report.format_lines().join("\n");
    assert!(
        !joined.contains(&path_value),
        "diagnostic output leaked PATH value"
    );

    // Case 2: unset env var -> Fail with export hint, no secret leaked.
    let fail_report = run_diagnostics(&DoctorInput {
        version: "0.1.0-test",
        config: &config,
        config_path: Some(&cfg_path),
        config_disabled: false,
        profile: Some("unset_env"),
        store_dir: &store_dir,
        recovery_dir: dir.path(),
        workspace_root: Some(dir.path()),
        active_tools: Some(&[WorkspaceTool::ReadFile]),
        sandbox: Some(&sandbox),
        external_tools: Some(&ext),
        user_rules_path: None,
    });

    let unset_item = fail_report
        .items
        .iter()
        .find(|item| {
            item.category == "credentials" && item.label == "Env TARS_DOCTOR_UNSET_KEY_999999"
        })
        .expect("unset credential item");
    assert_eq!(unset_item.status, DoctorStatus::Fail);
    assert_eq!(unset_item.detail, "not set");
    assert!(
        unset_item
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("TARS_DOCTOR_UNSET_KEY_999999"))
    );
}

#[test]
fn config_missing_valid_and_anthropic_without_max_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let store_dir = dir.path().join("data");
    let ext = unavailable_external_tools(dir.path());
    let sandbox = available_sandbox();

    // Missing default config (not disabled, source() is None) -> Warn.
    let empty_cfg = Config::load(ConfigSource::Disabled).unwrap();
    let missing_report = run_diagnostics(&DoctorInput {
        version: "0.1.0-test",
        config: &empty_cfg,
        config_path: None,
        config_disabled: false,
        profile: None,
        store_dir: &store_dir,
        recovery_dir: dir.path(),
        workspace_root: None,
        active_tools: None,
        sandbox: Some(&sandbox),
        external_tools: Some(&ext),
        user_rules_path: None,
    });
    let cfg_item = missing_report
        .items
        .iter()
        .find(|item| item.category == "config" && item.label == "Config file")
        .unwrap();
    assert_eq!(cfg_item.status, DoctorStatus::Warn);

    // Valid config + Anthropic missing max_tokens -> Config Ok, Anthropic max_tokens Fail.
    let cfg_path = dir.path().join("config.toml");
    write_file(
        &cfg_path,
        r#"
version = 1
store = "data"
recovery_dir = "."
default_profile = "claude"

[profiles.claude]
provider = "anthropic"
model = "claude-sonnet-4-5"
api_key_env = "PATH"
"#,
    );
    let config = Config::load(ConfigSource::File(cfg_path.clone())).unwrap();
    let report = run_diagnostics(&DoctorInput {
        version: "0.1.0-test",
        config: &config,
        config_path: Some(&cfg_path),
        config_disabled: false,
        profile: None,
        store_dir: &store_dir,
        recovery_dir: dir.path(),
        workspace_root: None,
        active_tools: None,
        sandbox: Some(&sandbox),
        external_tools: Some(&ext),
        user_rules_path: None,
    });

    let loaded_item = report
        .items
        .iter()
        .find(|item| item.category == "config" && item.label == "Config file")
        .unwrap();
    assert_eq!(loaded_item.status, DoctorStatus::Ok);

    let max_tokens_item = report
        .items
        .iter()
        .find(|item| item.category == "config" && item.label == "Anthropic max_tokens")
        .unwrap();
    assert_eq!(max_tokens_item.status, DoctorStatus::Fail);
}

#[test]
fn storage_check_is_read_only_verifies_sqlite_and_detects_legacy_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::load(ConfigSource::Disabled).unwrap();
    let ext = unavailable_external_tools(dir.path());
    let sandbox = available_sandbox();

    // 1. Non-existent store_dir and recovery_dir: must NOT create either!
    let absent_store = dir.path().join("absent-store");
    let absent_recovery = dir.path().join("absent-recovery");
    let report = run_diagnostics(&DoctorInput {
        version: "0.1.0-test",
        config: &cfg,
        config_path: None,
        config_disabled: true,
        profile: None,
        store_dir: &absent_store,
        recovery_dir: &absent_recovery,
        workspace_root: None,
        active_tools: None,
        sandbox: Some(&sandbox),
        external_tools: Some(&ext),
        user_rules_path: None,
    });
    assert!(!absent_store.exists(), "doctor must not create store_dir");
    assert!(
        !absent_store.join("tars.db").exists(),
        "doctor must not create tars.db"
    );
    assert!(
        !absent_recovery.exists(),
        "doctor must not create recovery_dir"
    );
    let rec_item = report
        .items
        .iter()
        .find(|item| item.category == "storage" && item.label == "Recovery directory")
        .unwrap();
    assert_eq!(rec_item.status, DoctorStatus::Warn);

    // 2. Real initialized tars.db + legacy sessions/*.json -> Ok for SQLite, Warn for legacy.
    let real_store = dir.path().join("real-store");
    let _conn = schema::open_read_write(&real_store.join("tars.db")).unwrap();
    drop(_conn);
    assert!(real_store.join("tars.db").is_file());
    write_file(&real_store.join("sessions").join("old-session.json"), "{}");

    let populated_report = run_diagnostics(&DoctorInput {
        version: "0.1.0-test",
        config: &cfg,
        config_path: None,
        config_disabled: true,
        profile: None,
        store_dir: &real_store,
        recovery_dir: dir.path(),
        workspace_root: None,
        active_tools: None,
        sandbox: Some(&sandbox),
        external_tools: Some(&ext),
        user_rules_path: None,
    });
    let sqlite_item = populated_report
        .items
        .iter()
        .find(|item| item.category == "storage" && item.label == "SQLite store")
        .unwrap();
    assert_eq!(sqlite_item.status, DoctorStatus::Ok);
    assert!(sqlite_item.detail.contains("schema v2"));

    let legacy_item = populated_report
        .items
        .iter()
        .find(|item| item.category == "storage" && item.label == "Legacy sessions")
        .unwrap();
    assert_eq!(legacy_item.status, DoctorStatus::Warn);
    assert!(
        legacy_item
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("tars session import-legacy"))
    );

    // 3. Corrupt tars.db -> Fail.
    let corrupt_store = dir.path().join("corrupt-store");
    write_file(&corrupt_store.join("tars.db"), "not a sqlite database");
    let corrupt_report = run_diagnostics(&DoctorInput {
        version: "0.1.0-test",
        config: &cfg,
        config_path: None,
        config_disabled: true,
        profile: None,
        store_dir: &corrupt_store,
        recovery_dir: dir.path(),
        workspace_root: None,
        active_tools: None,
        sandbox: Some(&sandbox),
        external_tools: Some(&ext),
        user_rules_path: None,
    });
    let corrupt_item = corrupt_report
        .items
        .iter()
        .find(|item| item.category == "storage" && item.label == "SQLite store")
        .unwrap();
    assert_eq!(corrupt_item.status, DoctorStatus::Fail);
}

#[test]
fn invalid_permissions_toml_fails_without_echoing_rule_values() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::load(ConfigSource::Disabled).unwrap();
    let ext = unavailable_external_tools(dir.path());
    let sandbox = available_sandbox();

    let bad_rules = dir.path().join("permissions.toml");
    write_file(
        &bad_rules,
        "version = 1\n[[command]]\nprefix = [\"bash\"]\ndecision = \"allow\"\n# secret-comment-do-not-echo\n",
    );

    let report = run_diagnostics(&DoctorInput {
        version: "0.1.0-test",
        config: &cfg,
        config_path: None,
        config_disabled: true,
        profile: None,
        store_dir: &dir.path().join("data"),
        recovery_dir: dir.path(),
        workspace_root: None,
        active_tools: None,
        sandbox: Some(&sandbox),
        external_tools: Some(&ext),
        user_rules_path: Some(&bad_rules),
    });

    let perm_item = report
        .items
        .iter()
        .find(|item| item.category == "permissions")
        .unwrap();
    assert_eq!(perm_item.status, DoctorStatus::Fail);
    let rendered = report.format_lines().join("\n");
    assert!(!rendered.contains("secret-comment-do-not-echo"));
    assert!(!rendered.contains("\"bash\""));
}

#[test]
fn mcp_stdio_probe_checks_existence_without_spawning_and_checks_env_refs() {
    let dir = tempfile::tempdir().unwrap();
    let sentinel = dir.path().join("spawned.txt");
    let script_path = dir.path().join("fake-mcp");
    write_file(
        &script_path,
        &format!("#!/bin/sh\ntouch {}\n", sentinel.display()),
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script_path, perms).unwrap();
    }

    let cfg_path = dir.path().join("config.toml");
    write_file(
        &cfg_path,
        r#"
version = 1
store = "data"
recovery_dir = "."
default_profile = "default"

[profiles.default]
provider = "openai-responses"
model = "gpt-5"
api_key_env = "PATH"
mcp = ["present", "missing"]
"#,
    );
    let mcp_path = dir.path().join("mcp.json");
    write_file(
        &mcp_path,
        &format!(
            r#"{{
  "mcpServers": {{
    "present": {{
      "command": "{}",
      "env": {{
        "OK_REF": "${{PATH}}"
      }}
    }},
    "missing": {{
      "command": "tars-definitely-missing-mcp-binary-xyz",
      "env": {{
        "API_TOKEN": "${{TARS_DOCTOR_MISSING_MCP_TOKEN}}"
      }}
    }}
  }}
}}"#,
            script_path.display().to_string().replace('\\', "\\\\")
        ),
    );

    let config = Config::load(ConfigSource::File(cfg_path.clone())).unwrap();
    let ext = unavailable_external_tools(dir.path());
    let sandbox = available_sandbox();

    let report = run_diagnostics(&DoctorInput {
        version: "0.1.0-test",
        config: &config,
        config_path: Some(&cfg_path),
        config_disabled: false,
        profile: None,
        store_dir: &dir.path().join("data"),
        recovery_dir: dir.path(),
        workspace_root: None,
        active_tools: None,
        sandbox: Some(&sandbox),
        external_tools: Some(&ext),
        user_rules_path: None,
    });

    assert!(
        !sentinel.exists(),
        "doctor must NEVER spawn MCP stdio commands"
    );

    let present_item = report
        .items
        .iter()
        .find(|item| item.label == "MCP stdio 'present'")
        .unwrap();
    assert_eq!(present_item.status, DoctorStatus::Ok);

    let missing_item = report
        .items
        .iter()
        .find(|item| item.label == "MCP stdio 'missing'")
        .unwrap();
    assert_eq!(missing_item.status, DoctorStatus::Warn);

    let missing_mcp_env = report
        .items
        .iter()
        .find(|item| item.label == "MCP env TARS_DOCTOR_MISSING_MCP_TOKEN (missing)")
        .unwrap();
    assert_eq!(missing_mcp_env.status, DoctorStatus::Warn);
}

#[test]
fn sandbox_tool_warning_and_format_lines_summary() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::load(ConfigSource::Disabled).unwrap();
    let ext = unavailable_external_tools(dir.path());
    let sandbox = unavailable_sandbox();

    write_file(&dir.path().join("AGENTS.md"), "# Workspace rules\n");

    let report = run_diagnostics(&DoctorInput {
        version: "0.2.0-test",
        config: &cfg,
        config_path: None,
        config_disabled: true,
        profile: None,
        store_dir: &dir.path().join("data"),
        recovery_dir: dir.path(),
        workspace_root: Some(dir.path()),
        active_tools: Some(&[WorkspaceTool::FindFiles, WorkspaceTool::RunCommand]),
        sandbox: Some(&sandbox),
        external_tools: Some(&ext),
        user_rules_path: None,
    });

    let ws_warn = report
        .items
        .iter()
        .find(|item| item.category == "workspace" && item.label == "Sandboxed tools")
        .unwrap();
    assert_eq!(ws_warn.status, DoctorStatus::Warn);
    assert!(ws_warn.detail.contains("find_files"));

    let ws_agents = report
        .items
        .iter()
        .find(|item| item.category == "prelude" && item.label == "Workspace AGENTS.md")
        .unwrap();
    assert_eq!(ws_agents.status, DoctorStatus::Ok);

    let lines = report.format_lines();
    assert!(lines.iter().any(|line| line.starts_with("[ok] ")));
    assert!(lines.iter().any(|line| line.starts_with("[warn] ")));
    let last = lines.last().unwrap();
    assert_eq!(
        last,
        &format!(
            "Summary: {} ok, {} warn, {} fail",
            report.ok_count(),
            report.warn_count(),
            report.fail_count()
        )
    );
    assert_eq!(
        report.ok_count() + report.warn_count() + report.fail_count(),
        report.items.len()
    );
}

#[test]
fn sqlite_immutable_read_only_creates_no_wal_or_shm_and_handles_special_uri_chars() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::load(ConfigSource::Disabled).unwrap();
    let ext = unavailable_external_tools(dir.path());
    let sandbox = available_sandbox();

    let store_dir = dir.path().join("store%20#1?test");
    let db_path = store_dir.join("tars.db");
    let conn = schema::open_read_write(&db_path).unwrap();
    drop(conn);

    let shm_path = store_dir.join("tars.db-shm");
    let wal_path = store_dir.join("tars.db-wal");
    assert!(!shm_path.exists());
    assert!(!wal_path.exists());

    let report = run_diagnostics(&DoctorInput {
        version: "0.1.0-test",
        config: &cfg,
        config_path: None,
        config_disabled: true,
        profile: None,
        store_dir: &store_dir,
        recovery_dir: dir.path(),
        workspace_root: None,
        active_tools: None,
        sandbox: Some(&sandbox),
        external_tools: Some(&ext),
        user_rules_path: None,
    });

    let sqlite_item = report
        .items
        .iter()
        .find(|item| item.category == "storage" && item.label == "SQLite store")
        .unwrap();
    assert_eq!(sqlite_item.status, DoctorStatus::Ok, "{sqlite_item:?}");
    assert!(!shm_path.exists(), "doctor must not create tars.db-shm");
    assert!(!wal_path.exists(), "doctor must not create tars.db-wal");
}

#[cfg(unix)]
#[test]
fn sqlite_check_succeeds_on_read_only_store_dir_and_db() {
    use std::os::unix::fs::PermissionsExt;

    struct ModeRestore {
        store_dir: PathBuf,
        db_path: PathBuf,
    }
    impl Drop for ModeRestore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.store_dir, fs::Permissions::from_mode(0o755));
            let _ = fs::set_permissions(&self.db_path, fs::Permissions::from_mode(0o644));
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::load(ConfigSource::Disabled).unwrap();
    let ext = unavailable_external_tools(dir.path());
    let sandbox = available_sandbox();

    let store_dir = dir.path().join("ro-store");
    let db_path = store_dir.join("tars.db");
    let conn = schema::open_read_write(&db_path).unwrap();
    drop(conn);

    let _guard = ModeRestore {
        store_dir: store_dir.clone(),
        db_path: db_path.clone(),
    };
    fs::set_permissions(&db_path, fs::Permissions::from_mode(0o444)).unwrap();
    fs::set_permissions(&store_dir, fs::Permissions::from_mode(0o555)).unwrap();

    let report = run_diagnostics(&DoctorInput {
        version: "0.1.0-test",
        config: &cfg,
        config_path: None,
        config_disabled: true,
        profile: None,
        store_dir: &store_dir,
        recovery_dir: dir.path(),
        workspace_root: None,
        active_tools: None,
        sandbox: Some(&sandbox),
        external_tools: Some(&ext),
        user_rules_path: None,
    });

    let sqlite_item = report
        .items
        .iter()
        .find(|item| item.category == "storage" && item.label == "SQLite store")
        .unwrap();
    assert_eq!(sqlite_item.status, DoctorStatus::Ok, "{sqlite_item:?}");
}

#[cfg(unix)]
#[test]
fn storage_check_fails_when_db_metadata_is_unreadable() {
    use std::os::unix::fs::PermissionsExt;

    if rustix::process::geteuid().is_root() {
        return;
    }

    struct ModeRestore(PathBuf);
    impl Drop for ModeRestore {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::load(ConfigSource::Disabled).unwrap();
    let ext = unavailable_external_tools(dir.path());
    let sandbox = available_sandbox();

    let store_dir = dir.path().join("noexec-store");
    let db_path = store_dir.join("tars.db");
    let conn = schema::open_read_write(&db_path).unwrap();
    drop(conn);

    let _guard = ModeRestore(store_dir.clone());
    // Mode 0600 removes directory search (+x) so fs::metadata(&store_dir) succeeds
    // while fs::metadata(&db_path) fails with PermissionDenied.
    fs::set_permissions(&store_dir, fs::Permissions::from_mode(0o600)).unwrap();

    let report = run_diagnostics(&DoctorInput {
        version: "0.1.0-test",
        config: &cfg,
        config_path: None,
        config_disabled: true,
        profile: None,
        store_dir: &store_dir,
        recovery_dir: dir.path(),
        workspace_root: None,
        active_tools: None,
        sandbox: Some(&sandbox),
        external_tools: Some(&ext),
        user_rules_path: None,
    });

    let sqlite_item = report
        .items
        .iter()
        .find(|item| item.category == "storage" && item.label == "SQLite store")
        .unwrap();
    assert_eq!(sqlite_item.status, DoctorStatus::Fail, "{sqlite_item:?}");
}

#[test]
fn command_exists_without_spawning_checks_path_executables_and_rejects_nonexec_or_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();

    // (a) Bare executable in PATH.
    let exec_path = bin_dir.join("good-tool");
    write_file(&exec_path, "#!/bin/sh\nexit 0\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&exec_path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    // (b) Non-executable regular file in PATH (Unix).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let nonexec_path = bin_dir.join("nonexec-tool");
        write_file(&nonexec_path, "#!/bin/sh\nexit 0\n");
        fs::set_permissions(&nonexec_path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    // (c) Directory with the same name as the command in PATH.
    let dir_named_tool = bin_dir.join("dir-tool");
    fs::create_dir_all(&dir_named_tool).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&dir_named_tool, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let path_env = std::env::join_paths([&bin_dir]).unwrap();
    assert!(command_exists_without_spawning(
        "good-tool",
        Some(&path_env)
    ));
    #[cfg(unix)]
    assert!(!command_exists_without_spawning(
        "nonexec-tool",
        Some(&path_env)
    ));
    assert!(!command_exists_without_spawning(
        "dir-tool",
        Some(&path_env)
    ));
    assert!(!command_exists_without_spawning(
        "missing-tool",
        Some(&path_env)
    ));
    assert!(!command_exists_without_spawning("good-tool", None));
}

#[test]
fn doctor_gives_actionable_linux_bwrap_and_auto_rg_hints() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config::load(ConfigSource::Disabled).unwrap();
    let ext = ExternalTools::resolve_with(&cfg, &ToolEnvironment::default());

    let missing_bwrap = SandboxStatus::Unavailable(tars_sandbox::test_support::unavailable(
        BackendId::BWRAP,
        "bwrap executable not found; install bubblewrap (e.g. apt install bubblewrap)",
    ));
    let report_missing = run_diagnostics(&DoctorInput {
        version: "0.2.0-test",
        config: &cfg,
        config_path: None,
        config_disabled: true,
        profile: None,
        store_dir: &dir.path().join("data"),
        recovery_dir: dir.path(),
        workspace_root: None,
        active_tools: None,
        sandbox: Some(&missing_bwrap),
        external_tools: Some(&ext),
        user_rules_path: None,
    });
    let sb_missing = report_missing
        .items
        .iter()
        .find(|item| item.category == "sandbox")
        .unwrap();
    assert_eq!(sb_missing.status, DoctorStatus::Warn);
    assert!(
        sb_missing
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("apt install bubblewrap")),
        "{:?}",
        sb_missing.hint
    );
    let rg_auto = report_missing
        .items
        .iter()
        .find(|item| item.category == "external_tools")
        .unwrap();
    assert_eq!(rg_auto.status, DoctorStatus::Warn);
    assert!(
        rg_auto
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("tars tools install rg") && h.contains("PATH")),
        "{:?}",
        rg_auto.hint
    );

    let userns_bwrap = SandboxStatus::Unavailable(tars_sandbox::test_support::unavailable(
        BackendId::BWRAP,
        "Roots probe failed: bwrap: setting up uid map: Permission denied (userns)",
    ));
    let report_userns = run_diagnostics(&DoctorInput {
        version: "0.2.0-test",
        config: &cfg,
        config_path: None,
        config_disabled: true,
        profile: None,
        store_dir: &dir.path().join("data"),
        recovery_dir: dir.path(),
        workspace_root: None,
        active_tools: None,
        sandbox: Some(&userns_bwrap),
        external_tools: Some(&ext),
        user_rules_path: None,
    });
    let sb_userns = report_userns
        .items
        .iter()
        .find(|item| item.category == "sandbox")
        .unwrap();
    assert!(
        sb_userns
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("AppArmor") && h.contains("user namespaces")),
        "{:?}",
        sb_userns.hint
    );

    let generic_bwrap = SandboxStatus::Unavailable(tars_sandbox::test_support::unavailable(
        BackendId::BWRAP,
        "HOME is not set; credential denials require a home directory",
    ));
    let report_generic = run_diagnostics(&DoctorInput {
        version: "0.2.0-test",
        config: &cfg,
        config_path: None,
        config_disabled: true,
        profile: None,
        store_dir: &dir.path().join("data"),
        recovery_dir: dir.path(),
        workspace_root: None,
        active_tools: None,
        sandbox: Some(&generic_bwrap),
        external_tools: Some(&ext),
        user_rules_path: None,
    });
    let sb_generic = report_generic
        .items
        .iter()
        .find(|item| item.category == "sandbox")
        .unwrap();
    assert!(
        sb_generic
            .hint
            .as_deref()
            .is_some_and(|h| h.contains("bwrap") && h.contains("user namespaces")),
        "{:?}",
        sb_generic.hint
    );
}

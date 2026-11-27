//! Integration tests for `tars auth`, `--credential`, and completions/help alignment.

use serde_json::{Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use tars_core::config::CredentialsStore;

#[path = "common/sse.rs"]
mod sse;

fn tars_cmd(tars_home: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tars"));
    cmd.env_remove("TARS_CONFIG").env("TARS_HOME", tars_home);
    cmd
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).unwrap()
}

fn assert_no_secret(output: &Output, secret: &str) {
    assert!(
        !text(&output.stdout).contains(secret),
        "stdout leaked secret: {}",
        text(&output.stdout)
    );
    assert!(
        !text(&output.stderr).contains(secret),
        "stderr leaked secret: {}",
        text(&output.stderr)
    );
}

fn server(status: u16, body: Value) -> (String, mpsc::Receiver<String>) {
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
        tx.send(String::from_utf8(bytes).unwrap()).unwrap();
        let (content_type, body) = sse::reply(status, body);
        let _ = write!(
            socket,
            "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    });
    (endpoint, rx)
}

fn chat_body(answer: &str) -> Value {
    json!({
        "choices": [{
            "message": { "role": "assistant", "content": answer },
            "finish_reason": "stop"
        }]
    })
}

#[test]
fn auth_set_stdin_list_remove_path_and_never_echoes_secret() {
    let dir = tempfile::tempdir().unwrap();
    let tars_home = dir.path().join("tars-home");
    let expected_creds = tars_home.join("credentials.toml");

    // `path` works without creating credentials.toml.
    let path_out = tars_cmd(&tars_home)
        .args(["auth", "path"])
        .output()
        .unwrap();
    assert!(path_out.status.success(), "{path_out:?}");
    assert_eq!(
        text(&path_out.stdout).trim(),
        expected_creds.to_str().unwrap()
    );
    assert!(!expected_creds.exists());

    // `list` and `list --json` on missing file return empty list without creating file.
    let empty_list = tars_cmd(&tars_home)
        .args(["auth", "list"])
        .output()
        .unwrap();
    assert!(empty_list.status.success(), "{empty_list:?}");
    assert_eq!(text(&empty_list.stdout), "");
    assert!(!expected_creds.exists());

    let empty_json = tars_cmd(&tars_home)
        .args(["auth", "list", "--json"])
        .output()
        .unwrap();
    assert!(empty_json.status.success(), "{empty_json:?}");
    let empty_val: Value = serde_json::from_slice(&empty_json.stdout).unwrap();
    assert_eq!(empty_val["path"], expected_creds.to_str().unwrap());
    assert_eq!(empty_val["credentials"], json!([]));
    assert!(!expected_creds.exists());

    // `remove` on missing file exits 1 without creating file.
    let missing_rm = tars_cmd(&tars_home)
        .args(["auth", "remove", "work"])
        .output()
        .unwrap();
    assert_eq!(missing_rm.status.code(), Some(1), "{missing_rm:?}");
    assert!(!expected_creds.exists());

    // `set --stdin` stores trimmed secret at 0600 (0700 parent dir on Unix) and never echoes secret.
    let secret_work = "sk-work-super-secret-111";
    let secret_alpha = "sk-alpha-super-secret-222";

    let mut child = tars_cmd(&tars_home)
        .args(["auth", "set", "work", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{secret_work}\r\n").as_bytes())
        .unwrap();
    let set_work = child.wait_with_output().unwrap();
    assert!(set_work.status.success(), "{set_work:?}");
    assert_no_secret(&set_work, secret_work);
    assert!(text(&set_work.stdout).contains("work"));
    assert!(expected_creds.exists());

    let mut child2 = tars_cmd(&tars_home)
        .args(["auth", "set", "alpha", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child2
        .stdin
        .take()
        .unwrap()
        .write_all(format!("{secret_alpha}\n").as_bytes())
        .unwrap();
    let set_alpha = child2.wait_with_output().unwrap();
    assert!(set_alpha.status.success(), "{set_alpha:?}");
    assert_no_secret(&set_alpha, secret_alpha);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir_mode = fs::metadata(&tars_home).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        let file_mode = fs::metadata(&expected_creds).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
    }

    // `list` outputs sorted names only, never secret values.
    let list_out = tars_cmd(&tars_home)
        .args(["auth", "list"])
        .output()
        .unwrap();
    assert!(list_out.status.success(), "{list_out:?}");
    assert_no_secret(&list_out, secret_work);
    assert_no_secret(&list_out, secret_alpha);
    assert_eq!(text(&list_out.stdout), "alpha\nwork\n");

    let json_out = tars_cmd(&tars_home)
        .args(["auth", "list", "--json"])
        .output()
        .unwrap();
    assert!(json_out.status.success(), "{json_out:?}");
    assert_no_secret(&json_out, secret_work);
    assert_no_secret(&json_out, secret_alpha);
    let val: Value = serde_json::from_slice(&json_out.stdout).unwrap();
    assert_eq!(val["path"], expected_creds.to_str().unwrap());
    assert_eq!(val["credentials"], json!(["alpha", "work"]));

    // `remove` removes one entry and leaves the other; removing again exits 1.
    let rm_out = tars_cmd(&tars_home)
        .args(["auth", "remove", "alpha"])
        .output()
        .unwrap();
    assert!(rm_out.status.success(), "{rm_out:?}");
    assert_no_secret(&rm_out, secret_alpha);
    assert_no_secret(&rm_out, secret_work);

    let list_after = tars_cmd(&tars_home)
        .args(["auth", "list"])
        .output()
        .unwrap();
    assert!(list_after.status.success());
    assert_eq!(text(&list_after.stdout), "work\n");

    let rm_absent = tars_cmd(&tars_home)
        .args(["auth", "remove", "alpha"])
        .output()
        .unwrap();
    assert_eq!(rm_absent.status.code(), Some(1), "{rm_absent:?}");
}

#[test]
fn auth_resolves_relative_config_and_tars_config_before_parent() {
    let dir = tempfile::tempdir().unwrap();
    let tars_home = dir.path().join("tars-home");
    let work_dir = dir.path().join("workspace");
    let nested = work_dir.join("nested");
    fs::create_dir_all(&nested).unwrap();

    // 1. `--config config.toml` relative to cwd resolves `<cwd>/credentials.toml`, not `""`.
    let out_root = tars_cmd(&tars_home)
        .current_dir(&work_dir)
        .args(["auth", "path", "--config", "config.toml"])
        .output()
        .unwrap();
    assert!(out_root.status.success(), "{out_root:?}");
    let canonical_work = work_dir.canonicalize().unwrap();
    let resolved_root = Path::new(text(&out_root.stdout).trim())
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    assert_eq!(resolved_root, canonical_work);
    assert_eq!(
        Path::new(text(&out_root.stdout).trim())
            .file_name()
            .unwrap(),
        "credentials.toml"
    );

    // 2. `TARS_CONFIG=nested/config.toml` resolves `<cwd>/nested/credentials.toml`.
    let out_env = Command::new(env!("CARGO_BIN_EXE_tars"))
        .current_dir(&work_dir)
        .env("TARS_HOME", &tars_home)
        .env("TARS_CONFIG", "nested/config.toml")
        .args(["auth", "path"])
        .output()
        .unwrap();
    assert!(out_env.status.success(), "{out_env:?}");
    let canonical_nested = nested.canonicalize().unwrap();
    let resolved_nested = Path::new(text(&out_env.stdout).trim())
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap();
    assert_eq!(resolved_nested, canonical_nested);

    // 3. `set --stdin --config config.toml` writes sibling credentials.toml without touching TARS_HOME.
    let secret = "sk-relative-cfg-secret";
    let mut child = tars_cmd(&tars_home)
        .current_dir(&work_dir)
        .args(["auth", "set", "rel", "--stdin", "--config", "config.toml"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(secret.as_bytes())
        .unwrap();
    let set_out = child.wait_with_output().unwrap();
    assert!(set_out.status.success(), "{set_out:?}");
    assert_no_secret(&set_out, secret);
    assert!(work_dir.join("credentials.toml").exists());
    assert!(!tars_home.join("credentials.toml").exists());
}

#[test]
fn auth_rejects_no_config_invalid_names_cli_secrets_and_non_tty_without_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let tars_home = dir.path().join("tars-home");

    // `--no-config` rejected with exit code 2 across all 4 auth subcommands.
    for args in [
        vec!["auth", "set", "work", "--stdin", "--no-config"],
        vec!["auth", "list", "--no-config"],
        vec!["auth", "remove", "work", "--no-config"],
        vec!["auth", "path", "--no-config"],
    ] {
        let out = tars_cmd(&tars_home).args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(text(&out.stderr).contains("--no-config"), "{out:?}");
    }

    // Plaintext CLI secret argument rejected with exit code 2 and zero echo, never creating file.
    let leaked = "sk-must-never-be-echoed-secret";
    let cli_secret = tars_cmd(&tars_home)
        .args(["auth", "set", "work", leaked])
        .output()
        .unwrap();
    assert_eq!(cli_secret.status.code(), Some(2), "{cli_secret:?}");
    assert_no_secret(&cli_secret, leaked);
    assert!(
        text(&cli_secret.stderr).contains("--stdin"),
        "{cli_secret:?}"
    );
    assert!(!tars_home.join("credentials.toml").exists());

    // Invalid credential names rejected with exit code 2 and zero echo.
    for bad in ["work.prod", "_leading", "has space", "sk=pasted"] {
        for sub in [
            vec!["auth", "set", bad, "--stdin"],
            vec!["auth", "remove", bad],
        ] {
            let out = tars_cmd(&tars_home).args(&sub).output().unwrap();
            assert_eq!(out.status.code(), Some(2), "{sub:?}: {out:?}");
            assert_no_secret(&out, bad);
        }
    }

    // Non-TTY stdin without `--stdin` exits 2 instructing `--stdin`.
    let non_tty = tars_cmd(&tars_home)
        .args(["auth", "set", "work"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(non_tty.status.code(), Some(2), "{non_tty:?}");
    assert!(text(&non_tty.stderr).contains("--stdin"), "{non_tty:?}");
    assert!(!tars_home.join("credentials.toml").exists());

    // `--stdin` with empty or control-character secret exits 1 without echoing or creating file.
    for bad_stdin in [b"".as_slice(), b"   \n", b"line1\nline2\n", b"sk\ttab\n"] {
        let mut child = tars_cmd(&tars_home)
            .args(["auth", "set", "work", "--stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(bad_stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{bad_stdin:?}: {out:?}");
        assert!(!tars_home.join("credentials.toml").exists());
    }
}

#[cfg(unix)]
#[test]
fn auth_fails_closed_on_insecure_permissions_and_symlinks() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let dir = tempfile::tempdir().unwrap();
    let tars_home = dir.path().join("tars-home");
    let creds_path = tars_home.join("credentials.toml");
    let secret = "sk-fail-closed-secret-999";
    CredentialsStore::set(&creds_path, "work", secret).unwrap();

    // 1. Insecure permissions (0644) fail closed on list, list --json, set --stdin, remove.
    fs::set_permissions(&creds_path, fs::Permissions::from_mode(0o644)).unwrap();
    for args in [
        vec!["auth", "list"],
        vec!["auth", "list", "--json"],
        vec!["auth", "remove", "work"],
    ] {
        let out = tars_cmd(&tars_home).args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
        assert_no_secret(&out, secret);
        assert!(text(&out.stderr).contains("chmod 600"), "{out:?}");
    }

    let mut set_child = tars_cmd(&tars_home)
        .args(["auth", "set", "other", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    set_child
        .stdin
        .take()
        .unwrap()
        .write_all(b"sk-other-secret\n")
        .unwrap();
    let set_out = set_child.wait_with_output().unwrap();
    assert_eq!(set_out.status.code(), Some(1), "{set_out:?}");
    assert_no_secret(&set_out, secret);
    assert_no_secret(&set_out, "sk-other-secret");
    assert!(text(&set_out.stderr).contains("chmod 600"), "{set_out:?}");

    // 2. Symlink credentials.toml fails closed on list, set --stdin, remove.
    let real_target = dir.path().join("real.toml");
    fs::rename(&creds_path, &real_target).unwrap();
    fs::set_permissions(&real_target, fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&real_target, &creds_path).unwrap();

    for args in [vec!["auth", "list"], vec!["auth", "remove", "work"]] {
        let out = tars_cmd(&tars_home).args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}: {out:?}");
        assert_no_secret(&out, secret);
        assert!(text(&out.stderr).contains("symlink"), "{out:?}");
    }
}

#[cfg(unix)]
#[test]
fn auth_set_interactive_pty_does_not_echo_secret_and_supports_ctrl_c() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let tars_home = dir.path().join("tars-home");
    let creds_path = tars_home.join("credentials.toml");
    let secret = "sk-pty-noecho-secret-42";

    let script = r#"
import errno
import os
import pty
import select
import subprocess
import sys
import time

bin_path, tars_home, mode, secret = sys.argv[1:5]
master, slave = pty.openpty()
env = os.environ.copy()
env["TARS_HOME"] = tars_home
env.pop("TARS_CONFIG", None)

proc = subprocess.Popen(
    [bin_path, "auth", "set", "ptykey"],
    env=env,
    stdin=slave,
    stdout=slave,
    stderr=slave,
    preexec_fn=os.setsid,
    close_fds=True,
)
os.close(slave)

buf = bytearray()
deadline = time.monotonic() + 10.0
while time.monotonic() < deadline:
    if b"Enter API key" in buf:
        break
    r, _, _ = select.select([master], [], [], 0.05)
    if master in r:
        buf.extend(os.read(master, 4096))
assert b"Enter API key" in buf, f"prompt missing: {buf!r}"

if mode == "submit":
    os.write(master, (secret + "\r").encode("utf-8"))
else:
    os.write(master, b"partial\x03")

deadline = time.monotonic() + 10.0
while time.monotonic() < deadline:
    rc = proc.poll()
    r, _, _ = select.select([master], [], [], 0.05)
    if master in r:
        try:
            chunk = os.read(master, 4096)
            if chunk:
                buf.extend(chunk)
        except OSError as exc:
            if exc.errno != errno.EIO:
                raise
    if rc is not None:
        break
else:
    proc.kill()
    proc.wait()
    raise AssertionError(f"timed out: {buf!r}")

os.close(master)
sys.stdout.buffer.write(buf)
sys.exit(rc)
"#;

    // 1. Ctrl+C cancels cleanly without creating credentials.toml or echoing partial input.
    let cancel = Command::new("python3")
        .args(["-c", script])
        .arg(env!("CARGO_BIN_EXE_tars"))
        .arg(&tars_home)
        .arg("cancel")
        .arg(secret)
        .output()
        .unwrap();
    assert_eq!(cancel.status.code(), Some(130), "{cancel:?}");
    assert!(!text(&cancel.stdout).contains("partial"));
    assert!(!creds_path.exists());

    // 2. Typing secret + Enter persists 0600 credentials.toml and never echoes secret to PTY.
    let submit = Command::new("python3")
        .args(["-c", script])
        .arg(env!("CARGO_BIN_EXE_tars"))
        .arg(&tars_home)
        .arg("submit")
        .arg(secret)
        .output()
        .unwrap();
    assert!(submit.status.success(), "{submit:?}");
    assert!(!text(&submit.stdout).contains(secret), "{submit:?}");
    assert!(text(&submit.stdout).contains("Saved credential ptykey"));

    let mode = fs::metadata(&creds_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(
        CredentialsStore::load(&creds_path).unwrap().get("ptykey"),
        Some(secret)
    );
}

#[test]
fn credential_flag_end_to_end_across_run_print_and_session_ignoring_global_env() {
    let dir = tempfile::tempdir().unwrap();
    let tars_home = dir.path().join("tars-home");
    let store_dir = dir.path().join("store");
    let creds_path = tars_home.join("credentials.toml");
    let secret = "sk-e2e-cred-flag-secret-777";
    CredentialsStore::set(&creds_path, "custom_cred", secret).unwrap();

    // 1. `tars run --credential custom_cred` sends `Bearer <secret>`, ignores OPENAI_API_KEY.
    let (ep1, rx1) = server(200, chat_body("run-ok"));
    let run_out = tars_cmd(&tars_home)
        .env("OPENAI_API_KEY", "collide-env-key")
        .args([
            "run",
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--credential",
            "custom_cred",
            "--endpoint",
            &ep1,
            "--input",
            "hi run",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(run_out.status.success(), "{run_out:?}");
    assert_no_secret(&run_out, secret);
    let req1 = rx1.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        req1.contains(&format!("authorization: Bearer {secret}")),
        "{req1}"
    );
    assert!(!req1.contains("collide-env-key"));

    // 2. `tars -p --credential custom_cred` sends `Bearer <secret>`, ignores OPENAI_API_KEY.
    let (ep2, rx2) = server(200, chat_body("print-ok"));
    let print_out = tars_cmd(&tars_home)
        .env("OPENAI_API_KEY", "collide-env-key")
        .args([
            "-p",
            "hi print",
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--credential",
            "custom_cred",
            "--endpoint",
            &ep2,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(print_out.status.success(), "{print_out:?}");
    assert_no_secret(&print_out, secret);
    let req2 = rx2.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        req2.contains(&format!("authorization: Bearer {secret}")),
        "{req2}"
    );
    assert!(!req2.contains("collide-env-key"));

    // 3. `tars session create --credential custom_cred` + `tars session run` sends `Bearer <secret>`.
    let (ep3, rx3) = server(200, chat_body("session-ok"));
    let create_out = tars_cmd(&tars_home)
        .env("OPENAI_API_KEY", "collide-env-key")
        .args([
            "session",
            "create",
            "--store",
            store_dir.to_str().unwrap(),
            "--provider",
            "openai-chat",
            "--model",
            "fixture-model",
            "--credential",
            "custom_cred",
            "--endpoint",
            &ep3,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(create_out.status.success(), "{create_out:?}");
    assert_no_secret(&create_out, secret);
    let create_val: Value = serde_json::from_slice(&create_out.stdout).unwrap();
    let conv_id = create_val["conversation_id"].as_str().unwrap();

    // Persisted SQLite database must store only the reference name, never the plaintext secret.
    let conn = rusqlite::Connection::open(store_dir.join("tars.db")).unwrap();
    let def_json: String = conn
        .query_row(
            "SELECT definition_json FROM agents WHERE session_id = ?1 LIMIT 1",
            [conv_id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(def_json.contains("\"store\":\"custom_cred\""), "{def_json}");
    assert!(!def_json.contains(secret));
    drop(conn);

    let srun_out = tars_cmd(&tars_home)
        .env("OPENAI_API_KEY", "collide-env-key")
        .args([
            "session",
            "run",
            conv_id,
            "--store",
            store_dir.to_str().unwrap(),
            "--input",
            "hi session",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(srun_out.status.success(), "{srun_out:?}");
    assert_no_secret(&srun_out, secret);
    let req3 = rx3.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        req3.contains(&format!("authorization: Bearer {secret}")),
        "{req3}"
    );
    assert!(!req3.contains("collide-env-key"));
}

#[test]
fn credential_conflicts_invalid_names_completions_and_help_contract() {
    let dir = tempfile::tempdir().unwrap();
    let tars_home = dir.path().join("tars-home");
    let store_dir = dir.path().join("store");

    // Mutual exclusion with `--api-key-env` exits 2 across `run`, `-p`, `--print`, and `session create`;
    // `session run` also rejects `--credential` with exit 2.
    for args in [
        vec![
            "run",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "m",
            "--credential",
            "work",
            "--api-key-env",
            "OPENAI_API_KEY",
            "--input",
            "hi",
        ],
        vec![
            "-p",
            "hi",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "m",
            "--credential",
            "work",
            "--api-key-env",
            "OPENAI_API_KEY",
        ],
        vec![
            "--print",
            "hi",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "m",
            "--credential",
            "work",
            "--api-key-env",
            "OPENAI_API_KEY",
        ],
        vec![
            "session",
            "create",
            "--no-config",
            "--store",
            store_dir.to_str().unwrap(),
            "--provider",
            "openai-chat",
            "--model",
            "m",
            "--credential",
            "work",
            "--api-key-env",
            "OPENAI_API_KEY",
        ],
    ] {
        let out = tars_cmd(&tars_home).args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert!(
            text(&out.stderr).contains("mutually exclusive"),
            "{args:?}: {out:?}"
        );
    }

    // `tars session run` rejects `--credential` as inapplicable (exit 2).
    let srun_cred = tars_cmd(&tars_home)
        .args([
            "session",
            "run",
            "some-id",
            "--no-config",
            "--store",
            store_dir.to_str().unwrap(),
            "--credential",
            "work",
            "--input",
            "hi",
        ])
        .output()
        .unwrap();
    assert_eq!(srun_cred.status.code(), Some(2), "{srun_cred:?}");

    // Duplicate `--credential` and invalid `--credential` names exit 2 without echoing invalid name.
    for args in [
        vec![
            "run",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "m",
            "--credential",
            "a",
            "--credential",
            "b",
            "--input",
            "hi",
        ],
        vec![
            "-p",
            "hi",
            "--no-config",
            "--provider",
            "openai-chat",
            "--model",
            "m",
            "--credential",
            "bad.dotted",
        ],
        vec![
            "session",
            "create",
            "--no-config",
            "--store",
            store_dir.to_str().unwrap(),
            "--provider",
            "openai-chat",
            "--model",
            "m",
            "--credential",
            "_invalid",
        ],
    ] {
        let out = tars_cmd(&tars_home).args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {out:?}");
        assert_no_secret(&out, "bad.dotted");
        assert_no_secret(&out, "_invalid");
    }

    // `--help` outputs on `auth`, `run`, `-p`, `session create`, and top-level include `auth` / `--credential`.
    for (args, needles) in [
        (
            vec!["--help"],
            vec!["tars auth <set | list | remove | path>", "--credential"],
        ),
        (
            vec!["auth", "--help"],
            vec!["Usage: tars auth set", "remove", "path"],
        ),
        (
            vec!["auth", "set", "--help"],
            vec!["Usage: tars auth set", "--stdin"],
        ),
        (
            vec!["auth", "list", "--help"],
            vec!["Usage: tars auth list", "--json"],
        ),
        (
            vec!["auth", "remove", "--help"],
            vec!["Usage: tars auth remove"],
        ),
        (
            vec!["auth", "path", "--help"],
            vec!["Usage: tars auth path"],
        ),
        (vec!["run", "--help"], vec!["--credential", "--api-key-env"]),
        (vec!["-p", "--help"], vec!["--credential", "--api-key-env"]),
        (
            vec!["session", "create", "--help"],
            vec!["--credential", "--api-key-env"],
        ),
    ] {
        let out = tars_cmd(&tars_home).args(&args).output().unwrap();
        assert!(out.status.success(), "{args:?}: {out:?}");
        let stdout = text(&out.stdout);
        for needle in needles {
            assert!(
                stdout.contains(needle),
                "{args:?}: missing {needle:?} in {stdout}"
            );
        }
    }

    // Shell completions include `auth`, subcommands, and `--credential` / `credential`.
    for (shell, needles) in [
        ("bash", vec!["auth", "set list remove path", "--credential"]),
        (
            "zsh",
            vec!["auth", "set:Store", "remove:Remove", "--credential"],
        ),
        (
            "fish",
            vec![
                "-a auth",
                "-a set",
                "-a list",
                "-a remove",
                "-a path",
                "-l credential",
            ],
        ),
    ] {
        let out = tars_cmd(&tars_home)
            .args(["completions", shell])
            .output()
            .unwrap();
        assert!(out.status.success(), "{shell}: {out:?}");
        let script = text(&out.stdout);
        for needle in needles {
            assert!(script.contains(needle), "{shell}: missing {needle:?}");
        }
    }
}

#[test]
fn session_compact_and_repair_resolve_store_backed_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let tars_home = dir.path().join("tars-home");
    let config_dir = dir.path().join("cfg");
    let config_path = config_dir.join("config.toml");
    let creds_path = config_dir.join("credentials.toml");
    let store_dir = dir.path().join("store");
    let secret = "sk-compact-repair-store-secret-999";

    fs::create_dir_all(&config_dir).unwrap();
    CredentialsStore::set(&creds_path, "work", secret).unwrap();

    // Multi-round server: streaming for `session run`, JSON for `session compact`.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/fixture", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for _ in 0..2 {
            let Ok(mut socket) = sse::accept_post(&listener) else {
                return;
            };
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
            let raw = String::from_utf8(bytes).unwrap();
            let body: Value = serde_json::from_str(&raw[split..]).unwrap();
            let streaming = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
            tx.send(raw).unwrap();
            let (content_type, payload) = if streaming {
                sse::reply(200, chat_body("turn-ok"))
            } else {
                (
                    "application/json",
                    json!({
                        "choices": [{
                            "message": { "role": "assistant", "content": "compacted summary" },
                            "finish_reason": "stop"
                        }]
                    })
                    .to_string(),
                )
            };
            let _ = write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
        }
    });

    fs::write(
        &config_path,
        format!(
            "version = 1\nstore = '{}'\ndefault_profile = 'work'\n[profiles.work]\nprovider = 'openai-chat'\nmodel = 'fixture-model'\nendpoint = '{endpoint}'\n",
            store_dir.display()
        ),
    )
    .unwrap();

    let create_out = tars_cmd(&tars_home)
        .args([
            "session",
            "create",
            "--config",
            config_path.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(create_out.status.success(), "{create_out:?}");
    let conv_id = serde_json::from_slice::<Value>(&create_out.stdout).unwrap()["conversation_id"]
        .as_str()
        .unwrap()
        .to_owned();

    let run_out = tars_cmd(&tars_home)
        .args([
            "session",
            "run",
            &conv_id,
            "--config",
            config_path.to_str().unwrap(),
            "--input",
            "hello",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(run_out.status.success(), "{run_out:?}");
    assert_no_secret(&run_out, secret);

    let repair_out = tars_cmd(&tars_home)
        .args([
            "session",
            "repair",
            &conv_id,
            "--config",
            config_path.to_str().unwrap(),
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(repair_out.status.success(), "{repair_out:?}");
    assert_no_secret(&repair_out, secret);
    let repair_json: Value = serde_json::from_slice(&repair_out.stdout).unwrap();
    assert_eq!(repair_json["status"], "not_needed");

    let compact_out = tars_cmd(&tars_home)
        .args([
            "session",
            "compact",
            &conv_id,
            "--config",
            config_path.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    assert!(compact_out.status.success(), "{compact_out:?}");
    assert_no_secret(&compact_out, secret);
    let compact_json: Value = serde_json::from_slice(&compact_out.stdout).unwrap();
    assert_eq!(compact_json["status"], "compacted");

    let req1 = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    let req2 = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(req1.contains(&format!("authorization: Bearer {secret}")));
    assert!(req2.contains(&format!("authorization: Bearer {secret}")));

    // Counterexample: `--no-config` with `--store` leaves `credentials_path = None` and fails closed.
    let no_cfg_repair = tars_cmd(&tars_home)
        .env("OPENAI_API_KEY", "collide-env-key")
        .args([
            "session",
            "repair",
            &conv_id,
            "--no-config",
            "--store",
            store_dir.to_str().unwrap(),
            "--dry-run",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(!no_cfg_repair.status.success());
    assert_no_secret(&no_cfg_repair, secret);
}

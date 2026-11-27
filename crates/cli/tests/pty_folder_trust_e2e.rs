#![cfg(unix)]

use serde_json::Value;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const PTY_DRIVER: &str = r#"
import errno
import fcntl
import os
import pty
import re
import select
import signal
import struct
import subprocess
import sys
import termios
import time

bin_path = sys.argv[1]
cwd = sys.argv[2]
home_dir = sys.argv[3]
tars_home = sys.argv[4]
path_env = sys.argv[5]
mode = sys.argv[6]

ANSI_CURSOR_RE = re.compile(r"\x1b\[[0-9;]*[Hf]")
ANSI_CSI_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")
ANSI_OSC_RE = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)")
ANSI_ESC_RE = re.compile(r"\x1b[@-_]")

def strip_ansi(raw: bytes) -> str:
    text = raw.decode("utf-8", errors="replace")
    text = ANSI_OSC_RE.sub("", text)
    text = ANSI_CURSOR_RE.sub(" ", text)
    text = ANSI_CSI_RE.sub("", text)
    text = ANSI_ESC_RE.sub("", text)
    return " ".join(text.split())

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 100, 0, 0))

env = os.environ.copy()
env["HOME"] = home_dir
env["TARS_HOME"] = tars_home
env["PATH"] = path_env
env["TERM"] = "xterm-256color"
env.pop("TARS_CONFIG", None)
env.pop("TARS_MCP_CONFIG", None)

proc = subprocess.Popen(
    [bin_path],
    cwd=cwd,
    env=env,
    stdin=slave,
    stdout=slave,
    stderr=slave,
    preexec_fn=os.setsid,
    close_fds=True,
)
os.close(slave)

buf = bytearray()

def read_until(needle: str, timeout: float = 12.0) -> str:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        cleaned = strip_ansi(bytes(buf))
        if needle in cleaned:
            return cleaned
        r, _, _ = select.select([master], [], [], 0.05)
        if master in r:
            try:
                chunk = os.read(master, 4096)
            except OSError as exc:
                if exc.errno == errno.EIO:
                    break
                raise
            if not chunk:
                break
            buf.extend(chunk)
        if proc.poll() is not None:
            # Drain any final bytes after exit.
            while True:
                r2, _, _ = select.select([master], [], [], 0.02)
                if master not in r2:
                    break
                try:
                    chunk = os.read(master, 4096)
                except OSError:
                    break
                if not chunk:
                    break
                buf.extend(chunk)
            break
    cleaned = strip_ansi(bytes(buf))
    if needle not in cleaned:
        raise AssertionError(f"timed out waiting for {needle!r}; got: {cleaned!r}")
    return cleaned

def wait_exit(timeout: float = 10.0) -> int:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        rc = proc.poll()
        if rc is not None:
            return rc
        r, _, _ = select.select([master], [], [], 0.05)
        if master in r:
            try:
                chunk = os.read(master, 4096)
                if chunk:
                    buf.extend(chunk)
            except OSError:
                pass
    proc.kill()
    proc.wait()
    raise AssertionError(f"process did not exit within {timeout}s; output={strip_ansi(bytes(buf))!r}")

try:
    if mode == "wizard_ctrl_c":
        read_until("Do you trust the files in this folder?")
        os.write(master, b"\x03")
        rc = wait_exit()
        assert rc == 0, f"expected exit 0, got {rc}"
    elif mode == "wizard_accept_y_then_ctrl_c":
        read_until("Do you trust the files in this folder?")
        time.sleep(0.65)  # past 500ms CONFIRM_GUARD_WINDOW
        buf.clear()
        os.write(master, b"y")
        screen = read_until("✓ Trusted")
        assert "Welcome to TARS" in screen or "No model configured" in screen, (
            f"expected post-trust welcome screen, got: {screen!r}"
        )
        os.write(master, b"\x03")
        rc = wait_exit()
        assert rc == 0, f"expected exit 0, got {rc}"
    elif mode == "trusted_relaunch_then_ctrl_c":
        screen = read_until("✓ Trusted")
        assert "Do you trust the files in this folder?" not in screen, (
            f"trusted folder must skip wizard, got: {screen!r}"
        )
        assert "Welcome to TARS" in screen, (
            f"expected welcome screen on trusted relaunch, got: {screen!r}"
        )
        os.write(master, b"\x03")
        rc = wait_exit()
        assert rc == 0, f"expected exit 0, got {rc}"
    else:
        raise ValueError(f"unknown mode: {mode}")
finally:
    try:
        os.close(master)
    except OSError:
        pass
    if proc.poll() is None:
        os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
        proc.wait()
"#;

fn create_fake_rg(bin_dir: &Path) {
    std::fs::create_dir_all(bin_dir).unwrap();
    let rg = bin_dir.join("rg");
    std::fs::write(&rg, "#!/bin/sh\necho 'ripgrep 15.2.0'\n").unwrap();
    let mut perms = std::fs::metadata(&rg).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&rg, perms).unwrap();
}

fn run_pty_case(ws: &Path, home: &Path, tars_home: &Path, path_env: &str, mode: &str) {
    let output = Command::new("python3")
        .arg("-c")
        .arg(PTY_DRIVER)
        .arg(env!("CARGO_BIN_EXE_tars"))
        .arg(ws)
        .arg(home)
        .arg(tars_home)
        .arg(path_env)
        .arg(mode)
        .output()
        .expect("run python3 PTY driver");
    assert!(
        output.status.success(),
        "mode={mode} failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn trust_list_json(home: &Path, tars_home: &Path) -> Vec<Value> {
    let out = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(["trust", "list", "--json"])
        .env("HOME", home)
        .env("TARS_HOME", tars_home)
        .env_remove("TARS_CONFIG")
        .output()
        .expect("run tars trust list");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn ft08_real_pty_folder_trust_wizard_ctrl_c_and_revoke_e2e() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("workspace");
    let home = dir.path().join("home");
    let tars_home = dir.path().join("tars-home");
    let bin_dir = dir.path().join("bin");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    create_fake_rg(&bin_dir);

    let system_path = std::env::var("PATH").unwrap_or_default();
    let path_env = format!("{}:{system_path}", bin_dir.display());
    let canonical_ws = ws.canonicalize().unwrap();

    // 1. Fresh untrusted directory, isolated HOME, no config.toml:
    //    wizard appears, Ctrl+C exits cleanly (code 0), no config.toml created, folder stays untrusted.
    run_pty_case(&ws, &home, &tars_home, &path_env, "wizard_ctrl_c");
    assert!(!home.join(".tars/config.toml").exists());
    assert!(!tars_home.join("config.toml").exists());
    assert!(trust_list_json(&home, &tars_home).is_empty());

    // 2. Relaunch, confirm trust with `y` past 500ms guard -> enters welcome screen with ✓ Trusted,
    //    still no config.toml, Ctrl+C exits cleanly (code 0), trust persisted in tars.db.
    run_pty_case(
        &ws,
        &home,
        &tars_home,
        &path_env,
        "wizard_accept_y_then_ctrl_c",
    );
    assert!(!home.join(".tars/config.toml").exists());
    assert!(!tars_home.join("config.toml").exists());
    let trusted = trust_list_json(&home, &tars_home);
    assert_eq!(trusted.len(), 1, "{trusted:?}");
    assert_eq!(trusted[0]["workspace_root"], canonical_ws.to_str().unwrap());

    // 3. Relaunch in the same trusted folder: wizard is skipped, main welcome screen appears,
    //    idle Ctrl+C exits cleanly (code 0).
    run_pty_case(
        &ws,
        &home,
        &tars_home,
        &path_env,
        "trusted_relaunch_then_ctrl_c",
    );

    // 4. Revoke via `tars trust revoke` and confirm relaunch prompts the wizard again.
    let revoke = Command::new(env!("CARGO_BIN_EXE_tars"))
        .args(["trust", "revoke"])
        .current_dir(&ws)
        .env("HOME", &home)
        .env("TARS_HOME", &tars_home)
        .env_remove("TARS_CONFIG")
        .output()
        .expect("run tars trust revoke");
    assert!(
        revoke.status.success(),
        "{}",
        String::from_utf8_lossy(&revoke.stderr)
    );
    assert!(trust_list_json(&home, &tars_home).is_empty());

    run_pty_case(&ws, &home, &tars_home, &path_env, "wizard_ctrl_c");
    assert!(trust_list_json(&home, &tars_home).is_empty());
}

use super::test_support::RecordingBackend;
use super::*;
use std::path::PathBuf;

fn full_report() -> BackendReport {
    BackendReport::new(
        BackendId::RECORDING,
        Capability::Full,
        Capability::Full,
        Capability::Full,
        Capability::Full,
        Capability::Partial(Limitation::ProcessGroupEscape),
    )
}

fn report(
    file_read: Capability,
    file_write: Capability,
    network: Capability,
    signal_isolation: Capability,
    descendant_lifecycle: Capability,
) -> BackendReport {
    BackendReport::new(
        BackendId::RECORDING,
        file_read,
        file_write,
        network,
        signal_isolation,
        descendant_lifecycle,
    )
}

fn policy() -> (tempfile::TempDir, SandboxPolicy) {
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    let policy = SandboxPolicy::new(vec![root.clone()], vec![root]).unwrap();
    (directory, policy)
}

fn launch() -> Launch {
    Launch::new(std::env::current_exe().unwrap())
}

#[test]
fn policy_paths_are_canonical_and_deduplicated() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let canonical = std::fs::canonicalize(&root).unwrap();
    let policy =
        SandboxPolicy::new(vec![root.clone(), root.clone()], vec![root.clone(), root]).unwrap();
    assert_eq!(policy.read_roots, std::slice::from_ref(&canonical));
    assert_eq!(policy.write_roots, &[canonical]);
}

#[test]
fn policy_rejects_relative_paths() {
    let result = SandboxPolicy::new(vec![PathBuf::from("relative")], Vec::new());
    assert!(matches!(result, Err(SandboxPolicyError::AbsolutePath(_))));
}

#[test]
fn policy_rejects_a_non_directory_root() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("not-a-directory");
    std::fs::write(&file, "x").unwrap();
    let result = SandboxPolicy::new(vec![file], Vec::new());
    assert!(matches!(result, Err(SandboxPolicyError::NotDirectory(_))));
}

#[test]
fn policy_rejects_control_characters_in_paths() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bad\nname");
    let result = SandboxPolicy::new(vec![path], Vec::new());
    assert!(matches!(
        result,
        Err(SandboxPolicyError::ControlCharacter(_))
    ));
}

#[cfg(unix)]
#[test]
fn policy_rejects_non_utf8_paths() {
    use std::os::unix::ffi::OsStringExt;
    let path = PathBuf::from(std::ffi::OsString::from_vec(b"/bad\x80name".to_vec()));
    let result = SandboxPolicy::new(vec![path], Vec::new());
    assert!(matches!(result, Err(SandboxPolicyError::NonUtf8Path(_))));
}

#[test]
fn read_only_subpath_can_target_a_missing_leaf() {
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    let git = root.join(".git");
    let policy = SandboxPolicy::new(vec![root.clone()], vec![root])
        .unwrap()
        .with_read_only_subpath(&git)
        .unwrap();
    assert_eq!(policy.read_only_subpaths, vec![git]);
}

#[cfg(unix)]
#[test]
fn read_only_subpath_keeps_the_lexical_entry_when_a_symlink_leaves_the_root() {
    let directory = tempfile::tempdir().unwrap();
    let write_root = directory.path().join("write");
    std::fs::create_dir(&write_root).unwrap();
    let root = std::fs::canonicalize(&write_root).unwrap();
    let outside = directory.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    let git = root.join(".git");
    std::os::unix::fs::symlink(&outside, &git).unwrap();
    let policy = SandboxPolicy::new(vec![root.clone()], vec![root])
        .unwrap()
        .with_read_only_subpath(&git)
        .unwrap();
    assert!(policy.read_only_subpaths.contains(&git));
    assert!(
        !policy
            .read_only_subpaths
            .contains(&std::fs::canonicalize(outside).unwrap())
    );
}

#[test]
fn policy_rejects_relative_components_even_under_an_absolute_root() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let policy = SandboxPolicy::new(vec![root.clone()], vec![root.clone()]).unwrap();
    let denied = root.join("missing/../secret");
    assert!(matches!(
        policy.with_denied_path(&denied),
        Err(SandboxPolicyError::RelativeComponent(_))
    ));
    assert!(matches!(
        SandboxPolicy::new(vec![root.join("missing/../root")], Vec::new()),
        Err(SandboxPolicyError::RelativeComponent(_))
    ));
}

#[test]
fn policy_has_no_backend_vocabulary() {
    let (_directory, policy) = policy();
    let debug = format!("{policy:?}");
    for word in ["seatbelt", "sbpl", "bwrap", "mxc"] {
        assert!(!debug.contains(word), "policy leaked {word}");
    }
}

#[test]
fn sandbox_and_policy_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Sandbox>();
    assert_send_sync::<SandboxPolicy>();
}

#[test]
fn read_only_subpath_must_be_inside_a_writable_root() {
    let directory = tempfile::tempdir().unwrap();
    let write_root = directory.path().join("write");
    let other = directory.path().join("other");
    std::fs::create_dir(&write_root).unwrap();
    std::fs::create_dir(&other).unwrap();
    let some_child = write_root.join("child");
    std::fs::create_dir(&some_child).unwrap();
    let policy = SandboxPolicy::new(vec![write_root.clone()], vec![write_root]).unwrap();
    assert!(policy.clone().with_read_only_subpath(&some_child).is_ok());
    assert!(matches!(
        policy.with_read_only_subpath(&other),
        Err(SandboxPolicyError::OutsideWritable(_))
    ));
}

#[test]
fn report_accepts_full_capabilities_and_partial_descendants() {
    let (_directory, policy) = policy();
    assert!(full_report().check(&policy).is_ok());
    let partial_descendants = report(
        Capability::Full,
        Capability::Full,
        Capability::Full,
        Capability::Full,
        Capability::Partial(Limitation::ProcessGroupEscape),
    );
    assert!(partial_descendants.check(&policy).is_ok());
}

#[test]
fn report_rejects_unsupported_and_unaccepted_partial_dimensions() {
    let (_directory, policy) = policy();
    let cases = [
        report(
            Capability::Partial(Limitation::Other("partial read".to_owned())),
            Capability::Full,
            Capability::Full,
            Capability::Full,
            Capability::Full,
        ),
        report(
            Capability::Full,
            Capability::Unsupported("no write boundary".to_owned()),
            Capability::Full,
            Capability::Full,
            Capability::Full,
        ),
        report(
            Capability::Full,
            Capability::Full,
            Capability::Unsupported("no network boundary".to_owned()),
            Capability::Full,
            Capability::Full,
        ),
        report(
            Capability::Full,
            Capability::Full,
            Capability::Full,
            Capability::Partial(Limitation::Other("partial signals".to_owned())),
            Capability::Full,
        ),
        report(
            Capability::Full,
            Capability::Full,
            Capability::Full,
            Capability::Full,
            Capability::Unsupported("no descendant control".to_owned()),
        ),
        report(
            Capability::Full,
            Capability::Full,
            Capability::Full,
            Capability::Full,
            Capability::Partial(Limitation::Other("no descendant control".to_owned())),
        ),
    ];
    for case in cases {
        assert!(case.check(&policy).is_err());
    }
}

#[test]
fn report_does_not_require_network_isolation_when_network_is_open() {
    let (_directory, policy) = policy();
    let policy = policy.with_network(NetworkAccess::Open);
    let open_network = report(
        Capability::Full,
        Capability::Full,
        Capability::Unsupported("no network boundary".to_owned()),
        Capability::Full,
        Capability::Full,
    );
    assert!(open_network.check(&policy).is_ok());
}

#[test]
fn sandbox_checks_the_report_before_calling_the_backend() {
    let (_directory, policy) = policy();
    let backend = RecordingBackend::available(report(
        Capability::Full,
        Capability::Full,
        Capability::Unsupported("no network boundary".to_owned()),
        Capability::Full,
        Capability::Full,
    ));
    let calls = backend.calls();
    let sandbox = Sandbox::open_with(Box::new(backend)).unwrap();
    assert!(matches!(
        sandbox.wrap(&policy, launch()),
        Err(SandboxError::Unavailable(_))
    ));
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn sandbox_fails_closed_when_the_probe_fails() {
    let backend = RecordingBackend::unavailable();
    let calls = backend.calls();
    let error = Sandbox::open_with(Box::new(backend)).unwrap_err();
    assert_eq!(error.backend, BackendId::RECORDING);
    assert!(calls.lock().unwrap().is_empty());
}

#[cfg(not(target_os = "macos"))]
#[test]
fn unavailable_backend_rejects_unconfined_launches() {
    use super::backend::SandboxBackend;
    let backend = super::unavailable::UnavailableBackend::new();
    // Match the error instead of expect_err: SandboxedCommand deliberately has
    // no Debug bound, and this test must also compile for non-macOS targets.
    let result = backend.unconfined(Launch::new("/bin/true"));
    assert!(
        matches!(result, Err(SandboxError::Unavailable(_))),
        "an unavailable backend must not launch unconfined"
    );
}

#[test]
fn recording_backend_wraps_and_records_the_policy() {
    let (_directory, policy) = policy();
    let backend = RecordingBackend::available(full_report());
    let calls = backend.calls();
    let sandbox = Sandbox::open_with(Box::new(backend)).unwrap();
    let launch = launch();
    let expected = launch.program().to_path_buf();
    let _wrapped = sandbox.wrap(&policy, launch).unwrap();
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, policy);
    assert_eq!(calls[0].1.program(), expected);
}

#[test]
fn recording_backend_records_launch_fields() {
    let (directory, policy) = policy();
    let backend = RecordingBackend::available(full_report());
    let calls = backend.calls();
    let sandbox = Sandbox::open_with(Box::new(backend)).unwrap();
    let program = std::env::current_exe().unwrap();
    let expected_program = program.clone();
    let launch = Launch::new(program)
        .arg("one")
        .args(["two", "three"])
        .current_dir(directory.path())
        .env("TARS_TEST", "value");
    let _wrapped = sandbox.wrap(&policy, launch).unwrap();
    let calls = calls.lock().unwrap();
    let launch = &calls[0].1;
    assert_eq!(launch.program(), expected_program.as_path());
    assert_eq!(
        launch.launch_args(),
        &[
            std::ffi::OsString::from("one"),
            std::ffi::OsString::from("two"),
            std::ffi::OsString::from("three")
        ]
    );
    assert_eq!(launch.working_directory(), Some(directory.path()));
    assert_eq!(
        launch.environment(),
        &[(std::ffi::OsString::from("TARS_TEST"), "value".into())]
    );
}

#[test]
fn sandbox_accepts_the_process_group_escape_limitation() {
    let (_directory, policy) = policy();
    let backend = RecordingBackend::available(full_report());
    let sandbox = Sandbox::open_with(Box::new(backend)).unwrap();
    assert!(sandbox.wrap(&policy, launch()).is_ok());
}

#[test]
fn sandbox_rejects_a_report_for_another_backend() {
    let mut report = full_report();
    report.backend = BackendId::SEATBELT;
    let backend = RecordingBackend::available(report);
    let error = Sandbox::open_with(Box::new(backend)).unwrap_err();
    assert_eq!(error.backend, BackendId::SEATBELT);
}

#[test]
fn sandbox_rejects_a_relative_launch_before_calling_the_backend() {
    let (_directory, policy) = policy();
    let backend = RecordingBackend::available(full_report());
    let calls = backend.calls();
    let sandbox = Sandbox::open_with(Box::new(backend)).unwrap();
    assert!(matches!(
        sandbox.wrap(&policy, Launch::new("relative-program")),
        Err(SandboxError::InvalidLaunch(_))
    ));
    assert!(calls.lock().unwrap().is_empty());
}

#[cfg(not(target_os = "macos"))]
#[test]
fn unsupported_platforms_fail_closed_through_the_public_entry() {
    let error = Sandbox::open().unwrap_err();
    assert_eq!(error.backend, BackendId::UNAVAILABLE);
}

#[cfg(target_os = "macos")]
mod seatbelt {
    use super::*;
    use crate::ProcessTree;
    use crate::backend::SandboxBackend;
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::process::{ExitStatus, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};
    use tokio::io::AsyncReadExt;

    const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
    const DNS_SOCKET: &str = "/private/var/run/mDNSResponder";

    struct RunOutput {
        status: ExitStatus,
        stdout: String,
        stderr: String,
    }

    fn open_sandbox() -> Sandbox {
        Sandbox::open().expect("the macOS Seatbelt backend must probe successfully")
    }

    fn canonical(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).unwrap()
    }

    fn base_policy(workspace: &Path, private_tmp: &Path) -> SandboxPolicy {
        SandboxPolicy::new(
            vec![canonical(workspace), canonical(private_tmp)],
            vec![canonical(workspace), canonical(private_tmp)],
        )
        .unwrap()
    }

    fn configure(command: &mut SandboxedCommand) {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    }

    fn bash_launch(
        script: &str,
        workspace: &Path,
        private_tmp: &Path,
        home: &Path,
        args: &[&Path],
    ) -> Launch {
        let mut launch = Launch::new("/bin/bash")
            .arg("-c")
            .arg(script)
            .arg("tars-test")
            .current_dir(canonical(workspace))
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", home.as_os_str())
            .env("LANG", "C")
            .env("TMPDIR", canonical(private_tmp).as_os_str());
        for arg in args {
            launch = launch.arg(arg.as_os_str());
        }
        launch
    }

    fn tool_launch(program: &str, workspace: &Path, private_tmp: &Path, home: &Path) -> Launch {
        Launch::new(program)
            .current_dir(canonical(workspace))
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", home.as_os_str())
            .env("LANG", "C")
            .env("TMPDIR", canonical(private_tmp).as_os_str())
    }

    async fn read_output(tree: &mut ProcessTree) -> (String, String) {
        let mut stdout = String::new();
        let mut stderr = String::new();
        if let Some(mut pipe) = tree.take_stdout() {
            let _ = pipe.read_to_string(&mut stdout).await;
        }
        if let Some(mut pipe) = tree.take_stderr() {
            let _ = pipe.read_to_string(&mut stderr).await;
        }
        (stdout, stderr)
    }

    async fn run(sandbox: &Sandbox, policy: &SandboxPolicy, launch: Launch) -> RunOutput {
        let mut wrapped = sandbox.wrap(policy, launch).expect("wrap command");
        configure(&mut wrapped);
        let mut tree = wrapped.spawn().expect("spawn command");
        let status = match tokio::time::timeout(COMMAND_TIMEOUT, tree.wait()).await {
            Ok(result) => result.expect("wait command"),
            Err(_) => {
                let status = tree
                    .terminate(Duration::from_secs(1))
                    .await
                    .expect("terminate timed out command");
                let (stdout, stderr) = read_output(&mut tree).await;
                panic!("command timed out: {status:?} stdout={stdout} stderr={stderr}");
            }
        };
        let (stdout, stderr) = read_output(&mut tree).await;
        RunOutput {
            status,
            stdout,
            stderr,
        }
    }

    fn spawn(sandbox: &Sandbox, policy: &SandboxPolicy, launch: Launch) -> ProcessTree {
        let mut wrapped = sandbox.wrap(policy, launch).expect("wrap command");
        configure(&mut wrapped);
        wrapped.spawn().expect("spawn command")
    }

    async fn wait_for_file(path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn read_pid(path: &Path) -> u32 {
        std::fs::read_to_string(path)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    fn alive(pid: u32) -> bool {
        let Some(pid) = rustix::process::Pid::from_raw(pid as i32) else {
            return false;
        };
        match rustix::process::test_kill_process(pid) {
            Ok(()) => true,
            Err(rustix::io::Errno::SRCH) => false,
            Err(_) => true,
        }
    }

    fn kill(pid: u32) {
        let Some(pid) = rustix::process::Pid::from_raw(pid as i32) else {
            return;
        };
        let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
    }

    struct KillOnDrop(u32);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            if alive(self.0) {
                kill(self.0);
            }
        }
    }

    async fn wait_dead(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(pid) {
            assert!(
                Instant::now() < deadline,
                "process {pid} survived process-tree cleanup"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    struct Listener {
        port: u16,
        accepted: Arc<AtomicBool>,
        stop: Arc<AtomicBool>,
        handle: std::thread::JoinHandle<()>,
    }

    impl Listener {
        fn start() -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            listener.set_nonblocking(true).unwrap();
            let accepted = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let accepted_for_thread = accepted.clone();
            let stop_for_thread = stop.clone();
            let handle = std::thread::spawn(move || {
                while !stop_for_thread.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) if carries_marker(&stream) => {
                            accepted_for_thread.store(true, Ordering::SeqCst);
                            break;
                        }
                        Ok(_) => continue,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                port,
                accepted,
                stop,
                handle,
            }
        }

        fn finish(self, expect_accept: bool) -> bool {
            let deadline = Instant::now() + Duration::from_secs(2);
            while expect_accept
                && !self.accepted.load(Ordering::SeqCst)
                && Instant::now() < deadline
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            self.stop.store(true, Ordering::SeqCst);
            self.handle.join().unwrap();
            self.accepted.load(Ordering::SeqCst)
        }
    }

    /// Sent by the sandboxed client so a listener can tell its connection
    /// from other local programs that connect to new ports.
    const MARKER: &[u8] = b"tars-sandbox-probe";

    fn carries_marker(stream: &std::net::TcpStream) -> bool {
        let mut received = [0; MARKER.len()];
        stream.set_nonblocking(false).is_ok()
            && stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .is_ok()
            && std::io::Read::read_exact(&mut &*stream, &mut received).is_ok()
            && received == MARKER
    }

    fn nc_launch(
        host: &str,
        port: u16,
        workspace: &Path,
        private_tmp: &Path,
        home: &Path,
    ) -> Launch {
        let port = port.to_string();
        let marker = std::str::from_utf8(MARKER).unwrap();
        Launch::new("/bin/sh")
            .args([
                "-c",
                "printf '%s' \"$0\" | /usr/bin/nc -w 1 \"$1\" \"$2\"",
                marker,
                host,
                port.as_str(),
            ])
            .current_dir(canonical(workspace))
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", home.as_os_str())
            .env("LANG", "C")
            .env("TMPDIR", canonical(private_tmp).as_os_str())
    }

    fn unix_launch(socket: &Path, workspace: &Path, private_tmp: &Path, home: &Path) -> Launch {
        Launch::new("/usr/bin/nc")
            .args(["-U", "-w", "1"])
            .arg(socket)
            .current_dir(canonical(workspace))
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", home.as_os_str())
            .env("LANG", "C")
            .env("TMPDIR", canonical(private_tmp).as_os_str())
    }

    #[test]
    fn probe_reports_the_seatbelt_backend() {
        let sandbox = open_sandbox();
        assert_eq!(sandbox.report().backend, BackendId::SEATBELT);
        assert!(matches!(
            sandbox.report().descendant_lifecycle,
            Capability::Partial(_)
        ));
    }

    #[tokio::test]
    async fn file_boundaries_hold_for_writes_reads_and_symlinks() {
        let sandbox = open_sandbox();
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let outside = base.path().join("outside");
        let home = base.path().join("home");
        std::fs::create_dir_all(workspace.join(".git/hooks")).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), "TOP-SECRET\n").unwrap();
        std::fs::create_dir_all(home.join(".aws")).unwrap();
        std::fs::write(home.join(".aws/credentials"), "AWS-SECRET\n").unwrap();
        std::os::unix::fs::symlink(&outside, workspace.join("link")).unwrap();
        std::fs::write(workspace.join("denied.txt"), "denied\n").unwrap();
        std::fs::write(workspace.join("denied-secret"), "TOP-SECRET\n").unwrap();

        let workspace = canonical(&workspace);
        let private_tmp = canonical(&private_tmp);
        let outside = canonical(&outside);
        let home = canonical(&home);
        let policy = base_policy(&workspace, &private_tmp)
            .with_read_only_subpath(workspace.join(".git"))
            .unwrap()
            .with_denied_path(workspace.join("denied.txt"))
            .unwrap()
            .with_denied_path(workspace.join("denied-secret"))
            .unwrap();

        let script = r#"set -u
ws=$1
outside=$2
home=$3
printf ok > "$ws/inside.txt" || exit 60
printf more >> "$ws/inside.txt" || exit 61
mkdir -p "$ws/sub" || exit 62
mv "$ws/inside.txt" "$ws/renamed.txt" || exit 63
rm "$ws/renamed.txt" || exit 64
rmdir "$ws/sub" || exit 65
if printf x > "$outside/escape.txt" 2>/dev/null; then echo outside-write-allowed; exit 66; fi
if [ -e "$outside/escape.txt" ]; then echo outside-write-exists; exit 67; fi
if printf x > "$ws/../outside/parent.txt" 2>/dev/null; then echo parent-write-allowed; exit 68; fi
if printf x > "$ws/link/escape.txt" 2>/dev/null; then echo symlink-write-allowed; exit 69; fi
if printf x > "$ws/.git/hooks/x" 2>/dev/null; then echo git-write-allowed; exit 70; fi
if grep -q TOP-SECRET "$home/.ssh/id_ed25519" 2>/dev/null; then echo secret-leaked; exit 71; fi
if grep -q AWS-SECRET "$home/.aws/credentials" 2>/dev/null; then echo aws-secret-leaked; exit 74; fi
if printf x > "$ws/denied.txt" 2>/dev/null; then echo denied-write-allowed; exit 72; fi
if grep -q TOP-SECRET "$ws/denied-secret" 2>/dev/null; then echo denied-read-leaked; exit 73; fi
echo boundary-ok
"#;
        let launch = bash_launch(
            script,
            &workspace,
            &private_tmp,
            &home,
            &[&workspace, &outside, &home],
        );
        let output = run(&sandbox, &policy, launch).await;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("boundary-ok"));
        assert!(!output.stdout.contains("TOP-SECRET"));
        assert!(!output.stderr.contains("TOP-SECRET"));
        assert!(!output.stdout.contains("AWS-SECRET"));
        assert!(!output.stderr.contains("AWS-SECRET"));
        assert!(!outside.join("escape.txt").exists());
        assert!(!workspace.join(".git/hooks/x").exists());
        assert_eq!(
            std::fs::read_to_string(workspace.join("denied.txt")).unwrap(),
            "denied\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("denied-secret")).unwrap(),
            "TOP-SECRET\n"
        );
    }

    #[tokio::test]
    async fn read_only_git_applies_before_the_leaf_exists() {
        let sandbox = open_sandbox();
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        let workspace = canonical(&workspace);
        let private_tmp = canonical(&private_tmp);
        let home = canonical(&home);
        let policy = base_policy(&workspace, &private_tmp)
            .with_read_only_subpath(workspace.join(".git"))
            .unwrap();

        let script = r#"set -u
if mkdir "$1/.git" 2>/dev/null; then echo git-created; exit 1; fi
if printf x > "$1/.git" 2>/dev/null; then echo git-file-written; exit 2; fi
echo git-missing-denied
"#;
        let launch = bash_launch(script, &workspace, &private_tmp, &home, &[&workspace]);
        let output = run(&sandbox, &policy, launch).await;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("git-missing-denied"));
        assert!(!workspace.join(".git").exists());
    }

    #[tokio::test]
    async fn cargo_config_is_readable_while_credentials_stay_denied() {
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        std::fs::write(home.join(".cargo/config.toml"), "PROBE-CONFIG\n").unwrap();
        std::fs::write(home.join(".cargo/credentials.toml"), "TOKEN\n").unwrap();

        let backend = super::super::seatbelt::SeatbeltBackend::with_home(home.clone());
        let sandbox =
            Sandbox::open_with(Box::new(backend)).expect("a fake home must not break the probe");
        let policy = base_policy(&workspace, &private_tmp);
        let script = r#"set -u
if ! grep -q PROBE-CONFIG "$HOME/.cargo/config.toml" 2>/dev/null; then echo config-unreadable; exit 1; fi
if cat "$HOME/.cargo/credentials.toml" >/dev/null 2>&1; then echo credentials-leaked; exit 2; fi
echo credential-denied
"#;
        let launch = bash_launch(script, &workspace, &private_tmp, &home, &[]);
        let output = run(&sandbox, &policy, launch).await;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("credential-denied"));
        assert!(!output.stdout.contains("TOKEN"));
        assert!(!output.stderr.contains("TOKEN"));
    }

    #[test]
    fn credential_denies_follow_every_allow() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        let backend = super::super::seatbelt::SeatbeltBackend::with_home(home.clone());
        let (_policy_directory, policy) = policy();
        let profile = backend.profile(&policy).unwrap();
        let allow = profile
            .rfind("(allow file-read*")
            .expect("profile has a read allow");
        let deny = profile
            .find("(deny file-read* file-write* (subpath")
            .expect("profile has a credential deny");
        assert!(deny > allow, "a later allow could re-open a credential");
    }

    #[tokio::test]
    async fn network_is_denied_by_default_and_can_be_opened() {
        let sandbox = open_sandbox();
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();

        let closed_listener = Listener::start();
        let closed_policy = base_policy(&workspace, &private_tmp);
        let closed = run(
            &sandbox,
            &closed_policy,
            nc_launch(
                "127.0.0.1",
                closed_listener.port,
                &workspace,
                &private_tmp,
                &home,
            ),
        )
        .await;
        assert!(!closed.status.success(), "network was not denied");
        assert!(!closed_listener.finish(false));
        let dns = run(
            &sandbox,
            &closed_policy,
            nc_launch("example.com", 443, &workspace, &private_tmp, &home),
        )
        .await;
        assert!(!dns.status.success(), "DNS was not denied");

        let open_listener = Listener::start();
        let open_policy = base_policy(&workspace, &private_tmp).with_network(NetworkAccess::Open);
        let open = run(
            &sandbox,
            &open_policy,
            nc_launch(
                "127.0.0.1",
                open_listener.port,
                &workspace,
                &private_tmp,
                &home,
            ),
        )
        .await;
        assert!(
            open.status.success(),
            "stdout={} stderr={}",
            open.stdout,
            open.stderr
        );
        assert!(
            open_listener.finish(true),
            "the listener did not accept a connection"
        );
    }

    #[tokio::test]
    async fn signals_are_limited_to_the_sandbox_and_keychain_queries_fail() {
        let sandbox = open_sandbox();
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        let policy = base_policy(&workspace, &private_tmp);

        let host_pid = std::process::id().to_string();
        let script = r#"set -u
if kill -0 "$TARS_HOST_PID" 2>/dev/null; then echo host-signal-allowed; exit 80; fi
/bin/sleep 30 & p=$!
if ! kill -0 "$p" 2>/dev/null; then echo own-signal-failed; kill -9 "$p" 2>/dev/null; exit 81; fi
kill -TERM "$p" 2>/dev/null
wait "$p" 2>/dev/null
echo signal-ok
"#;
        let launch = bash_launch(script, &workspace, &private_tmp, &home, &[])
            .env("TARS_HOST_PID", host_pid.as_str());
        let output = run(&sandbox, &policy, launch).await;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("signal-ok"));

        let keychain = run(
            &sandbox,
            &policy,
            tool_launch("/usr/bin/security", &workspace, &private_tmp, &home).arg("list-keychains"),
        )
        .await;
        assert!(
            !keychain.status.success(),
            "a keychain query unexpectedly succeeded"
        );
        let find = run(
            &sandbox,
            &policy,
            tool_launch("/usr/bin/security", &workspace, &private_tmp, &home).args([
                "find-generic-password",
                "-s",
                "tars-probe-no-such-item",
            ]),
        )
        .await;
        assert!(
            !find.status.success(),
            "a keychain lookup unexpectedly succeeded"
        );
    }

    async fn spawn_ignoring_term(
        sandbox: &Sandbox,
        policy: &SandboxPolicy,
        workspace: &Path,
        private_tmp: &Path,
        home: &Path,
    ) -> ProcessTree {
        let script = r#"trap "" TERM
( trap "" TERM; exec /bin/sleep 60 ) & echo $! > "$1/child.pid"
echo $$ > "$1/shell.pid"
wait
"#;
        let launch = bash_launch(script, workspace, private_tmp, home, &[workspace]);
        spawn(sandbox, policy, launch)
    }

    #[tokio::test]
    async fn termination_kills_the_process_group() {
        let sandbox = open_sandbox();
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        let policy = base_policy(&workspace, &private_tmp);

        let mut tree =
            spawn_ignoring_term(&sandbox, &policy, &workspace, &private_tmp, &home).await;
        wait_for_file(&workspace.join("child.pid")).await;
        wait_for_file(&workspace.join("shell.pid")).await;
        let group_pid = read_pid(&workspace.join("child.pid"));
        let shell_pid = read_pid(&workspace.join("shell.pid"));

        let status = tree
            .terminate(Duration::from_millis(200))
            .await
            .expect("terminate");
        assert!(!status.success());
        wait_dead(group_pid).await;
        wait_dead(shell_pid).await;
    }

    #[tokio::test]
    async fn normal_exit_kills_a_background_process() {
        let sandbox = open_sandbox();
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        let policy = base_policy(&workspace, &private_tmp);

        let script = r#"/bin/sleep 60 & echo $! > "$1/child.pid"
exit 0
"#;
        let launch = bash_launch(script, &workspace, &private_tmp, &home, &[&workspace]);
        let mut tree = spawn(&sandbox, &policy, launch);
        wait_for_file(&workspace.join("child.pid")).await;
        let child_pid = read_pid(&workspace.join("child.pid"));
        let status = tree.wait().await.expect("wait");
        assert!(status.success());
        wait_dead(child_pid).await;
    }

    #[tokio::test]
    async fn leader_exit_during_termination_kills_a_background_process() {
        let sandbox = open_sandbox();
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        let policy = base_policy(&workspace, &private_tmp);

        let script = r#"( trap "" TERM; exec /bin/sleep 60 ) & echo $! > "$1/child.pid"
echo $$ > "$1/shell.pid"
wait
"#;
        let launch = bash_launch(script, &workspace, &private_tmp, &home, &[&workspace]);
        let mut tree = spawn(&sandbox, &policy, launch);
        wait_for_file(&workspace.join("child.pid")).await;
        wait_for_file(&workspace.join("shell.pid")).await;
        let group_pid = read_pid(&workspace.join("child.pid"));
        let shell_pid = read_pid(&workspace.join("shell.pid"));

        let _ = tree
            .terminate(Duration::from_millis(200))
            .await
            .expect("terminate");
        wait_dead(group_pid).await;
        wait_dead(shell_pid).await;
    }

    #[tokio::test]
    async fn dropping_the_tree_kills_the_process_group() {
        let sandbox = open_sandbox();
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        let policy = base_policy(&workspace, &private_tmp);

        let tree = spawn_ignoring_term(&sandbox, &policy, &workspace, &private_tmp, &home).await;
        wait_for_file(&workspace.join("child.pid")).await;
        wait_for_file(&workspace.join("shell.pid")).await;
        let group_pid = read_pid(&workspace.join("child.pid"));
        let shell_pid = read_pid(&workspace.join("shell.pid"));

        drop(tree);
        wait_dead(group_pid).await;
        wait_dead(shell_pid).await;
    }

    #[tokio::test]
    async fn setsid_descendants_are_a_documented_residual() {
        let sandbox = open_sandbox();
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        let policy = base_policy(&workspace, &private_tmp);

        let script = r#"/usr/bin/perl -e 'use POSIX; POSIX::setsid(); open(my $f, ">", $ARGV[0]) or die; print $f $$; close $f; sleep 60' "$1/setsid.pid" &
echo $$ > "$1/shell.pid"
wait
"#;
        let launch = bash_launch(script, &workspace, &private_tmp, &home, &[&workspace]);
        let mut tree = spawn(&sandbox, &policy, launch);
        wait_for_file(&workspace.join("setsid.pid")).await;
        wait_for_file(&workspace.join("shell.pid")).await;
        let setsid_pid = read_pid(&workspace.join("setsid.pid"));
        let shell_pid = read_pid(&workspace.join("shell.pid"));
        let _cleanup = KillOnDrop(setsid_pid);

        let _ = tree
            .terminate(Duration::from_millis(200))
            .await
            .expect("terminate");
        assert!(!alive(shell_pid), "the shell should be in the killed group");
        assert!(
            alive(setsid_pid),
            "a setsid descendant escaped the process group; this residual is expected"
        );
    }

    #[tokio::test]
    async fn setpgid_descendants_are_a_documented_residual() {
        let sandbox = open_sandbox();
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        let policy = base_policy(&workspace, &private_tmp);

        let script = r#"/usr/bin/perl -e 'use POSIX; POSIX::setpgid(0, 0); open(my $f, ">", $ARGV[0]) or die; print $f $$; close $f; sleep 60' "$1/child.pid" &
echo $$ > "$1/shell.pid"
wait
"#;
        let launch = bash_launch(script, &workspace, &private_tmp, &home, &[&workspace]);
        let mut tree = spawn(&sandbox, &policy, launch);
        wait_for_file(&workspace.join("child.pid")).await;
        wait_for_file(&workspace.join("shell.pid")).await;
        let child_pid = read_pid(&workspace.join("child.pid"));
        let shell_pid = read_pid(&workspace.join("shell.pid"));
        let _cleanup = KillOnDrop(child_pid);

        let _ = tree
            .terminate(Duration::from_millis(200))
            .await
            .expect("terminate");
        assert!(!alive(shell_pid), "the shell should be in the killed group");
        assert!(
            alive(child_pid),
            "a setpgid descendant escaped the process group; this residual is expected"
        );
    }

    #[tokio::test]
    async fn sysctl_cannot_read_the_host_environment() {
        let sandbox = open_sandbox();
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        let policy = base_policy(&workspace, &private_tmp);
        let helper = compile_sysctl_probe(&workspace);
        let host = std::process::id().to_string();

        let launch =
            tool_launch(helper.to_str().unwrap(), &workspace, &private_tmp, &home).arg(host);
        let output = run(&sandbox, &policy, launch).await;
        assert_eq!(
            output.status.code(),
            Some(3),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(
            output.stderr.contains("size: Operation not permitted"),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(!output.stdout.contains("HOME="));
    }

    #[tokio::test]
    async fn process_info_cannot_read_the_host_process() {
        let sandbox = open_sandbox();
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        let policy = base_policy(&workspace, &private_tmp);
        let helper = compile_proc_pidinfo_probe(&workspace);

        let host = std::process::id().to_string();
        let output = run(
            &sandbox,
            &policy,
            tool_launch(helper.to_str().unwrap(), &workspace, &private_tmp, &home).arg(host),
        )
        .await;
        assert_eq!(
            output.status.code(),
            Some(3),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(
            output
                .stderr
                .contains("proc_pidinfo: Operation not permitted"),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );

        let output = run(
            &sandbox,
            &policy,
            tool_launch(helper.to_str().unwrap(), &workspace, &private_tmp, &home),
        )
        .await;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("proc_pidinfo-ok"));
    }

    #[tokio::test]
    async fn terminate_returns_when_the_direct_child_moves_groups() {
        let sandbox = open_sandbox();
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        let policy = base_policy(&workspace, &private_tmp);

        let script = r#"use POSIX;
my $pid = fork();
if ($pid == 0) {
    POSIX::setpgid(0, 0);
    open(my $f, ">", "$ARGV[0]/escaped.pid") or die;
    print $f $$;
    close $f;
    sleep 60;
    exit 0;
}
POSIX::setpgid(0, $pid);
open(my $f, ">", "$ARGV[0]/direct.pid") or die;
print $f $$;
close $f;
sleep 60;
"#;
        let launch = tool_launch("/usr/bin/perl", &workspace, &private_tmp, &home)
            .arg("-e")
            .arg(script)
            .arg(canonical(&workspace));
        let mut tree = spawn(&sandbox, &policy, launch);
        wait_for_file(&workspace.join("direct.pid")).await;
        wait_for_file(&workspace.join("escaped.pid")).await;
        let direct_pid = read_pid(&workspace.join("direct.pid"));
        let escaped_pid = read_pid(&workspace.join("escaped.pid"));
        let _cleanup = KillOnDrop(escaped_pid);

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            tree.terminate(Duration::from_millis(200)),
        )
        .await;
        assert!(
            result.is_ok(),
            "terminate hung after the child moved groups"
        );
        assert!(!result.unwrap().expect("terminate").success());
        wait_dead(direct_pid).await;
        assert!(
            alive(escaped_pid),
            "a moved-group descendant escaped; this residual is expected"
        );
    }

    /// The `/dev/tty` denial can only be observed when the sandboxed process
    /// has a controlling terminal. Run under `script(1)` so this test does not
    /// pass just because the host has no tty:
    /// `script -q /dev/null cargo test -p tars-core dev_tty -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a controlling terminal; run under script(1)"]
    async fn dev_tty_is_denied_with_a_controlling_terminal() {
        let host_can_open = std::process::Command::new("/bin/sh")
            .args(["-c", ": < /dev/tty"])
            .status()
            .is_ok_and(|status| status.success());
        assert!(
            host_can_open,
            "run this test under script(1) so /dev/tty exists"
        );
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        let policy = base_policy(&workspace, &private_tmp).with_read_scope(ReadScope::Host);

        let sandbox = open_sandbox();
        let script = r#"set -u
if /usr/bin/head -c 0 /dev/tty >/dev/null 2>&1; then echo dev-tty-open; exit 1; fi
echo dev-tty-denied
"#;
        let launch = bash_launch(script, &workspace, &private_tmp, &home, &[]);
        let output = run(&sandbox, &policy, launch).await;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("dev-tty-denied"));
    }

    #[tokio::test]
    async fn host_read_scope_reads_outside_and_denies_credentials_devices_and_storage() {
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let outside = base.path().join("outside");
        let home = base.path().join("home");
        let storage = base.path().join("storage/sessions");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join(".aws")).unwrap();
        std::fs::create_dir_all(&storage).unwrap();
        std::fs::write(outside.join("visible"), "OUTSIDE-OK\n").unwrap();
        std::fs::write(home.join(".cargo/config.toml"), "PROBE-CONFIG\n").unwrap();
        std::fs::write(home.join(".cargo/credentials.toml"), "PROBE-CANARY\n").unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), "PROBE-CANARY\n").unwrap();
        std::fs::write(home.join(".aws/credentials"), "PROBE-CANARY\n").unwrap();
        std::fs::write(home.join(".zsh_history"), "PROBE-CANARY\n").unwrap();
        std::fs::write(storage.join("record.json"), "STORAGE-SECRET\n").unwrap();

        let backend = super::super::seatbelt::SeatbeltBackend::with_home(home.clone());
        let sandbox =
            Sandbox::open_with(Box::new(backend)).expect("a fake home must not break the probe");
        let policy = base_policy(&workspace, &private_tmp)
            .with_read_scope(ReadScope::Host)
            .with_denied_path(&storage)
            .unwrap()
            .with_denied_path(outside.join("blocked"))
            .unwrap();
        let script = r#"set -u
outside=$1
home=$2
storage=$3
if ! grep -q OUTSIDE-OK "$outside/visible" 2>/dev/null; then echo outside-unreadable; exit 1; fi
if ! grep -q PROBE-CONFIG "$home/.cargo/config.toml" 2>/dev/null; then echo config-unreadable; exit 2; fi
for path in \
  "$home/.cargo/credentials.toml" \
  "$home/.ssh/id_ed25519" \
  "$home/.aws/credentials" \
  "$home/.zsh_history"; do
  if grep -q PROBE-CANARY "$path" 2>/dev/null; then echo credential-leaked; exit 3; fi
done
if /usr/bin/head -c 0 /dev/tty >/dev/null 2>&1; then echo dev-tty-open; exit 4; fi
if ! cat /dev/null >/dev/null 2>&1; then echo dev-null-unreadable; exit 5; fi
/bin/dd if=/dev/zero of=/dev/null bs=1 count=1 2>/dev/null || exit 8
/usr/bin/head -c 1 /dev/urandom >/dev/null 2>&1 || exit 9
/usr/bin/head -c 1 /dev/random >/dev/null 2>&1 || exit 10
if grep -q STORAGE-SECRET "$storage/record.json" 2>/dev/null; then echo storage-leaked; exit 6; fi
if printf x > "$storage/new" 2>/dev/null; then echo storage-write-allowed; exit 7; fi
if mkdir "$outside/blocked" 2>/dev/null; then echo missing-denied-created; exit 11; fi
if printf x > "$outside/blocked/x" 2>/dev/null; then echo missing-denied-written; exit 12; fi
echo host-read-ok
"#;
        let launch = bash_launch(
            script,
            &workspace,
            &private_tmp,
            &home,
            &[&outside, &home, &storage],
        );
        let output = run(&sandbox, &policy, launch).await;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("host-read-ok"));
        assert!(!output.stdout.contains("PROBE-CANARY"));
        assert!(!output.stdout.contains("STORAGE-SECRET"));
        assert!(!storage.join("new").exists());
    }

    #[tokio::test]
    async fn roots_read_scope_does_not_read_outside_files() {
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let outside = base.path().join("outside");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::create_dir(&home).unwrap();
        std::fs::write(outside.join("visible"), "OUTSIDE-OK\n").unwrap();
        let policy = base_policy(&workspace, &private_tmp);
        assert_eq!(policy.read_scope(), ReadScope::Roots);

        let sandbox = open_sandbox();
        let script = r#"set -u
if cat "$1/visible" >/dev/null 2>&1; then echo outside-readable; exit 1; fi
echo roots-ok
"#;
        let launch = bash_launch(script, &workspace, &private_tmp, &home, &[&outside]);
        let output = run(&sandbox, &policy, launch).await;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("roots-ok"));
    }

    #[tokio::test]
    async fn git_metadata_protection_keeps_objects_refs_and_index_writable() {
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        for directory in [
            ".git/hooks",
            ".git/objects",
            ".git/refs",
            ".git/modules/sub",
            ".git/worktrees/wt",
        ] {
            std::fs::create_dir_all(workspace.join(directory)).unwrap();
        }
        std::fs::write(workspace.join(".git/config"), "original\n").unwrap();
        std::fs::write(workspace.join(".git/config.worktree"), "original\n").unwrap();
        let policy = base_policy(&workspace, &private_tmp)
            .with_git_metadata_protection(&workspace)
            .unwrap()
            .with_read_scope(ReadScope::Host);

        let sandbox = open_sandbox();
        let script = r#"set -u
ws=$1
printf x > "$ws/.git/objects/x" || exit 1
printf x > "$ws/.git/refs/x" || exit 2
printf x > "$ws/.git/index" || exit 3
if printf x > "$ws/.git/hooks/pre-commit" 2>/dev/null; then exit 4; fi
if printf x > "$ws/.git/config" 2>/dev/null; then exit 5; fi
if printf x > "$ws/.git/config.worktree" 2>/dev/null; then exit 6; fi
if printf x > "$ws/.git/modules/sub/hooks/pre-commit" 2>/dev/null; then exit 7; fi
if printf x > "$ws/.git/worktrees/wt/config.worktree" 2>/dev/null; then exit 9; fi
if mv "$ws/.git" "$ws/.git-moved" 2>/dev/null; then exit 8; fi
echo git-protection-ok
"#;
        let launch = bash_launch(script, &workspace, &private_tmp, &home, &[&workspace]);
        let output = run(&sandbox, &policy, launch).await;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("git-protection-ok"));
        assert!(!workspace.join(".git/hooks/pre-commit").exists());
        assert!(!workspace.join(".git-moved").exists());
        assert!(workspace.join(".git/objects/x").exists());
        assert!(workspace.join(".git/refs/x").exists());
        assert!(workspace.join(".git/index").exists());
        assert_eq!(
            std::fs::read_to_string(workspace.join(".git/config")).unwrap(),
            "original\n"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join(".git/config.worktree")).unwrap(),
            "original\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_metadata_symlink_targets_are_read_only() {
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(workspace.join(".git")).unwrap();
        std::fs::create_dir(workspace.join(".githooks")).unwrap();
        std::fs::write(workspace.join("gitconfig-real"), "original\n").unwrap();
        std::os::unix::fs::symlink(workspace.join(".githooks"), workspace.join(".git/hooks"))
            .unwrap();
        std::os::unix::fs::symlink(
            workspace.join("gitconfig-real"),
            workspace.join(".git/config"),
        )
        .unwrap();
        let policy = base_policy(&workspace, &private_tmp)
            .with_git_metadata_protection(&workspace)
            .unwrap()
            .with_read_scope(ReadScope::Host);

        let sandbox = open_sandbox();
        let script = r#"set -u
ws=$1
if printf x > "$ws/.githooks/pre-commit" 2>/dev/null; then exit 1; fi
if printf x > "$ws/.git/hooks/pre-commit" 2>/dev/null; then exit 2; fi
if printf x > "$ws/gitconfig-real" 2>/dev/null; then exit 3; fi
if printf x > "$ws/.git/config" 2>/dev/null; then exit 4; fi
echo symlink-protected
"#;
        let launch = bash_launch(script, &workspace, &private_tmp, &home, &[&workspace]);
        let output = run(&sandbox, &policy, launch).await;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("symlink-protected"));
        assert!(!workspace.join(".githooks/pre-commit").exists());
        assert_eq!(
            std::fs::read_to_string(workspace.join("gitconfig-real")).unwrap(),
            "original\n"
        );
    }

    #[tokio::test]
    async fn credentials_stay_denied_when_the_workspace_is_home() {
        let base = tempfile::tempdir().unwrap();
        let home = base.path().join("home");
        let private_tmp = base.path().join("private-tmp");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), "TOP-SECRET\n").unwrap();
        let backend = super::super::seatbelt::SeatbeltBackend::with_home(home.clone());
        let sandbox =
            Sandbox::open_with(Box::new(backend)).expect("a fake home must not break the probe");
        let policy = base_policy(&home, &private_tmp).with_read_scope(ReadScope::Host);

        let script = r#"set -u
ws=$1
if grep -q TOP-SECRET "$ws/.ssh/id_ed25519" 2>/dev/null; then echo credential-readable; exit 1; fi
if printf pwn > "$ws/.ssh/id_ed25519" 2>/dev/null; then echo credential-writable; exit 2; fi
echo credential-denied
"#;
        let launch = bash_launch(script, &home, &private_tmp, &home, &[&home]);
        let output = run(&sandbox, &policy, launch).await;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("credential-denied"));
        assert_eq!(
            std::fs::read_to_string(home.join(".ssh/id_ed25519")).unwrap(),
            "TOP-SECRET\n"
        );
    }

    #[tokio::test]
    async fn a_missing_git_entry_cannot_be_created() {
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        let policy = base_policy(&workspace, &private_tmp)
            .with_git_metadata_protection(&workspace)
            .unwrap()
            .with_read_scope(ReadScope::Host);

        let sandbox = open_sandbox();
        let script = r#"set -u
ws=$1
if mkdir "$ws/.git" 2>/dev/null; then exit 1; fi
if printf x > "$ws/.git" 2>/dev/null; then exit 2; fi
echo missing-git-protected
"#;
        let launch = bash_launch(script, &workspace, &private_tmp, &home, &[&workspace]);
        let output = run(&sandbox, &policy, launch).await;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("missing-git-protected"));
        assert!(!workspace.join(".git").exists());
    }

    #[tokio::test]
    async fn a_git_file_entry_cannot_be_written_renamed_or_deleted() {
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        std::fs::write(workspace.join(".git"), "gitdir: /tmp/example\n").unwrap();
        let policy = base_policy(&workspace, &private_tmp)
            .with_git_metadata_protection(&workspace)
            .unwrap()
            .with_read_scope(ReadScope::Host);

        let sandbox = open_sandbox();
        let script = r#"set -u
ws=$1
if printf x > "$ws/.git" 2>/dev/null; then exit 1; fi
if mv "$ws/.git" "$ws/moved" 2>/dev/null; then exit 2; fi
if rm "$ws/.git" 2>/dev/null; then exit 3; fi
echo git-file-protected
"#;
        let launch = bash_launch(script, &workspace, &private_tmp, &home, &[&workspace]);
        let output = run(&sandbox, &policy, launch).await;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("git-file-protected"));
        assert_eq!(
            std::fs::read_to_string(workspace.join(".git")).unwrap(),
            "gitdir: /tmp/example\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dangling_git_leaf_symlink_targets_are_read_only() {
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(workspace.join(".git")).unwrap();
        std::os::unix::fs::symlink("../.githooks", workspace.join(".git/hooks")).unwrap();
        std::os::unix::fs::symlink("../.gitconfig", workspace.join(".git/config")).unwrap();
        let policy = base_policy(&workspace, &private_tmp)
            .with_git_metadata_protection(&workspace)
            .unwrap()
            .with_read_scope(ReadScope::Host);

        let sandbox = open_sandbox();
        let script = r#"set -u
ws=$1
if mkdir "$ws/.githooks" 2>/dev/null; then exit 1; fi
if printf x > "$ws/.githooks/pre-commit" 2>/dev/null; then exit 2; fi
if printf x > "$ws/.gitconfig" 2>/dev/null; then exit 3; fi
if printf x > "$ws/.git/hooks/pre-commit" 2>/dev/null; then exit 4; fi
if printf x > "$ws/.git/config" 2>/dev/null; then exit 5; fi
echo dangling-leaf-protected
"#;
        let launch = bash_launch(script, &workspace, &private_tmp, &home, &[&workspace]);
        let output = run(&sandbox, &policy, launch).await;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("dangling-leaf-protected"));
        assert!(!workspace.join(".githooks").exists());
        assert!(!workspace.join(".gitconfig").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_dangling_git_symlink_protects_its_target() {
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        std::os::unix::fs::symlink(".gitdir", workspace.join(".git")).unwrap();
        let policy = base_policy(&workspace, &private_tmp)
            .with_git_metadata_protection(&workspace)
            .unwrap()
            .with_read_scope(ReadScope::Host);

        let sandbox = open_sandbox();
        let script = r#"set -u
ws=$1
if mkdir "$ws/.gitdir" 2>/dev/null; then exit 1; fi
if printf x > "$ws/.gitdir/hooks/pre-commit" 2>/dev/null; then exit 2; fi
if printf x > "$ws/.gitdir/config" 2>/dev/null; then exit 3; fi
echo dangling-git-protected
"#;
        let launch = bash_launch(script, &workspace, &private_tmp, &home, &[&workspace]);
        let output = run(&sandbox, &policy, launch).await;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("dangling-git-protected"));
        assert!(!workspace.join(".gitdir").exists());
        assert!(
            std::fs::symlink_metadata(workspace.join(".git"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[tokio::test]
    async fn git_entry_cannot_be_deleted_or_renamed() {
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(workspace.join(".git")).unwrap();
        let policy = base_policy(&workspace, &private_tmp)
            .with_git_metadata_protection(&workspace)
            .unwrap()
            .with_read_scope(ReadScope::Host);

        let sandbox = open_sandbox();
        let script = r#"set -u
ws=$1
if rmdir "$ws/.git" 2>/dev/null; then exit 1; fi
if mv "$ws/.git" "$ws/.git-moved" 2>/dev/null; then exit 2; fi
echo git-entry-protected
"#;
        let launch = bash_launch(script, &workspace, &private_tmp, &home, &[&workspace]);
        let output = run(&sandbox, &policy, launch).await;
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            output.stdout,
            output.stderr
        );
        assert!(output.stdout.contains("git-entry-protected"));
        assert!(workspace.join(".git").is_dir());
        assert!(!workspace.join(".git-moved").exists());
    }

    #[tokio::test]
    async fn open_network_allows_the_dns_socket_but_not_other_unix_sockets() {
        let sandbox = open_sandbox();
        let base = tempfile::tempdir().unwrap();
        let workspace = base.path().join("workspace");
        let private_tmp = base.path().join("private-tmp");
        let home = base.path().join("home");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(&private_tmp).unwrap();
        std::fs::create_dir(&home).unwrap();
        let agent = base.path().join("agent.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&agent).unwrap();
        assert!(
            Path::new(DNS_SOCKET).exists(),
            "the macOS DNS socket is missing: {DNS_SOCKET}"
        );

        let closed = base_policy(&workspace, &private_tmp);
        let closed_dns = run(
            &sandbox,
            &closed,
            unix_launch(Path::new(DNS_SOCKET), &workspace, &private_tmp, &home),
        )
        .await;
        assert!(
            !closed_dns.status.success(),
            "the DNS socket was reachable under network = none"
        );
        let closed_agent = run(
            &sandbox,
            &closed,
            unix_launch(&agent, &workspace, &private_tmp, &home),
        )
        .await;
        assert!(
            !closed_agent.status.success(),
            "a unix socket was reachable under network = none"
        );

        let open = base_policy(&workspace, &private_tmp).with_network(NetworkAccess::Open);
        let open_dns = run(
            &sandbox,
            &open,
            unix_launch(Path::new(DNS_SOCKET), &workspace, &private_tmp, &home),
        )
        .await;
        assert!(
            open_dns.status.success(),
            "the DNS socket was denied under network = open: stdout={} stderr={}",
            open_dns.stdout,
            open_dns.stderr
        );
        let open_agent = run(
            &sandbox,
            &open,
            unix_launch(&agent, &workspace, &private_tmp, &home),
        )
        .await;
        assert!(
            !open_agent.status.success(),
            "a non-DNS unix socket was reachable under network = open"
        );
    }

    #[test]
    fn probe_fails_when_credential_denials_are_removed() {
        let directory = tempfile::tempdir().unwrap();
        let backend = super::super::seatbelt::SeatbeltBackend::with_home(directory.path())
            .without_credential_denials();
        let error = backend.probe().unwrap_err();
        assert!(
            error
                .reason
                .contains("roots probe failed with exit status: 18"),
            "{}",
            error.reason
        );
        assert!(!error.reason.contains("PROBE-CANARY"));
    }

    #[test]
    fn probe_fails_without_a_home_directory() {
        let backend = super::super::seatbelt::SeatbeltBackend::without_home();
        let error = backend.probe().unwrap_err();
        assert!(error.reason.contains("HOME is not set"), "{}", error.reason);
    }

    fn compile_sysctl_probe(directory: &Path) -> PathBuf {
        let source = directory.join("sysctl_probe.c");
        let binary = directory.join("sysctl_probe");
        std::fs::write(
            &source,
            r#"#include <sys/types.h>
#include <sys/sysctl.h>
#include <stdio.h>
#include <stdlib.h>
int main(int argc, char **argv) {
    if (argc < 2) return 2;
    int mib[3] = { CTL_KERN, KERN_PROCARGS2, atoi(argv[1]) };
    size_t size = 0;
    if (sysctl(mib, 3, NULL, &size, NULL, 0) != 0) { perror("size"); return 3; }
    char *buffer = malloc(size);
    if (!buffer) return 4;
    if (sysctl(mib, 3, buffer, &size, NULL, 0) != 0) { perror("read"); return 5; }
    fwrite(buffer, 1, size, stdout);
    return 0;
}
"#,
        )
        .unwrap();
        let output = std::process::Command::new("cc")
            .args(["-O0", "-o"])
            .arg(&binary)
            .arg(&source)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "cc failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        binary
    }

    fn compile_proc_pidinfo_probe(directory: &Path) -> PathBuf {
        let source = directory.join("proc_pidinfo_probe.c");
        let binary = directory.join("proc_pidinfo_probe");
        std::fs::write(
            &source,
            r#"#include <libproc.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
int main(int argc, char **argv) {
    int pid = argc > 1 ? atoi(argv[1]) : getpid();
    struct proc_taskallinfo info;
    if (proc_pidinfo(pid, PROC_PIDTASKALLINFO, 0, &info, sizeof(info)) <= 0) {
        perror("proc_pidinfo");
        return 3;
    }
    printf("proc_pidinfo-ok\n");
    return 0;
}
"#,
        )
        .unwrap();
        let output = std::process::Command::new("cc")
            .args(["-O0", "-o"])
            .arg(&binary)
            .arg(&source)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "cc failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        binary
    }

    #[test]
    fn missing_executable_fails_closed() {
        let backend = super::super::seatbelt::SeatbeltBackend::with_executable(
            "/nonexistent/tars-sandbox-exec",
        );
        let error = backend.probe().unwrap_err();
        assert_eq!(error.backend, BackendId::SEATBELT);
        assert!(error.reason.contains("not found"), "{}", error.reason);
    }

    #[test]
    fn failed_probe_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("sandbox-exec");
        std::fs::write(&executable, "#!/bin/sh\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&executable, permissions).unwrap();

        let backend = super::super::seatbelt::SeatbeltBackend::with_executable(&executable);
        let error = backend.probe().unwrap_err();
        assert_eq!(error.backend, BackendId::SEATBELT);
        assert!(
            error.reason.contains("probe failed with"),
            "{}",
            error.reason
        );
    }

    #[test]
    fn profile_is_deny_by_default_escapes_paths_and_opens_network_on_request() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("quote\"and\\back");
        std::fs::create_dir(&root).unwrap();
        let root = canonical(&root);
        let git = root.join(".git");
        std::fs::create_dir(&git).unwrap();
        let policy = SandboxPolicy::new(vec![root.clone()], vec![root.clone()])
            .unwrap()
            .with_read_only_subpath(&git)
            .unwrap()
            .with_network(NetworkAccess::Open);
        let backend = super::super::seatbelt::SeatbeltBackend::new();
        let profile = backend.profile(&policy).unwrap();

        assert!(profile.starts_with("(version 1)\n(deny default)"));
        assert!(profile.contains("(allow signal (target same-sandbox))"));
        assert!(profile.contains("quote\\\"and\\\\back"));
        assert!(profile.contains("(allow network-outbound (remote ip))"));
        assert!(
            profile
                .contains("(remote unix-socket (path-literal \"/private/var/run/mDNSResponder\"))")
        );
        assert!(!profile.contains("(allow network-outbound)\n"));
        assert!(profile.contains("(deny file-read* (subpath \"/dev\"))"));
        assert!(profile.contains("(literal \"/dev/null\")"));
        assert!(profile.contains("(subpath \"/Library/Keychains\")"));
        assert!(profile.contains("(subpath \"/Library/Preferences/SystemConfiguration\")"));
        assert!(!profile.contains("SecurityServer"));
        assert!(profile.contains("(deny sysctl-read (sysctl-name-prefix \"kern.proc\"))"));
        assert!(profile.contains("(deny process-info*)"));
        assert!(profile.contains("(allow process-info* (target same-sandbox))"));
        let allow = profile.find("(allow file-write* (subpath").unwrap();
        let deny = profile.find("(deny file-write* (subpath").unwrap();
        assert!(
            allow < deny,
            "read-only subpaths must be denied after allows"
        );
    }

    #[test]
    fn profile_omits_the_system_baseline_when_disabled() {
        let directory = tempfile::tempdir().unwrap();
        let root = canonical(directory.path());
        let policy = SandboxPolicy::new(vec![root.clone()], vec![root])
            .unwrap()
            .with_system_baseline(false);
        let backend = super::super::seatbelt::SeatbeltBackend::new();
        let profile = backend.profile(&policy).unwrap();
        assert!(!profile.contains("(allow file-read-metadata)"));
        assert!(!profile.contains("(allow mach-lookup"));
        assert!(!profile.contains("(subpath \"/usr\")"));
    }
}

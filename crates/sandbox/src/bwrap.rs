//! Linux Bubblewrap (`bwrap`) + seccomp-BPF sandbox backend.
//!
//! `bwrap` provides mount, user, PID, IPC, UTS, and optional network
//! namespaces along with `--die-with-parent` and `--new-session`. A pure
//! safe-Rust cBPF filter passed over `--seccomp <fd>` blocks dangerous syscalls
//! (`io_uring_*`, `ptrace`, `process_vm_*`, `bpf`, `kexec_*`) and blocks
//! `socket(AF_UNIX)` / `socket(AF_VSOCK)` in every mode while preserving
//! `socketpair` for internal runtime IPC.

pub(crate) mod seccomp;

#[cfg(target_os = "linux")]
use super::backend::{
    BackendId, BackendReport, Capability, Launch, SandboxBackend, SandboxError, SandboxUnavailable,
    SandboxedCommand,
};
#[cfg(all(unix, test, not(target_os = "linux")))]
use super::backend::{Launch, SandboxError};
#[cfg(any(target_os = "linux", all(unix, test)))]
use super::credentials;
#[cfg(any(target_os = "linux", all(unix, test)))]
use super::policy::{NetworkAccess, ReadScope, SandboxPolicy};
#[cfg(target_os = "linux")]
use super::process::ProcessGroupControl;
#[cfg(any(target_os = "linux", all(unix, test)))]
use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::io::Read;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::unix::fs::PermissionsExt;
#[cfg(target_os = "linux")]
use std::os::unix::process::CommandExt;
#[cfg(any(target_os = "linux", all(unix, test)))]
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::{Command, Stdio};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const BWRAP_MISSING_REASON: &str =
    "bwrap executable not found; install bubblewrap (e.g. apt install bubblewrap)";
#[cfg(any(target_os = "linux", all(unix, test)))]
const SYSTEM_READ_PATHS: &[&str] = &[
    "/usr", "/bin", "/sbin", "/lib", "/lib64", "/lib32", "/etc", "/opt", "/nix",
];
#[cfg(any(target_os = "linux", all(unix, test)))]
const HOME_TOOLCHAIN_READ_PATHS: &[&str] = &[".cargo", ".rustup"];
#[cfg(any(target_os = "linux", all(unix, test)))]
const HOME_PLATFORM_DENIED_DIRECTORIES: &[&str] = &[".local/share/keyrings", ".config/sops"];

#[cfg(target_os = "linux")]
pub(crate) struct BwrapBackend {
    executable: Option<PathBuf>,
    home: Option<PathBuf>,
    #[cfg(test)]
    omit_credential_denials: bool,
}

#[cfg(target_os = "linux")]
impl BwrapBackend {
    pub(crate) fn new() -> Self {
        Self {
            executable: resolve_executable(),
            home: std::env::var_os("HOME").map(PathBuf::from),
            #[cfg(test)]
            omit_credential_denials: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_executable(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: Some(executable.into()),
            home: std::env::var_os("HOME").map(PathBuf::from),
            omit_credential_denials: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn without_home() -> Self {
        Self {
            executable: resolve_executable(),
            home: None,
            omit_credential_denials: false,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn with_home(home: impl Into<PathBuf>) -> Self {
        Self {
            executable: resolve_executable(),
            home: Some(home.into()),
            #[cfg(test)]
            omit_credential_denials: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn without_credential_denials(mut self) -> Self {
        self.omit_credential_denials = true;
        self
    }

    fn unavailable(&self, reason: impl Into<String>) -> SandboxUnavailable {
        SandboxUnavailable::new(BackendId::BWRAP, reason)
    }

    fn require_executable(&self) -> Result<&Path, SandboxUnavailable> {
        let Some(executable) = self.executable.as_deref() else {
            return Err(self.unavailable(BWRAP_MISSING_REASON));
        };
        if !is_executable_file(executable) {
            return Err(self.unavailable(format!(
                "bwrap executable not found or not executable: {}",
                executable.display()
            )));
        }
        Ok(executable)
    }

    fn include_credential_denials(&self) -> bool {
        #[cfg(test)]
        if self.omit_credential_denials {
            return false;
        }
        true
    }

    fn probe_backend(&self, home: PathBuf) -> Self {
        Self {
            executable: self.executable.clone(),
            home: Some(home),
            #[cfg(test)]
            omit_credential_denials: self.omit_credential_denials,
        }
    }

    fn run_probe(&self) -> Result<(), SandboxUnavailable> {
        let Some(arch) = seccomp::SeccompArch::current() else {
            return Err(self.unavailable(format!(
                "unsupported Linux architecture for seccomp-BPF: {}",
                std::env::consts::ARCH
            )));
        };
        let executable = self.require_executable()?;
        if self.home.is_none() {
            return Err(
                self.unavailable("HOME is not set; credential denials require a home directory")
            );
        }

        let directory = tempfile::tempdir().map_err(|error| {
            self.unavailable(format!("could not create probe directory: {error}"))
        })?;
        let workspace = directory.path().join("workspace");
        let outside = directory.path().join("outside");
        let home = directory.path().join("home");
        std::fs::create_dir_all(workspace.join(".git/hooks"))
            .and_then(|()| std::fs::write(workspace.join(".git/config"), "[core]\n"))
            .and_then(|()| std::fs::create_dir(&outside))
            .and_then(|()| std::fs::write(outside.join("canary"), "CANARY\n"))
            .and_then(|()| std::fs::write(outside.join("denied"), "DENIED-CANARY\n"))
            .and_then(|()| std::fs::create_dir_all(home.join(".cargo")))
            .and_then(|()| std::fs::create_dir_all(home.join(".ssh")))
            .and_then(|()| std::fs::create_dir_all(home.join(".aws")))
            .and_then(|()| std::fs::write(home.join(".cargo/config.toml"), "PROBE-CONFIG\n"))
            .and_then(|()| std::fs::write(home.join(".cargo/credentials.toml"), "PROBE-CANARY\n"))
            .and_then(|()| std::fs::write(home.join(".ssh/id_ed25519"), "PROBE-CANARY\n"))
            .and_then(|()| std::fs::write(home.join(".aws/credentials"), "PROBE-CANARY\n"))
            .and_then(|()| std::fs::write(home.join(".zsh_history"), "PROBE-CANARY\n"))
            .map_err(|error| self.unavailable(format!("could not prepare probe: {error}")))?;

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .map_err(|error| self.unavailable(format!("could not bind probe listener: {error}")))?;
        let port = listener
            .local_addr()
            .map_err(|error| self.unavailable(format!("could not read probe port: {error}")))?
            .port()
            .to_string();
        let agent_socket = outside.join("agent.sock");
        let _agent_listener = std::os::unix::net::UnixListener::bind(&agent_socket)
            .map_err(|error| self.unavailable(format!("could not bind probe socket: {error}")))?;

        let workspace = std::fs::canonicalize(&workspace)
            .map_err(|error| self.unavailable(format!("could not canonicalize probe: {error}")))?;
        let outside = std::fs::canonicalize(&outside)
            .map_err(|error| self.unavailable(format!("could not canonicalize probe: {error}")))?;
        let home = std::fs::canonicalize(&home)
            .map_err(|error| self.unavailable(format!("could not canonicalize probe: {error}")))?;

        let roots = SandboxPolicy::new(vec![workspace.clone()], vec![workspace.clone()])
            .and_then(|policy| policy.with_git_metadata_protection(&workspace))
            .and_then(|policy| policy.with_denied_path(outside.join("denied")))
            .map_err(|error| self.unavailable(format!("could not build probe policy: {error}")))?;
        let policies = [
            ("roots", roots.clone()),
            ("host", roots.with_read_scope(ReadScope::Host)),
        ];
        let host = std::process::id().to_string();
        let shell = probe_shell().ok_or_else(|| {
            self.unavailable("neither /bin/bash nor /bin/sh exists to run the sandbox probe")
        })?;
        let script = r#"set -u
scope=$1
ws=$2
outside=$3
home=$4
port=$5
agent=$6
host=$7
if ! uname -a >/dev/null 2>&1; then exit 23; fi
printf ok > "$ws/ok" || exit 11
printf ok > /dev/null || exit 24
if printf x > "$outside/escape" 2>/dev/null; then exit 12; fi
if [ "$scope" = roots ]; then
  if cat "$outside/canary" >/dev/null 2>&1; then exit 13; fi
elif ! grep -q CANARY "$outside/canary" 2>/dev/null; then
  exit 25
fi
if grep -q DENIED-CANARY "$outside/denied" 2>/dev/null; then exit 32; fi
if printf x > "$outside/denied" 2>/dev/null; then exit 33; fi
if ! grep -q PROBE-CONFIG "$home/.cargo/config.toml" 2>/dev/null; then exit 17; fi
if grep -q PROBE-CANARY "$home/.cargo/credentials.toml" 2>/dev/null; then exit 18; fi
if grep -q PROBE-CANARY "$home/.ssh/id_ed25519" 2>/dev/null; then exit 20; fi
if grep -q PROBE-CANARY "$home/.aws/credentials" 2>/dev/null; then exit 26; fi
if grep -q PROBE-CANARY "$home/.zsh_history" 2>/dev/null; then exit 27; fi
if (exec < /dev/tty) 2>/dev/null; then exit 28; fi
if printf x > "$ws/.git/hooks/pre-commit" 2>/dev/null; then exit 14; fi
if printf x > "$ws/.git/config" 2>/dev/null; then exit 29; fi
if mv "$ws/.git" "$ws/.git-moved" 2>/dev/null; then exit 30; fi
if kill -0 "$host" 2>/dev/null; then exit 15; fi
if command -v python3 >/dev/null 2>&1; then
  python3 -c 'import socket, sys
port, agent = int(sys.argv[1]), sys.argv[2]
for family, addr, code in (
    (socket.AF_INET, ("127.0.0.1", port), 16),
    (socket.AF_UNIX, agent, 31),
):
    try:
        s = socket.socket(family, socket.SOCK_STREAM)
        s.settimeout(1.0)
        s.connect(addr)
        s.close()
        sys.exit(code)
    except OSError:
        pass
' "$port" "$agent"
  rc=$?
  if [ "$rc" -ne 0 ]; then exit "$rc"; fi
elif command -v nc >/dev/null 2>&1; then
  if nc -z -w 1 127.0.0.1 "$port" >/dev/null 2>&1; then exit 16; fi
  if nc -U -z -w 1 "$agent" >/dev/null 2>&1; then exit 31; fi
else
  exit 19
fi
echo probe-ok
"#;

        for (scope, policy) in policies {
            let backend = self.probe_backend(home.clone());
            let launch = Launch::new(shell)
                .arg("-c")
                .arg(script)
                .arg("tars-probe")
                .arg(scope)
                .arg(&workspace)
                .arg(&outside)
                .arg(&home)
                .arg(&port)
                .arg(&agent_socket)
                .arg(&host)
                .current_dir(&workspace)
                .env(
                    "PATH",
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                );
            let seccomp_fd = seccomp::seccomp_pipe(arch, policy.network()).map_err(|error| {
                self.unavailable(format!("could not create {scope} seccomp pipe: {error}"))
            })?;
            let args = build_args(
                &policy,
                &launch,
                backend.home.as_deref(),
                seccomp_fd.as_raw_fd(),
                backend.include_credential_denials(),
            )
            .map_err(|error| {
                self.unavailable(format!("could not build {scope} probe args: {error}"))
            })?;

            rustix::io::fcntl_setfd(&seccomp_fd, rustix::io::FdFlags::empty()).map_err(
                |error| {
                    self.unavailable(format!(
                        "could not prepare {scope} probe seccomp fd: {error}"
                    ))
                },
            )?;
            let mut command = Command::new(executable);
            command
                .args(&args)
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0);
            let spawn_result = command.spawn();
            drop(seccomp_fd);
            let mut child = spawn_result.map_err(|error| {
                self.unavailable(format!("could not start {scope} probe: {error}"))
            })?;

            let deadline = Instant::now() + PROBE_TIMEOUT;
            let status = loop {
                match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) => {}
                    Err(error) => {
                        return Err(
                            self.unavailable(format!("could not wait for {scope} probe: {error}"))
                        );
                    }
                }
                if Instant::now() >= deadline {
                    if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                        let _ =
                            rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(self.unavailable(format!("{scope} probe timed out")));
                }
                std::thread::sleep(Duration::from_millis(10));
            };

            let mut stdout = String::new();
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stdout.take() {
                let _ = pipe.by_ref().take(2048).read_to_string(&mut stdout);
            }
            if let Some(mut pipe) = child.stderr.take() {
                let _ = pipe.by_ref().take(2048).read_to_string(&mut stderr);
            }
            if status.success() && stdout.contains("probe-ok") {
                continue;
            }
            let stdout = stdout.trim();
            let stderr = stderr.trim();
            let detail = [stdout, stderr]
                .into_iter()
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join(": ");
            let hint = userns_hint(stderr);
            let detail = if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            };
            return Err(
                self.unavailable(format!("{scope} probe failed with {status}{detail}{hint}"))
            );
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl SandboxBackend for BwrapBackend {
    fn id(&self) -> BackendId {
        BackendId::BWRAP
    }

    fn probe(&self) -> Result<BackendReport, SandboxUnavailable> {
        self.run_probe()?;
        Ok(BackendReport {
            backend: BackendId::BWRAP,
            file_read: Capability::Full,
            file_write: Capability::Full,
            network: Capability::Full,
            signal_isolation: Capability::Full,
            descendant_lifecycle: Capability::Full,
        })
    }

    fn wrap(
        &self,
        policy: &SandboxPolicy,
        launch: Launch,
    ) -> Result<SandboxedCommand, SandboxError> {
        launch.validate()?;
        let arch = seccomp::SeccompArch::current().ok_or_else(|| {
            SandboxError::Unavailable(self.unavailable(format!(
                "unsupported Linux architecture for seccomp-BPF: {}",
                std::env::consts::ARCH
            )))
        })?;
        let executable = self.require_executable()?;
        let seccomp_fd = seccomp::seccomp_pipe(arch, policy.network()).map_err(|error| {
            SandboxError::Unavailable(
                self.unavailable(format!("could not create seccomp pipe: {error}")),
            )
        })?;
        let args = build_args(
            policy,
            &launch,
            self.home.as_deref(),
            seccomp_fd.as_raw_fd(),
            self.include_credential_denials(),
        )?;
        let mut command = tokio::process::Command::new(executable);
        command.args(&args);
        command.env_clear();
        command.process_group(0);
        Ok(SandboxedCommand::with_inherited_fds(
            command,
            Box::new(ProcessGroupControl),
            vec![seccomp_fd],
        ))
    }

    fn unconfined(&self, launch: Launch) -> Result<SandboxedCommand, SandboxError> {
        launch.validate()?;
        let mut command = tokio::process::Command::new(launch.program());
        command.args(launch.launch_args());
        if let Some(directory) = launch.working_directory() {
            command.current_dir(directory);
        }
        command.env_clear();
        for (name, value) in launch.environment() {
            command.env(name, value);
        }
        command.process_group(0);
        Ok(SandboxedCommand::new(
            command,
            Box::new(ProcessGroupControl),
        ))
    }
}

#[cfg(target_os = "linux")]
fn resolve_executable() -> Option<PathBuf> {
    for candidate in [Path::new("/usr/bin/bwrap"), Path::new("/bin/bwrap")] {
        if is_executable_file(candidate) {
            return Some(candidate.to_path_buf());
        }
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if !dir.is_absolute() {
            continue;
        }
        let candidate = dir.join("bwrap");
        if is_executable_file(&candidate) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    metadata.is_file() && (metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(target_os = "linux")]
fn probe_shell() -> Option<&'static str> {
    ["/bin/bash", "/bin/sh"]
        .into_iter()
        .find(|&candidate| is_executable_file(Path::new(candidate)))
}

#[cfg(target_os = "linux")]
fn userns_hint(stderr: &str) -> &'static str {
    let mentions_userns = [
        "uid map",
        "Operation not permitted",
        "Permission denied",
        "RTM_NEWADDR",
        "userns",
    ]
    .iter()
    .any(|needle| stderr.contains(needle));
    if mentions_userns {
        " (unprivileged user namespaces may be restricted; on Ubuntu 24.04+ check kernel.apparmor_restrict_unprivileged_userns or install an AppArmor profile for bwrap)"
    } else {
        ""
    }
}

#[cfg(any(target_os = "linux", all(unix, test)))]
pub(crate) fn build_args(
    policy: &SandboxPolicy,
    launch: &Launch,
    home: Option<&Path>,
    seccomp_fd: i32,
    include_credential_denials: bool,
) -> Result<Vec<OsString>, SandboxError> {
    validate_absolute_path(launch.program())?;
    if let Some(directory) = launch.working_directory() {
        validate_absolute_path(directory)?;
    }

    let mut args = Vec::<OsString>::new();
    for flag in [
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--die-with-parent",
        "--new-session",
    ] {
        args.push(OsString::from(flag));
    }
    if policy.network() == NetworkAccess::None {
        args.push(OsString::from("--unshare-net"));
    }

    let canonical_home =
        home.map(|path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
    let mut visible_roots = Vec::<PathBuf>::new();
    let mut mounted_run_paths = Vec::<PathBuf>::new();
    let mut var_run_is_tmpfs = false;

    match policy.read_scope() {
        ReadScope::Host => {
            push_pair(&mut args, "--ro-bind", "/", "/");
            push_unary(&mut args, "--proc", "/proc");
            push_unary(&mut args, "--dev", "/dev");
            push_unary(&mut args, "--tmpfs", "/run");
            if std::fs::symlink_metadata("/var/run")
                .is_ok_and(|meta| !meta.file_type().is_symlink() && meta.is_dir())
            {
                push_unary(&mut args, "--tmpfs", "/var/run");
                var_run_is_tmpfs = true;
            }
            if policy.network() == NetworkAccess::Open {
                mount_open_network_dns(&mut args, &mut mounted_run_paths)?;
            }
        }
        ReadScope::Roots => {
            if policy.system_baseline() {
                for dir in SYSTEM_READ_PATHS {
                    let path = Path::new(dir);
                    let Ok(meta) = std::fs::symlink_metadata(path) else {
                        continue;
                    };
                    if meta.file_type().is_symlink() {
                        if let Ok(target) = std::fs::read_link(path) {
                            validate_path_text(&target)?;
                            validate_absolute_path(path)?;
                            args.push(OsString::from("--symlink"));
                            args.push(target.into_os_string());
                            args.push(path.as_os_str().to_os_string());
                        }
                    } else if meta.is_dir() {
                        push_path_pair(&mut args, "--ro-bind", path, path)?;
                        visible_roots.push(path.to_path_buf());
                    }
                }
                if let Some(home_dir) = &canonical_home {
                    for relative in HOME_TOOLCHAIN_READ_PATHS {
                        let candidate = home_dir.join(relative);
                        if std::fs::metadata(&candidate).is_ok_and(|meta| meta.is_dir()) {
                            push_path_pair(&mut args, "--ro-bind", &candidate, &candidate)?;
                            visible_roots.push(candidate);
                        }
                    }
                }
            }
            push_unary(&mut args, "--proc", "/proc");
            push_unary(&mut args, "--dev", "/dev");
            push_unary(&mut args, "--tmpfs", "/tmp");
            push_unary(&mut args, "--tmpfs", "/run");
            if policy.system_baseline() && policy.network() == NetworkAccess::Open {
                mount_open_network_dns(&mut args, &mut mounted_run_paths)?;
            }

            for root in sorted_by_depth(&policy.read_roots) {
                validate_absolute_path(&root)?;
                if !policy.write_roots.contains(&root) {
                    push_path_pair(&mut args, "--ro-bind", &root, &root)?;
                }
                visible_roots.push(root);
            }
        }
    }

    for root in sorted_by_depth(&policy.write_roots) {
        validate_absolute_path(&root)?;
        push_path_pair(&mut args, "--bind", &root, &root)?;
        if policy.read_scope() == ReadScope::Roots {
            visible_roots.push(root);
        }
    }

    let (denied_directories, denied_files) = collect_denials(
        policy,
        canonical_home.as_deref(),
        include_credential_denials,
    );
    let existing_denied_dirs: Vec<PathBuf> = denied_directories
        .iter()
        .filter(|dir| std::fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir()))
        .cloned()
        .collect();
    let existing_denied_files: Vec<PathBuf> = denied_files
        .iter()
        .filter(|file| std::fs::symlink_metadata(file).is_ok_and(|meta| meta.is_file()))
        .cloned()
        .collect();

    // Pin protected entries as independent mount points so `mv`/`rmdir`/`rm`
    // fails with `EBUSY`, while skipping symlinks (which cannot be pinned by
    // bind-mount without dereferencing) and entries already covered by
    // read-only or denied subtrees.
    for entry in sorted_by_depth(&policy.protected_entries) {
        if !policy
            .write_roots
            .iter()
            .any(|root| entry.starts_with(root))
        {
            continue;
        }
        if policy
            .read_only_subpaths
            .iter()
            .any(|ro| entry.starts_with(ro))
        {
            continue;
        }
        if existing_denied_dirs
            .iter()
            .any(|denied| entry.starts_with(denied))
        {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(&entry) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            push_path_pair(&mut args, "--bind", &entry, &entry)?;
        } else if meta.is_file() && !existing_denied_files.contains(&entry) {
            push_path_pair(&mut args, "--ro-bind", &entry, &entry)?;
        }
    }

    let mut mounted_ro_dirs = Vec::<PathBuf>::new();
    for path in sorted_by_depth(&policy.read_only_subpaths) {
        if mounted_ro_dirs
            .iter()
            .any(|parent| path.starts_with(parent))
        {
            continue;
        }
        if existing_denied_dirs
            .iter()
            .any(|denied| path.starts_with(denied))
        {
            continue;
        }
        // Skip symlinks: dereferencing via `--ro-bind` would mount an
        // outside target into the sandbox; `SandboxPolicy` already inserts
        // any in-root symlink targets as separate entries.
        if let Ok(meta) = std::fs::symlink_metadata(&path)
            && !meta.file_type().is_symlink()
        {
            push_path_pair(&mut args, "--ro-bind", &path, &path)?;
            if meta.is_dir() {
                mounted_ro_dirs.push(path);
            }
        }
    }

    let is_visible = |path: &Path| -> bool {
        match policy.read_scope() {
            ReadScope::Roots => {
                visible_roots.iter().any(|root| path.starts_with(root))
                    || mounted_run_paths
                        .iter()
                        .any(|mounted| path.starts_with(mounted))
            }
            ReadScope::Host => {
                if path.starts_with("/run") {
                    policy.write_roots.iter().any(|root| path.starts_with(root))
                        || mounted_run_paths
                            .iter()
                            .any(|mounted| path.starts_with(mounted))
                } else if var_run_is_tmpfs && path.starts_with("/var/run") {
                    policy.write_roots.iter().any(|root| path.starts_with(root))
                } else {
                    true
                }
            }
        }
    };

    // Emit denials last so they override both `ReadScope::Host` and
    // `write_roots` (including `workspace == HOME`), deduplicating ancestors so
    // we never mount inside an already `--tmpfs --chmod 0000` directory.
    let mut emitted_denied_dirs = Vec::<PathBuf>::new();
    for dir in sorted_by_depth(&denied_directories) {
        if !is_visible(&dir) {
            continue;
        }
        if emitted_denied_dirs
            .iter()
            .any(|parent| dir.starts_with(parent))
        {
            continue;
        }
        if !std::fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        validate_absolute_path(&dir)?;
        args.push(OsString::from("--tmpfs"));
        args.push(dir.as_os_str().to_os_string());
        args.push(OsString::from("--chmod"));
        args.push(OsString::from("0000"));
        emitted_denied_dirs.push(dir);
    }

    let mut sorted_files = denied_files;
    sorted_files.sort();
    sorted_files.dedup();
    for file in sorted_files {
        if !is_visible(&file) {
            continue;
        }
        if emitted_denied_dirs
            .iter()
            .any(|parent| file.starts_with(parent))
        {
            continue;
        }
        if !std::fs::symlink_metadata(&file).is_ok_and(|meta| meta.is_file()) {
            continue;
        }
        validate_absolute_path(&file)?;
        push_path_pair(&mut args, "--ro-bind", Path::new("/dev/null"), &file)?;
    }

    args.push(OsString::from("--seccomp"));
    args.push(OsString::from(seccomp_fd.to_string()));
    args.push(OsString::from("--clearenv"));
    for (name, value) in launch.environment() {
        args.push(OsString::from("--setenv"));
        args.push(name.clone());
        args.push(value.clone());
    }
    if let Some(directory) = launch.working_directory() {
        validate_absolute_path(directory)?;
        args.push(OsString::from("--chdir"));
        args.push(directory.as_os_str().to_os_string());
    }
    args.push(OsString::from("--"));
    args.push(launch.program().as_os_str().to_os_string());
    args.extend(launch.launch_args().iter().cloned());

    Ok(args)
}

#[cfg(any(target_os = "linux", all(unix, test)))]
fn mount_open_network_dns(
    args: &mut Vec<OsString>,
    mounted_run_paths: &mut Vec<PathBuf>,
) -> Result<(), SandboxError> {
    mount_open_network_dns_with(
        args,
        mounted_run_paths,
        Path::new("/run/systemd/resolve"),
        Path::new("/etc/resolv.conf"),
        Path::new("/run"),
    )
}

#[cfg(any(target_os = "linux", all(unix, test)))]
fn mount_open_network_dns_with(
    args: &mut Vec<OsString>,
    mounted_run_paths: &mut Vec<PathBuf>,
    systemd_resolve: &Path,
    resolv_conf: &Path,
    run_prefix: &Path,
) -> Result<(), SandboxError> {
    if std::fs::metadata(systemd_resolve).is_ok_and(|meta| meta.is_dir()) {
        push_path_pair(args, "--ro-bind", systemd_resolve, systemd_resolve)?;
        mounted_run_paths.push(systemd_resolve.to_path_buf());
    }
    if std::fs::symlink_metadata(resolv_conf).is_ok_and(|meta| meta.file_type().is_symlink())
        && let Ok(target) = std::fs::canonicalize(resolv_conf)
        && target.starts_with(run_prefix)
        && !target.starts_with(systemd_resolve)
        && std::fs::metadata(&target).is_ok_and(|meta| meta.is_file())
    {
        push_path_pair(args, "--ro-bind", &target, &target)?;
        mounted_run_paths.push(target);
    }
    Ok(())
}

#[cfg(any(target_os = "linux", all(unix, test)))]
fn collect_denials(
    policy: &SandboxPolicy,
    home: Option<&Path>,
    include_credential_denials: bool,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut directories = Vec::new();
    let mut files = Vec::new();
    if include_credential_denials && let Some(home_dir) = home {
        let denials = credentials::home_denials(home_dir);
        directories.extend(denials.directories);
        files.extend(denials.files);
        for relative in HOME_PLATFORM_DENIED_DIRECTORIES {
            let path = home_dir.join(relative);
            if let Ok(resolved) = std::fs::canonicalize(&path) {
                directories.push(resolved);
            }
            directories.push(path);
        }
    }
    for path in &policy.denied_paths {
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.is_dir() {
                directories.push(path.clone());
            } else if meta.is_file() {
                files.push(path.clone());
            }
        }
    }
    directories.sort();
    directories.dedup();
    files.sort();
    files.dedup();
    (directories, files)
}

#[cfg(any(target_os = "linux", all(unix, test)))]
fn sorted_by_depth(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut sorted = paths.to_vec();
    sorted.sort_by(|left, right| {
        left.components()
            .count()
            .cmp(&right.components().count())
            .then_with(|| left.cmp(right))
    });
    sorted.dedup();
    sorted
}

#[cfg(any(target_os = "linux", all(unix, test)))]
fn validate_path_text(path: &Path) -> Result<(), SandboxError> {
    let Some(text) = path.to_str() else {
        return Err(SandboxError::UnrepresentablePath(path.to_path_buf()));
    };
    if text.chars().any(char::is_control) {
        return Err(SandboxError::UnrepresentablePath(path.to_path_buf()));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", all(unix, test)))]
fn validate_absolute_path(path: &Path) -> Result<(), SandboxError> {
    if !path.is_absolute() {
        return Err(SandboxError::UnrepresentablePath(path.to_path_buf()));
    }
    validate_path_text(path)
}

#[cfg(any(target_os = "linux", all(unix, test)))]
fn push_unary(args: &mut Vec<OsString>, flag: &str, value: &str) {
    args.push(OsString::from(flag));
    args.push(OsString::from(value));
}

#[cfg(any(target_os = "linux", all(unix, test)))]
fn push_pair(args: &mut Vec<OsString>, flag: &str, src: &str, dst: &str) {
    args.push(OsString::from(flag));
    args.push(OsString::from(src));
    args.push(OsString::from(dst));
}

#[cfg(any(target_os = "linux", all(unix, test)))]
fn push_path_pair(
    args: &mut Vec<OsString>,
    flag: &str,
    src: &Path,
    dst: &Path,
) -> Result<(), SandboxError> {
    validate_absolute_path(src)?;
    validate_absolute_path(dst)?;
    args.push(OsString::from(flag));
    args.push(src.as_os_str().to_os_string());
    args.push(dst.as_os_str().to_os_string());
    Ok(())
}

#[cfg(all(unix, test))]
mod tests {
    use super::*;

    fn find_subslice(haystack: &[OsString], needle: &[&str]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window.iter().zip(needle).all(|(a, b)| a == *b))
    }

    #[test]
    fn build_args_orders_mounts_and_deduplicates_nested_denials() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join(".cargo")).unwrap();
        std::fs::create_dir_all(home.join(".local/share/keyrings")).unwrap();
        std::fs::write(home.join(".ssh/id_ed25519"), "secret").unwrap();
        std::fs::write(home.join(".cargo/config.toml"), "[net]\n").unwrap();
        std::fs::write(home.join(".cargo/credentials.toml"), "secret").unwrap();
        std::fs::create_dir_all(ws.join(".git/hooks")).unwrap();
        std::fs::write(ws.join(".git/config"), "[core]\n").unwrap();
        std::fs::create_dir_all(ws.join("sub/deep/store")).unwrap();

        let home = std::fs::canonicalize(&home).unwrap();
        let ws = std::fs::canonicalize(&ws).unwrap();

        let policy = SandboxPolicy::new(vec![ws.clone()], vec![ws.clone()])
            .unwrap()
            .with_git_metadata_protection(&ws)
            .unwrap()
            .with_protected_read_only_path(&ws, ws.join("sub/deep/store"))
            .unwrap()
            .with_protected_read_only_path(&ws, ws.join("missing/.tars"))
            .unwrap()
            .with_network(NetworkAccess::None);

        let launch = Launch::new("/bin/echo")
            .arg("hello")
            .current_dir(&ws)
            .env("FOO", "bar");

        // Roots scope: `$HOME/.ssh` is not mounted in Roots, so `--tmpfs $HOME/.ssh`
        // must NOT be emitted, while `$HOME/.cargo` IS mounted (system_baseline)
        // so `--ro-bind /dev/null $HOME/.cargo/credentials.toml` MUST be emitted.
        let roots_args = build_args(&policy, &launch, Some(&home), 7, true).unwrap();
        assert!(roots_args.iter().any(|arg| arg == "--unshare-net"));
        assert!(roots_args.iter().any(|arg| arg == "--new-session"));
        assert!(roots_args.iter().any(|arg| arg == "--die-with-parent"));
        assert!(find_subslice(&roots_args, &["--seccomp", "7"]));
        assert!(find_subslice(&roots_args, &["--setenv", "FOO", "bar"]));

        let ssh_dir = home.join(".ssh");
        let ssh_str = ssh_dir.to_str().unwrap();
        assert!(!find_subslice(&roots_args, &["--tmpfs", ssh_str]));
        let cargo_creds = home.join(".cargo/credentials.toml");
        assert!(find_subslice(
            &roots_args,
            &["--ro-bind", "/dev/null", cargo_creds.to_str().unwrap()]
        ));

        // Protected entries and read-only subpaths:
        // `ws/sub` and `ws/sub/deep` are `--bind`; `ws/sub/deep/store` is `--ro-bind` ONLY.
        let sub_str = ws.join("sub").to_str().unwrap().to_owned();
        let store_str = ws.join("sub/deep/store").to_str().unwrap().to_owned();
        assert!(find_subslice(&roots_args, &["--bind", &sub_str, &sub_str]));
        assert!(!find_subslice(
            &roots_args,
            &["--bind", &store_str, &store_str]
        ));
        assert!(find_subslice(
            &roots_args,
            &["--ro-bind", &store_str, &store_str]
        ));
        // Decision 1A: missing protected path must not be created on disk.
        assert!(!ws.join("missing").exists());

        // Host scope: `$HOME/.ssh` IS visible so `--tmpfs $HOME/.ssh --chmod 0000`
        // is emitted, and nested `$HOME/.ssh/id_ed25519` must NOT be mounted inside it.
        let host_policy = policy.with_read_scope(ReadScope::Host);
        let host_args = build_args(&host_policy, &launch, Some(&home), 8, true).unwrap();
        assert!(!find_subslice(&host_args, &["--tmpfs", "/tmp"]));
        assert!(find_subslice(
            &host_args,
            &["--tmpfs", ssh_str, "--chmod", "0000"]
        ));
        let ssh_key = home.join(".ssh/id_ed25519");
        assert!(!find_subslice(
            &host_args,
            &["--ro-bind", "/dev/null", ssh_key.to_str().unwrap()]
        ));
    }

    #[test]
    fn build_args_handles_workspace_equal_home_and_explicit_nested_denied_dir() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join("nested/inner")).unwrap();
        std::fs::write(home.join("nested/inner/secret"), "nope").unwrap();
        let home = std::fs::canonicalize(&home).unwrap();

        let policy = SandboxPolicy::new(vec![home.clone()], vec![home.clone()])
            .unwrap()
            .with_denied_path(home.join("nested"))
            .unwrap()
            .with_denied_path(home.join("nested/inner"))
            .unwrap()
            .with_denied_path(home.join("nested/inner/secret"))
            .unwrap()
            .with_network(NetworkAccess::Open);

        let launch = Launch::new("/bin/true").current_dir(&home);
        let args = build_args(&policy, &launch, Some(&home), 9, true).unwrap();
        assert!(!args.iter().any(|arg| arg == "--unshare-net"));

        let ssh_dir = home.join(".ssh");
        assert!(find_subslice(
            &args,
            &["--tmpfs", ssh_dir.to_str().unwrap(), "--chmod", "0000"]
        ));
        let nested_dir = home.join("nested");
        let inner_dir = home.join("nested/inner");
        let secret_file = home.join("nested/inner/secret");
        assert!(find_subslice(
            &args,
            &["--tmpfs", nested_dir.to_str().unwrap(), "--chmod", "0000"]
        ));
        assert!(!find_subslice(
            &args,
            &["--tmpfs", inner_dir.to_str().unwrap()]
        ));
        assert!(!find_subslice(
            &args,
            &["--ro-bind", "/dev/null", secret_file.to_str().unwrap()]
        ));
    }

    #[test]
    fn build_args_skips_outside_pointing_symlink_in_read_only_subpaths() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(outside.join("secret_dir")).unwrap();
        std::fs::write(outside.join("secret_dir/token"), "SECRET\n").unwrap();
        std::os::unix::fs::symlink(outside.join("secret_dir"), ws.join("ext_link")).unwrap();

        let ws = std::fs::canonicalize(&ws).unwrap();
        let policy = SandboxPolicy::new(vec![ws.clone()], vec![ws.clone()])
            .unwrap()
            .with_protected_read_only_path(&ws, ws.join("ext_link"))
            .unwrap();
        let launch = Launch::new("/bin/true").current_dir(&ws);
        let args = build_args(&policy, &launch, None, 10, true).unwrap();

        let ext_link = ws.join("ext_link");
        let ext_link_str = ext_link.to_str().unwrap();
        assert!(!find_subslice(
            &args,
            &["--ro-bind", ext_link_str, ext_link_str]
        ));
        assert!(!find_subslice(
            &args,
            &["--bind", ext_link_str, ext_link_str]
        ));
    }

    #[test]
    fn build_args_pins_regular_file_protected_entry_such_as_git_worktree_file() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join(".git"), "gitdir: /tmp/worktree\n").unwrap();

        let ws = std::fs::canonicalize(&ws).unwrap();
        let policy = SandboxPolicy::new(vec![ws.clone()], vec![ws.clone()])
            .unwrap()
            .with_git_metadata_protection(&ws)
            .unwrap();
        let launch = Launch::new("/bin/true").current_dir(&ws);
        let args = build_args(&policy, &launch, None, 11, true).unwrap();

        let git_file = ws.join(".git");
        let git_str = git_file.to_str().unwrap();
        assert!(find_subslice(&args, &["--ro-bind", git_str, git_str]));
        assert!(!find_subslice(&args, &["--bind", git_str, git_str]));
    }

    #[test]
    fn open_network_dns_mounts_work_after_tmpfs_run_in_roots_and_host_scopes() {
        let dir = tempfile::tempdir().unwrap();
        let fake_root = std::fs::canonicalize(dir.path()).unwrap();
        let fake_run = fake_root.join("run");
        let fake_systemd_resolve = fake_run.join("systemd/resolve");
        let fake_nm_run = fake_run.join("NetworkManager");
        let fake_etc = fake_root.join("etc");
        std::fs::create_dir_all(&fake_systemd_resolve).unwrap();
        std::fs::create_dir_all(&fake_nm_run).unwrap();
        std::fs::create_dir_all(&fake_etc).unwrap();
        let nm_resolv = fake_nm_run.join("resolv.conf");
        std::fs::write(&nm_resolv, "nameserver 1.1.1.1\n").unwrap();
        let etc_resolv = fake_etc.join("resolv.conf");
        std::fs::symlink_metadata(&etc_resolv).ok();
        std::os::unix::fs::symlink(&nm_resolv, &etc_resolv).unwrap();

        let mut args = Vec::new();
        let mut mounted_run_paths = Vec::new();
        mount_open_network_dns_with(
            &mut args,
            &mut mounted_run_paths,
            &fake_systemd_resolve,
            &etc_resolv,
            &fake_run,
        )
        .unwrap();

        let systemd_str = fake_systemd_resolve.to_str().unwrap();
        let nm_str = nm_resolv.to_str().unwrap();
        assert!(find_subslice(
            &args,
            &["--ro-bind", systemd_str, systemd_str]
        ));
        assert!(find_subslice(&args, &["--ro-bind", nm_str, nm_str]));
        assert_eq!(mounted_run_paths, vec![fake_systemd_resolve, nm_resolv]);

        // Verify `build_args` in ReadScope::Roots + NetworkAccess::Open emits
        // `--tmpfs /run` before any `/run/...` mounts, and omits `--unshare-net`.
        let ws = fake_root.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let open_roots = SandboxPolicy::new(vec![ws.clone()], vec![ws.clone()])
            .unwrap()
            .with_read_scope(ReadScope::Roots)
            .with_network(NetworkAccess::Open);
        let launch = Launch::new("/bin/true").current_dir(&ws);
        let roots_args = build_args(&open_roots, &launch, None, 12, true).unwrap();
        assert!(!roots_args.iter().any(|arg| arg == "--unshare-net"));
        let tmpfs_run_pos = roots_args
            .windows(2)
            .position(|window| window[0] == "--tmpfs" && window[1] == "/run")
            .expect("ReadScope::Roots must mount --tmpfs /run");
        if let Some(resolve_pos) = roots_args
            .windows(3)
            .position(|window| window[0] == "--ro-bind" && window[1] == "/run/systemd/resolve")
        {
            assert!(
                tmpfs_run_pos < resolve_pos,
                "--tmpfs /run must precede /run/systemd/resolve bind"
            );
        }

        // With system_baseline disabled, `/run/systemd/resolve` must not be mounted.
        let no_baseline = open_roots.with_system_baseline(false);
        let no_baseline_args = build_args(&no_baseline, &launch, None, 13, true).unwrap();
        assert!(!no_baseline_args.iter().any(|arg| arg == "/etc"));
        assert!(
            !no_baseline_args
                .iter()
                .any(|arg| arg == "/run/systemd/resolve")
        );
    }
}

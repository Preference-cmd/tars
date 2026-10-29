use super::backend::{
    BackendId, BackendReport, Capability, Launch, Limitation, SandboxBackend, SandboxError,
    SandboxUnavailable, SandboxedCommand,
};
use super::credentials;
use super::policy::{NetworkAccess, ReadScope, SandboxPolicy};
use super::process::ProcessGroupControl;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const DNS_SOCKET: &str = "/private/var/run/mDNSResponder";

const SYSTEM_READ_PATHS: &[&str] = &[
    "/usr",
    "/bin",
    "/sbin",
    "/System",
    "/Library",
    "/private/etc",
    "/private/var/db/timezone",
    "/private/var/select",
];

const SYSTEM_READ_DEVICES: &[&str] = &["/dev/null", "/dev/zero", "/dev/urandom", "/dev/random"];
const PLATFORM_DENIED_DIRECTORIES: &[&str] = &[
    "/Library/Keychains",
    "/Library/Preferences/SystemConfiguration",
];
const HOME_PLATFORM_DENIED_DIRECTORIES: &[&str] = &["Library/Keychains"];
const TOOLCHAIN_READ_PATHS: &[&str] = &["/opt/homebrew", "/usr/local"];
const HOME_TOOLCHAIN_READ_PATHS: &[&str] = &[".cargo", ".rustup"];

const MACH_SERVICES: &[&str] = &[
    "com.apple.DirectoryService.Framework.1",
    "com.apple.analyticsd",
    "com.apple.bsd.dirhelper",
    "com.apple.cfprefsd.agent",
    "com.apple.cfprefsd.daemon",
    "com.apple.diagnosticd",
    "com.apple.distributed_notifications.2",
    "com.apple.dnssd.service",
    "com.apple.nehelper",
    "com.apple.nesessionmanager",
    "com.apple.networkd",
    "com.apple.secinitd",
    "com.apple.system.DirectoryService.libinfo_v1",
    "com.apple.system.logger",
    "com.apple.system.notification_center",
    "com.apple.system.opendirectoryd.libinfo",
    "com.apple.system.opendirectoryd.membership",
];

pub(crate) struct SeatbeltBackend {
    executable: PathBuf,
    home: Option<PathBuf>,
    #[cfg(test)]
    omit_credential_denials: bool,
}

impl SeatbeltBackend {
    pub(crate) fn new() -> Self {
        Self {
            executable: PathBuf::from(SANDBOX_EXEC),
            home: std::env::var_os("HOME").map(PathBuf::from),
            #[cfg(test)]
            omit_credential_denials: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_executable(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            home: std::env::var_os("HOME").map(PathBuf::from),
            omit_credential_denials: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn without_home() -> Self {
        Self {
            executable: PathBuf::from(SANDBOX_EXEC),
            home: None,
            omit_credential_denials: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_home(home: impl Into<PathBuf>) -> Self {
        Self {
            executable: PathBuf::from(SANDBOX_EXEC),
            home: Some(home.into()),
            omit_credential_denials: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn without_credential_denials(mut self) -> Self {
        self.omit_credential_denials = true;
        self
    }

    fn unavailable(&self, reason: impl Into<String>) -> SandboxUnavailable {
        SandboxUnavailable::new(BackendId::SEATBELT, reason)
    }

    fn probe_backend(&self, home: PathBuf) -> Self {
        Self {
            executable: self.executable.clone(),
            home: Some(home),
            #[cfg(test)]
            omit_credential_denials: self.omit_credential_denials,
        }
    }

    fn credential_denials(&self, home: &Path) -> credentials::HomeDenials {
        #[cfg(test)]
        if self.omit_credential_denials {
            return credentials::HomeDenials {
                directories: Vec::new(),
                files: Vec::new(),
            };
        }
        credentials::home_denials(home)
    }

    pub(crate) fn profile(&self, policy: &SandboxPolicy) -> Result<String, SandboxError> {
        let mut profile = String::from("(version 1)\n(deny default)\n");
        profile.push_str("(allow process-exec)\n");
        profile.push_str("(allow process-fork)\n");
        profile.push_str("(allow signal (target same-sandbox))\n");

        if policy.system_baseline() {
            // `kern.proc*` exposes another process's environment through
            // kern.procargs2. On macOS 27 this deny and the process-info pair
            // below are both required; either alone still permits the read.
            profile.push_str("(allow sysctl-read)\n");
            profile.push_str("(deny sysctl-read (sysctl-name-prefix \"kern.proc\"))\n");
            if policy.read_scope() == ReadScope::Roots {
                profile.push_str("(allow file-read-metadata)\n");
                push_rules(
                    &mut profile,
                    "allow",
                    "file-read*",
                    "subpath",
                    SYSTEM_READ_PATHS,
                )?;
                // `/` itself is readable so sandbox-exec and absolute-path
                // resolution can walk it; this exposes the top-level listing.
                push_rule(
                    &mut profile,
                    "allow",
                    "file-read*",
                    "literal",
                    Path::new("/"),
                )?;

                let mut toolchain = TOOLCHAIN_READ_PATHS
                    .iter()
                    .filter_map(existing_directory)
                    .collect::<Vec<_>>();
                if let Some(home) = &self.home {
                    for relative in HOME_TOOLCHAIN_READ_PATHS {
                        if let Some(path) = existing_directory(home.join(relative)) {
                            toolchain.push(path);
                        }
                    }
                }
                push_rules(&mut profile, "allow", "file-read*", "subpath", &toolchain)?;
            }
            profile.push_str("(allow mach-lookup");
            for service in MACH_SERVICES {
                profile.push_str(" (global-name \"");
                profile.push_str(service);
                profile.push_str("\")");
            }
            profile.push_str(")\n");
        }

        if policy.read_scope() == ReadScope::Host {
            profile.push_str("(allow file-read*)\n");
        }
        push_rules(
            &mut profile,
            "allow",
            "file-read*",
            "subpath",
            &policy.read_roots,
        )?;
        push_rules(
            &mut profile,
            "allow",
            "file-read*",
            "subpath",
            &policy.write_roots,
        )?;
        push_rules(
            &mut profile,
            "allow",
            "file-write*",
            "subpath",
            &policy.write_roots,
        )?;
        push_rule(
            &mut profile,
            "allow",
            "file-write*",
            "literal",
            Path::new("/dev/null"),
        )?;

        // Seatbelt is last-match-wins. Every deny is emitted after every allow
        // so a toolchain, policy or device allow cannot re-open it. `/dev` is
        // narrowed for both read scopes: commands share Tarz's controlling
        // terminal, and reading `/dev/tty` would take the user's keyboard.
        profile.push_str("(deny file-read* (subpath \"/dev\"))\n");
        push_rules(
            &mut profile,
            "allow",
            "file-read*",
            "literal",
            SYSTEM_READ_DEVICES,
        )?;

        let mut denied_directories = PLATFORM_DENIED_DIRECTORIES
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        let mut denied_files = Vec::new();
        if let Some(home) = &self.home {
            let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.clone());
            let denials = self.credential_denials(&home);
            denied_directories.extend(denials.directories);
            denied_files.extend(denials.files);
            for relative in HOME_PLATFORM_DENIED_DIRECTORIES {
                let path = home.join(relative);
                if let Ok(resolved) = std::fs::canonicalize(&path) {
                    denied_directories.push(resolved);
                }
                denied_directories.push(path);
            }
        }
        denied_directories.sort();
        denied_directories.dedup();
        denied_files.sort();
        denied_files.dedup();
        push_rules(
            &mut profile,
            "deny",
            "file-read* file-write*",
            "subpath",
            &denied_directories,
        )?;
        push_rules(
            &mut profile,
            "deny",
            "file-read* file-write*",
            "literal",
            &denied_files,
        )?;
        push_rules(
            &mut profile,
            "deny",
            "file-write*",
            "literal",
            &policy.protected_entries,
        )?;
        push_rules(
            &mut profile,
            "deny",
            "file-write*",
            "subpath",
            &policy.read_only_subpaths,
        )?;
        push_rules(
            &mut profile,
            "deny",
            "file-read* file-write*",
            "subpath",
            &policy.denied_paths,
        )?;
        profile.push_str("(deny process-info*)\n");
        profile.push_str("(allow process-info* (target same-sandbox))\n");

        if policy.network() == NetworkAccess::Open {
            // IP traffic plus the system resolver socket. Other unix sockets
            // stay closed so the command cannot reach ssh-agent or Docker.
            profile.push_str("(allow network-outbound (remote ip))\n");
            profile.push_str(&format!(
                "(allow network-outbound (remote unix-socket (path-literal \"{DNS_SOCKET}\")))\n"
            ));
        }
        Ok(profile)
    }

    fn run_probe(&self) -> Result<(), SandboxUnavailable> {
        if !self.executable.is_file() {
            return Err(self.unavailable(format!(
                "executable not found: {}",
                self.executable.display()
            )));
        }
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
        let script = r#"set -u
scope=$1
ws=$2
outside=$3
home=$4
port=$5
agent=$6
host=$7
if ! [ -x /usr/bin/nc ]; then exit 19; fi
if ! /usr/bin/uname -a >/dev/null 2>&1; then exit 23; fi
printf ok > "$ws/ok" || exit 11
printf ok > /dev/null || exit 24
if printf x > "$outside/escape" 2>/dev/null; then exit 12; fi
if [ "$scope" = roots ]; then
  if cat "$outside/canary" >/dev/null 2>&1; then exit 13; fi
elif ! grep -q CANARY "$outside/canary" 2>/dev/null; then
  exit 25
fi
if grep -q DENIED-CANARY "$outside/denied" 2>/dev/null; then exit 32; fi
if ! grep -q PROBE-CONFIG "$home/.cargo/config.toml" 2>/dev/null; then exit 17; fi
if grep -q PROBE-CANARY "$home/.cargo/credentials.toml" 2>/dev/null; then exit 18; fi
if grep -q PROBE-CANARY "$home/.ssh/id_ed25519" 2>/dev/null; then exit 20; fi
if grep -q PROBE-CANARY "$home/.aws/credentials" 2>/dev/null; then exit 26; fi
if grep -q PROBE-CANARY "$home/.zsh_history" 2>/dev/null; then exit 27; fi
if /usr/bin/head -c 0 /dev/tty >/dev/null 2>&1; then exit 28; fi
if printf x > "$ws/.git/hooks/pre-commit" 2>/dev/null; then exit 14; fi
if printf x > "$ws/.git/config" 2>/dev/null; then exit 29; fi
if mv "$ws/.git" "$ws/.git-moved" 2>/dev/null; then exit 30; fi
if kill -0 "$host" 2>/dev/null; then exit 15; fi
if /usr/bin/nc -z -w 1 127.0.0.1 "$port" >/dev/null 2>&1; then exit 16; fi
if /usr/bin/nc -U -z -w 1 "$agent" >/dev/null 2>&1; then exit 31; fi
if cat /Library/Keychains/System.keychain >/dev/null 2>&1; then exit 21; fi
echo probe-ok
"#;
        for (scope, policy) in policies {
            let backend = self.probe_backend(home.clone());
            let profile = backend.profile(&policy).map_err(|error| {
                self.unavailable(format!("could not build {scope} probe profile: {error}"))
            })?;
            if !profile.contains("(deny sysctl-read (sysctl-name-prefix \"kern.proc\"))")
                || !profile.contains("(deny process-info*)")
                || !profile.contains("(deny file-read* (subpath \"/dev\"))")
            {
                return Err(self.unavailable(format!(
                    "{scope} probe profile is missing a sysctl, process-info or /dev restriction"
                )));
            }
            let mut child = Command::new(&self.executable)
                .arg("-p")
                .arg(profile)
                .arg("/bin/bash")
                .arg("-c")
                .arg(script)
                .arg("tarz-probe")
                .arg(scope)
                .arg(&workspace)
                .arg(&outside)
                .arg(&home)
                .arg(&port)
                .arg(&agent_socket)
                .arg(&host)
                .current_dir(&workspace)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .process_group(0)
                .spawn()
                .map_err(|error| {
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
            let detail = if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            };
            return Err(self.unavailable(format!("{scope} probe failed with {status}{detail}")));
        }
        Ok(())
    }
}

impl SandboxBackend for SeatbeltBackend {
    fn id(&self) -> BackendId {
        BackendId::SEATBELT
    }

    fn probe(&self) -> Result<BackendReport, SandboxUnavailable> {
        self.run_probe()?;
        Ok(BackendReport::new(
            BackendId::SEATBELT,
            Capability::Full,
            Capability::Full,
            Capability::Full,
            Capability::Full,
            Capability::Partial(Limitation::ProcessGroupEscape),
        ))
    }

    fn wrap(
        &self,
        policy: &SandboxPolicy,
        launch: Launch,
    ) -> Result<SandboxedCommand, SandboxError> {
        launch.validate()?;
        let profile = self.profile(policy)?;
        let mut command = Self::base_command(&self.executable, &launch);
        command.arg("-p");
        command.arg(profile);
        command.arg(launch.program());
        command.args(launch.launch_args());
        Ok(SandboxedCommand::new(
            command,
            Box::new(ProcessGroupControl),
        ))
    }

    fn unconfined(&self, launch: Launch) -> Result<SandboxedCommand, SandboxError> {
        launch.validate()?;
        let mut command = Self::base_command(launch.program(), &launch);
        command.args(launch.launch_args());
        Ok(SandboxedCommand::new(
            command,
            Box::new(ProcessGroupControl),
        ))
    }
}

impl SeatbeltBackend {
    fn base_command(program: &Path, launch: &Launch) -> tokio::process::Command {
        let mut command = tokio::process::Command::new(program);
        if let Some(directory) = launch.working_directory() {
            command.current_dir(directory);
        }
        command.env_clear();
        for (name, value) in launch.environment() {
            command.env(name, value);
        }
        command.process_group(0);
        command
    }
}

fn push_rules<P: AsRef<Path>>(
    profile: &mut String,
    verb: &str,
    operations: &str,
    matcher: &str,
    paths: &[P],
) -> Result<(), SandboxError> {
    if paths.is_empty() {
        return Ok(());
    }
    profile.push('(');
    profile.push_str(verb);
    profile.push(' ');
    profile.push_str(operations);
    for path in paths {
        profile.push_str(" (");
        profile.push_str(matcher);
        profile.push_str(" \"");
        profile.push_str(&escape(path.as_ref())?);
        profile.push_str("\")");
    }
    profile.push_str(")\n");
    Ok(())
}

fn push_rule(
    profile: &mut String,
    verb: &str,
    operations: &str,
    matcher: &str,
    path: &Path,
) -> Result<(), SandboxError> {
    push_rules(profile, verb, operations, matcher, &[path])
}

fn escape(path: &Path) -> Result<String, SandboxError> {
    let text = path
        .to_str()
        .ok_or_else(|| SandboxError::UnrepresentablePath(path.to_path_buf()))?;
    if text.chars().any(char::is_control) {
        return Err(SandboxError::UnrepresentablePath(path.to_path_buf()));
    }
    Ok(text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn existing_directory(path: impl AsRef<Path>) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path.as_ref()).ok()?;
    if !canonical.is_dir() {
        return None;
    }
    let text = canonical.to_str()?;
    if text.chars().any(|character| character.is_control()) {
        return None;
    }
    Some(canonical)
}

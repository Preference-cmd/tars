use super::backend::{
    BackendId, BackendReport, Capability, Launch, Limitation, SandboxBackend, SandboxError,
    SandboxUnavailable, SandboxedCommand,
};
use super::policy::{NetworkAccess, SandboxPolicy};
use super::process::ProcessGroupControl;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

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
const SYSTEM_DENIED_PATHS: &[&str] = &[
    "/Library/Keychains",
    "/Library/Preferences/SystemConfiguration",
];
const TOOLCHAIN_READ_PATHS: &[&str] = &["/opt/homebrew", "/usr/local"];
const HOME_TOOLCHAIN_READ_PATHS: &[&str] = &[".cargo", ".rustup"];
const HOME_CREDENTIAL_FILES: &[&str] = &[
    ".cargo/credentials.toml",
    ".cargo/credentials",
    ".cargo/config.toml",
    ".cargo/config",
];

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
}

impl SeatbeltBackend {
    pub(crate) fn new() -> Self {
        Self {
            executable: PathBuf::from(SANDBOX_EXEC),
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_executable(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            home: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_home(home: impl Into<PathBuf>) -> Self {
        Self {
            executable: PathBuf::from(SANDBOX_EXEC),
            home: Some(home.into()),
        }
    }

    fn unavailable(&self, reason: impl Into<String>) -> SandboxUnavailable {
        SandboxUnavailable::new(BackendId::SEATBELT, reason)
    }

    pub(crate) fn profile(&self, policy: &SandboxPolicy) -> Result<String, SandboxError> {
        let mut profile = String::from("(version 1)\n(deny default)\n");
        profile.push_str("(allow process-exec)\n");
        profile.push_str("(allow process-fork)\n");
        profile.push_str("(allow signal (target same-sandbox))\n");
        let mut credential_denies = Vec::new();
        if let Some(home) = &self.home {
            for relative in HOME_CREDENTIAL_FILES {
                credential_denies.push(home.join(relative));
            }
            if let Some(cargo) = existing_directory(home.join(".cargo")) {
                for name in ["credentials.toml", "credentials", "config.toml", "config"] {
                    credential_denies.push(cargo.join(name));
                }
            }
        }

        if policy.system_baseline() {
            // `kern.proc*` includes kern.procargs2, which exposes another
            // process's environment. On macOS 27 this prefix deny and the
            // process-info pair below are both required; either alone still
            // permits the read.
            profile.push_str("(allow sysctl-read)\n");
            profile.push_str("(deny sysctl-read (sysctl-name-prefix \"kern.proc\"))\n");
            profile.push_str("(allow file-read-metadata)\n");
            push_rules(
                &mut profile,
                "allow",
                "file-read*",
                "subpath",
                SYSTEM_READ_PATHS,
            )?;
            push_rules(
                &mut profile,
                "allow",
                "file-read*",
                "literal",
                SYSTEM_READ_DEVICES,
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
            profile.push_str("(allow mach-lookup");
            for service in MACH_SERVICES {
                profile.push_str(" (global-name \"");
                profile.push_str(service);
                profile.push_str("\")");
            }
            profile.push_str(")\n");

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
        // so a toolchain, policy or device allow cannot re-open it.
        credential_denies.sort();
        credential_denies.dedup();
        push_rules(
            &mut profile,
            "deny",
            "file-read*",
            "literal",
            &credential_denies,
        )?;
        push_rules(
            &mut profile,
            "deny",
            "file-read*",
            "subpath",
            SYSTEM_DENIED_PATHS,
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
            profile.push_str("(allow network-outbound)\n");
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
        let directory = tempfile::tempdir().map_err(|error| {
            self.unavailable(format!("could not create probe directory: {error}"))
        })?;
        let workspace = directory.path().join("workspace");
        let outside = directory.path().join("outside");
        let home = directory.path().join("home");
        std::fs::create_dir(&workspace)
            .and_then(|()| std::fs::create_dir(&outside))
            .and_then(|()| std::fs::create_dir(workspace.join(".git")))
            .and_then(|()| std::fs::write(outside.join("canary"), "CANARY\n"))
            .and_then(|()| std::fs::create_dir_all(home.join(".cargo")))
            .and_then(|()| std::fs::create_dir_all(home.join(".ssh")))
            .and_then(|()| std::fs::write(home.join(".cargo/config.toml"), "PROBE-CANARY\n"))
            .and_then(|()| std::fs::write(home.join(".cargo/credentials.toml"), "PROBE-CANARY\n"))
            .and_then(|()| std::fs::write(home.join(".ssh/id_ed25519"), "PROBE-CANARY\n"))
            .map_err(|error| self.unavailable(format!("could not prepare probe: {error}")))?;
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
            .map_err(|error| self.unavailable(format!("could not bind probe listener: {error}")))?;
        let port = listener
            .local_addr()
            .map_err(|error| self.unavailable(format!("could not read probe port: {error}")))?
            .port()
            .to_string();
        let policy = SandboxPolicy::new(vec![workspace.clone()], vec![workspace.clone()])
            .and_then(|policy| policy.with_read_only_subpath(workspace.join(".git")))
            .map_err(|error| self.unavailable(format!("could not build probe policy: {error}")))?;
        let probe_backend = SeatbeltBackend {
            executable: self.executable.clone(),
            home: Some(home.clone()),
        };
        let profile = probe_backend
            .profile(&policy)
            .map_err(|error| self.unavailable(format!("could not build probe profile: {error}")))?;
        if !profile.contains("(deny sysctl-read (sysctl-name-prefix \"kern.proc\"))")
            || !profile.contains("(deny process-info*)")
        {
            return Err(
                self.unavailable("probe profile is missing the sysctl or process-info restriction")
            );
        }
        let workspace = std::fs::canonicalize(&workspace)
            .map_err(|error| self.unavailable(format!("could not canonicalize probe: {error}")))?;
        let outside = std::fs::canonicalize(&outside)
            .map_err(|error| self.unavailable(format!("could not canonicalize probe: {error}")))?;
        let home = std::fs::canonicalize(&home)
            .map_err(|error| self.unavailable(format!("could not canonicalize probe: {error}")))?;
        let host = std::process::id().to_string();
        let script = r#"set -u
ws=$1
outside=$2
canary=$3
host=$4
port=$5
home=$6
if ! [ -x /usr/bin/nc ]; then exit 19; fi
if ! /usr/bin/uname -a >/dev/null 2>&1; then exit 23; fi
printf ok > "$ws/ok" || exit 11
if printf x > "$outside/escape" 2>/dev/null; then exit 12; fi
if grep -q CANARY "$canary" 2>/dev/null; then exit 13; fi
if printf x > "$ws/.git/newfile" 2>/dev/null; then exit 14; fi
if kill -0 "$host" 2>/dev/null; then exit 15; fi
if /usr/bin/nc -z -w 1 127.0.0.1 "$port" >/dev/null 2>&1; then exit 16; fi
if [ -n "$home" ]; then
  if cat "$home/.cargo/config.toml" >/dev/null 2>&1; then exit 17; fi
  if cat "$home/.cargo/credentials.toml" >/dev/null 2>&1; then exit 18; fi
  if cat "$home/.ssh/id_ed25519" >/dev/null 2>&1; then exit 20; fi
fi
if cat /Library/Keychains/System.keychain >/dev/null 2>&1; then exit 21; fi
echo probe-ok
"#;
        let mut child = Command::new(&self.executable)
            .arg("-p")
            .arg(profile)
            .arg("/bin/bash")
            .arg("-c")
            .arg(script)
            .arg("tarz-probe")
            .arg(&workspace)
            .arg(&outside)
            .arg(outside.join("canary"))
            .arg(host)
            .arg(port)
            .arg(&home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|error| self.unavailable(format!("could not start probe: {error}")))?;

        let deadline = Instant::now() + PROBE_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {}
                Err(error) => {
                    return Err(self.unavailable(format!("could not wait for probe: {error}")));
                }
            }
            if Instant::now() >= deadline {
                if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(self.unavailable("probe timed out"));
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
            return Ok(());
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
        Err(self.unavailable(format!("probe failed with {status}{detail}")))
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
        let mut command = tokio::process::Command::new(&self.executable);
        command.arg("-p");
        command.arg(profile);
        command.arg(launch.program());
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

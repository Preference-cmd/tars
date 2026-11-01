//! Test-only sandbox seams.
//!
//! This module is not a stable interface. It is compiled for this package's
//! own tests and with the `test-support` Cargo feature, which `tars-core`
//! enables from `[dev-dependencies]`; normal and release builds do not
//! include it. An injected backend performs no isolation, so tests observe
//! policy checks and launch attempts instead.

use super::backend::{
    BackendId, BackendReport, Launch, SandboxError, SandboxUnavailable, SandboxedCommand,
};
use super::policy::SandboxPolicy;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub use super::backend::SandboxBackend;

/// A backend that records launches and performs no isolation.
///
/// On Unix it attaches a real process group so command tests can exercise
/// termination; elsewhere the tree is inert. Production cannot select it.
pub struct RecordingBackend {
    report: Result<BackendReport, SandboxUnavailable>,
    calls: Arc<Mutex<Vec<(SandboxPolicy, Launch)>>>,
    unconfined: Arc<Mutex<Vec<Launch>>>,
}

impl RecordingBackend {
    /// A backend whose probe reports `report`.
    pub fn available(report: BackendReport) -> Self {
        Self {
            report: Ok(report),
            calls: Arc::new(Mutex::new(Vec::new())),
            unconfined: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// A backend whose probe fails, for fail-closed tests.
    pub fn unavailable() -> Self {
        Self {
            report: Err(SandboxUnavailable::new(
                BackendId::RECORDING,
                "probe failed",
            )),
            calls: Arc::new(Mutex::new(Vec::new())),
            unconfined: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The wrapped launches recorded so far, with the policy checked for each.
    pub fn calls(&self) -> Arc<Mutex<Vec<(SandboxPolicy, Launch)>>> {
        self.calls.clone()
    }

    /// The unconfined launches recorded so far.
    pub fn unconfined_calls(&self) -> Arc<Mutex<Vec<Launch>>> {
        self.unconfined.clone()
    }
}

fn command(launch: &Launch) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(launch.program());
    command.args(launch.launch_args());
    if let Some(directory) = launch.working_directory() {
        command.current_dir(directory);
    }
    command.env_clear();
    for (name, value) in launch.environment() {
        command.env(name, value);
    }
    #[cfg(unix)]
    command.process_group(0);
    command
}

fn control() -> Box<dyn super::process::TreeControl> {
    #[cfg(unix)]
    {
        Box::new(super::process::ProcessGroupControl)
    }
    #[cfg(not(unix))]
    {
        Box::new(super::process::RecordingControl)
    }
}

impl SandboxBackend for RecordingBackend {
    fn id(&self) -> BackendId {
        BackendId::RECORDING
    }

    fn probe(&self) -> Result<BackendReport, SandboxUnavailable> {
        self.report.clone()
    }

    fn wrap(
        &self,
        policy: &SandboxPolicy,
        launch: Launch,
    ) -> Result<SandboxedCommand, SandboxError> {
        launch.validate()?;
        self.calls
            .lock()
            .unwrap()
            .push((policy.clone(), launch.clone()));
        Ok(SandboxedCommand::new(command(&launch), control()))
    }

    fn unconfined(&self, launch: Launch) -> Result<SandboxedCommand, SandboxError> {
        launch.validate()?;
        self.unconfined.lock().unwrap().push(launch.clone());
        Ok(SandboxedCommand::new(command(&launch), control()))
    }
}

/// Build an unavailable error for injected test fixtures.
///
/// `tars-core` assembly tests need an unavailable status without a real
/// failed probe; production code receives these errors from backend probes.
pub fn unavailable(backend: BackendId, reason: impl Into<String>) -> SandboxUnavailable {
    SandboxUnavailable::new(backend, reason)
}

/// The paths a policy denies for both reads and writes.
///
/// This is a view for `tars-core` tests that assert on the policy the command
/// tool builds; production code has no accessor.
pub fn denied_paths(policy: &SandboxPolicy) -> &[PathBuf] {
    &policy.denied_paths
}

/// Every path list in a policy: read roots, write roots, read-only subpaths,
/// protected entries and denied paths.
///
/// This is a view for `tars-core` tests that assert the generated policy holds
/// only canonical absolute paths; production code has no accessor.
pub fn policy_paths(policy: &SandboxPolicy) -> [&[PathBuf]; 5] {
    [
        &policy.read_roots,
        &policy.write_roots,
        &policy.read_only_subpaths,
        &policy.protected_entries,
        &policy.denied_paths,
    ]
}

/// The environment variables a launch request applies after clearing the
/// inherited environment.
pub fn launch_environment(launch: &Launch) -> &[(OsString, OsString)] {
    launch.environment()
}

/// The program a launch request starts.
pub fn launch_program(launch: &Launch) -> &std::path::Path {
    launch.program()
}

/// The arguments a launch request passes.
pub fn launch_args(launch: &Launch) -> &[OsString] {
    launch.launch_args()
}

/// The working directory a launch request applies, when one is set.
pub fn launch_working_directory(launch: &Launch) -> Option<&std::path::Path> {
    launch.working_directory()
}

/// Open the macOS Seatbelt backend with an explicit home directory.
///
/// The probe resolves credential denials under `home`, so a test can keep its
/// canaries in a temporary directory instead of the real home.
#[cfg(target_os = "macos")]
pub fn open_with_home(home: PathBuf) -> Result<super::Sandbox, SandboxUnavailable> {
    super::Sandbox::open_with(Box::new(super::seatbelt::SeatbeltBackend::with_home(home)))
}

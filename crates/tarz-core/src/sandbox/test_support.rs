use super::backend::{
    BackendId, BackendReport, Launch, SandboxBackend, SandboxError, SandboxUnavailable,
    SandboxedCommand,
};
use super::policy::SandboxPolicy;
use std::sync::{Arc, Mutex};

/// A test-only backend that records launches and performs no isolation.
///
/// On Unix it attaches a real process group so command tests can exercise
/// termination; elsewhere the tree is inert. Production cannot select it.
pub(crate) struct RecordingBackend {
    report: Result<BackendReport, SandboxUnavailable>,
    calls: Arc<Mutex<Vec<(SandboxPolicy, Launch)>>>,
    unconfined: Arc<Mutex<Vec<Launch>>>,
}

impl RecordingBackend {
    pub(crate) fn available(report: BackendReport) -> Self {
        Self {
            report: Ok(report),
            calls: Arc::new(Mutex::new(Vec::new())),
            unconfined: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn unavailable() -> Self {
        Self {
            report: Err(SandboxUnavailable::new(
                BackendId::RECORDING,
                "probe failed",
            )),
            calls: Arc::new(Mutex::new(Vec::new())),
            unconfined: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn calls(&self) -> Arc<Mutex<Vec<(SandboxPolicy, Launch)>>> {
        self.calls.clone()
    }

    pub(crate) fn unconfined_calls(&self) -> Arc<Mutex<Vec<Launch>>> {
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

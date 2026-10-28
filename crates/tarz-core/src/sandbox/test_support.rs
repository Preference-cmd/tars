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
}

impl RecordingBackend {
    pub(crate) fn available(report: BackendReport) -> Self {
        Self {
            report: Ok(report),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn unavailable() -> Self {
        Self {
            report: Err(SandboxUnavailable::new(
                BackendId::RECORDING,
                "probe failed",
            )),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn calls(&self) -> Arc<Mutex<Vec<(SandboxPolicy, Launch)>>> {
        self.calls.clone()
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
        #[cfg(unix)]
        let control = Box::new(super::process::ProcessGroupControl);
        #[cfg(not(unix))]
        let control = Box::new(super::process::RecordingControl);
        Ok(SandboxedCommand::new(command, control))
    }
}

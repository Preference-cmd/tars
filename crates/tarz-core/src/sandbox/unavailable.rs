use super::backend::{
    BackendId, BackendReport, Launch, SandboxBackend, SandboxError, SandboxUnavailable,
    SandboxedCommand,
};
use super::policy::SandboxPolicy;

pub(crate) struct UnavailableBackend;

impl UnavailableBackend {
    pub(crate) fn new() -> Self {
        Self
    }

    fn unavailable(&self) -> SandboxUnavailable {
        SandboxUnavailable::new(
            BackendId::UNAVAILABLE,
            "no sandbox backend is implemented for this platform",
        )
    }
}

impl SandboxBackend for UnavailableBackend {
    fn id(&self) -> BackendId {
        BackendId::UNAVAILABLE
    }

    fn probe(&self) -> Result<BackendReport, SandboxUnavailable> {
        Err(self.unavailable())
    }

    fn wrap(
        &self,
        _policy: &SandboxPolicy,
        _launch: Launch,
    ) -> Result<SandboxedCommand, SandboxError> {
        Err(self.unavailable().into())
    }
}

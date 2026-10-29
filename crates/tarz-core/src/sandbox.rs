//! Platform-independent sandbox policy, backend selection and process control.
//!
//! A [`Sandbox`] is opened once for a bound tool. Opening probes the platform
//! backend and records its capability report; every launch is checked against
//! that report before a command is built, and a policy the backend cannot
//! enforce is rejected instead of falling back to an unsandboxed process.
//!
//! macOS uses `/usr/bin/sandbox-exec` with a generated Seatbelt profile. Other
//! platforms have no backend yet and report an unavailable sandbox.
//!
//! Seatbelt has no PID namespace. The backend starts every command in its own
//! process group and [`ProcessTree`] kills that group after a soft grace
//! period. A descendant that starts a new session or process group can leave
//! the group; that residual is reported as a partial descendant-lifecycle
//! capability.

mod backend;
mod credentials;
mod policy;
mod process;
#[cfg(target_os = "macos")]
mod seatbelt;
mod status;
#[cfg(not(target_os = "macos"))]
mod unavailable;

pub use backend::{
    BackendId, BackendReport, Capability, Launch, Limitation, Sandbox, SandboxError,
    SandboxUnavailable, SandboxedCommand,
};
pub use policy::{NetworkAccess, ReadScope, SandboxPolicy, SandboxPolicyError};
pub use process::ProcessTree;
pub(crate) use process::exit_status_code;
pub use status::SandboxStatus;

#[cfg(all(test, target_os = "macos"))]
pub(crate) fn open_with_home(home: std::path::PathBuf) -> Result<Sandbox, SandboxUnavailable> {
    Sandbox::open_with(Box::new(seatbelt::SeatbeltBackend::with_home(home)))
}

#[cfg(test)]
mod status_tests;
#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;

use super::policy::{NetworkAccess, SandboxPolicy};
use super::process::{ProcessTree, TreeControl};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Stable name of a sandbox backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BackendId(&'static str);

impl BackendId {
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    pub(crate) const fn new(name: &'static str) -> Self {
        Self(name)
    }

    /// The macOS Seatbelt backend.
    pub const SEATBELT: Self = Self::new("seatbelt");
    /// The placeholder used when a platform has no backend.
    pub const UNAVAILABLE: Self = Self::new("unavailable");
    /// The recording backend used by tests and the `test-support` feature.
    #[cfg(any(test, feature = "test-support"))]
    pub const RECORDING: Self = Self::new("recording");

    /// Return the stable name.
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for BackendId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

/// A known limitation of a partially enforced capability.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Limitation {
    /// A descendant that starts a new session or process group can leave it.
    ProcessGroupEscape,
    /// Another limitation described by the backend.
    Other(String),
}

impl std::fmt::Display for Limitation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ProcessGroupEscape => formatter.write_str(
                "a descendant that starts a new session or process group can leave the process group",
            ),
            Self::Other(reason) => formatter.write_str(reason),
        }
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full => formatter.write_str("full"),
            Self::Partial(limitation) => write!(formatter, "partial ({limitation})"),
            Self::Unsupported(reason) => write!(formatter, "unsupported ({reason})"),
        }
    }
}

/// How completely a backend enforces one policy dimension.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Capability {
    /// The backend enforces the dimension completely.
    Full,
    /// The backend enforces the dimension with a known limitation.
    Partial(Limitation),
    /// The backend cannot enforce the dimension.
    Unsupported(String),
}

/// What a probed backend can enforce.
///
/// Capabilities are reported separately so a caller can reject a policy
/// before any process is started. Only a partial descendant-lifecycle
/// capability is accepted for P0; every other partial or unsupported
/// dimension rejects the policy.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct BackendReport {
    /// The backend that produced this report.
    pub backend: BackendId,
    /// Filesystem read isolation.
    pub file_read: Capability,
    /// Filesystem write isolation.
    pub file_write: Capability,
    /// Network isolation or, when the policy requests it, outbound access.
    pub network: Capability,
    /// Signalling outside the sandbox.
    pub signal_isolation: Capability,
    /// Lifetime of descendants after the direct child exits.
    pub descendant_lifecycle: Capability,
}

impl BackendReport {
    /// Build a capability report for an injected backend.
    ///
    /// This belongs to the `test-support` seam: a test backend has no
    /// platform probe to produce a report with. Normal builds construct
    /// reports from the platform backend instead.
    #[cfg(any(test, feature = "test-support"))]
    pub fn new(
        backend: BackendId,
        file_read: Capability,
        file_write: Capability,
        network: Capability,
        signal_isolation: Capability,
        descendant_lifecycle: Capability,
    ) -> Self {
        Self {
            backend,
            file_read,
            file_write,
            network,
            signal_isolation,
            descendant_lifecycle,
        }
    }

    /// Check that this report can enforce `policy`.
    ///
    /// This is the same check [`Sandbox::wrap`] performs before asking a
    /// backend to wrap a launch. The command tool runs it earlier so an
    /// unenforceable policy is rejected before the user is asked to approve a
    /// call. Only a partial descendant-lifecycle capability is accepted;
    /// every other partial or unsupported dimension is rejected.
    pub fn check(&self, policy: &SandboxPolicy) -> Result<(), SandboxUnavailable> {
        self.require(&self.file_read, "file read", false)?;
        self.require(&self.file_write, "file write", false)?;
        match policy.network() {
            NetworkAccess::None => self.require(&self.network, "network", false)?,
            NetworkAccess::Open => {}
        }
        self.require(&self.signal_isolation, "signal isolation", false)?;
        self.require(&self.descendant_lifecycle, "descendant lifecycle", true)?;
        Ok(())
    }

    fn require(
        &self,
        capability: &Capability,
        dimension: &str,
        allow_partial: bool,
    ) -> Result<(), SandboxUnavailable> {
        match capability {
            Capability::Full => Ok(()),
            Capability::Partial(Limitation::ProcessGroupEscape) if allow_partial => Ok(()),
            Capability::Partial(limitation) => Err(SandboxUnavailable::new(
                self.backend,
                format!("{dimension} is only partially enforced: {limitation}"),
            )),
            Capability::Unsupported(reason) => Err(SandboxUnavailable::new(
                self.backend,
                format!("{dimension} is not supported: {reason}"),
            )),
        }
    }
}

/// Why a sandbox cannot be opened or cannot enforce a policy.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SandboxUnavailable {
    /// The backend that reported the failure.
    pub backend: BackendId,
    /// A bounded, user-facing reason.
    pub reason: String,
}

impl std::fmt::Display for SandboxUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.backend == BackendId::UNAVAILABLE {
            formatter.write_str(&self.reason)
        } else {
            write!(
                formatter,
                "{} sandbox is unavailable: {}",
                self.backend, self.reason
            )
        }
    }
}

impl std::error::Error for SandboxUnavailable {}

impl SandboxUnavailable {
    pub(crate) fn new(backend: BackendId, reason: impl Into<String>) -> Self {
        Self {
            backend,
            reason: reason.into(),
        }
    }
}

/// Why a sandboxed command could not be prepared.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SandboxError {
    /// The probed sandbox cannot enforce the requested policy.
    #[error(transparent)]
    Unavailable(#[from] SandboxUnavailable),
    /// The launch request is not valid for a sandbox backend.
    #[error("invalid sandbox launch: {0}")]
    InvalidLaunch(String),
    /// A policy path cannot be represented in the backend's policy language.
    #[error("sandbox path cannot be represented in the backend policy: {0}")]
    UnrepresentablePath(PathBuf),
}

/// A platform-neutral request to start one sandboxed process.
///
/// The environment is applied after clearing the inherited environment; the
/// caller decides which variables a tool may receive.
#[derive(Clone, Debug)]
pub struct Launch {
    program: PathBuf,
    args: Vec<OsString>,
    current_dir: Option<PathBuf>,
    env: Vec<(OsString, OsString)>,
}

impl Launch {
    /// Start a launch request for an absolute program path.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            env: Vec::new(),
        }
    }

    /// Append one argument.
    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Append several arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set the working directory.
    pub fn current_dir(mut self, directory: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(directory.into());
        self
    }

    /// Set one environment variable after the inherited environment is cleared.
    pub fn env(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((name.into(), value.into()));
        self
    }

    pub(crate) fn validate(&self) -> Result<(), SandboxError> {
        if !self.program.is_absolute() {
            return Err(SandboxError::InvalidLaunch(format!(
                "program must be absolute: {}",
                self.program.display()
            )));
        }
        if let Some(directory) = &self.current_dir
            && !directory.is_absolute()
        {
            return Err(SandboxError::InvalidLaunch(format!(
                "working directory must be absolute: {}",
                directory.display()
            )));
        }
        Ok(())
    }

    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    pub(crate) fn program(&self) -> &Path {
        &self.program
    }

    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    pub(crate) fn launch_args(&self) -> &[OsString] {
        &self.args
    }

    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    pub(crate) fn working_directory(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }

    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    pub(crate) fn environment(&self) -> &[(OsString, OsString)] {
        &self.env
    }
}

/// A command already wrapped by a backend and ready to spawn.
pub struct SandboxedCommand {
    command: tokio::process::Command,
    tree: Box<dyn TreeControl>,
}

impl SandboxedCommand {
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
    pub(crate) fn new(mut command: tokio::process::Command, tree: Box<dyn TreeControl>) -> Self {
        command.kill_on_drop(true);
        Self { command, tree }
    }

    /// Set the stdin of the sandboxed process.
    pub fn stdin(&mut self, stdio: std::process::Stdio) -> &mut Self {
        self.command.stdin(stdio);
        self
    }

    /// Set the stdout of the sandboxed process.
    pub fn stdout(&mut self, stdio: std::process::Stdio) -> &mut Self {
        self.command.stdout(stdio);
        self
    }

    /// Set the stderr of the sandboxed process.
    pub fn stderr(&mut self, stdio: std::process::Stdio) -> &mut Self {
        self.command.stderr(stdio);
        self
    }

    /// Spawn the wrapped command and return its process-tree handle.
    ///
    /// This is the only way to spawn the wrapped command, so the returned
    /// handle always owns the child and its process-group cleanup.
    pub fn spawn(mut self) -> std::io::Result<ProcessTree> {
        let child = self.command.spawn()?;
        self.tree.attach(child)
    }
}

/// A probed sandbox that checks its capability report before every launch.
pub struct Sandbox {
    backend: Arc<dyn SandboxBackend>,
    report: BackendReport,
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Sandbox")
            .field("report", &self.report)
            .finish_non_exhaustive()
    }
}

impl Sandbox {
    /// Probe the backend for this platform.
    ///
    /// This fails closed when the platform has no backend or when its probe
    /// fails; no unsandboxed fallback exists. The probe is blocking; call this
    /// from a blocking context or through `spawn_blocking` when binding a
    /// session on an async runtime.
    pub fn open() -> Result<Self, SandboxUnavailable> {
        Self::from_backend(platform_backend())
    }

    /// Open a sandbox over an injected backend instead of the platform one.
    ///
    /// This is the backend-injection seam of the `test-support` feature; the
    /// injected backend is still probed and its report is checked as usual.
    #[cfg(any(test, feature = "test-support"))]
    pub fn open_with(backend: Box<dyn SandboxBackend>) -> Result<Self, SandboxUnavailable> {
        Self::from_backend(backend)
    }

    fn from_backend(backend: Box<dyn SandboxBackend>) -> Result<Self, SandboxUnavailable> {
        let report = backend.probe()?;
        if report.backend != backend.id() {
            return Err(SandboxUnavailable::new(
                report.backend,
                "the backend probe reported a different backend",
            ));
        }
        Ok(Self {
            backend: Arc::from(backend),
            report,
        })
    }

    /// The capability report recorded when the sandbox was opened.
    pub fn report(&self) -> &BackendReport {
        &self.report
    }

    /// Check the policy against the report, then ask the backend to wrap it.
    pub fn wrap(
        &self,
        policy: &SandboxPolicy,
        launch: Launch,
    ) -> Result<SandboxedCommand, SandboxError> {
        self.report.check(policy)?;
        launch.validate()?;
        self.backend.wrap(policy, launch)
    }

    /// Build one process without sandbox isolation, but with the same
    /// process-tree control as a wrapped launch.
    ///
    /// This is a direct spawn. A backend that cannot provide it returns an
    /// error, so a platform without a sandbox also has no unconfined launch.
    pub fn unconfined(&self, launch: Launch) -> Result<SandboxedCommand, SandboxError> {
        launch.validate()?;
        self.backend.unconfined(launch)
    }
}

/// A platform backend that probes its capabilities and wraps launches.
///
/// Production selects exactly one backend per platform. The `test-support`
/// feature re-exports this trait so tests can inject a recording backend; it
/// is not a stable interface.
pub trait SandboxBackend: Send + Sync {
    /// The stable name of this backend.
    fn id(&self) -> BackendId;

    /// Probe the backend and report what it can enforce.
    fn probe(&self) -> Result<BackendReport, SandboxUnavailable>;

    /// Wrap one launch in the backend's isolation for the given policy.
    fn wrap(
        &self,
        policy: &SandboxPolicy,
        launch: Launch,
    ) -> Result<SandboxedCommand, SandboxError>;

    /// Build one process without isolation but with this backend's process
    /// tree control.
    fn unconfined(&self, launch: Launch) -> Result<SandboxedCommand, SandboxError>;
}

pub(crate) fn platform_backend() -> Box<dyn SandboxBackend> {
    #[cfg(target_os = "macos")]
    {
        Box::new(super::seatbelt::SeatbeltBackend::new())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(super::unavailable::UnavailableBackend::new())
    }
}

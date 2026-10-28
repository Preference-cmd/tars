use std::io;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::process::{Child, ChildStderr, ChildStdout};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TreeSignal {
    Term,
    Kill,
}

/// Backend-provided process-tree control.
pub(crate) trait TreeControl: Send + Sync {
    fn attach(&self, child: Child) -> io::Result<ProcessTree>;
}

/// Backend-specific process-tree signal implementation.
pub(crate) trait TreeSignaller: Send + Sync {
    fn signal(&self, signal: TreeSignal) -> io::Result<()>;
}

/// A backend-neutral handle to one sandboxed command and its process tree.
///
/// The handle owns the direct child. [`ProcessTree::wait`] reaps it and then
/// kills any remaining members of its process group, so a command that exits
/// after starting a background process does not leak that process.
/// [`ProcessTree::terminate`] sends a soft stop, waits up to the grace period
/// while watching the direct child, then sends a hard kill. Drop sends a hard
/// kill without waiting.
///
/// Tokio reaps exited children in its signal driver. Once the group leader has
/// exited, its process group id can be reused; if the group is already empty,
/// the final kill is normally a no-op, but a recycled group id in a very small
/// window could name an unrelated group. A descendant that starts a new
/// session or process group escapes the group entirely; that is reported as a
/// partial descendant-lifecycle capability.
pub struct ProcessTree {
    child: Child,
    armed: AtomicBool,
    signaller: Option<Box<dyn TreeSignaller>>,
}

impl ProcessTree {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    pub(crate) fn new(child: Child, signaller: Box<dyn TreeSignaller>) -> Self {
        Self {
            child,
            armed: AtomicBool::new(true),
            signaller: Some(signaller),
        }
    }

    #[cfg(all(test, not(unix)))]
    pub(crate) fn inactive(child: Child) -> Self {
        Self {
            child,
            armed: AtomicBool::new(false),
            signaller: None,
        }
    }

    /// Take the direct child's stdout pipe.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    /// Take the direct child's stderr pipe.
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// Wait for the direct child, kill any remaining process-group members,
    /// and disarm the handle.
    pub async fn wait(&mut self) -> io::Result<std::process::ExitStatus> {
        let status = self.child.wait().await?;
        self.kill_remaining_group();
        self.disarm();
        Ok(status)
    }

    /// Ask the process group to stop, then force it after `grace`.
    ///
    /// Returns the direct child's status after reaping it. If the child exits
    /// during the grace period, any descendants still in its group are killed
    /// before the handle is disarmed.
    pub async fn terminate(&mut self, grace: Duration) -> io::Result<std::process::ExitStatus> {
        if !self.armed.load(Ordering::SeqCst) {
            let _ = self.child.start_kill();
            return self.wait().await;
        }
        self.signal(TreeSignal::Term)?;
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.kill_remaining_group();
                self.disarm();
                return Ok(status);
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if !self.armed.load(Ordering::SeqCst) {
            return self.wait().await;
        }
        if let Some(status) = self.child.try_wait()? {
            self.kill_remaining_group();
            self.disarm();
            return Ok(status);
        }
        self.signal(TreeSignal::Kill)?;
        // The direct child may have moved to another process group; kill it
        // directly before waiting so terminate always returns.
        let _ = self.child.start_kill();
        let status = self.child.wait().await?;
        self.disarm();
        Ok(status)
    }

    fn disarm(&self) {
        self.armed.store(false, Ordering::SeqCst);
    }

    fn signal(&self, signal: TreeSignal) -> io::Result<()> {
        match &self.signaller {
            Some(signaller) => signaller.signal(signal),
            None => Ok(()),
        }
    }

    fn kill_remaining_group(&self) {
        if self.armed.swap(false, Ordering::SeqCst) {
            let _ = self.signal(TreeSignal::Kill);
        }
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        if !self.armed.load(Ordering::SeqCst) {
            return;
        }
        let exited = matches!(self.child.try_wait(), Ok(Some(_)));
        if exited {
            self.kill_remaining_group();
        } else {
            let _ = self.signal(TreeSignal::Kill);
        }
        self.disarm();
    }
}

#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) struct ProcessGroupControl;

#[cfg(unix)]
impl TreeControl for ProcessGroupControl {
    fn attach(&self, child: Child) -> io::Result<ProcessTree> {
        let id = child.id().ok_or_else(|| {
            io::Error::other("sandboxed child exited before its process group was attached")
        })?;
        let pgid = rustix::process::Pid::from_raw(id as i32)
            .ok_or_else(|| io::Error::other("sandboxed child has an invalid process id"))?;
        Ok(ProcessTree::new(
            child,
            Box::new(ProcessGroupSignaller { pgid }),
        ))
    }
}

#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct ProcessGroupSignaller {
    pgid: rustix::process::Pid,
}

#[cfg(unix)]
impl TreeSignaller for ProcessGroupSignaller {
    fn signal(&self, signal: TreeSignal) -> io::Result<()> {
        let signal = match signal {
            TreeSignal::Term => rustix::process::Signal::TERM,
            TreeSignal::Kill => rustix::process::Signal::KILL,
        };
        match rustix::process::kill_process_group(self.pgid, signal) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(all(test, not(unix)))]
pub(crate) struct RecordingControl;

#[cfg(all(test, not(unix)))]
impl TreeControl for RecordingControl {
    fn attach(&self, child: Child) -> io::Result<ProcessTree> {
        Ok(ProcessTree::inactive(child))
    }
}

/// Map a process exit status to the reported exit code and signal.
pub(crate) fn exit_status_code(status: ExitStatus) -> (Option<i32>, Option<i32>) {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return (Some(128 + signal), Some(signal));
        }
    }
    (status.code(), None)
}

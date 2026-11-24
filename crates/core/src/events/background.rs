//! Supervised background task registry (`BackgroundTaskRegistry`).

use super::types::{OnParentComplete, OperationStatus};
use causa::kernel::CancellationToken;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;
use tokio::sync::Notify;

/// Error returned when parsing an unrecognized [`BackgroundTaskKind`] string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown background task kind: '{0}'")]
pub struct ParseBackgroundTaskKindError(pub String);

/// Category of supervised background work tracked in [`BackgroundTaskRegistry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundTaskKind {
    /// Background delegated agent operation.
    AgentOperation,
    /// Background shell command execution.
    ShellCommand,
    /// Scheduled background timer.
    Timer,
}

impl BackgroundTaskKind {
    /// Canonical `snake_case` wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentOperation => "agent_operation",
            Self::ShellCommand => "shell_command",
            Self::Timer => "timer",
        }
    }
}

impl fmt::Display for BackgroundTaskKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BackgroundTaskKind {
    type Err = ParseBackgroundTaskKindError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "agent_operation" => Ok(Self::AgentOperation),
            "shell_command" => Ok(Self::ShellCommand),
            "timer" => Ok(Self::Timer),
            other => Err(ParseBackgroundTaskKindError(other.to_owned())),
        }
    }
}

/// Lifecycle policy governing how a background task interacts with parent completion
/// and non-interactive process exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskLifecyclePolicy {
    /// Bounded task that blocks non-interactive exit until settled, and obeys `on_parent_complete`.
    Finite {
        /// Whether this task continues or is cancelled when its parent completes normally.
        on_parent_complete: OnParentComplete,
    },
    /// Long-lived session daemon task that does not block non-interactive exit and stops on close.
    Daemon,
}

/// Read-only point-in-time snapshot of a registered background task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundTaskSnapshot {
    /// Unique task/operation identifier.
    pub task_id: String,
    /// Task category.
    pub kind: BackgroundTaskKind,
    /// Owning/parent agent identifier used for sibling debouncing.
    pub owner_agent_id: String,
    /// Optional parent operation identifier forming the supervision tree.
    pub parent_operation_id: Option<String>,
    /// Task lifecycle policy.
    pub policy: TaskLifecyclePolicy,
    /// Current lifecycle status.
    pub status: OperationStatus,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: u64,
}

/// Registration descriptor passed to [`BackgroundTaskRegistry::register`].
#[derive(Clone, Debug)]
pub struct BackgroundTaskRegistration {
    /// Unique task/operation identifier.
    pub task_id: String,
    /// Task category.
    pub kind: BackgroundTaskKind,
    /// Owning/parent agent identifier used for sibling debouncing.
    pub owner_agent_id: String,
    /// Optional parent operation identifier forming the supervision tree.
    pub parent_operation_id: Option<String>,
    /// Task lifecycle policy.
    pub policy: TaskLifecyclePolicy,
    /// Initial lifecycle status (`Creating` or `Running`).
    pub status: OperationStatus,
    /// Cooperative cancellation token triggered on task, parent, or session cancellation.
    pub cancel_token: CancellationToken,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: u64,
}

/// Error returned by [`BackgroundTaskRegistry`] operations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BackgroundRegistryError {
    /// Attempted to register a task ID that is already registered.
    #[error("background task already registered: {0}")]
    DuplicateTask(String),
    /// Attempted to transition an unregistered task ID.
    #[error("background task not found: {0}")]
    UnknownTask(String),
}

struct TaskRecord {
    kind: BackgroundTaskKind,
    owner_agent_id: String,
    parent_operation_id: Option<String>,
    policy: TaskLifecyclePolicy,
    status: OperationStatus,
    cancel_token: CancellationToken,
    created_at_ms: u64,
}

/// Thread-safe registry supervising active and terminal background tasks.
#[derive(Default)]
pub struct BackgroundTaskRegistry {
    records: Mutex<HashMap<String, TaskRecord>>,
    settled: Notify,
}

impl BackgroundTaskRegistry {
    /// Create an empty background task registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new supervised background task.
    pub fn register(&self, reg: BackgroundTaskRegistration) -> Result<(), BackgroundRegistryError> {
        let mut records = self.records.lock().expect("background registry lock");
        if records.contains_key(&reg.task_id) {
            return Err(BackgroundRegistryError::DuplicateTask(reg.task_id));
        }
        records.insert(
            reg.task_id,
            TaskRecord {
                kind: reg.kind,
                owner_agent_id: reg.owner_agent_id,
                parent_operation_id: reg.parent_operation_id,
                policy: reg.policy,
                status: reg.status,
                cancel_token: reg.cancel_token,
                created_at_ms: reg.created_at_ms,
            },
        );
        Ok(())
    }

    /// Transition a task's status monotonically (`Creating -> Running`, or active -> terminal).
    ///
    /// Terminal statuses are never overwritten (returns `Ok(false)`). When the last active
    /// `Finite` task transitions to a terminal status, all [`Self::wait_all_finite_settled`]
    /// waiters are notified.
    pub fn transition(
        &self,
        task_id: &str,
        next_status: OperationStatus,
    ) -> Result<bool, BackgroundRegistryError> {
        let (changed, all_finite_settled) = {
            let mut records = self.records.lock().expect("background registry lock");
            let Some(record) = records.get_mut(task_id) else {
                return Err(BackgroundRegistryError::UnknownTask(task_id.to_owned()));
            };
            if record.status.is_terminal() || record.status == next_status {
                return Ok(false);
            }
            if record.status == OperationStatus::Running && next_status == OperationStatus::Creating
            {
                return Ok(false);
            }
            record.status = next_status;
            let settled = next_status.is_terminal() && count_active_finite(&records) == 0;
            (true, settled)
        };
        if all_finite_settled {
            self.settled.notify_waiters();
        }
        Ok(changed)
    }

    /// Trigger cooperative cancellation for `task_id` if it is currently active.
    pub fn cancel_task(&self, task_id: &str) -> bool {
        let token = {
            let records = self.records.lock().expect("background registry lock");
            records
                .get(task_id)
                .filter(|record| record.status.is_active())
                .map(|record| record.cancel_token.clone())
        };
        if let Some(token) = token {
            token.cancel();
            true
        } else {
            false
        }
    }

    /// Cancel every active task in the descendant tree rooted at `parent_operation_id`.
    ///
    /// Traversal continues through intermediate tasks even if an intermediate node has
    /// already reached a terminal status. Returns the cancelled `task_id`s.
    pub fn cancel_descendants_of_operation(&self, parent_operation_id: &str) -> Vec<String> {
        let targets = {
            let records = self.records.lock().expect("background registry lock");
            collect_descendants_to_cancel(&records, &[parent_operation_id])
        };
        cancel_collected(targets)
    }

    /// Cancel active direct children of `parent_operation_id` configured with
    /// [`TaskLifecyclePolicy::Finite`] and [`OnParentComplete::Cancel`], plus all active
    /// descendants in their subtrees. Leaves [`OnParentComplete::Continue`] children untouched.
    pub fn cancel_for_parent_normal_completion(&self, parent_operation_id: &str) -> Vec<String> {
        let targets = {
            let records = self.records.lock().expect("background registry lock");
            let mut seeds = Vec::new();
            let mut direct_targets = Vec::new();
            for (task_id, record) in &*records {
                if record.parent_operation_id.as_deref() == Some(parent_operation_id)
                    && record.status.is_active()
                    && matches!(
                        record.policy,
                        TaskLifecyclePolicy::Finite {
                            on_parent_complete: OnParentComplete::Cancel
                        }
                    )
                {
                    seeds.push(task_id.as_str());
                    direct_targets.push((task_id.clone(), record.cancel_token.clone()));
                }
            }
            let mut all_targets = collect_descendants_to_cancel(&records, &seeds);
            all_targets.extend(direct_targets);
            dedup_targets(all_targets)
        };
        cancel_collected(targets)
    }

    /// Cancel every active background task (`Finite` and `Daemon`) for session close.
    pub fn cancel_all(&self) -> Vec<String> {
        let targets = {
            let records = self.records.lock().expect("background registry lock");
            let mut list: Vec<(String, CancellationToken)> = records
                .iter()
                .filter(|(_, record)| record.status.is_active())
                .map(|(task_id, record)| (task_id.clone(), record.cancel_token.clone()))
                .collect();
            list.sort_by(|a, b| a.0.cmp(&b.0));
            list
        };
        cancel_collected(targets)
    }

    /// Count active [`TaskLifecyclePolicy::Finite`] tasks owned by `owner_agent_id`,
    /// optionally excluding `exclude_task_id`.
    pub fn active_sibling_count(
        &self,
        owner_agent_id: &str,
        exclude_task_id: Option<&str>,
    ) -> usize {
        let records = self.records.lock().expect("background registry lock");
        records
            .iter()
            .filter(|(task_id, record)| {
                record.status.is_active()
                    && matches!(record.policy, TaskLifecyclePolicy::Finite { .. })
                    && record.owner_agent_id == owner_agent_id
                    && exclude_task_id != Some(task_id.as_str())
            })
            .count()
    }

    /// Count all currently active [`TaskLifecyclePolicy::Finite`] tasks in this registry.
    pub fn active_finite_count(&self) -> usize {
        let records = self.records.lock().expect("background registry lock");
        count_active_finite(&records)
    }

    /// Suspend until [`Self::active_finite_count`] drops to zero.
    pub async fn wait_all_finite_settled(&self) {
        loop {
            let notified = self.settled.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.active_finite_count() == 0 {
                return;
            }
            notified.await;
        }
    }

    /// Return a deterministic snapshot of all registered tasks sorted by `(created_at_ms, task_id)`.
    pub fn snapshot(&self) -> Vec<BackgroundTaskSnapshot> {
        let records = self.records.lock().expect("background registry lock");
        let mut out: Vec<BackgroundTaskSnapshot> = records
            .iter()
            .map(|(task_id, record)| BackgroundTaskSnapshot {
                task_id: task_id.clone(),
                kind: record.kind,
                owner_agent_id: record.owner_agent_id.clone(),
                parent_operation_id: record.parent_operation_id.clone(),
                policy: record.policy,
                status: record.status,
                created_at_ms: record.created_at_ms,
            })
            .collect();
        out.sort_by(|a, b| (a.created_at_ms, &a.task_id).cmp(&(b.created_at_ms, &b.task_id)));
        out
    }
}

fn count_active_finite(records: &HashMap<String, TaskRecord>) -> usize {
    records
        .values()
        .filter(|record| {
            record.status.is_active() && matches!(record.policy, TaskLifecyclePolicy::Finite { .. })
        })
        .count()
}

fn collect_descendants_to_cancel(
    records: &HashMap<String, TaskRecord>,
    roots: &[&str],
) -> Vec<(String, CancellationToken)> {
    let mut children_by_parent: HashMap<&str, Vec<&str>> = HashMap::new();
    for (task_id, record) in records {
        if let Some(parent) = record.parent_operation_id.as_deref() {
            children_by_parent
                .entry(parent)
                .or_default()
                .push(task_id.as_str());
        }
    }

    let mut visited: HashSet<&str> = roots.iter().copied().collect();
    let mut queue: VecDeque<&str> = roots.iter().copied().collect();
    let mut targets = Vec::new();

    while let Some(parent_id) = queue.pop_front() {
        let Some(children) = children_by_parent.get(parent_id) else {
            continue;
        };
        for &child_id in children {
            if visited.insert(child_id) {
                if let Some(record) = records.get(child_id)
                    && record.status.is_active()
                {
                    targets.push((child_id.to_owned(), record.cancel_token.clone()));
                }
                queue.push_back(child_id);
            }
        }
    }

    dedup_targets(targets)
}

fn dedup_targets(
    mut targets: Vec<(String, CancellationToken)>,
) -> Vec<(String, CancellationToken)> {
    targets.sort_by(|a, b| a.0.cmp(&b.0));
    let mut seen = HashSet::new();
    targets.retain(|(id, _)| seen.insert(id.clone()));
    targets
}

fn cancel_collected(targets: Vec<(String, CancellationToken)>) -> Vec<String> {
    let mut cancelled_ids = Vec::with_capacity(targets.len());
    for (task_id, token) in targets {
        token.cancel();
        cancelled_ids.push(task_id);
    }
    cancelled_ids
}

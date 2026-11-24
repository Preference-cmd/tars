//! Plane 2 unified observability broadcast bus (`SessionEvent` and `SessionBus`).

use super::background::BackgroundTaskKind;
use super::inbox::{EventDeliveryState, WakePolicy};
use super::types::{AgentInteraction, OperationStatus};
use std::sync::Arc;
use tokio::sync::broadcast;

/// Default bounded capacity of [`SessionBus`] broadcast channels.
pub const DEFAULT_SESSION_BUS_CAPACITY: usize = 256;

/// Non-blocking observability event published by a session coordinator.
#[derive(Clone, Debug)]
#[allow(clippy::large_enum_variant)]
pub enum SessionEvent {
    /// The session's overall lifecycle state changed (for example, `open`, `closing`, `closed`).
    SessionLifecycleChanged {
        /// Session identifier.
        session_id: String,
        /// Lifecycle label.
        lifecycle: String,
    },
    /// A new agent was registered in the session.
    AgentRegistered {
        /// Session identifier.
        session_id: String,
        /// Registered agent identifier.
        agent_id: String,
        /// Display name of the agent.
        name: String,
        /// Optional preset name from which the agent was created.
        preset_name: Option<String>,
        /// Agent interaction policy.
        interaction: AgentInteraction,
    },
    /// An operation transitioned into a new lifecycle status.
    OperationStateChanged {
        /// Session identifier.
        session_id: String,
        /// Operation identifier.
        operation_id: String,
        /// Target agent executing the operation.
        target_agent_id: String,
        /// Optional parent operation identifier.
        parent_operation_id: Option<String>,
        /// Current operation status.
        status: OperationStatus,
    },
    /// An inbox event row was created or changed delivery state.
    InboxEventChanged {
        /// Session identifier.
        session_id: String,
        /// Target agent identifier whose mailbox holds the event.
        target_agent_id: String,
        /// Monotonic SQLite `agent_events.event_id`.
        event_id: i64,
        /// Delivery state.
        state: EventDeliveryState,
        /// Wakeup policy.
        wake_policy: WakePolicy,
    },
    /// Advisory progress update emitted during an agent's active model or tool run.
    AgentProgress {
        /// Session identifier.
        session_id: String,
        /// Agent identifier producing the progress update.
        agent_id: String,
        /// Operation identifier executing the work.
        operation_id: String,
        /// Underlying work progress payload.
        progress: crate::session::WorkProgress,
    },
    /// A supervised background task registered or changed lifecycle status.
    BackgroundTaskChanged {
        /// Session identifier.
        session_id: String,
        /// Background task identifier.
        task_id: String,
        /// Background task category.
        kind: BackgroundTaskKind,
        /// Current task status.
        status: OperationStatus,
    },
    /// An agent reached its automatic idle-wakeup budget and downgraded remaining events.
    AutoWakeupLimitReached {
        /// Session identifier.
        session_id: String,
        /// Agent identifier whose wakeup budget was exhausted.
        agent_id: String,
        /// Configured maximum auto-wakeups per user interaction cycle.
        max_auto_wakeups: usize,
    },
}

/// Bounded broadcast hub for Plane 2 session observability events.
#[derive(Clone, Debug)]
pub struct SessionBus {
    sender: broadcast::Sender<Arc<SessionEvent>>,
}

impl Default for SessionBus {
    fn default() -> Self {
        Self::new(DEFAULT_SESSION_BUS_CAPACITY)
    }
}

impl SessionBus {
    /// Construct a new broadcast bus with capacity clamped to at least 1.
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self { sender }
    }

    /// Publish an event to all active subscribers, ignoring zero-receiver errors.
    pub fn publish(&self, event: SessionEvent) {
        let _ = self.sender.send(Arc::new(event));
    }

    /// Subscribe to future [`SessionEvent`] broadcasts from this bus.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<SessionEvent>> {
        self.sender.subscribe()
    }

    /// Current number of active broadcast receivers.
    pub fn receiver_count(&self) -> usize {
        self.sender.receiver_count()
    }
}

//! Unified three-plane event bus, causal mailbox, and background task registry.
//!
//! This module is intentionally decoupled from SQLite connections: persistence
//! commits live in `tars-core::store`, while `events` owns pure domain types,
//! coalescing, UTF-8 pagination, `<task-notification>` rendering, the in-memory
//! [`InboxDrainHandle`], the [`SessionBus`] broadcast channel, and the
//! [`BackgroundTaskRegistry`] supervision tree.

mod background;
mod bus;
mod inbox;
mod types;

#[cfg(test)]
mod tests;

pub use background::{
    BackgroundRegistryError, BackgroundTaskKind, BackgroundTaskRegistration,
    BackgroundTaskRegistry, BackgroundTaskSnapshot, ParseBackgroundTaskKindError,
    TaskLifecyclePolicy,
};
pub use bus::{DEFAULT_SESSION_BUS_CAPACITY, SessionBus, SessionEvent};
pub use inbox::{
    AgentNotificationPayload, CoalescedInbox, ConsumedInboxEvents, EventDeliveryState,
    EventPriority, EventSourceKind, InboxDrainHandle, InboxEvent, NewInboxEvent,
    ParseEventDeliveryStateError, ParseEventPriorityError, ParseEventSourceKindError,
    ParseWakePolicyError, Utf8PageSlice, WakePolicy, coalesce_inbox_events,
    render_task_notifications, slice_utf8_page, truncate_utf8_boundary,
};
pub use types::{
    AgentInteraction, DEFAULT_DELEGATION_MAX_DEPTH, DEFAULT_MAX_ACTIVE_CHILD_OPS_PER_SESSION,
    DEFAULT_MAX_ACTIVE_OPS_PER_AGENT, DEFAULT_MAX_AUTO_WAKEUPS,
    DEFAULT_NOTIFICATION_INLINE_MAX_BYTES, DEFAULT_SIBLING_DEBOUNCE_MS, MAX_OPERATION_INPUT_BYTES,
    MAX_PROMPT_BYTES, MAX_READ_AGENT_PAGE_BYTES, OnParentComplete, OperationExecutionMode,
    OperationStatus, ParseAgentInteractionError, ParseOnParentCompleteError,
    ParseOperationExecutionModeError, ParseOperationStatusError, TASK_NOTIFICATION_SOURCE,
};

//! Plane 1 causal mailbox types, coalescing, UTF-8 pagination, XML rendering,
//! and the in-memory `InboxDrainHandle` used by `ContextPreparer`.

use super::types::{MAX_READ_AGENT_PAGE_BYTES, OperationStatus};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;

/// Error returned when parsing an unrecognized [`EventPriority`] string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown event priority: '{0}' (expected 'urgent', 'normal', or 'low')")]
pub struct ParseEventPriorityError(pub String);

/// Delivery priority for [`InboxEvent`] ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventPriority {
    /// Delivered before normal and low priority events.
    Urgent,
    /// Default priority for completed background operations and normal notifications.
    Normal,
    /// Best-effort advisory notifications delivered after urgent and normal events.
    Low,
}

impl EventPriority {
    /// Canonical `snake_case` database and wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Urgent => "urgent",
            Self::Normal => "normal",
            Self::Low => "low",
        }
    }

    /// Numeric sort rank where smaller values have higher delivery priority (`Urgent = 0`).
    pub const fn rank(self) -> u8 {
        match self {
            Self::Urgent => 0,
            Self::Normal => 1,
            Self::Low => 2,
        }
    }
}

impl fmt::Display for EventPriority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventPriority {
    type Err = ParseEventPriorityError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "urgent" => Ok(Self::Urgent),
            "normal" => Ok(Self::Normal),
            "low" => Ok(Self::Low),
            other => Err(ParseEventPriorityError(other.to_owned())),
        }
    }
}

/// Error returned when parsing an unrecognized [`EventSourceKind`] string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown event source kind: '{0}' (expected 'agent_operation', 'background_command', 'timer', or 'user_steer')"
)]
pub struct ParseEventSourceKindError(pub String);

/// Producer category recorded in `agent_events.source_kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSourceKind {
    /// Result or lifecycle notification from a delegated agent operation.
    AgentOperation,
    /// Completion notification from a background shell command.
    BackgroundCommand,
    /// Notification fired by a scheduled timer.
    Timer,
    /// Explicit steering input queued for an active or idle agent.
    UserSteer,
}

impl EventSourceKind {
    /// Canonical `snake_case` database and wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AgentOperation => "agent_operation",
            Self::BackgroundCommand => "background_command",
            Self::Timer => "timer",
            Self::UserSteer => "user_steer",
        }
    }
}

impl fmt::Display for EventSourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventSourceKind {
    type Err = ParseEventSourceKindError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "agent_operation" => Ok(Self::AgentOperation),
            "background_command" => Ok(Self::BackgroundCommand),
            "timer" => Ok(Self::Timer),
            "user_steer" => Ok(Self::UserSteer),
            other => Err(ParseEventSourceKindError(other.to_owned())),
        }
    }
}

/// Error returned when parsing an unrecognized [`WakePolicy`] string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown wake policy: '{0}' (expected 'wake_if_idle', 'deliver_on_next_turn', or 'suppress_wake')"
)]
pub struct ParseWakePolicyError(pub String);

/// Policy controlling whether an [`InboxEvent`] may trigger an automatic idle wakeup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakePolicy {
    /// May automatically wake an idle target agent if all admission guards pass.
    WakeIfIdle,
    /// Wait for the target agent's next running turn without auto-waking.
    DeliverOnNextTurn,
    /// Never trigger automatic wakeup (for example, sync operations or cancelled trees).
    SuppressWake,
}

impl WakePolicy {
    /// Canonical `snake_case` database and wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WakeIfIdle => "wake_if_idle",
            Self::DeliverOnNextTurn => "deliver_on_next_turn",
            Self::SuppressWake => "suppress_wake",
        }
    }
}

impl fmt::Display for WakePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WakePolicy {
    type Err = ParseWakePolicyError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "wake_if_idle" => Ok(Self::WakeIfIdle),
            "deliver_on_next_turn" => Ok(Self::DeliverOnNextTurn),
            "suppress_wake" => Ok(Self::SuppressWake),
            other => Err(ParseWakePolicyError(other.to_owned())),
        }
    }
}

/// Error returned when parsing an unrecognized [`EventDeliveryState`] string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "unknown event delivery state: '{0}' (expected 'pending', 'claimed', 'delivered', or 'dropped')"
)]
pub struct ParseEventDeliveryStateError(pub String);

/// Persistence state of an `agent_events` row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventDeliveryState {
    /// Queued and not yet claimed or delivered.
    Pending,
    /// Claimed for an in-flight wakeup operation, awaiting target commit.
    Claimed,
    /// Atomically committed into the target agent's Context.
    Delivered,
    /// Superseded by a newer event sharing the same `coalesce_key`.
    Dropped,
}

impl EventDeliveryState {
    /// Canonical `snake_case` database and wire representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Claimed => "claimed",
            Self::Delivered => "delivered",
            Self::Dropped => "dropped",
        }
    }
}

impl fmt::Display for EventDeliveryState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for EventDeliveryState {
    type Err = ParseEventDeliveryStateError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "claimed" => Ok(Self::Claimed),
            "delivered" => Ok(Self::Delivered),
            "dropped" => Ok(Self::Dropped),
            other => Err(ParseEventDeliveryStateError(other.to_owned())),
        }
    }
}

/// Domain mirror of an `agent_events` mailbox row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxEvent {
    /// Monotonic SQLite row identifier (`agent_events.event_id`).
    pub event_id: i64,
    /// Owning session identifier.
    pub session_id: String,
    /// Target agent identifier whose mailbox owns this event.
    pub target_agent_id: String,
    /// Delivery priority.
    pub priority: EventPriority,
    /// Producer category.
    pub source_kind: EventSourceKind,
    /// Source entity identifier (such as `operation_id`).
    pub source_id: String,
    /// Optional coalescing key; newer events with the same key supersede older ones.
    pub coalesce_key: Option<String>,
    /// Idle-wakeup policy.
    pub wake_policy: WakePolicy,
    /// JSON-encoded event payload.
    pub payload_json: String,
    /// Current delivery state.
    pub state: EventDeliveryState,
    /// Optional operation ID that claimed this event for wakeup.
    pub claimed_op_id: Option<String>,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: u64,
    /// Delivery timestamp in Unix milliseconds, if delivered.
    pub delivered_at_ms: Option<u64>,
}

/// Pre-insert event descriptor submitted to transactional outbox commits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewInboxEvent {
    /// Target agent identifier whose mailbox will receive the event.
    pub target_agent_id: String,
    /// Delivery priority.
    pub priority: EventPriority,
    /// Producer category.
    pub source_kind: EventSourceKind,
    /// Source entity identifier.
    pub source_id: String,
    /// Optional coalescing key.
    pub coalesce_key: Option<String>,
    /// Idle-wakeup policy.
    pub wake_policy: WakePolicy,
    /// JSON-encoded event payload.
    pub payload_json: String,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: u64,
}

/// Structured payload carried by [`EventSourceKind::AgentOperation`] inbox events.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentNotificationPayload {
    /// Identifier of the finished operation.
    pub operation_id: String,
    /// Identifier of the agent that executed the operation.
    pub agent_id: String,
    /// Display name of the agent that executed the operation.
    pub agent_name: String,
    /// Terminal operation status.
    pub status: OperationStatus,
    /// Inline output summary text.
    pub summary_text: String,
    /// Total UTF-8 byte length of the full archived `operations.result_text`.
    pub total_bytes: usize,
    /// Whether `summary_text` was truncated relative to `total_bytes`.
    pub truncated: bool,
    /// Optional error message when the operation did not complete normally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Result of coalescing queued [`InboxEvent`] items by `coalesce_key`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoalescedInbox {
    /// Kept events ordered by `(priority.rank(), event_id)`.
    pub kept: Vec<InboxEvent>,
    /// Superseded `event_id`s that were dropped during coalescing, sorted ascending.
    pub dropped_ids: Vec<i64>,
}

/// Collapse events sharing a `coalesce_key` so only the highest `event_id` survives,
/// and sort kept events by `(priority.rank(), event_id)`.
///
/// Events with `coalesce_key: None` are never dropped.
pub fn coalesce_inbox_events(events: Vec<InboxEvent>) -> CoalescedInbox {
    let mut unkeyed = Vec::new();
    let mut keyed: HashMap<String, InboxEvent> = HashMap::new();
    let mut dropped_ids = Vec::new();

    for event in events {
        match event.coalesce_key.clone() {
            None => unkeyed.push(event),
            Some(key) => match keyed.get(&key) {
                None => {
                    keyed.insert(key, event);
                }
                Some(existing) if event.event_id > existing.event_id => {
                    let superseded = keyed.insert(key, event).expect("checked existing key");
                    dropped_ids.push(superseded.event_id);
                }
                Some(_) => {
                    dropped_ids.push(event.event_id);
                }
            },
        }
    }

    let mut kept = Vec::with_capacity(unkeyed.len() + keyed.len());
    kept.extend(unkeyed);
    kept.extend(keyed.into_values());
    kept.sort_by_key(|event| (event.priority.rank(), event.event_id));
    dropped_ids.sort_unstable();
    dropped_ids.dedup();

    CoalescedInbox { kept, dropped_ids }
}

/// Truncate `text` to at most `max_bytes` bytes on a valid UTF-8 character boundary.
///
/// Returns `(prefix, was_truncated)`.
pub fn truncate_utf8_boundary(text: &str, max_bytes: usize) -> (&str, bool) {
    if text.len() <= max_bytes {
        return (text, false);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], true)
}

/// UTF-8 safe page slice returned by [`slice_utf8_page`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Utf8PageSlice {
    /// Extracted UTF-8 slice content.
    pub content: String,
    /// Actual aligned start byte offset used for this page.
    pub offset_bytes: usize,
    /// Next aligned byte offset to pass when reading the following page.
    pub next_offset_bytes: usize,
    /// Total UTF-8 byte length of the full source text.
    pub total_bytes: usize,
    /// Whether unread bytes remain after `next_offset_bytes`.
    pub has_more: bool,
}

/// Slice a UTF-8 page from `text` starting at `offset_bytes` up to `limit_bytes`.
///
/// Behavior:
/// - `limit_bytes` is clamped to [`MAX_READ_AGENT_PAGE_BYTES`] (`64 KiB`).
/// - If `offset_bytes >= text.len()`, returns an empty slice at `text.len()` with `has_more = false`.
/// - If `offset_bytes` falls inside a multi-byte UTF-8 codepoint, `start` advances to the next
///   character boundary so partial codepoints are never duplicated or panicked on.
/// - If `limit_bytes == 0`, returns an empty slice at the aligned `start` offset.
/// - If `limit_bytes >= 1` and `start < text.len()` but the next character is wider than
///   `effective_limit`, that single character is included so pagination always makes forward progress.
pub fn slice_utf8_page(text: &str, offset_bytes: usize, limit_bytes: usize) -> Utf8PageSlice {
    let total_bytes = text.len();
    let effective_limit = limit_bytes.min(MAX_READ_AGENT_PAGE_BYTES);

    if offset_bytes >= total_bytes {
        return Utf8PageSlice {
            content: String::new(),
            offset_bytes: total_bytes,
            next_offset_bytes: total_bytes,
            total_bytes,
            has_more: false,
        };
    }

    let mut start = offset_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }

    if start >= total_bytes {
        return Utf8PageSlice {
            content: String::new(),
            offset_bytes: total_bytes,
            next_offset_bytes: total_bytes,
            total_bytes,
            has_more: false,
        };
    }

    if effective_limit == 0 {
        return Utf8PageSlice {
            content: String::new(),
            offset_bytes: start,
            next_offset_bytes: start,
            total_bytes,
            has_more: true,
        };
    }

    let max_end = start.saturating_add(effective_limit).min(total_bytes);
    let mut end = max_end;
    while end > start && !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == start {
        let ch_len = text[start..].chars().next().map_or(0, char::len_utf8);
        end = start + ch_len;
    }

    Utf8PageSlice {
        content: text[start..end].to_owned(),
        offset_bytes: start,
        next_offset_bytes: end,
        total_bytes,
        has_more: end < total_bytes,
    }
}

fn escape_xml_attr(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn neutralize_closing_tag(text: &str) -> String {
    const NEEDLE: &str = "</task-notification";
    let lower = text.to_ascii_lowercase();
    if !lower.contains(NEEDLE) {
        return text.to_owned();
    }
    // Lowercase ASCII replacement operates over ASCII-only needle bytes, preserving UTF-8 indices
    // only when non-ASCII chars don't change byte length; to stay 100% UTF-8 safe even if a
    // char's lowercase changed length, scan case-insensitively on ASCII bytes in `text`.
    let bytes = text.as_bytes();
    let needle_bytes = NEEDLE.as_bytes();
    let mut out = String::with_capacity(text.len() + 8);
    let mut i = 0;
    let mut last = 0;
    while i + needle_bytes.len() <= bytes.len() {
        if bytes[i..i + needle_bytes.len()].eq_ignore_ascii_case(needle_bytes) {
            out.push_str(&text[last..i]);
            out.push_str("&lt;");
            out.push_str(&text[i + 1..i + needle_bytes.len()]);
            i += needle_bytes.len();
            last = i;
        } else {
            i += 1;
        }
    }
    out.push_str(&text[last..]);
    out
}

/// Render a sequence of [`InboxEvent`] items into XML `<task-notification>` envelopes
/// separated by `\n`, enforcing `inline_max_bytes` per notification body on a UTF-8 boundary.
pub fn render_task_notifications(events: &[InboxEvent], inline_max_bytes: usize) -> String {
    let mut rendered = Vec::with_capacity(events.len());
    for event in events {
        rendered.push(render_single_notification(event, inline_max_bytes));
    }
    rendered.join("\n")
}

fn render_single_notification(event: &InboxEvent, inline_max_bytes: usize) -> String {
    if event.source_kind == EventSourceKind::AgentOperation
        && let Ok(payload) = serde_json::from_str::<AgentNotificationPayload>(&event.payload_json)
    {
        let (summary_prefix, inline_summary_truncated) =
            truncate_utf8_boundary(&payload.summary_text, inline_max_bytes);
        let mut combined = summary_prefix.to_owned();
        if let Some(error) = &payload.error {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str("error: ");
            combined.push_str(error);
        }
        let (bounded_body, combined_truncated) =
            truncate_utf8_boundary(&combined, inline_max_bytes);
        let mut body = neutralize_closing_tag(bounded_body);
        if inline_summary_truncated || payload.truncated {
            let next_offset = summary_prefix.len().min(payload.total_bytes);
            body.push_str(&format!(
                "\n[output truncated; call read_agent(operation_id=\"{}\", offset_bytes={}, limit_bytes={MAX_READ_AGENT_PAGE_BYTES}) to read the full archived output]",
                escape_xml_attr(&payload.operation_id),
                next_offset,
            ));
        } else if combined_truncated {
            body.push_str("\n[error text truncated]");
        }
        return format!(
            "<task-notification source=\"{}\" id=\"{}\" operation_id=\"{}\" agent_id=\"{}\" agent_name=\"{}\" status=\"{}\" priority=\"{}\">\n{}\n</task-notification>",
            event.source_kind.as_str(),
            event.event_id,
            escape_xml_attr(&payload.operation_id),
            escape_xml_attr(&payload.agent_id),
            escape_xml_attr(&payload.agent_name),
            payload.status.as_str(),
            event.priority.as_str(),
            body,
        );
    }

    let (prefix, truncated) = truncate_utf8_boundary(&event.payload_json, inline_max_bytes);
    let mut body = neutralize_closing_tag(prefix);
    if truncated {
        body.push_str("\n[payload truncated]");
    }
    format!(
        "<task-notification source=\"{}\" id=\"{}\" source_id=\"{}\" priority=\"{}\">\n{}\n</task-notification>",
        event.source_kind.as_str(),
        event.event_id,
        escape_xml_attr(&event.source_id),
        event.priority.as_str(),
        body,
    )
}

/// Snapshot of event IDs consumed by [`InboxDrainHandle::drain_notification`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConsumedInboxEvents {
    /// Event IDs rendered and delivered into Context, awaiting atomic SQLite commit.
    pub delivered_ids: Vec<i64>,
    /// Superseded event IDs dropped during coalescing, awaiting atomic SQLite commit.
    pub dropped_ids: Vec<i64>,
}

/// Detailed drain result allowing rollback back into the mailbox if submission fails
/// before the target turn starts.
#[derive(Clone, Debug)]
pub struct DrainedNotification {
    /// Rendered `<task-notification>` envelope block.
    pub rendered: String,
    /// Original uncoalesced events drained in this batch.
    pub drained_events: Vec<InboxEvent>,
    /// Consumed event ID lists (`delivered_ids` / `dropped_ids`).
    pub consumed: ConsumedInboxEvents,
}

#[derive(Default)]
struct InboxDrainState {
    queued: Vec<InboxEvent>,
    seen_ids: HashSet<i64>,
    delivered_ids: Vec<i64>,
    dropped_ids: Vec<i64>,
}

/// Thread-safe in-memory per-work mailbox handle used by `ContextPreparer`.
///
/// `InboxDrainHandle` never touches SQLite directly: rows stay `pending` in SQLite until
/// the parent Context commit transaction atomically marks `delivered_ids` as `delivered`
/// and `dropped_ids` as `dropped`.
pub struct InboxDrainHandle {
    state: Mutex<InboxDrainState>,
    inline_max_bytes: usize,
}

impl InboxDrainHandle {
    /// Create an empty drain handle enforcing `inline_max_bytes` per notification.
    pub fn new(inline_max_bytes: usize) -> Self {
        Self {
            state: Mutex::new(InboxDrainState::default()),
            inline_max_bytes,
        }
    }

    /// Create a drain handle pre-populated with initial pending events.
    pub fn with_initial(events: Vec<InboxEvent>, inline_max_bytes: usize) -> Self {
        let handle = Self::new(inline_max_bytes);
        handle.enqueue_many(events);
        handle
    }

    /// Enqueue a single event, ignoring duplicates already queued, delivered, or dropped.
    pub fn enqueue(&self, event: InboxEvent) {
        let mut state = self.state.lock().expect("inbox drain lock");
        if state.seen_ids.insert(event.event_id) {
            state.queued.push(event);
        }
    }

    /// Enqueue multiple events, ignoring duplicates already queued, delivered, or dropped.
    pub fn enqueue_many(&self, events: impl IntoIterator<Item = InboxEvent>) {
        let mut state = self.state.lock().expect("inbox drain lock");
        for event in events {
            if state.seen_ids.insert(event.event_id) {
                state.queued.push(event);
            }
        }
    }

    /// Drain and coalesce currently queued events, recording consumed IDs and returning
    /// the rendered `<task-notification>` envelope block if any events were queued.
    pub fn drain_notification(&self) -> Option<String> {
        self.drain_notification_detailed().map(|d| d.rendered)
    }

    /// Drain and coalesce currently queued events, returning both the rendered text
    /// and the drained events/IDs so a caller can call [`Self::restore_drained`] if
    /// turn submission fails before the work begins.
    pub fn drain_notification_detailed(&self) -> Option<DrainedNotification> {
        let (kept, original_events, dropped_ids) = {
            let mut state = self.state.lock().expect("inbox drain lock");
            if state.queued.is_empty() {
                return None;
            }
            let queued = std::mem::take(&mut state.queued);
            let original_events = queued.clone();
            let coalesced = coalesce_inbox_events(queued);
            for event in &coalesced.kept {
                state.delivered_ids.push(event.event_id);
            }
            state
                .dropped_ids
                .extend(coalesced.dropped_ids.iter().copied());
            (coalesced.kept, original_events, coalesced.dropped_ids)
        };
        if kept.is_empty() {
            return None;
        }
        let consumed = ConsumedInboxEvents {
            delivered_ids: kept.iter().map(|e| e.event_id).collect(),
            dropped_ids,
        };
        Some(DrainedNotification {
            rendered: render_task_notifications(&kept, self.inline_max_bytes),
            drained_events: original_events,
            consumed,
        })
    }

    /// Restore previously drained events back into the mailbox when turn submission fails
    /// before entering execution.
    pub fn restore_drained(&self, drained: &DrainedNotification) {
        let mut state = self.state.lock().expect("inbox drain lock");
        let delivered_set: HashSet<i64> = drained.consumed.delivered_ids.iter().copied().collect();
        let dropped_set: HashSet<i64> = drained.consumed.dropped_ids.iter().copied().collect();
        state.delivered_ids.retain(|id| !delivered_set.contains(id));
        state.dropped_ids.retain(|id| !dropped_set.contains(id));

        let existing: HashSet<i64> = state.queued.iter().map(|e| e.event_id).collect();
        let mut restored = Vec::with_capacity(drained.drained_events.len() + state.queued.len());
        for ev in &drained.drained_events {
            if !existing.contains(&ev.event_id) {
                restored.push(ev.clone());
            }
        }
        restored.append(&mut state.queued);
        state.queued = restored;
    }

    /// Return a non-destructive snapshot of event IDs consumed so far.
    pub fn consumed_snapshot(&self) -> ConsumedInboxEvents {
        let state = self.state.lock().expect("inbox drain lock");
        ConsumedInboxEvents {
            delivered_ids: state.delivered_ids.clone(),
            dropped_ids: state.dropped_ids.clone(),
        }
    }

    /// Take and clear the accumulated `(delivered_ids, dropped_ids)` for SQLite outbox commit,
    /// while retaining seen IDs so duplicate re-enqueues remain ignored.
    pub fn take_consumed(&self) -> ConsumedInboxEvents {
        let mut state = self.state.lock().expect("inbox drain lock");
        ConsumedInboxEvents {
            delivered_ids: std::mem::take(&mut state.delivered_ids),
            dropped_ids: std::mem::take(&mut state.dropped_ids),
        }
    }

    /// Number of currently queued events not yet drained.
    pub fn pending_count(&self) -> usize {
        self.state.lock().expect("inbox drain lock").queued.len()
    }
}

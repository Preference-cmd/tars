use causa::kernel::Context;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::UNIX_EPOCH;
use uuid::Uuid;

use super::StoreError;
use crate::definition::SessionDefinition;

pub(crate) const RECORD_FORMAT: &str = "tars-session-v2.2";
/// Check the format before decoding the fixed permission-bearing definition.
pub(super) fn decode(bytes: &[u8], identity: &str) -> Result<SavedSessionRecord, StoreError> {
    #[derive(Deserialize)]
    struct Format {
        format: String,
    }
    let format: Format = serde_json::from_slice(bytes)?;
    if format.format != RECORD_FORMAT {
        return Err(StoreError::UnsupportedFormat(format.format));
    }
    let record: SavedSessionRecord = crate::material::decode_stored(bytes)?;
    record.validate(identity)?;
    Ok(record)
}

/// Full wire record; Context's deserializer rejects duplicate block IDs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SavedSessionRecord {
    pub(crate) format: String,
    pub(crate) conversation_id: String,
    pub(crate) revision: u64,
    pub(crate) last_commit_id: String,
    pub(crate) definition: SessionDefinition,
    pub(crate) context: Context,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) title: Option<String>,
    pub(crate) updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) permission_mode: Option<crate::permission::PermissionMode>,
    /// Running automatic-compaction switch when it differs from the definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) auto_compact: Option<bool>,
    /// Running trigger percentage when it differs from the definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) compact_threshold: Option<u8>,
    /// Replaced conversation segments, in creation order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) history: Vec<crate::compact::HistoryRecordEntry>,
    /// Cumulative session model usage, omitted when zero or untracked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) usage: Option<WireSessionUsage>,
}
impl PartialEq for SavedSessionRecord {
    fn eq(&self, other: &Self) -> bool {
        self.format == other.format
            && self.conversation_id == other.conversation_id
            && self.revision == other.revision
            && self.last_commit_id == other.last_commit_id
            && self.definition == other.definition
            && self.context.blocks() == other.context.blocks()
            && self.title == other.title
            && self.updated_at_ms == other.updated_at_ms
            && self.permission_mode == other.permission_mode
            && self.auto_compact == other.auto_compact
            && self.compact_threshold == other.compact_threshold
            && self.history == other.history
            && self.usage == other.usage
    }
}
impl SavedSessionRecord {
    /// Return the session's cumulative usage statistics, or zero totals if omitted.
    pub(crate) fn session_usage(&self) -> crate::usage::SessionUsage {
        self.usage
            .as_ref()
            .map(WireSessionUsage::to_session_usage)
            .unwrap_or_default()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        conversation_id: String,
        last_commit_id: String,
        expected_revision: Option<u64>,
        definition: SessionDefinition,
        context: Context,
        current_mode: Option<crate::permission::PermissionMode>,
        title: Option<String>,
        updated_at_ms: u64,
    ) -> Result<Self, StoreError> {
        Self::new_with_compaction(
            conversation_id,
            last_commit_id,
            expected_revision,
            definition,
            context,
            current_mode,
            None,
            None,
            Vec::new(),
            None,
            title,
            updated_at_ms,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new_with_compaction(
        conversation_id: String,
        last_commit_id: String,
        expected_revision: Option<u64>,
        definition: SessionDefinition,
        context: Context,
        current_mode: Option<crate::permission::PermissionMode>,
        auto_compact: Option<bool>,
        compact_threshold: Option<u8>,
        history: Vec<crate::compact::HistoryRecordEntry>,
        usage: Option<crate::usage::SessionUsage>,
        title: Option<String>,
        updated_at_ms: u64,
    ) -> Result<Self, StoreError> {
        let initial_mode = definition.initial_permission_mode();
        let permission_mode = current_mode.filter(|&mode| mode != initial_mode);
        let usage = usage
            .map(|u| WireSessionUsage::from_session_usage(&u))
            .filter(|u| !u.is_empty());
        let record = Self {
            format: RECORD_FORMAT.into(),
            conversation_id,
            revision: successor(expected_revision)?,
            last_commit_id,
            definition,
            context,
            title,
            updated_at_ms,
            permission_mode,
            auto_compact,
            compact_threshold,
            history,
            usage,
        };
        record.validate(&record.conversation_id)?;
        Ok(record)
    }
    pub(super) fn validate(&self, identity: &str) -> Result<(), StoreError> {
        if self.format != RECORD_FORMAT {
            return Err(StoreError::UnsupportedFormat(self.format.clone()));
        }
        canonical_uuid(&self.conversation_id)?;
        canonical_uuid(&self.last_commit_id)?;
        if self.conversation_id != identity {
            return Err(StoreError::IdentityMismatch {
                expected: identity.into(),
                actual: self.conversation_id.clone(),
            });
        }
        if self.revision == 0 {
            return Err(StoreError::InvalidRevision);
        }
        if let Some(usage) = &self.usage {
            usage.validate()?;
        }
        self.definition
            .validate()
            .map_err(|e| StoreError::Definition(Arc::new(e)))?;
        if !(crate::definition::MIN_COMPACT_THRESHOLD..=crate::definition::MAX_COMPACT_THRESHOLD)
            .contains(&self.definition.compact_threshold)
        {
            return Err(StoreError::Definition(Arc::new(
                crate::definition::DefinitionError::InvalidCompactThreshold,
            )));
        }
        if let Some(threshold) = self.compact_threshold
            && !(crate::definition::MIN_COMPACT_THRESHOLD
                ..=crate::definition::MAX_COMPACT_THRESHOLD)
                .contains(&threshold)
        {
            return Err(StoreError::Definition(Arc::new(
                crate::definition::DefinitionError::InvalidCompactThreshold,
            )));
        }
        crate::compact::history::validate_index(&self.history)?;
        crate::material::validate_references(&self.context, None)
            .map_err(|e| StoreError::References(Arc::new(e)))
    }
}

/// Longest session title, in characters.
const MAX_TITLE_CHARS: usize = 80;

/// Title and last-activity time of the last stored record of a session.
///
/// Session state keeps only stored values: they change when a record is
/// stored, never when a commit is merely prepared.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RecordMetadata {
    /// Stored title, if any.
    pub(crate) title: Option<String>,
    /// The stored Context had a user block, so the title is final, even
    /// when there is none.
    pub(crate) title_settled: bool,
    /// Stored last-activity time in Unix milliseconds.
    pub(crate) updated_at_ms: u64,
}

impl RecordMetadata {
    /// A session with no stored record yet; its last activity is its
    /// creation time when the id carries one.
    pub(crate) fn unsaved(conversation_id: &str) -> Self {
        Self {
            title: None,
            title_settled: false,
            updated_at_ms: crate::time::created_at_from_id(conversation_id).unwrap_or(0),
        }
    }

    /// Values of a stored record.
    pub(crate) fn of(record: &SavedSessionRecord) -> Self {
        Self {
            title: record.title.clone(),
            title_settled: record.title.is_some() || first_user_parts(&record.context).is_some(),
            updated_at_ms: record.updated_at_ms,
        }
    }

    /// Title and last-activity time for the next record over `context`.
    /// `floor` is the time of a prepared commit the next one replaces, so
    /// the time never goes backwards even if that commit was stored.
    pub(crate) fn next(&self, context: &Context, floor: u64) -> (Option<String>, u64) {
        let title = if self.title_settled {
            self.title.clone()
        } else {
            first_user_parts(context).and_then(title_from_parts)
        };
        let updated_at_ms = crate::time::current_time_ms()
            .max(self.updated_at_ms)
            .max(floor);
        (title, updated_at_ms)
    }
}

/// Content parts of the first user message block, if there is one.
fn first_user_parts(context: &Context) -> Option<&[causa::kernel::ContentPart]> {
    context
        .blocks()
        .iter()
        .find_map(|block| match block.content() {
            causa::kernel::BlockContent::Message {
                role: causa::kernel::Role::User,
                parts,
            } => Some(parts.as_slice()),
            _ => None,
        })
}

/// The first non-blank line of the message text, with whitespace runs
/// collapsed to one space, cut to [`MAX_TITLE_CHARS`] characters.
fn title_from_parts(parts: &[causa::kernel::ContentPart]) -> Option<String> {
    let mut text = String::new();
    for part in parts {
        if let causa::kernel::ContentPart::Text(payload) = part {
            text.push_str(&payload.0);
        }
    }
    text.lines().find_map(|line| {
        let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        (!collapsed.is_empty()).then(|| collapsed.chars().take(MAX_TITLE_CHARS).collect())
    })
}

/// Fixed before I/O and reused exactly on every retry/confirmation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PreparedCommit {
    pub(crate) expected_revision: Option<u64>,
    pub(crate) record: SavedSessionRecord,
}
impl PreparedCommit {
    pub(crate) fn new(
        expected_revision: Option<u64>,
        record: SavedSessionRecord,
    ) -> Result<Self, StoreError> {
        if record.revision != successor(expected_revision)? {
            return Err(StoreError::InvalidRevision);
        }
        record.validate(&record.conversation_id)?;
        Ok(Self {
            expected_revision,
            record,
        })
    }
    pub(super) fn validate(&self, identity: &str) -> Result<(), StoreError> {
        if self.record.revision != successor(self.expected_revision)? {
            return Err(StoreError::InvalidRevision);
        }
        self.record.validate(identity)
    }
}
pub(super) fn canonical_uuid(id: &str) -> Result<(), StoreError> {
    match Uuid::parse_str(id) {
        Ok(uuid) if uuid.hyphenated().to_string() == id => Ok(()),
        _ => Err(StoreError::InvalidIdentity(id.into())),
    }
}
fn successor(revision: Option<u64>) -> Result<u64, StoreError> {
    revision
        .unwrap_or(0)
        .checked_add(1)
        .filter(|_| revision != Some(0))
        .ok_or(StoreError::InvalidRevision)
}

/// Wire representation of a single model call's raw usage figures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireCallUsage {
    /// Uncached input tokens.
    pub(crate) input_tokens: u64,
    /// Cached tokens read from prompt cache, or `None` if unreported (serialized as `null`).
    pub(crate) cache_read_tokens: Option<u64>,
    /// Tokens written to prompt cache, or `None` if unreported (serialized as `null`).
    pub(crate) cache_write_tokens: Option<u64>,
    /// Output tokens produced by the model.
    pub(crate) output_tokens: u64,
    /// Reasoning tokens, or `None` if unreported (serialized as `null`).
    pub(crate) reasoning_tokens: Option<u64>,
}

impl WireCallUsage {
    pub(crate) fn from_call_usage(usage: &crate::usage::CallUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            output_tokens: usage.output_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        }
    }

    pub(crate) fn to_call_usage(self) -> crate::usage::CallUsage {
        crate::usage::CallUsage {
            input_tokens: self.input_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            output_tokens: self.output_tokens,
            reasoning_tokens: self.reasoning_tokens,
        }
    }
}

/// Wire representation of a model call purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WireUsagePurpose {
    /// Regular conversation turn or tool round.
    Conversation,
    /// Context compaction summary generation.
    Compaction,
}

impl WireUsagePurpose {
    pub(crate) fn from_purpose(purpose: crate::usage::UsagePurpose) -> Self {
        match purpose {
            crate::usage::UsagePurpose::Conversation => Self::Conversation,
            crate::usage::UsagePurpose::Compaction => Self::Compaction,
        }
    }

    pub(crate) fn to_purpose(self) -> crate::usage::UsagePurpose {
        match self {
            Self::Conversation => crate::usage::UsagePurpose::Conversation,
            Self::Compaction => crate::usage::UsagePurpose::Compaction,
        }
    }
}

/// Wire representation of facts about the most recent successful model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireLastCall {
    /// Purpose of the call (`conversation` or `compaction`).
    pub(crate) purpose: WireUsagePurpose,
    /// Raw usage reported by the provider, or `None` (serialized as `null`) if unreported.
    pub(crate) usage: Option<WireCallUsage>,
    /// Wall-clock timestamp when the call completed, as integer Unix milliseconds.
    pub(crate) timestamp_ms: u64,
}

impl WireLastCall {
    pub(crate) fn from_last_call(last_call: &crate::usage::LastCall) -> Self {
        Self {
            purpose: WireUsagePurpose::from_purpose(last_call.purpose),
            usage: last_call.usage.as_ref().map(WireCallUsage::from_call_usage),
            timestamp_ms: crate::usage::unix_millis(last_call.timestamp),
        }
    }

    pub(crate) fn to_last_call(self) -> crate::usage::LastCall {
        crate::usage::LastCall {
            purpose: self.purpose.to_purpose(),
            usage: self.usage.map(WireCallUsage::to_call_usage),
            // `validate` rejects times this platform cannot represent.
            timestamp: crate::usage::from_unix_millis(self.timestamp_ms).unwrap_or(UNIX_EPOCH),
        }
    }
}

/// Wire representation of call counts categorized by outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireCallCounts {
    /// Number of successful calls.
    pub(crate) succeeded: u64,
    /// Number of failed calls.
    pub(crate) failed: u64,
    /// Number of successful calls that did not report usage.
    pub(crate) unreported: u64,
}

impl WireCallCounts {
    pub(crate) fn is_empty(&self) -> bool {
        self.succeeded == 0 && self.failed == 0 && self.unreported == 0
    }

    pub(crate) fn from_call_counts(counts: &crate::usage::CallCounts) -> Self {
        Self {
            succeeded: counts.succeeded,
            failed: counts.failed,
            unreported: counts.unreported,
        }
    }

    pub(crate) fn to_call_counts(self) -> crate::usage::CallCounts {
        crate::usage::CallCounts {
            succeeded: self.succeeded,
            failed: self.failed,
            unreported: self.unreported,
        }
    }
}

/// Wire representation of aggregated token totals across model calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireUsageTotals {
    /// Total uncached input tokens across reported calls.
    pub(crate) input_tokens: u64,
    /// Total output tokens across reported calls.
    pub(crate) output_tokens: u64,
    /// Sum of reported cache read tokens.
    pub(crate) cache_read_tokens: u64,
    /// Number of successful calls where cache read tokens were not reported.
    pub(crate) cache_read_unreported: u64,
    /// Prompt total of the calls that did report cache read tokens.
    pub(crate) cache_read_reported_prompt_tokens: u64,
    /// Sum of reported cache write tokens.
    pub(crate) cache_write_tokens: u64,
    /// Number of successful calls where cache write tokens were not reported.
    pub(crate) cache_write_unreported: u64,
    /// Sum of reported reasoning tokens.
    pub(crate) reasoning_tokens: u64,
    /// Number of successful calls where reasoning tokens were not reported.
    pub(crate) reasoning_unreported: u64,
}

impl WireUsageTotals {
    pub(crate) fn is_empty(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.cache_read_tokens == 0
            && self.cache_read_unreported == 0
            && self.cache_read_reported_prompt_tokens == 0
            && self.cache_write_tokens == 0
            && self.cache_write_unreported == 0
            && self.reasoning_tokens == 0
            && self.reasoning_unreported == 0
    }

    pub(crate) fn from_usage_totals(totals: &crate::usage::UsageTotals) -> Self {
        Self {
            input_tokens: totals.input_tokens,
            output_tokens: totals.output_tokens,
            cache_read_tokens: totals.cache_read_tokens,
            cache_read_unreported: totals.cache_read_unreported,
            cache_read_reported_prompt_tokens: totals.cache_read_reported_prompt_tokens,
            cache_write_tokens: totals.cache_write_tokens,
            cache_write_unreported: totals.cache_write_unreported,
            reasoning_tokens: totals.reasoning_tokens,
            reasoning_unreported: totals.reasoning_unreported,
        }
    }

    pub(crate) fn to_usage_totals(self) -> crate::usage::UsageTotals {
        crate::usage::UsageTotals {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_read_unreported: self.cache_read_unreported,
            cache_read_reported_prompt_tokens: self.cache_read_reported_prompt_tokens,
            cache_write_tokens: self.cache_write_tokens,
            cache_write_unreported: self.cache_write_unreported,
            reasoning_tokens: self.reasoning_tokens,
            reasoning_unreported: self.reasoning_unreported,
        }
    }
}

/// Wire representation of combined call counts and token totals for a single purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WirePurposeUsage {
    /// Call counts.
    #[serde(default)]
    pub(crate) calls: WireCallCounts,
    /// Token totals.
    #[serde(default)]
    pub(crate) tokens: WireUsageTotals,
}

impl WirePurposeUsage {
    pub(crate) fn is_empty(&self) -> bool {
        self.calls.is_empty() && self.tokens.is_empty()
    }

    pub(crate) fn from_purpose_usage(purpose: &crate::usage::PurposeUsage) -> Self {
        Self {
            calls: WireCallCounts::from_call_counts(&purpose.calls),
            tokens: WireUsageTotals::from_usage_totals(&purpose.tokens),
        }
    }

    pub(crate) fn to_purpose_usage(self) -> crate::usage::PurposeUsage {
        crate::usage::PurposeUsage {
            calls: self.calls.to_call_counts(),
            tokens: self.tokens.to_usage_totals(),
        }
    }
}

/// Persistent wire format for cumulative session-wide model usage.
///
/// Serialized only when at least one usage metric is non-zero or `last_call` is
/// present. Omitted on new sessions and when no calls have occurred.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireSessionUsage {
    /// Cumulative call counts and token totals for conversation turns.
    #[serde(default)]
    pub(crate) conversation: WirePurposeUsage,
    /// Cumulative call counts and token totals for compaction summaries.
    #[serde(default)]
    pub(crate) compaction: WirePurposeUsage,
    /// Number of works that entered the runner (`WorkResult::Run(_)`).
    #[serde(default)]
    pub(crate) input_rounds: u64,
    /// Facts about the most recent successful model call, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_call: Option<WireLastCall>,
}

impl WireSessionUsage {
    pub(crate) fn is_empty(&self) -> bool {
        self.conversation.is_empty()
            && self.compaction.is_empty()
            && self.input_rounds == 0
            && self.last_call.is_none()
    }

    /// Reject a last-call time this platform cannot represent.
    fn validate(&self) -> Result<(), StoreError> {
        match self.last_call {
            Some(last) if crate::usage::from_unix_millis(last.timestamp_ms).is_none() => {
                Err(StoreError::InvalidUsage(format!(
                    "last call time {} ms is out of range",
                    last.timestamp_ms
                )))
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn from_session_usage(usage: &crate::usage::SessionUsage) -> Self {
        Self {
            conversation: WirePurposeUsage::from_purpose_usage(&usage.conversation),
            compaction: WirePurposeUsage::from_purpose_usage(&usage.compaction),
            input_rounds: usage.input_rounds,
            last_call: usage.last_call.as_ref().map(WireLastCall::from_last_call),
        }
    }

    pub(crate) fn to_session_usage(&self) -> crate::usage::SessionUsage {
        crate::usage::SessionUsage {
            conversation: self.conversation.to_purpose_usage(),
            compaction: self.compaction.to_purpose_usage(),
            input_rounds: self.input_rounds,
            last_call: self.last_call.map(WireLastCall::to_last_call),
        }
    }
}

#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn legacy_format_is_rejected_before_decoding_permission_fields() {
        let bytes = br#"{"format":"tars-session-v1","definition":{"tools":["execute_command"]}}"#;
        assert!(
            matches!(super::decode(bytes, "unused"), Err(super::StoreError::UnsupportedFormat(format)) if format == "tars-session-v1")
        );
    }

    #[test]
    fn legacy_v2_format_is_rejected_with_unsupported_format() {
        let bytes = br#"{"format":"tars-session-v2","definition":{"tools":["execute_command"]}}"#;
        let result = super::decode(bytes, "unused");
        match result {
            Err(super::StoreError::UnsupportedFormat(format)) => {
                assert_eq!(format, "tars-session-v2");
            }
            other => panic!("expected UnsupportedFormat for v2, got {other:?}"),
        }
        let err = super::StoreError::UnsupportedFormat("tars-session-v2".into());
        let msg = err.to_string();
        assert!(msg.contains("expected tars-session-v2.2"));
        assert!(msg.contains("tars session export-materials"));
    }

    #[test]
    fn legacy_v2_1_format_is_rejected_with_unsupported_format() {
        let bytes = br#"{"format":"tars-session-v2.1","definition":{"tools":["execute_command"]}}"#;
        let result = super::decode(bytes, "unused");
        match result {
            Err(super::StoreError::UnsupportedFormat(format)) => {
                assert_eq!(format, "tars-session-v2.1");
            }
            other => panic!("expected UnsupportedFormat for v2.1, got {other:?}"),
        }
        let err = super::StoreError::UnsupportedFormat("tars-session-v2.1".into());
        let msg = err.to_string();
        assert!(msg.contains("expected tars-session-v2.2"));
        assert!(msg.contains("tars-session-v2.1"));
        assert!(msg.contains("tars session export-materials"));
    }

    #[test]
    fn wire_round_trip_of_usage_preserves_none_cache_options_and_restores_totals() {
        let usage = crate::usage::SessionUsage {
            conversation: crate::usage::PurposeUsage {
                calls: crate::usage::CallCounts {
                    succeeded: 3,
                    failed: 1,
                    unreported: 1,
                },
                tokens: crate::usage::UsageTotals {
                    input_tokens: 150,
                    output_tokens: 45,
                    cache_read_tokens: 20,
                    cache_read_unreported: 1,
                    cache_read_reported_prompt_tokens: 120,
                    cache_write_tokens: 10,
                    cache_write_unreported: 2,
                    reasoning_tokens: 8,
                    reasoning_unreported: 0,
                },
            },
            compaction: crate::usage::PurposeUsage {
                calls: crate::usage::CallCounts {
                    succeeded: 1,
                    failed: 0,
                    unreported: 0,
                },
                tokens: crate::usage::UsageTotals {
                    input_tokens: 50,
                    output_tokens: 15,
                    cache_read_tokens: 0,
                    cache_read_unreported: 0,
                    cache_read_reported_prompt_tokens: 0,
                    cache_write_tokens: 0,
                    cache_write_unreported: 0,
                    reasoning_tokens: 0,
                    reasoning_unreported: 1,
                },
            },
            input_rounds: 2,
            last_call: Some(crate::usage::LastCall {
                purpose: crate::usage::UsagePurpose::Conversation,
                usage: Some(crate::usage::CallUsage {
                    input_tokens: 100,
                    cache_read_tokens: None,
                    cache_write_tokens: None,
                    output_tokens: 25,
                    reasoning_tokens: None,
                }),
                timestamp: crate::usage::from_unix_millis(1_700_000_000_123).unwrap(),
            }),
        };

        let wire = WireSessionUsage::from_session_usage(&usage);
        let serialized = serde_json::to_string(&wire).unwrap();

        // None cache and reasoning fields serialize as null
        assert!(serialized.contains(r#""cache_read_tokens":null"#));
        assert!(serialized.contains(r#""cache_write_tokens":null"#));
        assert!(serialized.contains(r#""reasoning_tokens":null"#));
        assert!(serialized.contains(r#""timestamp_ms":1700000000123"#));

        let decoded: WireSessionUsage = serde_json::from_str(&serialized).unwrap();
        assert_eq!(wire, decoded);

        let restored = decoded.to_session_usage();
        assert_eq!(usage, restored);
    }

    #[test]
    fn unknown_field_inside_usage_is_rejected() {
        let json_with_unknown = serde_json::json!({
            "conversation": {
                "calls": { "succeeded": 1, "failed": 0, "unreported": 0 },
                "tokens": {
                    "input_tokens": 10,
                    "output_tokens": 5,
                    "cache_read_tokens": 0,
                    "cache_read_unreported": 0,
                    "cache_read_reported_prompt_tokens": 0,
                    "cache_write_tokens": 0,
                    "cache_write_unreported": 0,
                    "reasoning_tokens": 0,
                    "reasoning_unreported": 0
                }
            },
            "compaction": {
                "calls": { "succeeded": 0, "failed": 0, "unreported": 0 },
                "tokens": {
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cache_read_tokens": 0,
                    "cache_read_unreported": 0,
                    "cache_read_reported_prompt_tokens": 0,
                    "cache_write_tokens": 0,
                    "cache_write_unreported": 0,
                    "reasoning_tokens": 0,
                    "reasoning_unreported": 0
                }
            },
            "input_rounds": 1,
            "extra_field": "disallowed"
        });
        let result: Result<WireSessionUsage, _> = serde_json::from_value(json_with_unknown);
        assert!(result.is_err());
    }

    #[test]
    fn record_without_usage_decodes_as_zero_totals() {
        let id = Uuid::now_v7().to_string();
        let commit = Uuid::now_v7().to_string();
        let raw = format!(
            r#"{{"format":"tars-session-v2.2","conversation_id":"{id}","revision":1,"last_commit_id":"{commit}","updated_at_ms":1000,"definition":{{"provider":"openai-chat","model":"legacy-model","endpoint":null,"credential":{{"environment":"PATH"}},"work_timeout_secs":null,"retained_work_capacity":16,"max_model_rounds":8,"max_tool_calls":0,"workspace_root":null,"tools":[],"tool_limits":{{"max_concurrent_operations":4,"call_timeout_secs":10,"max_output_bytes":65536,"max_directory_entries":200,"max_file_scan_bytes":2097152,"max_search_files":1000,"max_search_depth":32,"max_search_scan_bytes":8388608,"max_search_matches":200}}}},"context":{{"blocks":[]}}}}"#
        );
        let record = super::decode(raw.as_bytes(), &id).unwrap();
        assert_eq!(record.usage, None);
        assert_eq!(
            record.session_usage(),
            crate::usage::SessionUsage::default()
        );
    }

    #[test]
    fn an_out_of_range_last_call_time_never_panics() {
        let id = Uuid::now_v7().to_string();
        let commit = Uuid::now_v7().to_string();
        let mut value: serde_json::Value = serde_json::from_str(&format!(
            r#"{{"format":"tars-session-v2.2","conversation_id":"{id}","revision":1,"last_commit_id":"{commit}","updated_at_ms":1000,"definition":{{"provider":"openai-chat","model":"m","endpoint":null,"credential":{{"environment":"KEY"}},"work_timeout_secs":null,"retained_work_capacity":16,"max_model_rounds":8,"max_tool_calls":0,"workspace_root":null,"tools":[],"tool_limits":{{"max_concurrent_operations":4,"call_timeout_secs":10,"max_output_bytes":65536,"max_directory_entries":200,"max_file_scan_bytes":2097152,"max_search_files":1000,"max_search_depth":32,"max_search_scan_bytes":8388608,"max_search_matches":200}}}},"context":{{"blocks":[]}}}}"#
        ))
        .unwrap();
        let mut usage = serde_json::to_value(WireSessionUsage::from_session_usage(
            &crate::usage::SessionUsage::default(),
        ))
        .unwrap();
        usage["last_call"] = serde_json::json!({
            "purpose": "conversation",
            "usage": null,
            "timestamp_ms": u64::MAX,
        });
        value["usage"] = usage;
        let bytes = serde_json::to_vec(&value).unwrap();
        // Platforms that can hold the time read it; the others reject the
        // record. Neither may panic.
        match super::decode(&bytes, &id) {
            Ok(record) => {
                let last = record.session_usage().last_call.unwrap();
                assert_eq!(crate::usage::unix_millis(last.timestamp), u64::MAX);
            }
            Err(error) => assert!(matches!(error, StoreError::InvalidUsage(_)), "{error:?}"),
        }
    }
}

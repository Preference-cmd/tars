//! Ordered, lazily loaded conversation segments shown before the current Context.
//!
//! The session record's history index is the authority for which segments
//! exist. A segment file is read only when the display needs it: never while
//! opening, and one at a time as the current Context fills the screen or the
//! user scrolls up. A loaded segment is cached for the life of the open
//! session, and a segment produced live during the session is adopted from the
//! rows already on screen instead of being read again.

use crate::transcript::Transcript;
use std::collections::HashMap;
use std::sync::Arc;
use tars_core::compact::{HistorySegment, HistorySegmentDocument, HistoryTrigger};

/// One request to read a segment file that is not cached yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryRead {
    /// Canonical identity of the segment to read.
    pub(crate) segment: String,
    /// Whether this read prepends one earlier segment to the loaded display.
    pub(crate) earlier: bool,
}

/// Display state of one index entry.
enum SegmentState {
    /// An earlier segment that has never been read; only the header counts it.
    Unloaded,
    /// A read is requested or in flight.
    Pending,
    /// Content is in the cache.
    Loaded {
        transcript: Arc<Transcript>,
        trigger: HistoryTrigger,
        estimate_before: Option<u64>,
        estimate_after: Option<u64>,
    },
    /// The file is missing or corrupt; the row stays visible and loading continues.
    Unreadable,
}

/// Ordered history index plus the lazily loaded segments in display order.
pub(crate) struct HistoryView {
    index: Vec<HistorySegment>,
    states: Vec<SegmentState>,
    /// Number of leading index entries never loaded (the header count).
    start: usize,
    loading: Option<String>,
    want_earlier: bool,
    hints: HashMap<String, (HistoryTrigger, u64, u64)>,
    display: Option<Arc<Transcript>>,
    display_stale: bool,
    /// Whether one record index was already read for this open instance.
    opened: bool,
}

impl Default for HistoryView {
    fn default() -> Self {
        Self::new()
    }
}

impl HistoryView {
    pub(crate) fn new() -> Self {
        Self {
            index: Vec::new(),
            states: Vec::new(),
            start: 0,
            loading: None,
            want_earlier: false,
            hints: HashMap::new(),
            display: None,
            display_stale: false,
            opened: false,
        }
    }

    /// Forget every cached segment; called when an instance is opened or closed.
    pub(crate) fn clear(&mut self) {
        *self = Self::new();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// Number of earlier segments that are not loaded yet.
    pub(crate) fn earlier(&self) -> usize {
        self.start
    }

    pub(crate) fn loading(&self) -> bool {
        self.loading.is_some()
    }

    /// Whether a scroll reached the top and asked for one earlier segment.
    pub(crate) fn wants_earlier(&self) -> bool {
        self.want_earlier
    }

    /// Whether any segment row is already part of the display.
    ///
    /// Used to decide whether an earlier read must keep the visible rows in
    /// place: the first read only adds content below the header.
    pub(crate) fn has_loaded(&self) -> bool {
        let start = self.start.min(self.states.len());
        self.states[start..].iter().any(|state| {
            matches!(
                state,
                SegmentState::Loaded { .. } | SegmentState::Unreadable
            )
        })
    }

    pub(crate) fn request_earlier(&mut self) {
        self.want_earlier = true;
    }

    /// Remember the estimate of a segment observed through live progress, so a
    /// segment adopted from the displayed rows can still show its separator.
    pub(crate) fn note_hint(
        &mut self,
        segment: String,
        trigger: HistoryTrigger,
        estimate_before: u64,
        estimate_after: u64,
    ) {
        self.hints
            .insert(segment, (trigger, estimate_before, estimate_after));
    }

    /// The next segment file to read, or none when the display is satisfied.
    ///
    /// Newer segments that appeared after a compaction are read first; an
    /// earlier segment is read only when the screen is not full or the user
    /// scrolled to the top of the loaded conversation.
    pub(crate) fn next_read(&mut self, fill_screen: bool) -> Option<HistoryRead> {
        if self.loading.is_some() {
            return None;
        }
        if let Some(position) = self
            .states
            .iter()
            .position(|state| matches!(state, SegmentState::Pending))
        {
            let segment = self.index[position].id.clone();
            self.loading = Some(segment.clone());
            self.display_stale = true;
            return Some(HistoryRead {
                segment,
                earlier: false,
            });
        }
        let earlier = self.start > 0 && (fill_screen || self.want_earlier);
        self.want_earlier = false;
        if !earlier {
            return None;
        }
        self.start -= 1;
        let segment = self.index[self.start].id.clone();
        self.states[self.start] = SegmentState::Pending;
        self.loading = Some(segment.clone());
        self.display_stale = true;
        Some(HistoryRead {
            segment,
            earlier: true,
        })
    }

    /// Cache one read segment document.
    pub(crate) fn insert_loaded(&mut self, document: HistorySegmentDocument) {
        let Some(position) = self
            .index
            .iter()
            .position(|entry| entry.id == document.segment_id)
        else {
            if self.loading.as_deref() == Some(document.segment_id.as_str()) {
                self.loading = None;
            }
            return;
        };
        self.states[position] = SegmentState::Loaded {
            transcript: Arc::new(Transcript::from_history_context(&document.context)),
            trigger: document.trigger,
            estimate_before: Some(document.estimate_before),
            estimate_after: Some(document.estimate_after),
        };
        self.loading = None;
        self.display_stale = true;
    }

    /// Mark one listed segment as unreadable and keep loading earlier ones.
    pub(crate) fn mark_unreadable(&mut self, segment: &str) {
        if let Some(position) = self.index.iter().position(|entry| entry.id == segment) {
            self.states[position] = SegmentState::Unreadable;
        }
        if self.loading.as_deref() == Some(segment) {
            self.loading = None;
        }
        self.display_stale = true;
    }

    /// Abandon a read whose adapter disappeared; the segment can be retried later.
    pub(crate) fn cancel_read(&mut self) {
        let Some(segment) = self.loading.take() else {
            return;
        };
        if let Some(position) = self.index.iter().position(|entry| entry.id == segment)
            && matches!(self.states[position], SegmentState::Pending)
        {
            self.states[position] = SegmentState::Unloaded;
            if position == self.start {
                self.start += 1;
            }
        }
        self.display_stale = true;
    }

    /// Replace the index with the record's current list, keeping cached content.
    ///
    /// `previous` is the conversation that was on screen before this reload: a
    /// compaction inside the session replaces exactly that material, so the
    /// oldest new segment adopts those rows instead of reading its file again.
    pub(crate) fn refresh(
        &mut self,
        index: Vec<HistorySegment>,
        previous: Option<Arc<Transcript>>,
    ) {
        if index.len() < self.index.len() {
            // A stale snapshot cannot remove a segment that was already listed.
            return;
        }
        if !self.opened && self.index.is_empty() && self.states.is_empty() {
            // The record's index at open: no segment was displayed before it.
            self.start = index.len();
            self.states = index.iter().map(|_| SegmentState::Unloaded).collect();
            self.index = index;
            self.display_stale = true;
            self.opened = true;
            return;
        }
        if self.states.len() < index.len() {
            let mut adopted = previous.filter(|transcript| !transcript.text.is_empty());
            for entry in &index[self.states.len()..] {
                let hint = self.hints.get(&entry.id).copied();
                let state = match adopted.take() {
                    Some(transcript) => SegmentState::Loaded {
                        transcript,
                        trigger: entry.trigger,
                        estimate_before: hint.map(|hint| hint.1),
                        estimate_after: hint.map(|hint| hint.2),
                    },
                    None => SegmentState::Pending,
                };
                self.states.push(state);
            }
            self.display_stale = true;
        }
        self.index = index;
        self.hints
            .retain(|id, _| self.index.iter().any(|entry| &entry.id == id));
    }

    /// Rows of the loaded history part of the conversation, oldest first.
    pub(crate) fn display(&mut self) -> Option<Arc<Transcript>> {
        if self.index.is_empty() {
            return None;
        }
        if self.display_stale || self.display.is_none() {
            self.display = Some(Arc::new(self.compose()));
            self.display_stale = false;
        }
        self.display.clone()
    }

    fn compose(&self) -> Transcript {
        let header = if self.loading.is_some() {
            "loading earlier conversation".to_owned()
        } else if self.start > 0 {
            format!("↑ {} earlier segments", self.start)
        } else {
            "start of conversation".to_owned()
        };
        self.compose_with_header(header)
    }

    fn compose_with_header(&self, header: String) -> Transcript {
        let mut text = header;
        text.push_str("\n\n");
        for (offset, state) in self.states[self.start..].iter().enumerate() {
            match state {
                SegmentState::Unloaded => {}
                // The earliest pending entry is the one the header names.
                SegmentState::Pending if offset == 0 => {}
                SegmentState::Pending => text.push_str("loading earlier conversation\n\n"),
                SegmentState::Unreadable => {
                    text.push_str("this history segment cannot be read\n\n");
                }
                SegmentState::Loaded {
                    transcript,
                    trigger,
                    estimate_before,
                    estimate_after,
                } => {
                    text.push_str(&transcript.text);
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                    if !text.ends_with("\n\n") {
                        text.push('\n');
                    }
                    match (estimate_before, estimate_after) {
                        (Some(before), Some(after)) => text.push_str(&format!(
                            "── Compacted ({}, {} → {}) ──\n\n",
                            trigger.name(),
                            estimate_label(*before),
                            estimate_label(*after)
                        )),
                        _ => text.push_str(&format!("── Compacted ({}) ──\n\n", trigger.name())),
                    }
                }
            }
        }
        Transcript {
            text,
            ..Transcript::default()
        }
    }
}

/// Conservative token estimate in a short human form, such as `~140k`.
pub(crate) fn estimate_label(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("~{}M", (tokens + 500_000) / 1_000_000)
    } else if tokens >= 1_000 {
        format!("~{}k", (tokens + 500) / 1_000)
    } else {
        format!("~{tokens}")
    }
}

/// Context window in a short human form, such as `200k`.
pub(crate) fn window_label(window: u64) -> String {
    if window >= 1_000_000 && window.is_multiple_of(1_000_000) {
        format!("{}M", window / 1_000_000)
    } else if window >= 1_000 && window.is_multiple_of(1_000) {
        format!("{}k", window / 1_000)
    } else {
        window.to_string()
    }
}

impl super::Entry {
    /// Drop the cached history display; used when the instance changes.
    pub(super) fn clear_history(&mut self) {
        self.history.clear();
        self.history_rows = None;
        self.history_anchor = None;
    }

    /// Ask for one earlier segment after a scroll reached the top.
    pub(super) fn request_earlier_history(&mut self) {
        if self.history_rows.is_some() && self.history.earlier() > 0 {
            self.history.request_earlier();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use causa::kernel::Context;
    use tars_core::application::text_input;

    fn segment(id: &str, trigger: HistoryTrigger) -> HistorySegment {
        HistorySegment {
            id: id.to_owned(),
            created_at_ms: 1,
            trigger,
            blocks: 1,
            summary_block_id: causa::runtime::new_block_id(),
        }
    }

    fn transcript(text: &str) -> Arc<Transcript> {
        Arc::new(Transcript {
            text: text.to_owned(),
            ..Transcript::default()
        })
    }

    #[test]
    fn open_reads_no_segment_and_counts_earlier_rows() {
        let mut view = HistoryView::new();
        view.refresh(
            vec![
                segment("0190a000-0000-7000-8000-000000000001", HistoryTrigger::Auto),
                segment(
                    "0190a000-0000-7000-8000-000000000002",
                    HistoryTrigger::Manual,
                ),
            ],
            None,
        );
        assert_eq!(view.earlier(), 2);
        assert!(!view.loading());
        let display = view.display().unwrap();
        assert!(display.text.contains("↑ 2 earlier segments"));
        // Fill-screen loading starts from the segment next to the current Context.
        let read = view.next_read(true).unwrap();
        assert_eq!(read.segment, "0190a000-0000-7000-8000-000000000002");
        assert!(read.earlier);
        assert!(view.loading());
        assert!(view.next_read(true).is_none());
        // While the file is read the header reports the pending load.
        assert!(
            view.display()
                .unwrap()
                .text
                .contains("loading earlier conversation")
        );
    }

    #[test]
    fn loaded_segments_keep_their_estimates_and_never_reload() {
        let mut view = HistoryView::new();
        view.refresh(
            vec![segment(
                "0190a000-0000-7000-8000-000000000001",
                HistoryTrigger::Auto,
            )],
            None,
        );
        view.request_earlier();
        let read = view.next_read(false).unwrap();
        assert_eq!(read.segment, "0190a000-0000-7000-8000-000000000001");
        let document = HistorySegmentDocument {
            format: "tars-history-v1".into(),
            conversation_id: "conversation".into(),
            segment_id: read.segment.clone(),
            created_at_ms: 1,
            trigger: HistoryTrigger::Auto,
            estimate_before: 140_000,
            estimate_after: 6_000,
            summary_block_id: causa::runtime::new_block_id(),
            context: Context::from_blocks(vec![text_input("old words")]).unwrap(),
        };
        view.insert_loaded(document);
        assert!(!view.loading());
        let display = view.display().unwrap();
        assert!(display.text.contains("old words"));
        assert!(display.text.contains("Compacted (auto, ~140k → ~6k)"));
        assert!(display.text.contains("start of conversation"));
        // Nothing earlier remains and the loaded segment is never read twice.
        assert!(view.next_read(true).is_none());
        // A same-length refresh keeps the cache.
        let index = view.index.clone();
        view.refresh(index, Some(transcript("replacement")));
        assert!(view.display().unwrap().text.contains("old words"));
    }

    #[test]
    fn a_live_compaction_adopts_the_rows_already_on_screen() {
        let mut view = HistoryView::new();
        let first = segment("0190a000-0000-7000-8000-000000000001", HistoryTrigger::Auto);
        view.refresh(vec![first.clone()], None);
        // The work that compacted the context was showing this conversation.
        view.refresh(
            vec![
                first,
                segment("0190a000-0000-7000-8000-000000000002", HistoryTrigger::Auto),
            ],
            Some(transcript("the conversation that was on screen\n\n")),
        );
        let display = view.display().unwrap();
        assert!(display.text.contains("the conversation that was on screen"));
        // The adopted segment is loaded, the earlier one still needs a read.
        assert_eq!(view.earlier(), 1);
        let read = view.next_read(true).unwrap();
        assert_eq!(read.segment, "0190a000-0000-7000-8000-000000000001");
        assert!(read.earlier);
    }

    #[test]
    fn unreadable_segment_is_marked_and_earlier_loading_continues() {
        let mut view = HistoryView::new();
        view.refresh(
            vec![
                segment("0190a000-0000-7000-8000-000000000001", HistoryTrigger::Auto),
                segment(
                    "0190a000-0000-7000-8000-000000000002",
                    HistoryTrigger::Manual,
                ),
            ],
            None,
        );
        let read = view.next_read(true).unwrap();
        view.mark_unreadable(&read.segment);
        assert!(
            view.display()
                .unwrap()
                .text
                .contains("this history segment cannot be read")
        );
        assert_eq!(view.earlier(), 1);
        let read = view.next_read(true).unwrap();
        assert_eq!(read.segment, "0190a000-0000-7000-8000-000000000001");
    }

    #[test]
    fn labels_stay_short_and_round() {
        assert_eq!(estimate_label(0), "~0");
        assert_eq!(estimate_label(950), "~950");
        assert_eq!(estimate_label(6_000), "~6k");
        assert_eq!(estimate_label(140_000), "~140k");
        assert_eq!(estimate_label(1_000_000), "~1M");
        assert_eq!(window_label(200_000), "200k");
        assert_eq!(window_label(1_000_000), "1M");
        assert_eq!(window_label(204_800), "204800");
    }
}

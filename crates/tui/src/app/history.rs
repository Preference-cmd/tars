//! The expanded conversation shown before the current Context, loaded lazily.
//!
//! The current Context's leading blocks that a rewrite inserted (the tool
//! declaration and the brief) stand for older conversation. A core
//! [`ExpansionCursor`] expands them newest first and stops at each record it
//! needs; the view reads that record in a background job only when the screen
//! is not full or the user scrolled to the top. Opening reads no record. A
//! rewrite made during the session supplies its records in memory; the view
//! rebuilds from the new history run and replays known records synchronously.

use crate::transcript::Transcript;
use causa::kernel::ContextBlock;
use std::collections::HashMap;
use std::sync::Arc;
use tars_core::compact::{
    BlockStatus, ExpansionCursor, ExpansionStep, HistoryRecord, HistoryRecordEntry, HistoryTrigger,
};

/// One request to read a record the expansion needs next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryRead {
    /// Canonical identity of the record to read.
    pub(crate) record: String,
}

/// One displayed part of the history, in reading order.
enum Piece {
    /// Expanded original blocks.
    Rows(Arc<Transcript>),
    /// Where one rewrite's brief stood: after the originals it replaced.
    Divider {
        trigger: HistoryTrigger,
        estimates: Option<(u64, u64)>,
    },
    /// A record that cannot be read; loading continues with older ones.
    Unreadable,
}

/// The expansion cursor plus the pieces it produced so far.
pub(crate) struct HistoryView {
    cursor: Option<ExpansionCursor>,
    /// Displayed pieces, oldest first.
    pieces: Vec<Piece>,
    /// Number of index entries the view knows about.
    known: usize,
    /// Rewrite records supplied or loaded so far.
    records: HashMap<String, Arc<HistoryRecord>>,
    /// Record the cursor waits for whose read was not started yet.
    needed: Option<String>,
    loading: Option<String>,
    want_earlier: bool,
    finished: bool,
    display: Option<Arc<Transcript>>,
    display_stale: bool,
    /// Whether the record index was already taken for this open instance.
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
            cursor: None,
            pieces: Vec::new(),
            known: 0,
            records: HashMap::new(),
            needed: None,
            loading: None,
            want_earlier: false,
            finished: false,
            display: None,
            display_stale: false,
            opened: false,
        }
    }

    /// Forget every loaded piece; called when an instance is opened or closed.
    pub(crate) fn clear(&mut self) {
        *self = Self::new();
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.known == 0
    }

    /// Number of listed records not read yet.
    pub(crate) fn earlier(&self) -> usize {
        match &self.cursor {
            Some(cursor) if !self.finished => cursor.unread(),
            _ => 0,
        }
    }

    /// Whether older conversation remains to be loaded.
    pub(crate) fn has_earlier(&self) -> bool {
        self.cursor.is_some() && !self.finished
    }

    pub(crate) fn loading(&self) -> bool {
        self.loading.is_some()
    }

    /// Whether a scroll reached the top and asked for earlier conversation.
    pub(crate) fn wants_earlier(&self) -> bool {
        self.want_earlier
    }

    /// Whether any history row is already part of the display.
    ///
    /// Used to decide whether an earlier read must keep the visible rows in
    /// place: the first read only adds content below the header.
    pub(crate) fn has_loaded(&self) -> bool {
        !self.pieces.is_empty()
    }

    pub(crate) fn request_earlier(&mut self) {
        self.want_earlier = true;
    }

    /// The next record to read, or none when the display is satisfied.
    ///
    /// Earlier conversation is read only when the screen is not full or the
    /// user scrolled to the top of the loaded conversation.
    pub(crate) fn next_read(&mut self, fill_screen: bool) -> Option<HistoryRead> {
        self.supply_needed_if_cached();
        if self.loading.is_some() || !self.has_earlier() {
            return None;
        }
        let wanted = fill_screen || self.want_earlier;
        self.want_earlier = false;
        if !wanted {
            return None;
        }
        self.supply_needed_if_cached();
        let record = match self.needed.take() {
            Some(record) => record,
            None => self.advance()?,
        };
        self.loading = Some(record.clone());
        self.display_stale = true;
        Some(HistoryRead { record })
    }

    /// Continue the expansion with one read record.
    pub(crate) fn insert_loaded(&mut self, record: HistoryRecord) {
        let id = record.record_id.clone();
        let arc = Arc::new(record);
        self.records.insert(id.clone(), arc.clone());
        if self.loading.as_deref() == Some(&id) {
            self.loading = None;
            if let Some(cursor) = &mut self.cursor {
                cursor.supply_arc(&id, Ok(arc));
            }
            self.needed = self.advance();
            self.display_stale = true;
        }
        self.supply_needed_if_cached();
    }

    /// Continue the expansion past one record that cannot be read.
    pub(crate) fn mark_unreadable(&mut self, record: &str, error: String) {
        if self.loading.as_deref() == Some(record) {
            self.loading = None;
            if let Some(cursor) = &mut self.cursor {
                cursor.supply_arc(record, Err(error));
            }
            self.needed = self.advance();
            self.display_stale = true;
            self.supply_needed_if_cached();
        }
    }

    fn supply_needed_if_cached(&mut self) {
        while let Some(record_id) = self.needed.take() {
            if let Some(record) = self.records.get(&record_id).cloned() {
                if let Some(cursor) = &mut self.cursor {
                    cursor.supply_arc(&record_id, Ok(record));
                }
                self.needed = self.advance();
                self.display_stale = true;
            } else {
                self.needed = Some(record_id);
                break;
            }
        }
    }

    /// Abandon a read whose adapter disappeared; the record is read again later.
    pub(crate) fn cancel_read(&mut self) {
        if let Some(record) = self.loading.take() {
            self.needed = Some(record);
            self.supply_needed_if_cached();
        }
        self.display_stale = true;
    }

    /// Take the record's current index together with the Context it matches.
    ///
    /// `prefix` is the Context's leading blocks that a rewrite inserted and
    /// `records` are rewrite records supplied in memory. Any refresh that
    /// changes the index rebuilds the cursor from the new history run and
    /// replays known records synchronously (no I/O), stopping at the first
    /// record it does not have. Returns whether the history was rebuilt.
    pub(crate) fn refresh(
        &mut self,
        index: Vec<HistoryRecordEntry>,
        prefix: Vec<ContextBlock>,
        records: &[Arc<HistoryRecord>],
    ) -> bool {
        for record in records {
            self.records
                .insert(record.record_id.clone(), record.clone());
        }
        if self.opened && index.len() <= self.known {
            // Unchanged, or a stale snapshot that cannot remove a record.
            return false;
        }
        self.opened = true;
        self.cursor = Some(ExpansionCursor::new(&prefix, index.clone()));
        self.pieces.clear();
        self.loading = None;
        self.finished = false;
        self.needed = self.advance();
        self.known = index.len();
        self.display_stale = true;
        self.supply_needed_if_cached();
        true
    }

    /// Run the cursor until it needs a record, prepending what it produced.
    fn advance(&mut self) -> Option<String> {
        let cursor = self.cursor.as_mut()?;
        let mut blocks = Vec::new();
        let mut newest_first = Vec::new();
        let flush = |blocks: &mut Vec<ContextBlock>, pieces: &mut Vec<Piece>| {
            if !blocks.is_empty() {
                blocks.reverse();
                pieces.push(Piece::Rows(Arc::new(Transcript::from_history_blocks(
                    blocks,
                ))));
                blocks.clear();
            }
        };
        let needed = loop {
            match cursor.next() {
                ExpansionStep::Block(block) => {
                    // The inserted blocks of an unreadable record are the
                    // brief already shown below; the row says what is missing.
                    if !matches!(block.status, BlockStatus::Unreadable(_)) {
                        blocks.push(block.block);
                    }
                }
                ExpansionStep::Rewrite(mark) => {
                    flush(&mut blocks, &mut newest_first);
                    newest_first.push(Piece::Divider {
                        trigger: mark.trigger,
                        estimates: Some((mark.estimate_before, mark.estimate_after)),
                    });
                }
                ExpansionStep::Unreadable(_) => {
                    flush(&mut blocks, &mut newest_first);
                    newest_first.push(Piece::Unreadable);
                }
                ExpansionStep::NeedRecord(record) => {
                    if let Some(known) = self.records.get(&record).cloned() {
                        cursor.supply_arc(&record, Ok(known));
                        continue;
                    }
                    flush(&mut blocks, &mut newest_first);
                    break Some(record);
                }
                ExpansionStep::End => {
                    flush(&mut blocks, &mut newest_first);
                    self.finished = true;
                    break None;
                }
            }
        };
        if !newest_first.is_empty() {
            newest_first.reverse();
            newest_first.append(&mut self.pieces);
            self.pieces = newest_first;
            self.display_stale = true;
        }
        needed
    }

    /// Rows of the loaded history part of the conversation, oldest first.
    pub(crate) fn display(&mut self) -> Option<Arc<Transcript>> {
        if self.known == 0 {
            return None;
        }
        if self.display_stale || self.display.is_none() {
            self.display = Some(Arc::new(self.compose()));
            self.display_stale = false;
        }
        self.display.clone()
    }

    fn compose(&self) -> Transcript {
        let mut text = if self.loading.is_some() {
            "loading earlier conversation".to_owned()
        } else if self.finished {
            "start of conversation".to_owned()
        } else {
            match self.earlier() {
                0 => "↑ earlier conversation".to_owned(),
                1 => "↑ 1 earlier record".to_owned(),
                count => format!("↑ {count} earlier records"),
            }
        };
        text.push_str("\n\n");
        for piece in &self.pieces {
            match piece {
                Piece::Rows(transcript) => {
                    text.push_str(&transcript.text);
                    if !text.ends_with('\n') {
                        text.push('\n');
                    }
                    if !text.ends_with("\n\n") {
                        text.push('\n');
                    }
                }
                Piece::Divider { trigger, estimates } => match estimates {
                    Some((before, after)) => text.push_str(&format!(
                        "── Compacted ({}, {} → {}) ──\n\n",
                        trigger.name(),
                        estimate_label(*before),
                        estimate_label(*after)
                    )),
                    None => text.push_str(&format!("── Compacted ({}) ──\n\n", trigger.name())),
                },
                Piece::Unreadable => text.push_str("this history record cannot be read\n\n"),
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
        self.scroll_anchor = None;
    }

    /// Remember the line currently at the top of the viewport across a history rebuild.
    pub(super) fn capture_scroll_anchor(&mut self) {
        self.history_anchor = None;
        let Some(scroll) = self.scroll else {
            self.scroll_anchor = None;
            return;
        };
        let history_len = self.history_rows.as_ref().map_or(0, |r| r.ranges.len());
        let current_len = self.rows.as_ref().map_or(0, |r| r.ranges.len());
        let total = history_len + current_len;
        if total == 0 || scroll >= total {
            self.scroll_anchor = None;
            return;
        }

        let get_row = |idx: usize| -> Option<String> {
            if idx < history_len {
                let rows = self.history_rows.as_ref()?;
                let range = rows.ranges.get(idx)?.clone();
                Some(rows.transcript.text[range].to_string())
            } else if idx < total {
                let rows = self.rows.as_ref()?;
                let range = rows.ranges.get(idx - history_len)?.clone();
                Some(rows.transcript.text[range].to_string())
            } else {
                None
            }
        };

        let mut rows = Vec::with_capacity(3);
        for offset in 0..3 {
            if let Some(text) = get_row(scroll + offset) {
                rows.push(text);
            } else {
                break;
            }
        }

        if rows.is_empty() {
            self.scroll_anchor = None;
        } else {
            self.scroll_anchor = Some(super::state::ScrollAnchor {
                index: scroll,
                rows,
            });
        }
    }

    /// Restore the scroll position so the anchored line stays at the top of the viewport.
    pub(super) fn restore_scroll_anchor(&mut self) {
        let Some(anchor) = self.scroll_anchor.take() else {
            return;
        };
        if self.scroll != Some(anchor.index) {
            return;
        }
        let history_len = self.history_rows.as_ref().map_or(0, |r| r.ranges.len());
        let current_len = self.rows.as_ref().map_or(0, |r| r.ranges.len());
        let total = history_len + current_len;

        let get_row = |idx: usize| -> Option<&str> {
            if idx < history_len {
                let rows = self.history_rows.as_ref()?;
                let range = rows.ranges.get(idx)?.clone();
                Some(&rows.transcript.text[range])
            } else if idx < total {
                let rows = self.rows.as_ref()?;
                let range = rows.ranges.get(idx - history_len)?.clone();
                Some(&rows.transcript.text[range])
            } else {
                None
            }
        };

        let mut best_match = None;
        let mut best_dist = usize::MAX;
        let window_start = anchor.index.saturating_sub(64);
        let window_end = anchor.index.saturating_add(64);

        for pos in window_start..=window_end {
            if pos + anchor.rows.len() > total {
                continue;
            }
            let matches = anchor
                .rows
                .iter()
                .enumerate()
                .all(|(offset, expected)| get_row(pos + offset) == Some(expected.as_str()));
            if matches {
                let dist = pos.abs_diff(anchor.index);
                if dist < best_dist {
                    best_dist = dist;
                    best_match = Some(pos);
                }
            }
        }

        if let Some(new_idx) = best_match {
            self.scroll = Some(new_idx);
        } else {
            let max_scroll = self.conversation_len().saturating_sub(1);
            self.scroll = Some(anchor.index.min(max_scroll));
        }
    }

    /// Ask for earlier conversation after a scroll reached the top.
    pub(super) fn request_earlier_history(&mut self) {
        if self.history_rows.is_some() && self.history.has_earlier() {
            self.history.request_earlier();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use causa::kernel::Role;
    use tars_core::compact::{RangeReplacement, RewriteKind};

    const OLDER: &str = "0190a000-0000-7000-8000-000000000001";
    const NEWER: &str = "0190a000-0000-7000-8000-000000000002";
    const NEWEST: &str = "0190a000-0000-7000-8000-000000000003";

    fn block(text: &str) -> ContextBlock {
        ContextBlock::text(causa::runtime::new_block_id(), Role::User, text)
    }

    /// Two whole-Context compactions: OLDER replaced "old words" with brief
    /// A, NEWER replaced brief A and "middle words" with brief B.
    struct Chain {
        records: Vec<HistoryRecord>,
        prefix: Vec<ContextBlock>,
    }

    fn chain() -> Chain {
        let brief_a = block("brief A");
        let brief_b = block("brief B");
        let record =
            |id: &str, trigger, before, originals: Vec<ContextBlock>, inserted: &ContextBlock| {
                HistoryRecord {
                    format: "tars-history-v1.1".into(),
                    conversation_id: "conversation".into(),
                    record_id: id.into(),
                    created_at_ms: 1,
                    trigger,
                    kind: RewriteKind::Summarize,
                    estimate_before: before,
                    estimate_after: 6_000,
                    replacements: vec![RangeReplacement {
                        range: 0..originals.len(),
                        originals,
                        inserted_ids: vec![inserted.id()],
                    }],
                }
            };
        Chain {
            records: vec![
                record(
                    OLDER,
                    HistoryTrigger::Auto,
                    140_000,
                    vec![block("old words")],
                    &brief_a,
                ),
                record(
                    NEWER,
                    HistoryTrigger::Manual,
                    150_000,
                    vec![brief_a.clone(), block("middle words")],
                    &brief_b,
                ),
            ],
            prefix: vec![brief_b],
        }
    }

    fn index(records: &[HistoryRecord]) -> Vec<HistoryRecordEntry> {
        records
            .iter()
            .map(|r| HistoryRecordEntry {
                record_id: r.record_id.clone(),
                created_at_ms: r.created_at_ms,
                trigger: r.trigger,
                kind: r.kind,
                removed_block_count: r.replacements.iter().map(|rep| rep.originals.len()).sum(),
                inserted_block_ids: r
                    .replacements
                    .iter()
                    .flat_map(|rep| rep.inserted_ids.iter().copied())
                    .collect(),
            })
            .collect()
    }

    fn text(view: &mut HistoryView) -> String {
        view.display().unwrap().text.clone()
    }

    #[test]
    fn open_reads_no_record_and_loads_newest_first_on_demand() {
        let chain = chain();
        let mut view = HistoryView::new();
        view.refresh(index(&chain.records), chain.prefix.clone(), &[]);
        assert_eq!(view.earlier(), 2);
        assert!(!view.loading());
        assert!(text(&mut view).contains("↑ 2 earlier records"));
        // A full screen without a scroll to the top asks for nothing.
        assert!(view.next_read(false).is_none());

        let read = view.next_read(true).unwrap();
        assert_eq!(read.record, NEWER);
        assert!(view.next_read(true).is_none(), "one read at a time");
        assert!(text(&mut view).contains("loading earlier conversation"));
        view.insert_loaded(chain.records[1].clone());
        let shown = text(&mut view);
        assert!(shown.contains("↑ 1 earlier record"), "{shown}");
        assert!(shown.contains("middle words"), "{shown}");
        assert!(
            shown.contains("── Compacted (manual, ~150k → ~6k) ──"),
            "{shown}"
        );
        // The older brief is not shown as text: its record expands it.
        assert!(!shown.contains("brief A"), "{shown}");

        view.request_earlier();
        let read = view.next_read(false).unwrap();
        assert_eq!(read.record, OLDER);
        view.insert_loaded(chain.records[0].clone());
        let shown = text(&mut view);
        assert!(shown.starts_with("start of conversation"), "{shown}");
        let old = shown.find("old words").unwrap();
        let first = shown.find("Compacted (auto").unwrap();
        let middle = shown.find("middle words").unwrap();
        let second = shown.find("Compacted (manual").unwrap();
        assert!(old < first && first < middle && middle < second, "{shown}");
        assert!(!view.has_earlier());
        assert!(view.next_read(true).is_none());

        // A same-length refresh keeps what was loaded.
        view.refresh(index(&chain.records), chain.prefix, &[]);
        assert!(text(&mut view).contains("old words"));
    }

    #[test]
    fn a_live_compaction_supplies_records_in_memory_without_reading() {
        let chain = chain();
        let mut view = HistoryView::new();
        // Opened after the older compaction only.
        let older = index(&chain.records[..1]);
        let brief_a = chain.records[1].replacements[0].originals[0].clone();
        view.refresh(older, vec![brief_a], &[]);
        view.refresh(
            index(&chain.records),
            chain.prefix,
            &[Arc::new(chain.records[1].clone())],
        );
        let shown = text(&mut view);
        assert!(shown.contains("middle words"), "{shown}");
        assert!(!shown.contains("Context summary · collapsed"), "{shown}");
        assert!(
            shown.contains("── Compacted (manual, ~150k → ~6k) ──"),
            "{shown}"
        );
        // The supplied record needs no read; the older one still does.
        assert_eq!(view.earlier(), 1);
        let read = view.next_read(true).unwrap();
        assert_eq!(read.record, OLDER);
    }

    #[test]
    fn a_live_compaction_with_no_older_records_finishes_history() {
        let chain = chain();
        let mut view = HistoryView::new();
        view.refresh(vec![], vec![], &[]);
        let brief_b = chain.prefix[0].clone();
        view.refresh(
            index(&chain.records[1..]),
            vec![brief_b],
            &[Arc::new(chain.records[1].clone())],
        );
        assert_eq!(view.earlier(), 0);
        assert!(!view.has_earlier());
        assert!(text(&mut view).starts_with("start of conversation"));
    }

    #[test]
    fn an_unreadable_record_is_marked_and_older_loading_continues() {
        let chain = chain();
        let mut view = HistoryView::new();
        view.refresh(index(&chain.records), chain.prefix, &[]);
        let read = view.next_read(true).unwrap();
        assert_eq!(read.record, NEWER);
        view.mark_unreadable(&read.record, "missing".into());
        let shown = text(&mut view);
        assert!(
            shown.contains("this history record cannot be read"),
            "{shown}"
        );
        assert!(!shown.contains("brief B"), "{shown}");
        assert_eq!(view.earlier(), 1);
        let read = view.next_read(true).unwrap();
        assert_eq!(read.record, OLDER);
        view.insert_loaded(chain.records[0].clone());
        let shown = text(&mut view);
        assert!(shown.starts_with("start of conversation"), "{shown}");
        assert!(shown.find("old words").unwrap() < shown.find("cannot be read").unwrap());
    }

    #[test]
    fn a_late_read_for_a_cancelled_read_is_supplied_and_not_read_again() {
        let chain = chain();
        let mut view = HistoryView::new();
        view.refresh(index(&chain.records), chain.prefix, &[]);
        let read = view.next_read(true).unwrap();
        assert_eq!(read.record, NEWER);
        view.cancel_read();
        view.insert_loaded(chain.records[1].clone());
        // The cancelled record is supplied synchronously once received.
        assert!(text(&mut view).contains("middle words"));
        // The record is already in memory, so next_read asks for the older record.
        let next = view.next_read(true).unwrap();
        assert_eq!(next.record, OLDER);
    }

    #[test]
    fn a_late_read_for_a_rebuilt_view_is_ignored_and_not_read_again() {
        let mut chain = chain();
        let brief_c = block("brief C");
        let rec3 = HistoryRecord {
            format: "tars-history-v1.1".into(),
            conversation_id: "conversation".into(),
            record_id: NEWEST.into(),
            created_at_ms: 2,
            trigger: HistoryTrigger::Auto,
            kind: RewriteKind::Summarize,
            estimate_before: 160_000,
            estimate_after: 6_000,
            replacements: vec![RangeReplacement {
                range: 0..2,
                originals: vec![chain.prefix[0].clone(), block("newest words")],
                inserted_ids: vec![brief_c.id()],
            }],
        };
        let mut view = HistoryView::new();
        view.refresh(index(&chain.records), chain.prefix.clone(), &[]);
        let read = view.next_read(true).unwrap();
        assert_eq!(read.record, NEWER);

        // A new compaction lands before the read completes: index expands to 3.
        chain.records.push(rec3.clone());
        chain.prefix = vec![brief_c];
        view.refresh(index(&chain.records), chain.prefix.clone(), &[]);

        // The late read arrives for NEWER.
        view.insert_loaded(chain.records[1].clone());
        // Cursor waits for NEWEST, so stale middle words are not yet shown.
        assert!(!text(&mut view).contains("middle words"));

        // next_read asks for NEWEST.
        let read_newest = view.next_read(true).unwrap();
        assert_eq!(read_newest.record, NEWEST);

        // When NEWEST is loaded, NEWER is already cached and expands synchronously.
        view.insert_loaded(rec3);
        let shown = text(&mut view);
        assert!(shown.contains("newest words"), "{shown}");
        assert!(shown.contains("middle words"), "{shown}");

        // next_read now asks for OLDER, never NEWER again.
        let read_older = view.next_read(true).unwrap();
        assert_eq!(read_older.record, OLDER);
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

    #[test]
    fn tail_reply_with_sealed_items_stripped_is_shown_once() {
        use causa::kernel::{BlockContent, ContentPart, Context, SealedPart, TextPayload};
        let brief = block("brief text");
        let orig_reply = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::Message {
                role: Role::Assistant,
                parts: vec![
                    ContentPart::Text(TextPayload(
                        "assistant answer UNIQUE_ASSISTANT_REPLY".into(),
                    )),
                    ContentPart::Sealed(SealedPart::new(
                        "anthropic_messages",
                        serde_json::json!({"type": "thinking", "thinking": "secret thoughts", "signature": "sig"}),
                    )),
                ],
            },
            Default::default(),
        );
        let stripped_reply = ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::Message {
                role: Role::Assistant,
                parts: vec![ContentPart::Text(TextPayload(
                    "assistant answer UNIQUE_ASSISTANT_REPLY".into(),
                ))],
            },
            Default::default(),
        );
        let record = HistoryRecord {
            format: "tars-history-v1.1".into(),
            conversation_id: "conversation".into(),
            record_id: "0190a000-0000-7000-8000-000000000099".into(),
            created_at_ms: 1,
            trigger: HistoryTrigger::Auto,
            kind: RewriteKind::Summarize,
            estimate_before: 10_000,
            estimate_after: 5_000,
            replacements: vec![
                RangeReplacement {
                    range: 0..1,
                    originals: vec![block("earlier user message UNIQUE_EARLIER")],
                    inserted_ids: vec![brief.id()],
                },
                RangeReplacement {
                    range: 1..2,
                    originals: vec![orig_reply],
                    inserted_ids: vec![stripped_reply.id()],
                },
            ],
        };
        let tail_user = block("tail message UNIQUE_TAIL");
        let index_vec = index(std::slice::from_ref(&record));
        let context = Context::from_blocks(vec![brief, stripped_reply, tail_user]).unwrap();
        let transcript = Transcript::from_materials(&context, &index_vec);
        let prefix = context.blocks()[transcript.history_range.clone()].to_vec();

        // The stripped reply in history_run must NOT be rendered in current material:
        assert!(!transcript.text.contains("UNIQUE_ASSISTANT_REPLY"));
        // The tail block after history_run IS rendered in current material:
        assert!(transcript.text.contains("UNIQUE_TAIL"));

        let mut view = HistoryView::new();
        view.refresh(index_vec, prefix, &[Arc::new(record)]);
        let history_text = text(&mut view);
        assert!(history_text.contains("UNIQUE_EARLIER"));
        assert!(history_text.contains("UNIQUE_ASSISTANT_REPLY"));

        let earlier_pos = history_text.find("UNIQUE_EARLIER").unwrap();
        let divider_pos = history_text.find("Compacted (auto").unwrap();
        let reply_pos = history_text.find("UNIQUE_ASSISTANT_REPLY").unwrap();
        assert!(
            earlier_pos < divider_pos,
            "summarized originals appear before divider"
        );
        assert!(
            divider_pos < reply_pos,
            "divider stands before stripped tail reply"
        );

        // Combined conversation displays each block exactly once:
        let combined = format!("{}{}", history_text, transcript.text);
        assert_eq!(combined.matches("UNIQUE_ASSISTANT_REPLY").count(), 1);
        assert_eq!(combined.matches("UNIQUE_EARLIER").count(), 1);
        assert_eq!(combined.matches("UNIQUE_TAIL").count(), 1);
    }

    #[test]
    fn context_with_system_block_in_fixed_prefix_expands_history() {
        use causa::kernel::Context;
        let sys = ContextBlock::text(
            causa::runtime::new_block_id(),
            Role::System,
            "SYSTEM_PROMPT_UNIQUE",
        );
        let brief = block("brief text");
        let record = HistoryRecord {
            format: "tars-history-v1.1".into(),
            conversation_id: "conversation".into(),
            record_id: "0190a000-0000-7000-8000-000000000088".into(),
            created_at_ms: 1,
            trigger: HistoryTrigger::Manual,
            kind: RewriteKind::Summarize,
            estimate_before: 50_000,
            estimate_after: 5_000,
            replacements: vec![RangeReplacement {
                range: 0..1,
                originals: vec![block("OLD_CONVERSATION_UNIQUE")],
                inserted_ids: vec![brief.id()],
            }],
        };
        let index_vec = index(std::slice::from_ref(&record));
        let context = Context::from_blocks(vec![sys, brief.clone()]).unwrap();
        let transcript = Transcript::from_materials(&context, &index_vec);
        let prefix = context.blocks()[transcript.history_range.clone()].to_vec();
        assert_eq!(prefix.len(), 1);
        assert_eq!(prefix[0].id(), brief.id());

        // Fixed prefix system prompt is rendered in current transcript:
        assert!(transcript.text.contains("SYSTEM_PROMPT_UNIQUE"));

        // History expands successfully:
        let mut view = HistoryView::new();
        view.refresh(index_vec, prefix, &[Arc::new(record)]);
        let history_text = text(&mut view);
        assert!(history_text.contains("OLD_CONVERSATION_UNIQUE"));

        let combined = format!("{}{}", history_text, transcript.text);
        assert_eq!(combined.matches("SYSTEM_PROMPT_UNIQUE").count(), 1);
        assert_eq!(combined.matches("OLD_CONVERSATION_UNIQUE").count(), 1);
    }

    fn make_rows(lines: &[&str]) -> crate::transcript::Rows {
        let text = lines.join("\n");
        crate::transcript::Rows::wrap(
            Arc::new(crate::transcript::Transcript {
                text,
                ..Default::default()
            }),
            80,
        )
    }

    #[test]
    fn repeated_marker_or_blank_row_near_old_index_does_not_capture_anchor() {
        let mut entry = crate::app::Entry::new("test".into(), "model".into());
        let original_lines = vec![
            "●",
            "Item 1",
            "",
            "●",
            "Item 2",
            "description 2",
            "",
            "●",
            "Item 3",
        ];
        entry.rows = Some(make_rows(&original_lines));
        // Viewing row 3 ("●") at top of viewport. 3-row run: ["●", "Item 2", "description 2"].
        entry.scroll = Some(3);
        entry.capture_scroll_anchor();

        // Rebuild inserts another item, so row 3 is now a repeated "●" for Item 1.5.
        let updated_lines = vec![
            "●",
            "Item 1",
            "",
            "●",
            "Item 1.5",
            "description 1.5",
            "",
            "●",
            "Item 2",
            "description 2",
            "",
            "●",
            "Item 3",
        ];
        entry.rows = Some(make_rows(&updated_lines));
        entry.restore_scroll_anchor();

        // A single-row match would wrongly pick the repeated "●" at index 3 (dist 0).
        // The 3-row run disambiguates and matches Item 2 at index 7.
        assert_eq!(entry.scroll, Some(7));
    }

    #[test]
    fn shift_of_n_rows_is_followed() {
        let mut entry = crate::app::Entry::new("test".into(), "model".into());
        let original_lines = vec![
            "start of conversation",
            "first line",
            "target row 0",
            "target row 1",
            "target row 2",
            "end of conversation",
        ];
        entry.rows = Some(make_rows(&original_lines));
        entry.scroll = Some(2);
        entry.capture_scroll_anchor();

        let updated_lines = vec![
            "start of conversation",
            "first line",
            "divider row 1",
            "divider row 2",
            "divider row 3",
            "divider row 4",
            "divider row 5",
            "target row 0",
            "target row 1",
            "target row 2",
            "end of conversation",
        ];
        entry.rows = Some(make_rows(&updated_lines));
        entry.restore_scroll_anchor();

        assert_eq!(entry.scroll, Some(7));
    }

    #[test]
    fn no_match_leaves_scroll_unchanged_and_clamped() {
        let mut entry = crate::app::Entry::new("test".into(), "model".into());
        let original_lines = vec![
            "original line 0",
            "original line 1",
            "original line 2",
            "original line 3",
            "original line 4",
            "original line 5",
        ];
        entry.rows = Some(make_rows(&original_lines));
        entry.scroll = Some(2);
        entry.capture_scroll_anchor();

        let different_lines = vec![
            "completely different 0",
            "completely different 1",
            "completely different 2",
            "completely different 3",
            "completely different 4",
            "completely different 5",
        ];
        entry.rows = Some(make_rows(&different_lines));
        entry.restore_scroll_anchor();
        assert_eq!(entry.scroll, Some(2));

        entry.scroll = Some(20);
        entry.scroll_anchor = Some(super::super::state::ScrollAnchor {
            index: 20,
            rows: vec!["missing 0".into(), "missing 1".into(), "missing 2".into()],
        });
        let short_lines = vec!["short 0", "short 1", "short 2"];
        entry.rows = Some(make_rows(&short_lines));
        entry.restore_scroll_anchor();
        assert_eq!(entry.scroll, Some(2));
    }

    #[test]
    fn pending_history_anchor_is_cleared_by_rebuild() {
        let mut entry = crate::app::Entry::new("test".into(), "model".into());
        let lines = vec!["line 0", "line 1", "line 2", "line 3"];
        entry.rows = Some(make_rows(&lines));
        entry.scroll = Some(1);
        entry.history_anchor = Some(Some(1));

        entry.capture_scroll_anchor();
        assert!(entry.history_anchor.is_none());
        assert!(entry.scroll_anchor.is_some());
    }
}

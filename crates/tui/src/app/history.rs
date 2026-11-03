//! The expanded conversation shown before the current Context, loaded lazily.
//!
//! The current Context's leading blocks that a rewrite inserted (the tool
//! declaration and the brief) stand for older conversation. A core
//! [`ExpansionCursor`] expands them newest first and stops at each record it
//! needs; the view reads that record in a background job only when the screen
//! is not full or the user scrolled to the top. Opening reads no record. A
//! rewrite made during the session adopts the rows already on screen.

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
    /// Record the cursor waits for whose read was not started yet.
    needed: Option<String>,
    loading: Option<String>,
    want_earlier: bool,
    finished: bool,
    hints: HashMap<String, (HistoryTrigger, u64, u64)>,
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
            needed: None,
            loading: None,
            want_earlier: false,
            finished: false,
            hints: HashMap::new(),
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

    /// Remember the estimates of a rewrite observed through live progress, so
    /// its divider can show them when the rewrite is adopted without a read.
    pub(crate) fn note_hint(
        &mut self,
        record: String,
        trigger: HistoryTrigger,
        estimate_before: u64,
        estimate_after: u64,
    ) {
        self.hints
            .insert(record, (trigger, estimate_before, estimate_after));
    }

    /// The next record to read, or none when the display is satisfied.
    ///
    /// Earlier conversation is read only when the screen is not full or the
    /// user scrolled to the top of the loaded conversation.
    pub(crate) fn next_read(&mut self, fill_screen: bool) -> Option<HistoryRead> {
        if self.loading.is_some() || !self.has_earlier() {
            return None;
        }
        let wanted = fill_screen || self.want_earlier;
        self.want_earlier = false;
        if !wanted {
            return None;
        }
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
        self.supply(&id, Ok(record));
    }

    /// Continue the expansion past one record that cannot be read.
    pub(crate) fn mark_unreadable(&mut self, record: &str, error: String) {
        self.supply(record, Err(error));
    }

    fn supply(&mut self, record: &str, result: Result<HistoryRecord, String>) {
        if self.loading.as_deref() != Some(record) {
            // A read for a view that was rebuilt meanwhile.
            return;
        }
        self.loading = None;
        if let Some(cursor) = &mut self.cursor {
            cursor.supply(record, result);
        }
        self.needed = self.advance();
        self.display_stale = true;
    }

    /// Abandon a read whose adapter disappeared; the record is read again later.
    pub(crate) fn cancel_read(&mut self) {
        if let Some(record) = self.loading.take() {
            self.needed = Some(record);
        }
        self.display_stale = true;
    }

    /// Take the record's current index together with the Context it matches.
    ///
    /// `prefix` is the Context's leading blocks that a rewrite inserted and
    /// `previous` the conversation on screen before this reload. One new
    /// rewrite of the whole previous Context replaced exactly those rows, so
    /// they are adopted above its divider instead of being read again. Any
    /// other change starts the expansion again from the new Context.
    pub(crate) fn refresh(
        &mut self,
        index: Vec<HistoryRecordEntry>,
        prefix: Vec<ContextBlock>,
        previous: Option<Arc<Transcript>>,
    ) {
        if self.opened && index.len() <= self.known {
            // Unchanged, or a stale snapshot that cannot remove a record.
            return;
        }
        let adoptable = self.opened && index.len() == self.known + 1 && self.cursor.is_some();
        self.opened = true;
        match (adoptable, previous) {
            (true, Some(previous)) => {
                let entry = index.last().expect("one new entry").clone();
                let rows = &previous.text[previous.history_start.min(previous.text.len())..];
                if !rows.trim().is_empty() {
                    self.pieces.push(Piece::Rows(Arc::new(Transcript {
                        text: rows.to_owned(),
                        ..Transcript::default()
                    })));
                }
                let estimates = self
                    .hints
                    .get(&entry.record_id)
                    .map(|hint| (hint.1, hint.2));
                self.pieces.push(Piece::Divider {
                    trigger: entry.trigger,
                    estimates,
                });
                if let Some(cursor) = &mut self.cursor {
                    cursor.adopt(entry);
                }
            }
            _ => {
                self.cursor = Some(ExpansionCursor::new(&prefix, index.clone()));
                self.pieces.clear();
                self.needed = None;
                self.loading = None;
                self.finished = false;
            }
        }
        self.known = index.len();
        self.hints
            .retain(|id, _| index.iter().any(|entry| &entry.record_id == id));
        self.display_stale = true;
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
            .map(|record| HistoryRecordEntry {
                record_id: record.record_id.clone(),
                created_at_ms: record.created_at_ms,
                trigger: record.trigger,
                kind: record.kind,
                removed_block_count: record.replacements[0].originals.len(),
                inserted_block_ids: record.replacements[0].inserted_ids.clone(),
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
        view.refresh(index(&chain.records), chain.prefix.clone(), None);
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
        view.refresh(index(&chain.records), chain.prefix, None);
        assert!(text(&mut view).contains("old words"));
    }

    #[test]
    fn a_live_compaction_adopts_the_rows_already_on_screen() {
        let chain = chain();
        let mut view = HistoryView::new();
        // Opened after the older compaction only.
        let older = index(&chain.records[..1]);
        let brief_a = chain.records[1].replacements[0].originals[0].clone();
        view.refresh(older, vec![brief_a], None);
        let previous = Arc::new(Transcript {
            text: "● Context summary · collapsed\n\nmiddle words on screen\n\n".into(),
            history_start: "● Context summary · collapsed\n\n".len(),
            ..Transcript::default()
        });
        view.note_hint(NEWER.into(), HistoryTrigger::Manual, 150_000, 6_000);
        view.refresh(index(&chain.records), chain.prefix, Some(previous));
        let shown = text(&mut view);
        assert!(shown.contains("middle words on screen"), "{shown}");
        assert!(!shown.contains("Context summary · collapsed"), "{shown}");
        assert!(
            shown.contains("── Compacted (manual, ~150k → ~6k) ──"),
            "{shown}"
        );
        // The adopted record needs no read; the older one still does.
        assert_eq!(view.earlier(), 1);
        let read = view.next_read(true).unwrap();
        assert_eq!(read.record, OLDER);
    }

    #[test]
    fn an_unreadable_record_is_marked_and_older_loading_continues() {
        let chain = chain();
        let mut view = HistoryView::new();
        view.refresh(index(&chain.records), chain.prefix, None);
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
    fn a_late_read_for_a_rebuilt_view_is_ignored() {
        let chain = chain();
        let mut view = HistoryView::new();
        view.refresh(index(&chain.records), chain.prefix, None);
        let read = view.next_read(true).unwrap();
        view.cancel_read();
        view.insert_loaded(chain.records[1].clone());
        assert!(!text(&mut view).contains("middle words"));
        // The cancelled record is asked for again.
        assert_eq!(view.next_read(true).unwrap(), read);
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

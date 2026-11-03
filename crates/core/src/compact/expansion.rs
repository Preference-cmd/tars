//! Conversation expansion from the current Context and its rewrite records.
//!
//! Each rewrite replaced some ranges of the Context with inserted blocks and
//! kept the removed originals in its record. Expansion walks the Context from
//! the newest block backwards and puts every inserted run back as the
//! originals of its own range, recursively: an original may itself have been
//! inserted by an older rewrite.
//!
//! [`ExpansionCursor`] does the walk without reading files: it stops at each
//! record it needs and continues once the caller supplies it. A display can
//! therefore read records in the background, one at a time, only as far as
//! it scrolls. [`expand_conversation`] drives a cursor with a read function.

use super::HistoryTrigger;
use super::history::{HistoryRecord, HistoryRecordEntry, RewriteKind};
use crate::store::StoreError;
use causa::kernel::{BlockId, ContextBlock};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// Provenance of one block in an expanded conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "record_id", rename_all = "snake_case")]
pub enum BlockStatus {
    /// A block still in the current Context that no rewrite inserted.
    Original,
    /// A block removed by a summarize or merge rewrite (record id).
    Summarized(String),
    /// A block removed by an elide rewrite (record id).
    Elided(String),
    /// A block inserted by a rewrite whose record cannot be read (record id).
    ///
    /// The inserted block itself is shown in place of the originals.
    Unreadable(String),
}

impl std::fmt::Display for BlockStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Original => write!(f, "original"),
            Self::Summarized(id) => write!(f, "summarized ({id})"),
            Self::Elided(id) => write!(f, "elided ({id})"),
            Self::Unreadable(id) => write!(f, "unreadable ({id})"),
        }
    }
}

/// One block in an expanded conversation with its provenance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExpandedBlock {
    /// Whether this block is original, summarized, elided or from an unreadable record.
    #[serde(flatten)]
    pub status: BlockStatus,
    /// The block content and metadata.
    pub block: ContextBlock,
}

/// One rewrite record that expansion could not read or use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnreadableRecord {
    /// Identity of the record.
    pub record_id: String,
    /// Why it could not be read or used.
    pub error: String,
}

/// Facts of one rewrite whose originals the cursor is about to produce.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewriteMark {
    /// Identity of the record.
    pub record_id: String,
    /// What asked for the rewrite.
    pub trigger: HistoryTrigger,
    /// What kind of rewrite it was.
    pub kind: RewriteKind,
    /// Estimated request size before the rewrite.
    pub estimate_before: u64,
    /// Estimated request size after the rewrite.
    pub estimate_after: u64,
}

/// One step of an expansion, newest first.
#[derive(Clone, Debug, PartialEq)]
pub enum ExpansionStep {
    /// The next older block.
    Block(Box<ExpandedBlock>),
    /// The originals of this rewrite follow; a display puts its divider here,
    /// after them in reading order.
    Rewrite(RewriteMark),
    /// This record cannot be read or used; its inserted blocks follow with
    /// [`BlockStatus::Unreadable`]. Reported once per record.
    Unreadable(UnreadableRecord),
    /// The cursor needs this record: pass it to [`ExpansionCursor::supply`].
    NeedRecord(String),
    /// Nothing older remains.
    End,
}

/// The result of expanding a conversation.
#[derive(Clone, Debug, PartialEq)]
pub struct ExpandedConversation {
    /// Expanded blocks in chronological order.
    pub blocks: Vec<ExpandedBlock>,
    /// Records that could not be read or used, each reported once.
    pub unreadable_records: Vec<UnreadableRecord>,
}

enum Item {
    /// A block that may still be an inserted block of an older record.
    Expand(ContextBlock),
    /// A block shown as it stands.
    Show(ExpandedBlock),
}

struct Frame {
    /// Items in reading order; the newest is taken from the end.
    items: Vec<Item>,
    /// Only records older than this index position may own an item.
    below: usize,
    /// Status of items no record owns.
    status: BlockStatus,
}

struct Waiting {
    owner: usize,
    /// The inserted run to replace, or none for a record reached only by its
    /// place in the index (an older record behind an unreadable one).
    run: Option<Vec<ContextBlock>>,
}

/// Expansion that a caller drives one step at a time, newest first.
///
/// The cursor never reads a file. When it reaches an inserted run it returns
/// [`ExpansionStep::NeedRecord`] until the caller supplies that record. After
/// the walk, records the walk never reached (older records behind an
/// unreadable one) are expanded on their own, newest first, so loading can
/// continue past a missing file.
pub struct ExpansionCursor {
    index: Vec<HistoryRecordEntry>,
    owners: HashMap<BlockId, usize>,
    stack: Vec<Frame>,
    reached: Vec<bool>,
    supplied: Vec<bool>,
    /// Inserted blocks of each record expanded so far.
    consumed: Vec<usize>,
    /// Supplied records that still have inserted blocks to expand.
    records: HashMap<usize, Result<Arc<HistoryRecord>, String>>,
    waiting: Option<Waiting>,
    queued: VecDeque<ExpansionStep>,
}

impl ExpansionCursor {
    /// Start expanding `blocks`, newest first, against the record `index`.
    ///
    /// `index` is the session's record index in creation order.
    pub fn new(blocks: &[ContextBlock], index: Vec<HistoryRecordEntry>) -> Self {
        let mut owners = HashMap::new();
        for (position, entry) in index.iter().enumerate() {
            for id in &entry.inserted_block_ids {
                owners.entry(*id).or_insert(position);
            }
        }
        let count = index.len();
        Self {
            stack: vec![Frame {
                items: blocks.iter().cloned().map(Item::Expand).collect(),
                below: count,
                status: BlockStatus::Original,
            }],
            index,
            owners,
            reached: vec![false; count],
            supplied: vec![false; count],
            consumed: vec![0; count],
            records: HashMap::new(),
            waiting: None,
            queued: VecDeque::new(),
        }
    }

    /// Number of index entries whose record was neither supplied nor adopted.
    pub fn unread(&self) -> usize {
        self.supplied.iter().filter(|supplied| !**supplied).count()
    }

    /// Register a rewrite whose originals the caller already shows.
    ///
    /// Used when a rewrite replaced material that was on screen: the caller
    /// keeps those rows and this cursor continues with what is older.
    pub fn adopt(&mut self, entry: HistoryRecordEntry) {
        let position = self.index.len();
        for id in &entry.inserted_block_ids {
            self.owners.entry(*id).or_insert(position);
        }
        self.consumed.push(entry.inserted_block_ids.len());
        self.index.push(entry);
        self.reached.push(true);
        self.supplied.push(true);
    }

    /// The next step, newest first.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> ExpansionStep {
        loop {
            if let Some(step) = self.queued.pop_front() {
                return step;
            }
            if let Some(waiting) = &self.waiting {
                return ExpansionStep::NeedRecord(self.index[waiting.owner].record_id.clone());
            }
            let owners = &self.owners;
            let Some(frame) = self.stack.last_mut() else {
                match (0..self.index.len()).rev().find(|&p| !self.reached[p]) {
                    Some(position) => {
                        self.reached[position] = true;
                        self.start(position, None);
                        continue;
                    }
                    None => return ExpansionStep::End,
                }
            };
            let Some(item) = frame.items.pop() else {
                self.stack.pop();
                continue;
            };
            let block = match item {
                Item::Show(expanded) => return ExpansionStep::Block(Box::new(expanded)),
                Item::Expand(block) => block,
            };
            let below = frame.below;
            let owner_of = |id: BlockId| owners.get(&id).copied().filter(|p| *p < below);
            let Some(owner) = owner_of(block.id()) else {
                return ExpansionStep::Block(Box::new(ExpandedBlock {
                    status: frame.status.clone(),
                    block,
                }));
            };
            let mut run = vec![block];
            while let Some(Item::Expand(previous)) = frame.items.last()
                && owner_of(previous.id()) == Some(owner)
            {
                let Some(Item::Expand(previous)) = frame.items.pop() else {
                    unreachable!("checked above");
                };
                run.push(previous);
            }
            run.reverse();
            self.reached[owner] = true;
            self.start(owner, Some(run));
        }
    }

    /// Supply the record the last [`ExpansionStep::NeedRecord`] asked for.
    ///
    /// A failure is the reason the record cannot be read. A record the cursor
    /// is not waiting for is ignored.
    pub fn supply(&mut self, record_id: &str, record: Result<HistoryRecord, String>) {
        let Some(waiting) = self
            .waiting
            .take_if(|waiting| self.index[waiting.owner].record_id == record_id)
        else {
            return;
        };
        self.supplied[waiting.owner] = true;
        let record = record.and_then(|record| {
            if record.record_id == record_id {
                Ok(Arc::new(record))
            } else {
                Err("record identity does not match the index".to_owned())
            }
        });
        if let Err(error) = &record {
            self.queued
                .push_back(ExpansionStep::Unreadable(UnreadableRecord {
                    record_id: record_id.to_owned(),
                    error: error.clone(),
                }));
        }
        self.records.insert(waiting.owner, record);
        self.start(waiting.owner, waiting.run);
    }

    /// Expand one run of `owner`, or wait for its record.
    fn start(&mut self, owner: usize, run: Option<Vec<ContextBlock>>) {
        let Some(record) = self.records.get(&owner).cloned() else {
            self.waiting = Some(Waiting { owner, run });
            return;
        };
        let record_id = self.index[owner].record_id.clone();
        let total = self.index[owner].inserted_block_ids.len();
        self.consumed[owner] += run.as_ref().map_or(total, Vec::len);
        let items = match record {
            Err(_) => run
                .unwrap_or_default()
                .into_iter()
                .map(|block| {
                    Item::Show(ExpandedBlock {
                        status: BlockStatus::Unreadable(record_id.clone()),
                        block,
                    })
                })
                .collect(),
            Ok(record) => {
                self.queued.push_back(ExpansionStep::Rewrite(RewriteMark {
                    record_id: record_id.clone(),
                    trigger: record.trigger,
                    kind: record.kind,
                    estimate_before: record.estimate_before,
                    estimate_after: record.estimate_after,
                }));
                match run {
                    Some(run) => self.match_run(&record, &run),
                    None => record
                        .replacements
                        .iter()
                        .flat_map(|replacement| replacement.originals.iter().cloned())
                        .map(Item::Expand)
                        .collect(),
                }
            }
        };
        let status = match self.index[owner].kind {
            RewriteKind::Elide => BlockStatus::Elided(record_id),
            RewriteKind::Summarize | RewriteKind::Merge => BlockStatus::Summarized(record_id),
        };
        self.stack.push(Frame {
            items,
            below: owner,
            status,
        });
        if self.consumed[owner] >= total {
            self.records.remove(&owner);
        }
    }

    /// Replace each range's inserted blocks in `run` with that range's originals.
    ///
    /// The run may hold several adjacent ranges of one record; each is matched
    /// by its own inserted ids. A block no range describes is shown as it is.
    fn match_run(&mut self, record: &HistoryRecord, run: &[ContextBlock]) -> Vec<Item> {
        let mut newest_first = Vec::new();
        let mut end = run.len();
        let mut stray = false;
        while end > 0 {
            let tail = &run[..end];
            let matched = record.replacements.iter().find(|replacement| {
                let inserted = &replacement.inserted_ids;
                !inserted.is_empty()
                    && inserted.len() <= tail.len()
                    && tail[tail.len() - inserted.len()..]
                        .iter()
                        .map(ContextBlock::id)
                        .eq(inserted.iter().copied())
            });
            match matched {
                Some(replacement) => {
                    newest_first.extend(
                        replacement
                            .originals
                            .iter()
                            .rev()
                            .cloned()
                            .map(Item::Expand),
                    );
                    end -= replacement.inserted_ids.len();
                }
                None => {
                    stray = true;
                    newest_first.push(Item::Show(ExpandedBlock {
                        status: BlockStatus::Unreadable(record.record_id.clone()),
                        block: tail[end - 1].clone(),
                    }));
                    end -= 1;
                }
            }
        }
        if stray {
            self.queued
                .push_back(ExpansionStep::Unreadable(UnreadableRecord {
                    record_id: record.record_id.clone(),
                    error: "record does not describe all of its inserted blocks".into(),
                }));
        }
        newest_first.reverse();
        newest_first
    }
}

/// Expand the current Context into the full conversation.
///
/// `index` is the session's record index in creation order. With `last_n`,
/// only the newest N blocks are produced and only the records they need are
/// read. A missing or corrupt record is reported in `unreadable_records`; its
/// inserted blocks are shown with [`BlockStatus::Unreadable`] and the rest of
/// the conversation still expands.
pub(crate) fn expand_conversation<F>(
    blocks: &[ContextBlock],
    index: &[HistoryRecordEntry],
    last_n: Option<usize>,
    mut read_record: F,
) -> ExpandedConversation
where
    F: FnMut(&str) -> Result<HistoryRecord, StoreError>,
{
    let mut cursor = ExpansionCursor::new(blocks, index.to_vec());
    let mut newest_first = Vec::new();
    let mut unreadable_records = Vec::new();
    while last_n.is_none_or(|limit| newest_first.len() < limit) {
        match cursor.next() {
            ExpansionStep::Block(block) => newest_first.push(*block),
            ExpansionStep::Rewrite(_) => {}
            ExpansionStep::Unreadable(unreadable) => unreadable_records.push(unreadable),
            ExpansionStep::NeedRecord(record_id) => {
                let record = read_record(&record_id).map_err(|error| error.to_string());
                cursor.supply(&record_id, record);
            }
            ExpansionStep::End => break,
        }
    }
    newest_first.reverse();
    ExpandedConversation {
        blocks: newest_first,
        unreadable_records,
    }
}

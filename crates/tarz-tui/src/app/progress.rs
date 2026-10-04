//! Loss-tolerant UI observation; authoritative completion uses the existing wait job.
use super::state::{App, Entry, Key};
use crate::preview::Preview;
use std::sync::Arc;
use tarz_core::session::{ProgressUpdate, WorkProgress};
use tokio::sync::broadcast::{self, error::TryRecvError};

const MAX_TICK_UPDATES: usize = 128;

pub(crate) struct ConversationViewport {
    pub start: usize,
    pub end: usize,
    pub sticky: bool,
    pub reserved: usize,
    pub page: usize,
}

pub(super) struct ProgressFeed {
    key: Key,
    receiver: broadcast::Receiver<Arc<WorkProgress>>,
}
impl Entry {
    pub(super) fn bind_progress(&mut self, receiver: broadcast::Receiver<Arc<WorkProgress>>) {
        self.preview = Preview::default();
        self.progress = Some(ProgressFeed {
            key: self.key.clone(),
            receiver,
        });
    }
    pub(super) fn clear_progress(&mut self) {
        self.progress = None;
        self.preview = Preview::default();
    }
    fn apply_progress(&mut self, event: &WorkProgress) {
        if self.work.as_ref() != Some(&event.work)
            || event.invocation_id.turn_id != event.work.turn_id
        {
            return;
        }
        if let Some(current) = &self.preview.invocation
            && current.turn_id == event.invocation_id.turn_id
            && current.round_id.0 > event.invocation_id.round_id.0
        {
            return;
        }
        if self.preview.invocation.as_ref() != Some(&event.invocation_id) {
            self.preview.start(event.invocation_id.clone());
        }
        match &event.update {
            ProgressUpdate::ModelStarted => self.preview.start(event.invocation_id.clone()),
            ProgressUpdate::Text { delta, truncated } => self.preview.append(delta, *truncated),
            ProgressUpdate::ModelOutputReady => self.preview.ready = true,
        }
    }
    pub fn viewport(&self, height: usize) -> ConversationViewport {
        let total = self.conversation_len();
        let history = self.rows.as_ref().map_or(0, |rows| rows.ranges.len());
        let base_end = total.saturating_sub(height);
        let headings = 1 + usize::from(self.preview.warning().is_some());
        let sticky_at = |start| self.preview.is_active() && start > history;
        // A one-row viewport combines label and tail text in the same row.
        // Short viewports reserve at most height-1 rows to retain body text.
        let reserve = |start| {
            if sticky_at(start) {
                headings.min(height.saturating_sub(1))
            } else {
                0
            }
        };
        let end = total.saturating_sub(height.saturating_sub(reserve(base_end)));
        let start = self.scroll.unwrap_or(end).min(end);
        let reserved = reserve(start);
        ConversationViewport {
            start,
            end,
            sticky: sticky_at(start),
            reserved,
            page: height.saturating_sub(reserved).saturating_sub(1).max(1),
        }
    }
    pub fn conversation_len(&self) -> usize {
        self.rows.as_ref().map_or(0, |rows| rows.ranges.len()) + self.preview.line_count()
    }
}
impl App {
    pub(super) fn poll_progress(&mut self) {
        let count = self.entries.len();
        if count == 0 {
            return;
        }
        let mut remaining = MAX_TICK_UPDATES;
        for distance in 0..count {
            let index = (self.progress_cursor + distance) % count;
            let entry = &mut self.entries[index];
            while remaining > 0 {
                let Some(feed) = &mut entry.progress else {
                    break;
                };
                if feed.key != entry.key {
                    entry.clear_progress();
                    self.dirty = true;
                    break;
                }
                match feed.receiver.try_recv() {
                    Ok(event) => {
                        remaining -= 1;
                        entry.apply_progress(&event);
                        self.dirty = true;
                    }
                    Err(TryRecvError::Lagged(lost)) => {
                        remaining -= 1;
                        entry.preview.lost = entry.preview.lost.saturating_add(lost);
                        self.dirty = true;
                    }
                    Err(TryRecvError::Closed) => {
                        entry.clear_progress();
                        self.dirty = true;
                        break;
                    }
                    Err(TryRecvError::Empty) => break,
                }
            }
            if remaining == 0 {
                self.progress_cursor = (index + 1) % count;
                break;
            }
        }
        if let Some(entry) = self.entries.get_mut(self.selected) {
            entry.preview.layout(self.view_width);
        }
    }
}

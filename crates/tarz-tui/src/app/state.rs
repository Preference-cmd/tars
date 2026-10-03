use super::jobs::Message;
use crate::{
    input::{self, Form},
    options::Options,
    transcript::{Rows, Transcript},
};
use ratatui_textarea::TextArea;
use std::sync::Arc;
use tarz_core::application::{ManagedSession, MaterialVersion, PreparedCreate, SessionHost};
use tarz_core::session::WorkRef;
use tokio::task::JoinSet;

// Limits UI-owned in-flight work/results; core separately limits active instances.
pub(super) const MAX_JOBS: usize = 64;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct Key {
    pub id: String,
    pub generation: u64,
}
pub(crate) struct Entry {
    pub(super) key: Key,
    pub title: String,
    pub session: Option<ManagedSession>,
    pub(super) prepared: Option<PreparedCreate>,
    pub busy: Option<&'static str>,
    pub work: Option<WorkRef>,
    pub execution: String,
    pub status: String,
    pub notice: String,
    pub editor: TextArea<'static>,
    pub transcript: Arc<Transcript>,
    pub rows: Option<Rows>,
    pub scroll: Option<usize>,
    pub(super) material_version: Option<MaterialVersion>,
    pub(super) requested_material_version: Option<MaterialVersion>,
    #[cfg(test)]
    pub(super) material_reads: usize,
    pub(super) refreshing: bool,
    pub(super) wrapping: bool,
}
impl Entry {
    pub(super) fn new(id: String, title: String) -> Self {
        Self {
            key: Key { id, generation: 0 },
            title,
            session: None,
            prepared: None,
            busy: None,
            work: None,
            execution: "No work in this instance".into(),
            status: "Stored; F3 to open".into(),
            notice: String::new(),
            editor: input::editor(),
            transcript: Arc::new(Transcript::default()),
            rows: None,
            scroll: None,
            material_version: None,
            requested_material_version: None,
            #[cfg(test)]
            material_reads: 0,
            refreshing: false,
            wrapping: false,
        }
    }
    pub fn id(&self) -> &str {
        &self.key.id
    }
    pub fn has_draft(&self) -> bool {
        self.editor.lines().iter().any(|line| !line.is_empty())
    }
}
pub(crate) enum Confirmation {
    Quit,
    Close,
    Discard,
}
pub(crate) struct App {
    pub host: SessionHost,
    pub options: Options,
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub form: Option<Form>,
    pub confirmation: Option<Confirmation>,
    pub panel: Option<super::commands::Panel>,
    pub viewport_height: usize,
    pub notice: String,
    pub quitting: bool,
    pub exit_reports: Vec<String>,
    pub(super) jobs: JoinSet<Message>,
    pub(super) listing: bool,
    pub(super) view_width: u16,
    pub(super) dirty: bool,
    pub(super) done: bool,
}
impl App {
    pub fn new(options: Options) -> Self {
        let host = SessionHost::new(Some(options.store.clone()));
        let notice = options
            .config_path
            .as_ref()
            .map(|path| format!("Config: {} · F2 new / F3 open", path.display()))
            .unwrap_or_else(|| "F2 creates a session. F3 opens the selection.".into());
        Self {
            host,
            options,
            entries: Vec::new(),
            selected: 0,
            form: None,
            confirmation: None,
            panel: None,
            viewport_height: 12,
            notice,
            quitting: false,
            exit_reports: Vec::new(),
            jobs: JoinSet::new(),
            listing: false,
            view_width: 80,
            dirty: true,
            done: false,
        }
    }
    pub(super) fn entry_mut(&mut self, key: &Key) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|entry| entry.key == *key)
    }
    pub(super) fn capacity(&mut self) -> bool {
        if self.jobs.len() >= MAX_JOBS {
            self.notice = "Background operation capacity reached; retry after completion".into();
            false
        } else {
            true
        }
    }
}

use super::types::DefinitionError;
use serde::{Deserialize, Serialize};

/// Explicit workspace capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceTool {
    /// List a bounded single directory.
    ListDirectory,
    /// Read bounded UTF-8 file lines.
    ReadFile,
    /// Search a bounded tree for a literal string.
    SearchText,
    /// Create or overwrite a bounded workspace-relative file.
    WriteFile,
    /// Replace exact text segments in a bounded workspace-relative file.
    EditFile,
}

impl WorkspaceTool {
    /// Stable tool name advertised to model providers.
    pub fn name(self) -> &'static str {
        match self {
            Self::ListDirectory => "list_directory",
            Self::ReadFile => "read_file",
            Self::SearchText => "search_text",
            Self::WriteFile => "write_file",
            Self::EditFile => "edit_file",
        }
    }
}

/// Frozen bounds for workspace write and edit operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteToolLimits {
    /// Maximum size of an existing file that may be edited or overwritten.
    pub max_edit_file_bytes: usize,
    /// Maximum size of written or edited content.
    pub max_write_bytes: usize,
    /// Maximum number of replacement segments in a single edit_file call.
    pub max_edit_segments: usize,
}

impl Default for WriteToolLimits {
    fn default() -> Self {
        Self {
            max_edit_file_bytes: 2 * 1024 * 1024,
            max_write_bytes: 1024 * 1024,
            max_edit_segments: 32,
        }
    }
}

impl WriteToolLimits {
    /// Reject zero bounds and increases beyond the supported hard ceilings.
    pub fn validate(&self) -> Result<(), DefinitionError> {
        let maximum = Self::default();
        if self.max_edit_file_bytes == 0
            || self.max_edit_file_bytes > maximum.max_edit_file_bytes
            || self.max_write_bytes == 0
            || self.max_write_bytes > maximum.max_write_bytes
            || self.max_edit_segments == 0
            || self.max_edit_segments > maximum.max_edit_segments
        {
            return Err(DefinitionError::InvalidToolLimits);
        }
        Ok(())
    }
}

/// Frozen effective bounds; callers may lower but cannot raise supported limits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceToolLimits {
    /// Concurrent actual filesystem operations in one session.
    pub max_concurrent_operations: usize,
    /// Per-call deadline including waiting for operation slots, in seconds.
    pub call_timeout_secs: u64,
    /// Maximum serialized inline result size; at least 1024 bytes for metadata.
    pub max_output_bytes: usize,
    /// Maximum entries returned from one directory.
    pub max_directory_entries: usize,
    /// Maximum bytes scanned from a single file, including skipped lines.
    pub max_file_scan_bytes: usize,
    /// Maximum regular files visited during a search.
    pub max_search_files: usize,
    /// Maximum directory depth during a search.
    pub max_search_depth: usize,
    /// Maximum total bytes scanned during a search.
    pub max_search_scan_bytes: usize,
    /// Maximum returned literal matches.
    pub max_search_matches: usize,
}

impl Default for WorkspaceToolLimits {
    fn default() -> Self {
        Self {
            max_concurrent_operations: 4,
            call_timeout_secs: 10,
            max_output_bytes: 64 * 1024,
            max_directory_entries: 200,
            max_file_scan_bytes: 2 * 1024 * 1024,
            max_search_files: 1000,
            max_search_depth: 32,
            max_search_scan_bytes: 8 * 1024 * 1024,
            max_search_matches: 200,
        }
    }
}

impl WorkspaceToolLimits {
    /// Reject zero bounds and increases beyond the supported hard ceilings.
    pub fn validate(&self) -> Result<(), DefinitionError> {
        let maximum = Self::default();
        let bounded = [
            (
                self.max_concurrent_operations,
                maximum.max_concurrent_operations,
            ),
            (self.max_output_bytes, maximum.max_output_bytes),
            (self.max_directory_entries, maximum.max_directory_entries),
            (self.max_file_scan_bytes, maximum.max_file_scan_bytes),
            (self.max_search_files, maximum.max_search_files),
            (self.max_search_depth, maximum.max_search_depth),
            (self.max_search_scan_bytes, maximum.max_search_scan_bytes),
            (self.max_search_matches, maximum.max_search_matches),
        ];
        if self.max_output_bytes < 1024
            || self.call_timeout_secs == 0
            || self.call_timeout_secs > maximum.call_timeout_secs
            || bounded
                .iter()
                .any(|(value, ceiling)| *value == 0 || value > ceiling)
        {
            return Err(DefinitionError::InvalidToolLimits);
        }
        Ok(())
    }
}

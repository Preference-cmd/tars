use crate::definition::{WorkspaceTool, WorkspaceToolLimits, WriteToolLimits};
use crate::workspace::Workspace;
use serde::Deserialize;
use std::borrow::Cow;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListParams {
    pub path: String,
    pub limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReadParams {
    pub path: String,
    pub start_line: Option<usize>,
    pub max_lines: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SearchParams {
    pub path: String,
    pub query: String,
    pub max_matches: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WriteParams {
    pub path: String,
    pub content: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditSegment {
    pub old_text: String,
    pub new_text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EditParams {
    pub path: String,
    pub edits: Vec<EditSegment>,
}

pub(crate) enum Params {
    List(ListParams),
    Read(ReadParams),
    Search(SearchParams),
    Write(WriteParams),
    Edit(EditParams),
}

impl Params {
    pub fn decode(
        kind: WorkspaceTool,
        input: &serde_json::Value,
        limits: &WorkspaceToolLimits,
        write_limits: &WriteToolLimits,
    ) -> Result<Self, Cow<'static, str>> {
        let object = input
            .as_object()
            .ok_or(Cow::Borrowed("parameters must be an object"))?;
        let allowed: &[&str] = match kind {
            WorkspaceTool::ListDirectory => &["path", "limit"],
            WorkspaceTool::ReadFile => &["path", "start_line", "max_lines"],
            WorkspaceTool::SearchText => &["path", "query", "max_matches"],
            WorkspaceTool::WriteFile => &["path", "content"],
            WorkspaceTool::EditFile => &["path", "edits"],
        };
        if object.len() > allowed.len() || object.keys().any(|key| !allowed.contains(&key.as_str()))
        {
            return Err(Cow::Borrowed("unknown tool parameter"));
        }
        for (key, value) in object {
            match key.as_str() {
                "path" if value.as_str().is_none_or(|v| v.len() > 4096) => {
                    return Err(Cow::Borrowed("path must be a bounded string"));
                }
                "query" if value.as_str().is_none_or(|v| v.len() > 1024) => {
                    return Err(Cow::Borrowed("query must be a bounded string"));
                }
                "content" if value.as_str().is_none() => {
                    return Err(Cow::Borrowed("content must be a string"));
                }
                "edits" if value.as_array().is_none() => {
                    return Err(Cow::Borrowed("edits must be an array"));
                }
                "path" | "query" | "content" | "edits" => {}
                _ if value.as_u64().is_none() => {
                    return Err(Cow::Borrowed("limits must be positive integers"));
                }
                _ => {}
            }
        }
        let invalid = |_| Cow::Borrowed("invalid tool parameters");
        let params = match kind {
            WorkspaceTool::ListDirectory => {
                Self::List(ListParams::deserialize(input).map_err(invalid)?)
            }
            WorkspaceTool::ReadFile => Self::Read(ReadParams::deserialize(input).map_err(invalid)?),
            WorkspaceTool::SearchText => {
                Self::Search(SearchParams::deserialize(input).map_err(invalid)?)
            }
            WorkspaceTool::WriteFile => {
                Self::Write(WriteParams::deserialize(input).map_err(invalid)?)
            }
            WorkspaceTool::EditFile => Self::Edit(EditParams::deserialize(input).map_err(invalid)?),
        };
        let path = match &params {
            Self::List(p) => &p.path,
            Self::Read(p) => &p.path,
            Self::Search(p) => &p.path,
            Self::Write(p) => &p.path,
            Self::Edit(p) => &p.path,
        };
        Workspace::components(path)
            .map_err(|_| Cow::Borrowed("invalid workspace-relative path"))?;
        match &params {
            Self::List(p)
                if p.limit
                    .is_some_and(|n| n == 0 || n > limits.max_directory_entries) =>
            {
                return Err(Cow::Borrowed("directory limit exceeds configured bound"));
            }
            Self::Read(p)
                if p.start_line == Some(0) || p.max_lines.is_some_and(|n| n == 0 || n > 2000) =>
            {
                return Err(Cow::Borrowed("line range exceeds configured bound"));
            }
            Self::Read(_) | Self::Write(_) | Self::Edit(_)
                if Workspace::components(path).is_ok_and(|p| p.is_empty()) =>
            {
                return Err(Cow::Borrowed("tool requires a file path"));
            }
            Self::Search(p)
                if p.query.is_empty()
                    || p.query.len() > 1024
                    || p.query.contains('\0')
                    || p.query.contains(['\r', '\n'])
                    || p.max_matches
                        .is_some_and(|n| n == 0 || n > limits.max_search_matches) =>
            {
                return Err(Cow::Borrowed(
                    "search query or match limit exceeds configured bound",
                ));
            }
            Self::Write(p) => {
                if p.content.len() > write_limits.max_write_bytes {
                    return Err(Cow::Owned(format!(
                        "content size ({} bytes) exceeds maximum write size of {} bytes",
                        p.content.len(),
                        write_limits.max_write_bytes
                    )));
                }
            }
            Self::Edit(p) => {
                if p.edits.is_empty() {
                    return Err(Cow::Borrowed("edits must contain at least one replacement"));
                }
                if p.edits.len() > write_limits.max_edit_segments {
                    return Err(Cow::Owned(format!(
                        "edits count ({}) exceeds maximum allowed segments of {}",
                        p.edits.len(),
                        write_limits.max_edit_segments
                    )));
                }
            }
            _ => {}
        }
        Ok(params)
    }
}

use crate::definition::{WorkspaceTool, WorkspaceToolLimits};
use crate::workspace::Workspace;
use serde::Deserialize;

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

pub(crate) enum Params {
    List(ListParams),
    Read(ReadParams),
    Search(SearchParams),
}
impl Params {
    pub fn decode(
        kind: WorkspaceTool,
        input: &serde_json::Value,
        limits: &WorkspaceToolLimits,
    ) -> Result<Self, &'static str> {
        let object = input.as_object().ok_or("parameters must be an object")?;
        let allowed: &[&str] = match kind {
            WorkspaceTool::ListDirectory => &["path", "limit"],
            WorkspaceTool::ReadFile => &["path", "start_line", "max_lines"],
            WorkspaceTool::SearchText => &["path", "query", "max_matches"],
        };
        if object.len() > allowed.len() || object.keys().any(|key| !allowed.contains(&key.as_str()))
        {
            return Err("unknown tool parameter");
        }
        for (key, value) in object {
            match key.as_str() {
                "path" if value.as_str().is_none_or(|v| v.len() > 4096) => {
                    return Err("path must be a bounded string");
                }
                "query" if value.as_str().is_none_or(|v| v.len() > 1024) => {
                    return Err("query must be a bounded string");
                }
                "path" | "query" => {}
                _ if value.as_u64().is_none() => return Err("limits must be positive integers"),
                _ => {}
            }
        }
        let invalid = |_| "invalid tool parameters";
        let params = match kind {
            WorkspaceTool::ListDirectory => {
                Self::List(ListParams::deserialize(input).map_err(invalid)?)
            }
            WorkspaceTool::ReadFile => Self::Read(ReadParams::deserialize(input).map_err(invalid)?),
            WorkspaceTool::SearchText => {
                Self::Search(SearchParams::deserialize(input).map_err(invalid)?)
            }
        };
        let path = match &params {
            Self::List(p) => &p.path,
            Self::Read(p) => &p.path,
            Self::Search(p) => &p.path,
        };
        Workspace::components(path).map_err(|_| "invalid workspace-relative path")?;
        match &params {
            Self::List(p)
                if p.limit
                    .is_some_and(|n| n == 0 || n > limits.max_directory_entries) =>
            {
                return Err("directory limit exceeds configured bound");
            }
            Self::Read(p)
                if p.start_line == Some(0) || p.max_lines.is_some_and(|n| n == 0 || n > 2000) =>
            {
                return Err("line range exceeds configured bound");
            }
            Self::Read(_) if Workspace::components(path).is_ok_and(|p| p.is_empty()) => {
                return Err("read_file requires a file path");
            }
            Self::Search(p)
                if p.query.is_empty()
                    || p.query.len() > 1024
                    || p.query.contains('\0')
                    || p.query.contains(['\r', '\n'])
                    || p.max_matches
                        .is_some_and(|n| n == 0 || n > limits.max_search_matches) =>
            {
                return Err("search query or match limit exceeds configured bound");
            }
            _ => {}
        }
        Ok(params)
    }
}

use super::parameters::{ListParams, Params, ReadParams, SearchParams};
use super::resources::Counters;
use crate::definition::WorkspaceToolLimits;
use crate::workspace::{Workspace, WorkspaceError};
use causa::kernel::{CallControl, ControlError, ToolResultStatus};
use serde_json::{Value, json};
use std::io::Read;
use std::sync::{Arc, atomic::Ordering};
use tokio_util::sync::CancellationToken;

pub(crate) struct OperationControl {
    pub call: CallControl,
    pub dropstop: CancellationToken,
    pub counters: Arc<Counters>,
    pub host_counters: Arc<Counters>,
}
impl OperationControl {
    pub fn check(&self) -> Result<(), OperationError> {
        if self.dropstop.is_cancelled() {
            return Err(OperationError::Control(ControlError::Cancelled));
        }
        self.call.check().map_err(OperationError::Control)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum OperationError {
    #[error("{0}")]
    Access(#[from] WorkspaceError),
    #[error("{0}")]
    Control(#[from] ControlError),
    #[error("file content is binary or invalid UTF-8")]
    UnsupportedContent,
}
impl OperationError {
    pub fn status(&self) -> ToolResultStatus {
        match self {
            Self::Access(WorkspaceError::InvalidPath | WorkspaceError::Forbidden) => {
                ToolResultStatus::Rejected
            }
            Self::Control(ControlError::Cancelled) => ToolResultStatus::Cancelled,
            Self::Control(ControlError::TimedOut) => ToolResultStatus::TimedOut,
            _ => ToolResultStatus::Failed,
        }
    }
}

pub(crate) fn run(
    workspace: &Workspace,
    params: &Params,
    limits: &WorkspaceToolLimits,
    control: &OperationControl,
) -> Result<Value, OperationError> {
    control.check()?;
    #[cfg(unix)]
    match params {
        Params::List(p) => list(workspace, p, limits, control),
        Params::Read(p) => read(workspace, p, limits, control),
        Params::Search(p) => search(workspace, p, limits, control),
    }
    #[cfg(not(unix))]
    {
        let _ = (workspace, params, limits);
        Err(WorkspaceError::UnsupportedPlatform.into())
    }
}

fn fits(value: &Value, limit: usize) -> bool {
    serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() <= limit.saturating_sub(1024))
}

#[cfg(unix)]
fn list(
    workspace: &Workspace,
    p: &ListParams,
    limits: &WorkspaceToolLimits,
    control: &OperationControl,
) -> Result<Value, OperationError> {
    use rustix::fs::{Dir, FileType};
    let fd = workspace.directory(&p.path)?;
    let mut dir = Dir::new(fd).map_err(crate::workspace::map_errno)?;
    let limit = p.limit.unwrap_or(limits.max_directory_entries);
    let mut output = json!({"path":p.path,"entries":[],"truncated":false,"limit_reason":null});
    let mut count = 0;
    while let Some(entry) = dir.read() {
        control.check()?;
        let entry = entry.map_err(crate::workspace::map_errno)?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        if count == limit {
            output["truncated"] = json!(true);
            output["limit_reason"] = json!("directory_entries");
            break;
        }
        let name = match name.to_str() {
            Ok(name) => name,
            Err(_) => {
                output["truncated"] = json!(true);
                output["limit_reason"] = json!("non_utf8_entry");
                break;
            }
        };
        let kind = match entry.file_type() {
            FileType::RegularFile => "file",
            FileType::Directory => "directory",
            FileType::Symlink => "symlink",
            _ => "other",
        };
        let entries = output["entries"].as_array_mut().expect("entries array");
        entries.push(json!({"name":name,"type":kind}));
        if !fits(&output, limits.max_output_bytes) {
            output["entries"]
                .as_array_mut()
                .expect("entries array")
                .pop();
            output["truncated"] = json!(true);
            output["limit_reason"] = json!("output_bytes");
            break;
        }
        count += 1;
    }
    Ok(output)
}

fn bounded_text(
    mut file: std::fs::File,
    max_bytes: usize,
    control: &OperationControl,
    scanned: &mut usize,
) -> Result<(String, usize, bool), OperationError> {
    let mut bytes = Vec::with_capacity(max_bytes.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut eof = false;
    while bytes.len() < max_bytes {
        control.check()?;
        let count = file
            .read(&mut buffer[..(max_bytes - bytes.len()).min(8192)])
            .map_err(WorkspaceError::Io)?;
        control.counters.bytes.fetch_add(count, Ordering::Relaxed);
        control
            .host_counters
            .bytes
            .fetch_add(count, Ordering::Relaxed);
        *scanned += count;
        if count == 0 {
            eof = true;
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    control.check()?;
    let count = bytes.len();
    if bytes.contains(&0) {
        return Err(OperationError::UnsupportedContent);
    }
    if let Err(error) = std::str::from_utf8(&bytes) {
        // A scan limit may bisect the final scalar, but never masks an invalid interior byte.
        if !eof && error.error_len().is_none() {
            bytes.truncate(error.valid_up_to());
        } else {
            return Err(OperationError::UnsupportedContent);
        }
    }
    Ok((
        String::from_utf8(bytes).map_err(|_| OperationError::UnsupportedContent)?,
        count,
        !eof,
    ))
}

#[cfg(unix)]
fn read(
    workspace: &Workspace,
    p: &ReadParams,
    limits: &WorkspaceToolLimits,
    control: &OperationControl,
) -> Result<Value, OperationError> {
    let mut scanned = 0;
    let (text, _, scan_limited) = bounded_text(
        workspace.file(&p.path)?,
        limits.max_file_scan_bytes,
        control,
        &mut scanned,
    )?;
    let start = p.start_line.unwrap_or(1);
    let max_lines = p.max_lines.unwrap_or(200);
    let mut output = json!({"path":p.path,"start_line":start,"end_line":null,"text":"","scanned_bytes":scanned,"truncated":scan_limited,"limit_reason":if scan_limited { Some("file_scan_bytes") } else { None }});
    let mut content = String::new();
    let mut lines = 0;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        control.check()?;
        if index < start - 1 {
            continue;
        }
        if lines == max_lines {
            output["truncated"] = json!(true);
            output["limit_reason"] = json!("max_lines");
            break;
        }
        // A full long line is never copied into the output candidate. Scalar appends
        // are charged conservatively for maximum JSON escaping before allocation.
        let remaining = limits
            .max_output_bytes
            .saturating_sub(1024 + p.path.len() * 6 + content.len() * 6);
        let mut shortened = false;
        let mut taken = 0;
        for character in line.chars() {
            if taken + character.len_utf8() > remaining / 6 {
                shortened = true;
                break;
            }
            content.push(character);
            taken += character.len_utf8();
        }
        lines += 1;
        output["end_line"] = json!(index + 1);
        if shortened {
            output["truncated"] = json!(true);
            output["limit_reason"] = json!("output_bytes");
            break;
        }
    }
    output["text"] = json!(content);
    Ok(output)
}

#[cfg(unix)]
struct Search<'a> {
    workspace: &'a Workspace,
    params: &'a SearchParams,
    limits: &'a WorkspaceToolLimits,
    control: &'a OperationControl,
    output: Value,
    files: usize,
    entries: usize,
    bytes: usize,
    skipped: usize,
    stop: bool,
}

#[cfg(unix)]
impl Search<'_> {
    fn incomplete(&mut self, reason: &str) {
        self.output["complete"] = json!(false);
        self.output["truncated"] = json!(true);
        self.output["limit_reason"] = json!(reason);
    }
    fn issue(&mut self, path: &str, reason: &str) {
        self.skipped += 1;
        self.output["complete"] = json!(false);
        let issues = self.output["issues"].as_array_mut().expect("issues array");
        if issues.len() < 32 {
            issues.push(json!({"path":path,"reason":reason}));
            if !fits(&self.output, self.limits.max_output_bytes) {
                self.output["issues"]
                    .as_array_mut()
                    .expect("issues array")
                    .pop();
                self.incomplete("output_bytes");
                self.stop = true;
            }
        }
    }
    fn file(
        &mut self,
        parent: &rustix::fd::OwnedFd,
        name: &str,
        path: &str,
    ) -> Result<(), OperationError> {
        if self.files >= self.limits.max_search_files {
            self.incomplete("search_files");
            self.stop = true;
            return Ok(());
        }
        let remaining = self.limits.max_search_scan_bytes.saturating_sub(self.bytes);
        if remaining == 0 {
            self.incomplete("search_scan_bytes");
            self.stop = true;
            return Ok(());
        }
        self.files += 1;
        let mut scanned = 0;
        let outcome = self
            .workspace
            .child_file(parent, name)
            .map_err(OperationError::Access)
            .and_then(|file| {
                bounded_text(
                    file,
                    remaining.min(self.limits.max_file_scan_bytes),
                    self.control,
                    &mut scanned,
                )
            });
        self.bytes += scanned;
        match outcome {
            Ok((text, _, truncated)) => {
                if truncated {
                    self.issue(path, "file_scan_bytes");
                }
                for (index, line) in text.lines().enumerate() {
                    self.control.check()?;
                    let Some(position) = line.find(&self.params.query) else {
                        continue;
                    };
                    let count = self.output["matches"]
                        .as_array()
                        .expect("matches array")
                        .len();
                    if count
                        >= self
                            .params
                            .max_matches
                            .unwrap_or(self.limits.max_search_matches)
                    {
                        self.incomplete("max_matches");
                        self.stop = true;
                        break;
                    }
                    // Snippet generation remains bounded even for a multi-megabyte line.
                    let mut start = position.saturating_sub(128);
                    while !line.is_char_boundary(start) {
                        start += 1;
                    }
                    let mut end = (position + self.params.query.len() + 128).min(line.len());
                    while !line.is_char_boundary(end) {
                        end -= 1;
                    }
                    let snippet = &line[start..end];
                    self.output["matches"].as_array_mut().expect("matches array").push(json!({"path":path,"line":index+1,"text":snippet,"snippet_truncated":snippet.len()<line.len()}));
                    if !fits(&self.output, self.limits.max_output_bytes) {
                        self.output["matches"]
                            .as_array_mut()
                            .expect("matches array")
                            .pop();
                        self.incomplete("output_bytes");
                        self.stop = true;
                        break;
                    }
                }
            }
            Err(OperationError::Control(error)) => return Err(OperationError::Control(error)),
            Err(_) => {
                self.issue(path, "unreadable_or_unsupported");
            }
        }
        Ok(())
    }
    fn directory(
        &mut self,
        fd: rustix::fd::OwnedFd,
        path: &str,
        depth: usize,
    ) -> Result<(), OperationError> {
        use rustix::fs::{AtFlags, Dir, FileType, statat};
        if depth > self.limits.max_search_depth {
            self.issue(path, "search_depth");
            return Ok(());
        }
        // A fresh descriptor is owned by the iterator; a separate descriptor opens
        // children. At most two handles per active depth are retained.
        let iterator_fd = self.workspace.child_directory(&fd, ".")?;
        let mut dir = Dir::new(iterator_fd).map_err(crate::workspace::map_errno)?;
        loop {
            self.control.check()?;
            if self.stop {
                break;
            }
            if self.entries == 4000 {
                self.incomplete("search_entries");
                self.stop = true;
                break;
            }
            let Some(entry) = dir.read() else {
                break;
            };
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    self.issue(path, "directory_io");
                    break;
                }
            };
            if matches!(entry.file_name().to_bytes(), b"." | b"..") {
                continue;
            }
            self.entries += 1;
            let name = match entry.file_name().to_str() {
                Ok(name) => name,
                Err(_) => {
                    self.issue(path, "non_utf8_entry");
                    continue;
                }
            };
            let child = if path == "." {
                name.to_owned()
            } else {
                format!("{path}/{name}")
            };
            if child.len() > 4096 {
                self.issue(path, "path_bytes");
                continue;
            }
            let kind = match statat(&fd, name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(stat) => FileType::from_raw_mode(stat.st_mode),
                Err(_) => {
                    self.issue(&child, "entry_io");
                    continue;
                }
            };
            match kind {
                FileType::Directory => match self.workspace.child_directory(&fd, name) {
                    Ok(child_fd) => self.directory(child_fd, &child, depth + 1)?,
                    Err(_) => self.issue(&child, "directory_access"),
                },
                FileType::RegularFile => self.file(&fd, name, &child)?,
                _ => self.issue(&child, "symlink_or_special_file"),
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn search(
    workspace: &Workspace,
    p: &SearchParams,
    limits: &WorkspaceToolLimits,
    control: &OperationControl,
) -> Result<Value, OperationError> {
    let path = Workspace::components(&p.path)?.join("/");
    let path = if path.is_empty() { ".".into() } else { path };
    let fd = workspace.directory(&path)?;
    let mut search = Search {
        workspace,
        params: p,
        limits,
        control,
        output: json!({"path":path,"matches":[],"issues":[],"complete":true,"truncated":false,"limit_reason":null}),
        files: 0,
        entries: 0,
        bytes: 0,
        skipped: 0,
        stop: false,
    };
    search.directory(fd, &path, 0)?;
    search.output["scanned_files"] = json!(search.files);
    search.output["scanned_bytes"] = json!(search.bytes);
    search.output["scanned_entries"] = json!(search.entries);
    search.output["skipped"] = json!(search.skipped);
    Ok(search.output)
}

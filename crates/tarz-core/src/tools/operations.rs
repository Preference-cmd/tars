use super::PreparedWrite;
use super::compute_diff;
use super::parameters::{
    EditParams, EditSegment, ListParams, Params, ReadParams, SearchParams, WriteParams,
};
use super::resources::Counters;
use crate::definition::{WorkspaceToolLimits, WriteToolLimits};
use crate::permission::WriteAction;
use crate::workspace::{FileVersion, Workspace, WorkspaceError};
use causa::kernel::{CallControl, ControlError, ToolResultStatus};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex, atomic::Ordering};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use super::execute::TestGate;

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
    #[error("{0}")]
    Rejected(String),
    #[error("{0}")]
    Failed(String),
}
impl OperationError {
    pub fn status(&self) -> ToolResultStatus {
        match self {
            Self::Access(WorkspaceError::InvalidPath | WorkspaceError::Forbidden) => {
                ToolResultStatus::Rejected
            }
            Self::Control(ControlError::Cancelled) => ToolResultStatus::Cancelled,
            Self::Control(ControlError::TimedOut) => ToolResultStatus::TimedOut,
            Self::Rejected(_) => ToolResultStatus::Rejected,
            Self::Failed(_) => ToolResultStatus::Failed,
            _ => ToolResultStatus::Failed,
        }
    }
}

pub(crate) fn run(
    workspace: &Workspace,
    params: &Params,
    limits: &WorkspaceToolLimits,
    write_limits: &WriteToolLimits,
    observations: &Mutex<HashMap<String, FileVersion>>,
    control: &OperationControl,
    #[cfg(test)] write_gate: &Mutex<Option<Arc<TestGate>>>,
) -> Result<Value, OperationError> {
    control.check()?;
    #[cfg(unix)]
    match params {
        Params::List(p) => list(workspace, p, limits, control),
        Params::Read(p) => read(workspace, p, limits, observations, control),
        Params::Search(p) => search(workspace, p, limits, control),
        Params::Write(p) => write_operation(
            workspace,
            p,
            limits,
            write_limits,
            observations,
            control,
            #[cfg(test)]
            write_gate,
        ),
        Params::Edit(p) => edit_operation(
            workspace,
            p,
            limits,
            write_limits,
            observations,
            control,
            #[cfg(test)]
            write_gate,
        ),
    }
    #[cfg(not(unix))]
    {
        let _ = (
            workspace,
            params,
            limits,
            write_limits,
            observations,
            control,
        );
        Err(WorkspaceError::UnsupportedPlatform.into())
    }
}

fn fits(value: &Value, limit: usize) -> bool {
    serde_json::to_vec(value).is_ok_and(|bytes| bytes.len() <= limit.saturating_sub(1024))
}

#[cfg(unix)]
fn is_crlf(content: &str) -> bool {
    let bytes = content.as_bytes();
    let has_crlf = bytes.windows(2).any(|w| w == b"\r\n");
    if !has_crlf {
        return false;
    }
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'\n' && (i == 0 || bytes[i - 1] != b'\r') {
            return false;
        }
    }
    true
}

#[cfg(unix)]
fn normalize_lone_lf_to_crlf(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut prev = '\0';
    for ch in s.chars() {
        if ch == '\n' && prev != '\r' {
            out.push('\r');
        }
        out.push(ch);
        prev = ch;
    }
    out
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
    observations: &Mutex<HashMap<String, FileVersion>>,
    control: &OperationControl,
) -> Result<Value, OperationError> {
    let file = workspace.file(&p.path)?;
    let stat = rustix::fs::fstat(&file).map_err(crate::workspace::map_errno)?;
    let mut scanned = 0;
    let (text, _, scan_limited) =
        bounded_text(file, limits.max_file_scan_bytes, control, &mut scanned)?;
    let version = FileVersion::from_stat_and_content(&stat, text.as_bytes());
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
    if !scan_limited {
        let normalized = Workspace::components(&p.path)?.join("/");
        observations.lock().unwrap().insert(normalized, version);
    }
    Ok(output)
}

#[cfg(unix)]
pub(crate) fn precheck_write(
    workspace: &Workspace,
    p: &WriteParams,
    limits: &WorkspaceToolLimits,
    write_limits: &WriteToolLimits,
    observations: &Mutex<HashMap<String, FileVersion>>,
    control: &OperationControl,
) -> Result<PreparedWrite, OperationError> {
    control.check()?;
    let components = Workspace::components(&p.path)?;
    if components.is_empty() {
        return Err(OperationError::Rejected(
            "write_file requires a file path".into(),
        ));
    }
    let normalized = components.join("/");
    let parent_components: Vec<String> = components[..components.len() - 1]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let file_name = components[components.len() - 1].to_string();

    if p.content.len() > write_limits.max_write_bytes {
        return Err(OperationError::Rejected(format!(
            "content size ({} bytes) exceeds maximum write size of {} bytes",
            p.content.len(),
            write_limits.max_write_bytes
        )));
    }

    let parent_dir_opt = match workspace.parent_directory(&parent_components) {
        Ok(parent_dir) => Some(parent_dir),
        Err(WorkspaceError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => None,
        Err(err) => return Err(OperationError::Access(err)),
    };

    let existing_target = match parent_dir_opt {
        Some(parent_dir) => match workspace.child_file(&parent_dir, &file_name) {
            Ok(file) => Some((parent_dir, file)),
            Err(WorkspaceError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(OperationError::Access(err)),
        },
        None => None,
    };

    let (original_text, final_content, action, existing_mode, expected_version) = if let Some((
        _parent_dir,
        mut target_file,
    )) =
        existing_target
    {
        let stat = rustix::fs::fstat(&target_file).map_err(crate::workspace::map_errno)?;
        let recorded_version = {
            let obs = observations.lock().unwrap();
            obs.get(&normalized).copied()
        };
        let Some(recorded_version) = recorded_version else {
            return Err(OperationError::Rejected(format!(
                "file '{}' already exists and must be read with read_file before overwriting",
                p.path
            )));
        };

        if (stat.st_size as usize) > limits.max_file_scan_bytes {
            return Err(OperationError::Rejected(format!(
                "file '{}' size ({} bytes) exceeds maximum readable size of {} bytes and cannot be edited",
                p.path, stat.st_size, limits.max_file_scan_bytes
            )));
        }
        if (stat.st_size as usize) > write_limits.max_edit_file_bytes {
            return Err(OperationError::Rejected(format!(
                "file '{}' size ({} bytes) exceeds maximum editable size of {} bytes",
                p.path, stat.st_size, write_limits.max_edit_file_bytes
            )));
        }

        let mut original_bytes = Vec::new();
        target_file
            .read_to_end(&mut original_bytes)
            .map_err(WorkspaceError::Io)?;
        let original_text = String::from_utf8(original_bytes).map_err(|_| {
            OperationError::Rejected("file content is binary or invalid UTF-8".into())
        })?;
        let current_version = FileVersion::from_stat_and_content(&stat, original_text.as_bytes());
        if current_version != recorded_version {
            return Err(OperationError::Rejected(format!(
                "file '{}' has changed since it was read; read it again before overwriting",
                p.path
            )));
        }

        let final_content = if is_crlf(&original_text) {
            normalize_lone_lf_to_crlf(&p.content)
        } else {
            p.content.clone()
        };

        if final_content.len() > write_limits.max_write_bytes {
            return Err(OperationError::Rejected(format!(
                "content size ({} bytes) exceeds maximum write size of {} bytes",
                final_content.len(),
                write_limits.max_write_bytes
            )));
        }

        (
            original_text,
            final_content,
            WriteAction::Updated,
            Some((stat.st_mode & 0o7777) as u32),
            Some(current_version),
        )
    } else {
        (
            String::new(),
            p.content.clone(),
            WriteAction::Created,
            None,
            None,
        )
    };

    let diff_summary = compute_diff(&p.path, &original_text, &final_content);
    let bytes_written = final_content.len();

    Ok(PreparedWrite {
        path: p.path.clone(),
        normalized_path: normalized,
        expected_version,
        existing_mode,
        final_content,
        diff: diff_summary.diff.clone(),
        diff_summary,
        action,
        parent_components,
        file_name,
        bytes_written,
    })
}

#[cfg(unix)]
pub(crate) fn precheck_edit(
    workspace: &Workspace,
    p: &EditParams,
    limits: &WorkspaceToolLimits,
    write_limits: &WriteToolLimits,
    observations: &Mutex<HashMap<String, FileVersion>>,
    control: &OperationControl,
) -> Result<PreparedWrite, OperationError> {
    control.check()?;
    let components = Workspace::components(&p.path)?;
    if components.is_empty() {
        return Err(OperationError::Rejected(
            "edit_file requires a file path".into(),
        ));
    }
    let normalized = components.join("/");
    let parent_components: Vec<String> = components[..components.len() - 1]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let file_name = components[components.len() - 1].to_string();

    if p.edits.is_empty() {
        return Err(OperationError::Rejected(
            "edits must contain at least one replacement".into(),
        ));
    }
    if p.edits.len() > write_limits.max_edit_segments {
        return Err(OperationError::Rejected(format!(
            "edits count ({}) exceeds maximum allowed segments of {}",
            p.edits.len(),
            write_limits.max_edit_segments
        )));
    }

    let recorded_version = {
        let obs = observations.lock().unwrap();
        obs.get(&normalized).copied()
    };
    let Some(recorded_version) = recorded_version else {
        return Err(OperationError::Rejected(format!(
            "file '{}' must be read with read_file before editing",
            p.path
        )));
    };

    let parent_dir = match workspace.parent_directory(&parent_components) {
        Ok(dir) => dir,
        Err(WorkspaceError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(OperationError::Rejected(format!(
                "file '{}' has changed since it was read; read it again before editing",
                p.path
            )));
        }
        Err(err) => return Err(OperationError::Access(err)),
    };

    let mut target_file = match workspace.child_file(&parent_dir, &file_name) {
        Ok(file) => file,
        Err(WorkspaceError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(OperationError::Rejected(format!(
                "file '{}' has changed since it was read; read it again before editing",
                p.path
            )));
        }
        Err(err) => return Err(OperationError::Access(err)),
    };

    let stat = rustix::fs::fstat(&target_file).map_err(crate::workspace::map_errno)?;

    if (stat.st_size as usize) > limits.max_file_scan_bytes {
        return Err(OperationError::Rejected(format!(
            "file '{}' size ({} bytes) exceeds maximum readable size of {} bytes and cannot be edited",
            p.path, stat.st_size, limits.max_file_scan_bytes
        )));
    }
    if (stat.st_size as usize) > write_limits.max_edit_file_bytes {
        return Err(OperationError::Rejected(format!(
            "file '{}' size ({} bytes) exceeds maximum editable size of {} bytes",
            p.path, stat.st_size, write_limits.max_edit_file_bytes
        )));
    }

    let mut original_bytes = Vec::new();
    target_file
        .read_to_end(&mut original_bytes)
        .map_err(WorkspaceError::Io)?;
    let original_text = String::from_utf8(original_bytes)
        .map_err(|_| OperationError::Rejected("file content is binary or invalid UTF-8".into()))?;
    let current_version = FileVersion::from_stat_and_content(&stat, original_text.as_bytes());
    if current_version != recorded_version {
        return Err(OperationError::Rejected(format!(
            "file '{}' has changed since it was read; read it again before editing",
            p.path
        )));
    }

    let is_crlf_file = is_crlf(&original_text);
    let edits: Vec<EditSegment> = if is_crlf_file {
        p.edits
            .iter()
            .map(|e| EditSegment {
                old_text: normalize_lone_lf_to_crlf(&e.old_text),
                new_text: normalize_lone_lf_to_crlf(&e.new_text),
            })
            .collect()
    } else {
        p.edits.clone()
    };

    struct ReplacementMatch {
        index: usize,
        start: usize,
        end: usize,
        new_text: String,
    }

    let mut matches = Vec::with_capacity(edits.len());
    for (i, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(OperationError::Rejected(format!(
                "edits[{}]: old_text must not be empty",
                i
            )));
        }
        if edit.old_text == edit.new_text {
            return Err(OperationError::Rejected(format!(
                "edits[{}]: old_text and new_text are identical",
                i
            )));
        }
        let occurrences: Vec<usize> = original_text
            .match_indices(&edit.old_text)
            .map(|(idx, _)| idx)
            .collect();
        if occurrences.is_empty() {
            return Err(OperationError::Rejected(format!(
                "edits[{}]: old_text not found in file",
                i
            )));
        }
        if occurrences.len() > 1 {
            return Err(OperationError::Rejected(format!(
                "edits[{}]: old_text matches multiple locations ({}) in file",
                i,
                occurrences.len()
            )));
        }
        let start = occurrences[0];
        let end = start + edit.old_text.len();
        matches.push(ReplacementMatch {
            index: i,
            start,
            end,
            new_text: edit.new_text.clone(),
        });
    }

    for a in 0..matches.len() {
        for b in (a + 1)..matches.len() {
            let m1 = &matches[a];
            let m2 = &matches[b];
            if m1.start < m2.end && m2.start < m1.end {
                let (first, second) = if m1.index < m2.index {
                    (m1.index, m2.index)
                } else {
                    (m2.index, m1.index)
                };
                return Err(OperationError::Rejected(format!(
                    "edits[{}] and edits[{}]: replacement ranges overlap",
                    first, second
                )));
            }
        }
    }

    matches.sort_by_key(|m| m.start);
    let mut new_content = String::with_capacity(original_text.len());
    let mut last_offset = 0;
    for m in &matches {
        new_content.push_str(&original_text[last_offset..m.start]);
        new_content.push_str(&m.new_text);
        last_offset = m.end;
    }
    new_content.push_str(&original_text[last_offset..]);

    if new_content.len() > write_limits.max_write_bytes {
        return Err(OperationError::Rejected(format!(
            "new content size ({} bytes) exceeds maximum write size of {} bytes",
            new_content.len(),
            write_limits.max_write_bytes
        )));
    }

    let diff_summary = compute_diff(&p.path, &original_text, &new_content);
    let bytes_written = new_content.len();

    Ok(PreparedWrite {
        path: p.path.clone(),
        normalized_path: normalized,
        expected_version: Some(current_version),
        existing_mode: Some((stat.st_mode & 0o7777) as u32),
        final_content: new_content,
        diff: diff_summary.diff.clone(),
        diff_summary,
        action: WriteAction::Updated,
        parent_components,
        file_name,
        bytes_written,
    })
}

#[cfg(unix)]
pub(crate) fn execute_prepared_write(
    workspace: &Workspace,
    prepared: &PreparedWrite,
    observations: &Mutex<HashMap<String, FileVersion>>,
    control: &OperationControl,
    #[cfg(test)] write_gate: &Mutex<Option<Arc<TestGate>>>,
) -> Result<Value, OperationError> {
    control.check()?;

    let parent_dir = workspace.ensure_parent_directory(&prepared.parent_components)?;

    if let Some(expected_version) = prepared.expected_version {
        let mut target_file = match workspace.child_file(&parent_dir, &prepared.file_name) {
            Ok(file) => file,
            Err(WorkspaceError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(OperationError::Failed(
                    "file changed after confirmation".into(),
                ));
            }
            Err(err) => return Err(OperationError::Access(err)),
        };
        let stat = rustix::fs::fstat(&target_file).map_err(crate::workspace::map_errno)?;
        let mut original_bytes = Vec::new();
        target_file
            .read_to_end(&mut original_bytes)
            .map_err(WorkspaceError::Io)?;
        let current_version = FileVersion::from_stat_and_content(&stat, &original_bytes);
        if current_version != expected_version {
            return Err(OperationError::Failed(
                "file changed after confirmation".into(),
            ));
        }
    } else {
        match workspace.child_file(&parent_dir, &prepared.file_name) {
            Ok(_) => {
                return Err(OperationError::Failed(
                    "target file already exists after confirmation".into(),
                ));
            }
            Err(WorkspaceError::Io(err)) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(OperationError::Access(err)),
        }
    }

    let write_res = workspace.atomic_write(
        &parent_dir,
        &prepared.file_name,
        prepared.final_content.as_bytes(),
        prepared.existing_mode,
        || {
            #[cfg(test)]
            if let Some(gate) = write_gate.lock().expect("write gate lock").clone() {
                gate.pause(false);
            }
            control.check()
        },
    );

    let new_version = match write_res {
        Ok(version) => version,
        Err(crate::workspace::AtomicWriteError::AlreadyExists) => {
            return Err(OperationError::Rejected(format!(
                "file '{}' already exists and must be read with read_file before overwriting",
                prepared.path
            )));
        }
        Err(crate::workspace::AtomicWriteError::CancelledOrTimedOut(err)) => return Err(err),
        Err(crate::workspace::AtomicWriteError::Workspace(err)) => {
            return Err(OperationError::Access(err));
        }
    };

    control
        .counters
        .bytes
        .fetch_add(prepared.bytes_written, Ordering::Relaxed);
    control
        .host_counters
        .bytes
        .fetch_add(prepared.bytes_written, Ordering::Relaxed);

    observations
        .lock()
        .unwrap()
        .insert(prepared.normalized_path.clone(), new_version);

    #[cfg(test)]
    if let Some(gate) = write_gate.lock().expect("write gate lock").clone() {
        gate.pause(true);
    }

    Ok(json!({
        "path": prepared.path,
        "action": prepared.action.as_str(),
        "bytes_written": prepared.bytes_written,
        "lines_added": prepared.diff_summary.lines_added,
        "lines_removed": prepared.diff_summary.lines_removed,
    }))
}

#[cfg(not(unix))]
pub(crate) fn precheck_write(
    _workspace: &Workspace,
    _p: &WriteParams,
    _limits: &WorkspaceToolLimits,
    _write_limits: &WriteToolLimits,
    _observations: &Mutex<HashMap<String, FileVersion>>,
    _control: &OperationControl,
) -> Result<PreparedWrite, OperationError> {
    Err(WorkspaceError::UnsupportedPlatform.into())
}

#[cfg(not(unix))]
pub(crate) fn precheck_edit(
    _workspace: &Workspace,
    _p: &EditParams,
    _limits: &WorkspaceToolLimits,
    _write_limits: &WriteToolLimits,
    _observations: &Mutex<HashMap<String, FileVersion>>,
    _control: &OperationControl,
) -> Result<PreparedWrite, OperationError> {
    Err(WorkspaceError::UnsupportedPlatform.into())
}

#[cfg(not(unix))]
pub(crate) fn execute_prepared_write(
    _workspace: &Workspace,
    _prepared: &PreparedWrite,
    _observations: &Mutex<HashMap<String, FileVersion>>,
    _control: &OperationControl,
    #[cfg(test)] _write_gate: &Mutex<Option<Arc<TestGate>>>,
) -> Result<Value, OperationError> {
    Err(WorkspaceError::UnsupportedPlatform.into())
}

#[cfg(unix)]
fn write_operation(
    workspace: &Workspace,
    p: &WriteParams,
    limits: &WorkspaceToolLimits,
    write_limits: &WriteToolLimits,
    observations: &Mutex<HashMap<String, FileVersion>>,
    control: &OperationControl,
    #[cfg(test)] write_gate: &Mutex<Option<Arc<TestGate>>>,
) -> Result<Value, OperationError> {
    let prepared = precheck_write(workspace, p, limits, write_limits, observations, control)?;
    execute_prepared_write(
        workspace,
        &prepared,
        observations,
        control,
        #[cfg(test)]
        write_gate,
    )
}

#[cfg(unix)]
fn edit_operation(
    workspace: &Workspace,
    p: &EditParams,
    limits: &WorkspaceToolLimits,
    write_limits: &WriteToolLimits,
    observations: &Mutex<HashMap<String, FileVersion>>,
    control: &OperationControl,
    #[cfg(test)] write_gate: &Mutex<Option<Arc<TestGate>>>,
) -> Result<Value, OperationError> {
    let prepared = precheck_edit(workspace, p, limits, write_limits, observations, control)?;
    execute_prepared_write(
        workspace,
        &prepared,
        observations,
        control,
        #[cfg(test)]
        write_gate,
    )
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

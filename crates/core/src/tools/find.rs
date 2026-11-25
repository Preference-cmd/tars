// Workspace tools run only on Unix; elsewhere the traversal path is unused.
#![cfg_attr(not(unix), allow(dead_code, unused_imports, unused_variables))]

use super::operations::{OperationControl, OperationError, json_escaped_len};
use super::parameters::FindFilesParams;
use crate::definition::WorkspaceToolLimits;
use crate::workspace::Workspace;
use globset::{GlobBuilder, GlobMatcher, GlobSet, GlobSetBuilder};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::Read;
use std::sync::{Arc, atomic::Ordering};

/// Hard ceiling on visited directory entries in a single `find_files` call.
pub(crate) const MAX_VISITED_ENTRIES: usize = 100_000;
/// Maximum byte size read from a single `.gitignore` or `.ignore` file (64 KiB).
const MAX_IGNORE_FILE_BYTES: usize = 64 * 1024;
/// Maximum non-fatal issues recorded in a single `find_files` response.
const MAX_ISSUES: usize = 32;
/// Maximum byte length of a single issue reason string.
const MAX_ISSUE_BYTES: usize = 512;
/// Check cancellation and deadline every N entries during directory scans.
const CONTROL_CHECK_INTERVAL: usize = 64;
/// Bytes reserved for the outer `ToolResultPayload` wrapper within `max_output_bytes`.
const PAYLOAD_RESERVE_BYTES: usize = 512;

/// Compile a user-supplied `find_files` glob pattern.
///
/// Returns `(matcher, has_slash)`. When `has_slash` is true, any leading `./`
/// prefix is stripped and the pattern matches paths relative to the search root;
/// otherwise the pattern matches only file basenames.
pub(crate) fn compile_user_glob(
    raw: &str,
    ignore_case: bool,
) -> Result<(GlobMatcher, bool), String> {
    if raw.is_empty() || raw.len() > 256 || raw.contains(['\0', '\r', '\n']) {
        return Err(
            "pattern must be a nonempty string of at most 256 bytes without NUL or newlines"
                .to_owned(),
        );
    }
    let has_slash = raw.contains('/');
    let effective = if has_slash {
        let mut trimmed = raw;
        while let Some(rest) = trimmed.strip_prefix("./") {
            trimmed = rest;
        }
        if trimmed.is_empty() {
            return Err("invalid glob pattern: empty after stripping './'".to_owned());
        }
        trimmed
    } else {
        raw
    };
    let glob = GlobBuilder::new(effective)
        .literal_separator(true)
        .case_insensitive(ignore_case)
        .backslash_escape(true)
        .build()
        .map_err(|err| format!("invalid glob pattern: {err}"))?;
    Ok((glob.compile_matcher(), has_slash))
}

#[derive(Clone, Copy)]
struct IgnoreRuleMeta {
    order: usize,
    negated: bool,
    dir_only: bool,
}

struct ParsedIgnoreLine {
    pattern: String,
    negated: bool,
    dir_only: bool,
    anchored: bool,
}

fn strip_unescaped_trailing_spaces(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut end = bytes.len();
    while end > 0 && bytes[end - 1] == b' ' {
        let mut backslashes = 0usize;
        let mut idx = end - 1;
        while idx > 0 && bytes[idx - 1] == b'\\' {
            backslashes += 1;
            idx -= 1;
        }
        if backslashes % 2 == 1 {
            break;
        }
        end -= 1;
    }
    &line[..end]
}

fn unescape_gitignore_prefix(pat: &str) -> String {
    let mut out = String::with_capacity(pat.len());
    let mut chars = pat.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\'
            && let Some(&next) = chars.peek()
        {
            if matches!(next, '#' | '!' | ' ') {
                // Gitignore escapes `#`, `!`, and ` ` with a single backslash;
                // strip the backslash so globset matches the literal character.
                out.push(next);
            } else {
                // Keep `\\` (escaped backslash) and glob metacharacter escapes
                // (`\*`, `\?`, `\[`, etc.) intact for `GlobBuilder::backslash_escape(true)`.
                out.push('\\');
                out.push(next);
            }
            chars.next();
            continue;
        }
        out.push(ch);
    }
    out
}

fn parse_ignore_line(raw_line: &str) -> Option<ParsedIgnoreLine> {
    let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = strip_unescaped_trailing_spaces(line);
    if line.is_empty() {
        return None;
    }
    let (negated, rest) = if let Some(after) = line.strip_prefix('!') {
        (true, after)
    } else {
        (false, line)
    };
    if rest.is_empty() {
        return None;
    }
    let dir_only = rest.ends_with('/') && !rest.ends_with("\\/");
    let mut pat = if dir_only {
        rest.trim_end_matches('/')
    } else {
        rest
    };
    if pat.is_empty() {
        return None;
    }
    let anchored = pat.contains('/');
    if anchored {
        pat = pat.trim_start_matches('/');
        if pat.is_empty() {
            return None;
        }
    }
    let pattern = unescape_gitignore_prefix(pat);
    if pattern.is_empty() {
        return None;
    }
    Some(ParsedIgnoreLine {
        pattern,
        negated,
        dir_only,
        anchored,
    })
}

struct DirIgnoreRules {
    base_rel: String,
    basename_set: GlobSet,
    basename_meta: Vec<IgnoreRuleMeta>,
    path_set: GlobSet,
    path_meta: Vec<IgnoreRuleMeta>,
}

impl DirIgnoreRules {
    fn build(base_rel: String, contents: &[&str]) -> Option<Self> {
        let mut basename_builder = GlobSetBuilder::new();
        let mut basename_meta = Vec::new();
        let mut path_builder = GlobSetBuilder::new();
        let mut path_meta = Vec::new();
        let mut order = 0usize;

        for text in contents {
            for raw_line in text.lines() {
                let Some(parsed) = parse_ignore_line(raw_line) else {
                    continue;
                };
                let Ok(glob) = GlobBuilder::new(&parsed.pattern)
                    .literal_separator(true)
                    .backslash_escape(true)
                    .build()
                else {
                    continue;
                };
                let meta = IgnoreRuleMeta {
                    order,
                    negated: parsed.negated,
                    dir_only: parsed.dir_only,
                };
                order += 1;
                if parsed.anchored {
                    path_builder.add(glob);
                    path_meta.push(meta);
                } else {
                    basename_builder.add(glob);
                    basename_meta.push(meta);
                }
            }
        }

        if basename_meta.is_empty() && path_meta.is_empty() {
            return None;
        }
        let basename_set = basename_builder.build().ok()?;
        let path_set = path_builder.build().ok()?;
        Some(Self {
            base_rel,
            basename_set,
            basename_meta,
            path_set,
            path_meta,
        })
    }

    fn matches(
        &self,
        basename: &str,
        rel_ws: &str,
        is_dir: bool,
        scratch: &mut Vec<usize>,
    ) -> Option<bool> {
        let rel_to_dir = if self.base_rel.is_empty() {
            rel_ws
        } else {
            rel_ws
                .strip_prefix(&self.base_rel)
                .and_then(|r| r.strip_prefix('/'))?
        };

        let mut best: Option<(usize, bool)> = None;
        if !self.basename_meta.is_empty() {
            self.basename_set.matches_into(basename, scratch);
            for &idx in scratch.iter() {
                let meta = self.basename_meta[idx];
                if (!meta.dir_only || is_dir) && best.is_none_or(|(order, _)| meta.order > order) {
                    best = Some((meta.order, !meta.negated));
                }
            }
        }
        if !self.path_meta.is_empty() {
            self.path_set.matches_into(rel_to_dir, scratch);
            for &idx in scratch.iter() {
                let meta = self.path_meta[idx];
                if (!meta.dir_only || is_dir) && best.is_none_or(|(order, _)| meta.order > order) {
                    best = Some((meta.order, !meta.negated));
                }
            }
        }
        best.map(|(_, ignored)| ignored)
    }
}

struct IgnoreNode {
    parent: Option<Arc<IgnoreNode>>,
    rules: DirIgnoreRules,
}

fn is_ignored(
    node: &Option<Arc<IgnoreNode>>,
    basename: &str,
    rel_ws: &str,
    is_dir: bool,
    scratch: &mut Vec<usize>,
) -> bool {
    let mut current = node.as_deref();
    while let Some(entry) = current {
        if let Some(ignored) = entry.rules.matches(basename, rel_ws, is_dir, scratch) {
            return ignored;
        }
        current = entry.parent.as_deref();
    }
    false
}

#[cfg(unix)]
fn read_ignore_file(
    workspace: &Workspace,
    dir_fd: &rustix::fd::OwnedFd,
    name: &str,
    control: &OperationControl,
) -> Result<Option<String>, OperationError> {
    control.check()?;
    let file = match workspace.child_file(dir_fd, name) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_IGNORE_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return Ok(None);
    }
    control
        .counters
        .bytes
        .fetch_add(bytes.len(), Ordering::Relaxed);
    control
        .host_counters
        .bytes
        .fetch_add(bytes.len(), Ordering::Relaxed);
    if bytes.len() > MAX_IGNORE_FILE_BYTES || bytes.contains(&0) {
        return Ok(None);
    }
    Ok(String::from_utf8(bytes).ok())
}

#[cfg(unix)]
fn load_dir_ignore(
    workspace: &Workspace,
    dir_fd: &rustix::fd::OwnedFd,
    base_rel: &str,
    parent: Option<Arc<IgnoreNode>>,
    control: &OperationControl,
) -> Result<Option<Arc<IgnoreNode>>, OperationError> {
    // `.gitignore` loads before `.ignore`, so `.ignore` takes precedence in the same directory.
    let gitignore = read_ignore_file(workspace, dir_fd, ".gitignore", control)?;
    let dotignore = read_ignore_file(workspace, dir_fd, ".ignore", control)?;
    let mut sources = Vec::with_capacity(2);
    if let Some(ref text) = gitignore {
        sources.push(text.as_str());
    }
    if let Some(ref text) = dotignore {
        sources.push(text.as_str());
    }
    if sources.is_empty() {
        return Ok(parent);
    }
    match DirIgnoreRules::build(base_rel.to_owned(), &sources) {
        Some(rules) => Ok(Some(Arc::new(IgnoreNode { parent, rules }))),
        None => Ok(parent),
    }
}

fn bounded_reason(reason: &str) -> String {
    if reason.len() <= MAX_ISSUE_BYTES {
        return reason.to_owned();
    }
    let mut end = MAX_ISSUE_BYTES;
    while end > 0 && !reason.is_char_boundary(end) {
        end -= 1;
    }
    reason[..end].to_owned()
}

fn push_issue(issues: &mut Vec<Value>, had_issues: &mut bool, path: &str, reason: &str) {
    *had_issues = true;
    if issues.len() < MAX_ISSUES {
        issues.push(json!({
            "path": path,
            "reason": bounded_reason(reason),
        }));
    }
}

#[cfg(unix)]
struct DirTask {
    rel_ws: String,
    rel_root: String,
    dir_depth: usize,
    ignore_node: Option<Arc<IgnoreNode>>,
    preopened: Option<rustix::fd::OwnedFd>,
}

#[cfg(unix)]
pub(crate) fn find_files(
    workspace: &Workspace,
    p: &FindFilesParams,
    limits: &WorkspaceToolLimits,
    control: &OperationControl,
) -> Result<Value, OperationError> {
    find_files_with_visited_limit(workspace, p, limits, control, MAX_VISITED_ENTRIES)
}

#[cfg(unix)]
pub(crate) fn find_files_with_visited_limit(
    workspace: &Workspace,
    p: &FindFilesParams,
    limits: &WorkspaceToolLimits,
    control: &OperationControl,
    max_visited_entries: usize,
) -> Result<Value, OperationError> {
    control.check()?;
    let raw_path = p.path.as_deref().unwrap_or(".");
    let components = Workspace::components(raw_path)?;
    if let Some(vcs) = Workspace::vcs_metadata_component(&components) {
        return Err(OperationError::Rejected(format!(
            "find_files does not search version-control metadata ({vcs})"
        )));
    }
    let normalized_path = if components.is_empty() {
        ".".to_owned()
    } else {
        components.join("/")
    };

    let ignore_case = p.ignore_case.unwrap_or(false);
    let include_ignored = p.include_ignored.unwrap_or(false);
    let max_results = p.max_results.unwrap_or(limits.max_search_matches);
    let max_depth = p.max_depth.unwrap_or(limits.max_search_depth);

    let matcher = match p.pattern.as_deref() {
        Some(pat) => Some(compile_user_glob(pat, ignore_case).map_err(OperationError::Rejected)?),
        None => None,
    };

    // Walk from `workspace.root` down each component of `path` to validate
    // handle-relative access and inherit ancestor `.gitignore`/`.ignore` rules.
    let mut current_fd = workspace.child_directory(&workspace.root, ".")?;
    let mut ancestor_ignore: Option<Arc<IgnoreNode>> = None;
    let mut ancestor_ignored = false;
    let mut walked_parts = Vec::with_capacity(components.len());
    let mut scratch = Vec::new();

    for comp in &components {
        control.check()?;
        if !include_ignored && !ancestor_ignored {
            let base_rel = walked_parts.join("/");
            ancestor_ignore =
                load_dir_ignore(workspace, &current_fd, &base_rel, ancestor_ignore, control)?;
            let next_rel = if base_rel.is_empty() {
                comp.clone()
            } else {
                format!("{base_rel}/{comp}")
            };
            if is_ignored(&ancestor_ignore, comp, &next_rel, true, &mut scratch) {
                ancestor_ignored = true;
            }
        }
        current_fd = workspace.child_directory(&current_fd, comp)?;
        walked_parts.push(comp.clone());
    }

    let mut files: Vec<String> = Vec::new();
    let mut issues: Vec<Value> = Vec::new();
    let mut had_issues = false;
    let mut scanned_dirs: usize = 0;
    let mut visited_entries: usize = 0;
    let mut truncated = false;
    let mut limit_reason: Option<&'static str> = None;

    let payload_budget = limits
        .max_output_bytes
        .saturating_sub(PAYLOAD_RESERVE_BYTES);
    let mut estimated_files_bytes: usize = 0;

    if !ancestor_ignored {
        let start_rel_ws = if components.is_empty() {
            String::new()
        } else {
            normalized_path.clone()
        };
        let mut queue = VecDeque::new();
        queue.push_back(DirTask {
            rel_ws: start_rel_ws,
            rel_root: String::new(),
            dir_depth: 0,
            ignore_node: ancestor_ignore,
            preopened: Some(current_fd),
        });

        'bfs: while let Some(task) = queue.pop_front() {
            control.check()?;
            let dir_fd = match task.preopened {
                Some(fd) => fd,
                None => {
                    let target = if task.rel_ws.is_empty() {
                        "."
                    } else {
                        task.rel_ws.as_str()
                    };
                    match workspace.directory(target) {
                        Ok(fd) => fd,
                        Err(err) => {
                            push_issue(&mut issues, &mut had_issues, target, &err.to_string());
                            continue;
                        }
                    }
                }
            };

            let ignore_node = if include_ignored {
                None
            } else {
                load_dir_ignore(workspace, &dir_fd, &task.rel_ws, task.ignore_node, control)?
            };

            let mut dir = match rustix::fs::Dir::new(dir_fd) {
                Ok(dir) => dir,
                Err(err) => {
                    let display = if task.rel_ws.is_empty() {
                        "."
                    } else {
                        &task.rel_ws
                    };
                    push_issue(
                        &mut issues,
                        &mut had_issues,
                        display,
                        &crate::workspace::map_errno(err).to_string(),
                    );
                    continue;
                }
            };

            scanned_dirs += 1;
            let mut entries: Vec<(String, crate::workspace::EntryKind)> = Vec::new();
            let mut read_idx = 0usize;

            while let Some(entry_res) = dir.read() {
                read_idx += 1;
                if read_idx.is_multiple_of(CONTROL_CHECK_INTERVAL) {
                    control.check()?;
                }
                let entry = match entry_res {
                    Ok(entry) => entry,
                    Err(err) => {
                        let display = if task.rel_ws.is_empty() {
                            "."
                        } else {
                            &task.rel_ws
                        };
                        push_issue(
                            &mut issues,
                            &mut had_issues,
                            display,
                            &crate::workspace::map_errno(err).to_string(),
                        );
                        break;
                    }
                };
                let name_cstr = entry.file_name();
                if name_cstr.to_bytes() == b"." || name_cstr.to_bytes() == b".." {
                    continue;
                }
                visited_entries += 1;
                if visited_entries > max_visited_entries {
                    truncated = true;
                    limit_reason = Some("visited_entries");
                    break 'bfs;
                }

                let name = match name_cstr.to_str() {
                    Ok(name) => name,
                    Err(_) => {
                        let lossy = name_cstr.to_string_lossy();
                        let display = if task.rel_ws.is_empty() {
                            lossy.into_owned()
                        } else {
                            format!("{}/{}", task.rel_ws, lossy)
                        };
                        push_issue(
                            &mut issues,
                            &mut had_issues,
                            &display,
                            "non-UTF-8 entry name",
                        );
                        continue;
                    }
                };

                if Workspace::is_vcs_metadata_name(name) {
                    continue;
                }

                let kind = match dir.fd() {
                    Ok(fd) => workspace.entry_kind_nofollow(fd, name_cstr, entry.file_type()),
                    Err(err) => Err(crate::workspace::map_errno(err)),
                };
                let kind = match kind {
                    Ok(kind) => kind,
                    Err(err) => {
                        let display = if task.rel_ws.is_empty() {
                            name.to_owned()
                        } else {
                            format!("{}/{}", task.rel_ws, name)
                        };
                        push_issue(&mut issues, &mut had_issues, &display, &err.to_string());
                        continue;
                    }
                };

                if kind != crate::workspace::EntryKind::Other {
                    entries.push((name.to_owned(), kind));
                }
            }

            // Close directory handle before queueing/processing sorted children.
            drop(dir);
            entries.sort_by(|a, b| a.0.cmp(&b.0));

            for (idx, (name, kind)) in entries.into_iter().enumerate() {
                if idx.is_multiple_of(CONTROL_CHECK_INTERVAL) {
                    control.check()?;
                }
                let rel_ws = if task.rel_ws.is_empty() {
                    name.clone()
                } else {
                    format!("{}/{}", task.rel_ws, name)
                };
                let rel_root = if task.rel_root.is_empty() {
                    name.clone()
                } else {
                    format!("{}/{}", task.rel_root, name)
                };
                let is_dir = kind == crate::workspace::EntryKind::Directory;
                if !include_ignored
                    && is_ignored(&ignore_node, &name, &rel_ws, is_dir, &mut scratch)
                {
                    continue;
                }

                match kind {
                    crate::workspace::EntryKind::Directory => {
                        if task.dir_depth + 1 < max_depth {
                            queue.push_back(DirTask {
                                rel_ws,
                                rel_root,
                                dir_depth: task.dir_depth + 1,
                                ignore_node: ignore_node.clone(),
                                preopened: None,
                            });
                        }
                    }
                    crate::workspace::EntryKind::RegularFile => {
                        let matched = match &matcher {
                            Some((glob, true)) => glob.is_match(&rel_root),
                            Some((glob, false)) => glob.is_match(&name),
                            None => true,
                        };
                        if !matched {
                            continue;
                        }
                        if files.len() == max_results {
                            truncated = true;
                            limit_reason = Some("max_results");
                            break 'bfs;
                        }
                        let item_bytes = 3 + rel_ws.chars().map(json_escaped_len).sum::<usize>();
                        let candidate_est =
                            256 + estimated_files_bytes + item_bytes + issues.len() * 64;
                        if candidate_est > payload_budget {
                            files.push(rel_ws);
                            let probe = build_output(
                                &normalized_path,
                                p.pattern.as_deref(),
                                &files,
                                scanned_dirs,
                                false,
                                true,
                                Some("output_bytes"),
                                &issues,
                            );
                            if serde_json::to_vec(&probe)
                                .is_ok_and(|bytes| bytes.len() <= payload_budget)
                            {
                                estimated_files_bytes += item_bytes;
                            } else {
                                files.pop();
                                truncated = true;
                                limit_reason = Some("output_bytes");
                                break 'bfs;
                            }
                        } else {
                            files.push(rel_ws);
                            estimated_files_bytes += item_bytes;
                        }
                    }
                    crate::workspace::EntryKind::Other => {}
                }
            }
        }
    }

    let mut output = build_output(
        &normalized_path,
        p.pattern.as_deref(),
        &files,
        scanned_dirs,
        !truncated && !had_issues,
        truncated,
        limit_reason,
        &issues,
    );
    while !files.is_empty()
        && serde_json::to_vec(&output).map_or(true, |bytes| bytes.len() > payload_budget)
    {
        files.pop();
        limit_reason = Some("output_bytes");
        output = build_output(
            &normalized_path,
            p.pattern.as_deref(),
            &files,
            scanned_dirs,
            false,
            true,
            limit_reason,
            &issues,
        );
    }

    Ok(output)
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn build_output(
    path: &str,
    pattern: Option<&str>,
    files: &[String],
    scanned_dirs: usize,
    complete: bool,
    truncated: bool,
    limit_reason: Option<&str>,
    issues: &[Value],
) -> Value {
    json!({
        "path": path,
        "pattern": pattern,
        "files": files,
        "scanned_dirs": scanned_dirs,
        "complete": complete,
        "truncated": truncated,
        "limit_reason": limit_reason,
        "issues": issues,
    })
}

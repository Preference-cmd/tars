use super::frontmatter::parse_skill_document;
use super::types::{
    SkillDiagnostic, SkillDiagnosticKind, SkillDiscovery, SkillEntry, SkillRegistry, SkillResources,
};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Maximum size in bytes accepted for a single `SKILL.md` file (1 MiB).
pub const MAX_SKILL_FILE_BYTES: usize = 1024 * 1024;

const SKILL_FILE_NAME: &str = "SKILL.md";
const MAX_RESOURCE_DEPTH: usize = 16;
const MAX_RESOURCE_ENTRIES: usize = 1024;

impl SkillDiscovery {
    /// Discovers skills across ordered directories and optional workspace `.tars/skills`.
    ///
    /// Later directories in `dirs` override earlier entries with the same name.
    /// When `include_workspace` is `true` and `workspace_root` is provided,
    /// `<workspace_root>/.tars/skills` is scanned last (highest precedence); if
    /// that default workspace directory does not exist, it is silently skipped.
    /// Invalid individual skills are skipped and recorded in `diagnostics`.
    pub fn discover(
        dirs: &[PathBuf],
        workspace_root: Option<&Path>,
        include_workspace: bool,
    ) -> Self {
        let mut discovery = Self {
            registry: SkillRegistry::new(),
            diagnostics: Vec::new(),
        };

        for dir in dirs {
            scan_skills_root(dir, &mut discovery);
        }

        if include_workspace && let Some(root) = workspace_root {
            let ws_skills = root.join(".tars").join("skills");
            match fs::metadata(&ws_skills) {
                Ok(_) => scan_skills_root(&ws_skills, &mut discovery),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => discovery.diagnostics.push(SkillDiagnostic {
                    path: ws_skills,
                    kind: SkillDiagnosticKind::DirectoryUnavailable,
                    message: format!("failed to inspect workspace skills directory: {error}"),
                }),
            }
        }

        discovery
    }
}

fn scan_skills_root(root: &Path, discovery: &mut SkillDiscovery) {
    let canon_root = match fs::canonicalize(root) {
        Ok(path) => path,
        Err(error) => {
            discovery.diagnostics.push(SkillDiagnostic {
                path: root.to_owned(),
                kind: SkillDiagnosticKind::DirectoryUnavailable,
                message: format!("cannot open skills directory: {error}"),
            });
            return;
        }
    };
    match fs::metadata(&canon_root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            discovery.diagnostics.push(SkillDiagnostic {
                path: root.to_owned(),
                kind: SkillDiagnosticKind::DirectoryUnavailable,
                message: "skills path is not a directory".into(),
            });
            return;
        }
        Err(error) => {
            discovery.diagnostics.push(SkillDiagnostic {
                path: root.to_owned(),
                kind: SkillDiagnosticKind::DirectoryUnavailable,
                message: format!("cannot inspect skills directory: {error}"),
            });
            return;
        }
    }

    let entries = match read_sorted_dir(&canon_root) {
        Ok(entries) => entries,
        Err(error) => {
            discovery.diagnostics.push(SkillDiagnostic {
                path: canon_root,
                kind: SkillDiagnosticKind::DirectoryUnavailable,
                message: format!("cannot read skills directory: {error}"),
            });
            return;
        }
    };

    for (entry_name, entry_path) in entries {
        let Some(dir_name) = entry_name.to_str() else {
            discovery.diagnostics.push(SkillDiagnostic {
                path: entry_path,
                kind: SkillDiagnosticKind::InvalidName,
                message: "skill directory name is not valid UTF-8".into(),
            });
            continue;
        };

        // Check whether the entry resolves to a directory (supporting symlinked skill dirs).
        match fs::metadata(&entry_path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => continue,
            Err(error) => {
                discovery.diagnostics.push(SkillDiagnostic {
                    path: entry_path,
                    kind: SkillDiagnosticKind::IoError,
                    message: format!("cannot inspect skill entry: {error}"),
                });
                continue;
            }
        }

        let canon_skill_dir = match fs::canonicalize(&entry_path) {
            Ok(path) => path,
            Err(error) => {
                discovery.diagnostics.push(SkillDiagnostic {
                    path: entry_path,
                    kind: SkillDiagnosticKind::IoError,
                    message: format!("cannot canonicalize skill directory: {error}"),
                });
                continue;
            }
        };

        if let Some(entry) =
            load_skill_from_dir(dir_name, &canon_skill_dir, &mut discovery.diagnostics)
        {
            discovery.registry.insert(entry);
        }
    }
}

fn load_skill_from_dir(
    dir_name: &str,
    canon_skill_dir: &Path,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> Option<SkillEntry> {
    // Match exact `SKILL.md` filename in directory listing so case-insensitive
    // filesystems never treat `skill.md` as `SKILL.md`.
    let child_entries = match read_sorted_dir(canon_skill_dir) {
        Ok(entries) => entries,
        Err(error) => {
            diagnostics.push(SkillDiagnostic {
                path: canon_skill_dir.to_owned(),
                kind: SkillDiagnosticKind::IoError,
                message: format!("cannot read skill directory: {error}"),
            });
            return None;
        }
    };

    let skill_md_path = child_entries
        .into_iter()
        .find(|(name, _)| name == SKILL_FILE_NAME)
        .map(|(_, path)| path)?;

    let canon_skill_md = match fs::canonicalize(&skill_md_path) {
        Ok(path) => path,
        Err(error) => {
            diagnostics.push(SkillDiagnostic {
                path: skill_md_path,
                kind: SkillDiagnosticKind::IoError,
                message: format!("cannot resolve SKILL.md: {error}"),
            });
            return None;
        }
    };

    if !canon_skill_md.starts_with(canon_skill_dir) {
        diagnostics.push(SkillDiagnostic {
            path: skill_md_path,
            kind: SkillDiagnosticKind::SymlinkEscape,
            message: "SKILL.md symlink resolves outside its skill directory".into(),
        });
        return None;
    }

    match fs::metadata(&canon_skill_md) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => {
            diagnostics.push(SkillDiagnostic {
                path: canon_skill_md,
                kind: SkillDiagnosticKind::IoError,
                message: "SKILL.md is not a regular file".into(),
            });
            return None;
        }
        Err(error) => {
            diagnostics.push(SkillDiagnostic {
                path: canon_skill_md,
                kind: SkillDiagnosticKind::IoError,
                message: format!("cannot inspect SKILL.md: {error}"),
            });
            return None;
        }
    }

    let bytes = match read_bounded_file(&canon_skill_md, MAX_SKILL_FILE_BYTES) {
        Ok(bytes) => bytes,
        Err(BoundedReadError::TooLarge) => {
            diagnostics.push(SkillDiagnostic {
                path: canon_skill_md,
                kind: SkillDiagnosticKind::TooLarge,
                message: format!("SKILL.md exceeds {MAX_SKILL_FILE_BYTES} bytes"),
            });
            return None;
        }
        Err(BoundedReadError::Io(error)) => {
            diagnostics.push(SkillDiagnostic {
                path: canon_skill_md,
                kind: SkillDiagnosticKind::IoError,
                message: format!("failed to read SKILL.md: {error}"),
            });
            return None;
        }
    };

    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(_) => {
            diagnostics.push(SkillDiagnostic {
                path: canon_skill_md,
                kind: SkillDiagnosticKind::InvalidUtf8,
                message: "SKILL.md is not valid UTF-8".into(),
            });
            return None;
        }
    };

    let parsed = match parse_skill_document(text, dir_name) {
        Ok(parsed) => parsed,
        Err((kind, message)) => {
            diagnostics.push(SkillDiagnostic {
                path: canon_skill_md,
                kind,
                message,
            });
            return None;
        }
    };

    let sha256 = sha256_hex(&bytes);
    let resources = collect_resources(canon_skill_dir, diagnostics);

    Some(SkillEntry {
        name: parsed.meta.name,
        description: parsed.meta.description,
        dir: canon_skill_dir.to_owned(),
        skill_file: canon_skill_md,
        sha256,
        resources,
    })
}

fn collect_resources(
    canon_skill_dir: &Path,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> SkillResources {
    SkillResources {
        scripts: collect_resource_subdir(canon_skill_dir, "scripts", diagnostics),
        references: collect_resource_subdir(canon_skill_dir, "references", diagnostics),
        assets: collect_resource_subdir(canon_skill_dir, "assets", diagnostics),
    }
}

fn collect_resource_subdir(
    canon_skill_dir: &Path,
    subdir_name: &str,
    diagnostics: &mut Vec<SkillDiagnostic>,
) -> Vec<String> {
    let candidate = canon_skill_dir.join(subdir_name);
    match fs::symlink_metadata(&candidate) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            diagnostics.push(SkillDiagnostic {
                path: candidate,
                kind: SkillDiagnosticKind::IoError,
                message: format!("cannot inspect resource directory {subdir_name}: {error}"),
            });
            return Vec::new();
        }
    }

    let canon_subdir = match fs::canonicalize(&candidate) {
        Ok(path) => path,
        Err(error) => {
            diagnostics.push(SkillDiagnostic {
                path: candidate,
                kind: SkillDiagnosticKind::IoError,
                message: format!("cannot resolve resource directory {subdir_name}: {error}"),
            });
            return Vec::new();
        }
    };

    if !canon_subdir.starts_with(canon_skill_dir) {
        diagnostics.push(SkillDiagnostic {
            path: candidate,
            kind: SkillDiagnosticKind::SymlinkEscape,
            message: format!("resource directory {subdir_name} escapes skill directory"),
        });
        return Vec::new();
    }

    if !fs::metadata(&canon_subdir).is_ok_and(|m| m.is_dir()) {
        return Vec::new();
    }

    let mut visited_dirs = HashSet::new();
    visited_dirs.insert(canon_subdir.clone());
    let mut results = Vec::new();
    walk_resource_dir(
        canon_skill_dir,
        &canon_subdir,
        subdir_name,
        0,
        &mut visited_dirs,
        &mut results,
        diagnostics,
    );
    results.sort();
    results
}

fn walk_resource_dir(
    canon_skill_dir: &Path,
    current_dir: &Path,
    rel_prefix: &str,
    depth: usize,
    visited_dirs: &mut HashSet<PathBuf>,
    results: &mut Vec<String>,
    diagnostics: &mut Vec<SkillDiagnostic>,
) {
    if depth >= MAX_RESOURCE_DEPTH || results.len() >= MAX_RESOURCE_ENTRIES {
        return;
    }
    let entries = match read_sorted_dir(current_dir) {
        Ok(entries) => entries,
        Err(error) => {
            diagnostics.push(SkillDiagnostic {
                path: current_dir.to_owned(),
                kind: SkillDiagnosticKind::IoError,
                message: format!("cannot read resource directory: {error}"),
            });
            return;
        }
    };

    for (name, entry_path) in entries {
        if results.len() >= MAX_RESOURCE_ENTRIES {
            break;
        }
        let Some(name_str) = name.to_str() else {
            continue;
        };
        let rel = format!("{rel_prefix}/{name_str}");
        let canon_entry = match fs::canonicalize(&entry_path) {
            Ok(path) => path,
            Err(error) => {
                diagnostics.push(SkillDiagnostic {
                    path: entry_path,
                    kind: SkillDiagnosticKind::IoError,
                    message: format!("cannot resolve resource entry {rel}: {error}"),
                });
                continue;
            }
        };
        if !canon_entry.starts_with(canon_skill_dir) {
            diagnostics.push(SkillDiagnostic {
                path: entry_path,
                kind: SkillDiagnosticKind::SymlinkEscape,
                message: format!("resource entry {rel} resolves outside skill directory"),
            });
            continue;
        }
        let Ok(meta) = fs::metadata(&canon_entry) else {
            continue;
        };
        if meta.is_dir() {
            if visited_dirs.insert(canon_entry.clone()) {
                walk_resource_dir(
                    canon_skill_dir,
                    &canon_entry,
                    &rel,
                    depth + 1,
                    visited_dirs,
                    results,
                    diagnostics,
                );
            }
        } else if meta.is_file() {
            results.push(rel);
        }
    }
}

fn read_sorted_dir(dir: &Path) -> io::Result<Vec<(std::ffi::OsString, PathBuf)>> {
    let mut entries = Vec::new();
    for item in fs::read_dir(dir)? {
        let item = item?;
        entries.push((item.file_name(), item.path()));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}

enum BoundedReadError {
    TooLarge,
    Io(io::Error),
}

fn read_bounded_file(path: &Path, max_bytes: usize) -> Result<Vec<u8>, BoundedReadError> {
    let file = File::open(path).map_err(BoundedReadError::Io)?;
    let mut buf = Vec::new();
    file.take(max_bytes as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(BoundedReadError::Io)?;
    if buf.len() > max_bytes {
        return Err(BoundedReadError::TooLarge);
    }
    Ok(buf)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

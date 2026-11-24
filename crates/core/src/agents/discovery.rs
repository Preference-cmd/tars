use super::builtin::builtin_presets;
use super::frontmatter::parse_agent_document;
use super::types::{
    AgentDiagnostic, AgentDiagnosticKind, AgentDiscovery, AgentPreset, AgentPresetSource,
    AgentRegistry, WorkspaceAgentsSnapshot,
};
use crate::events::AgentInteraction;
use crate::permission::rules::MAX_RULES_FILE_BYTES;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

/// Maximum size in bytes accepted for a single agent `.md` definition file (1 MiB).
pub const MAX_AGENT_FILE_BYTES: usize = 1024 * 1024;

impl AgentDiscovery {
    /// Discovers built-in and configured/user agent presets without scanning any workspace.
    ///
    /// Precedence (low to high):
    /// 1. Compiled-in `explore` and `general`
    /// 2. `<config_dir>/agents/*.md` when `agent_dirs` is empty and `config_dir` is present,
    ///    otherwise each directory in `agent_dirs` in order.
    pub fn discover_non_workspace(config_dir: Option<&Path>, agent_dirs: &[PathBuf]) -> Self {
        Self::discover(config_dir, agent_dirs, &[])
    }

    /// Discovers built-in, configured/user, and pre-verified trusted workspace agent presets.
    ///
    /// Workspace presets must come from a verified [`WorkspaceAgentsSnapshot`]; this function
    /// never reads untrusted `<workspace>/.tars/agents` files directly.
    pub fn discover(
        config_dir: Option<&Path>,
        agent_dirs: &[PathBuf],
        trusted_workspace_presets: &[AgentPreset],
    ) -> Self {
        let mut discovery = Self {
            registry: AgentRegistry::new(),
            diagnostics: Vec::new(),
        };

        for preset in builtin_presets() {
            discovery.registry.insert(preset);
        }

        if agent_dirs.is_empty() {
            if let Some(cfg_dir) = config_dir {
                let default_dir = cfg_dir.join("agents");
                match fs::metadata(&default_dir) {
                    Ok(_) => scan_agent_dir(
                        &default_dir,
                        AgentPresetSource::User,
                        None,
                        &mut discovery.registry,
                        &mut discovery.diagnostics,
                    ),
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => discovery.diagnostics.push(AgentDiagnostic {
                        path: default_dir,
                        kind: AgentDiagnosticKind::DirectoryUnavailable,
                        message: format!("failed to inspect user agents directory: {err}"),
                    }),
                }
            }
        } else {
            let user_default = config_dir.map(|d| fs::canonicalize(d.join("agents")).ok());
            for dir in agent_dirs {
                let source = match &user_default {
                    Some(Some(user_dir))
                        if fs::canonicalize(dir).ok().as_ref() == Some(user_dir) =>
                    {
                        AgentPresetSource::User
                    }
                    _ => AgentPresetSource::ConfiguredDir,
                };
                scan_agent_dir(
                    dir,
                    source,
                    None,
                    &mut discovery.registry,
                    &mut discovery.diagnostics,
                );
            }
        }

        for preset in trusted_workspace_presets {
            discovery.registry.insert(preset.clone());
        }

        discovery
    }
}

/// Captures a single TOCTOU-safe snapshot of `<workspace_root>/.tars/agents/*.md`.
///
/// The returned [`WorkspaceAgentsSnapshot`] records a deterministic sorted manifest
/// `<filename>:<sha256>\n` over all `.md` files' raw bytes so any addition, edit,
/// or deletion invalidates previously persisted workspace trust.
pub fn scan_workspace_agents(workspace_root: &Path) -> WorkspaceAgentsSnapshot {
    let canon_ws = match fs::canonicalize(workspace_root) {
        Ok(path) => path,
        Err(err) => {
            return WorkspaceAgentsSnapshot {
                workspace_root: workspace_root.to_owned(),
                agents_dir: workspace_root.join(".tars").join("agents"),
                manifest_text: String::new(),
                presets: Vec::new(),
                diagnostics: vec![AgentDiagnostic {
                    path: workspace_root.to_owned(),
                    kind: AgentDiagnosticKind::DirectoryUnavailable,
                    message: format!("cannot resolve workspace root: {err}"),
                }],
            };
        }
    };
    let agents_dir = canon_ws.join(".tars").join("agents");
    match fs::symlink_metadata(&agents_dir) {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return WorkspaceAgentsSnapshot {
                workspace_root: canon_ws,
                agents_dir,
                manifest_text: String::new(),
                presets: Vec::new(),
                diagnostics: Vec::new(),
            };
        }
        Err(err) => {
            return WorkspaceAgentsSnapshot {
                workspace_root: canon_ws,
                agents_dir: agents_dir.clone(),
                manifest_text: String::new(),
                presets: Vec::new(),
                diagnostics: vec![AgentDiagnostic {
                    path: agents_dir,
                    kind: AgentDiagnosticKind::DirectoryUnavailable,
                    message: format!("cannot inspect workspace agents directory: {err}"),
                }],
            };
        }
    }

    let canon_agents_dir = match fs::canonicalize(&agents_dir) {
        Ok(path) => path,
        Err(err) => {
            return WorkspaceAgentsSnapshot {
                workspace_root: canon_ws,
                agents_dir: agents_dir.clone(),
                manifest_text: String::new(),
                presets: Vec::new(),
                diagnostics: vec![AgentDiagnostic {
                    path: agents_dir,
                    kind: AgentDiagnosticKind::DirectoryUnavailable,
                    message: format!("cannot resolve workspace agents directory: {err}"),
                }],
            };
        }
    };
    if !canon_agents_dir.starts_with(&canon_ws) {
        return WorkspaceAgentsSnapshot {
            workspace_root: canon_ws,
            agents_dir: agents_dir.clone(),
            manifest_text: String::new(),
            presets: Vec::new(),
            diagnostics: vec![AgentDiagnostic {
                path: agents_dir,
                kind: AgentDiagnosticKind::SymlinkEscape,
                message: "workspace .tars/agents directory resolves outside workspace_root".into(),
            }],
        };
    }

    let mut registry = AgentRegistry::new();
    let mut diagnostics = Vec::new();
    let manifest_entries = scan_agent_dir_with_manifest(
        &canon_agents_dir,
        AgentPresetSource::Workspace,
        Some(&canon_ws),
        &mut registry,
        &mut diagnostics,
    );

    let mut manifest_text = String::new();
    for (filename, sha256) in manifest_entries {
        let line = format!("{filename}:{sha256}\n");
        if manifest_text.len().saturating_add(line.len()) > MAX_RULES_FILE_BYTES {
            diagnostics.push(AgentDiagnostic {
                path: canon_agents_dir.clone(),
                kind: AgentDiagnosticKind::TooLarge,
                message: format!("workspace agents manifest exceeds {MAX_RULES_FILE_BYTES} bytes"),
            });
            return WorkspaceAgentsSnapshot {
                workspace_root: canon_ws,
                agents_dir: canon_agents_dir,
                manifest_text: String::new(),
                presets: Vec::new(),
                diagnostics,
            };
        }
        manifest_text.push_str(&line);
    }

    WorkspaceAgentsSnapshot {
        workspace_root: canon_ws,
        agents_dir: canon_agents_dir,
        manifest_text,
        presets: registry.iter().cloned().collect(),
        diagnostics,
    }
}

fn scan_agent_dir(
    dir: &Path,
    source: AgentPresetSource,
    workspace_boundary: Option<&Path>,
    registry: &mut AgentRegistry,
    diagnostics: &mut Vec<AgentDiagnostic>,
) {
    let _ = scan_agent_dir_with_manifest(dir, source, workspace_boundary, registry, diagnostics);
}

fn scan_agent_dir_with_manifest(
    dir: &Path,
    source: AgentPresetSource,
    workspace_boundary: Option<&Path>,
    registry: &mut AgentRegistry,
    diagnostics: &mut Vec<AgentDiagnostic>,
) -> Vec<(String, String)> {
    let mut manifest = Vec::new();
    let canon_dir = match fs::canonicalize(dir) {
        Ok(path) => path,
        Err(err) => {
            diagnostics.push(AgentDiagnostic {
                path: dir.to_owned(),
                kind: AgentDiagnosticKind::DirectoryUnavailable,
                message: format!("cannot open agents directory: {err}"),
            });
            return manifest;
        }
    };
    if let Some(boundary) = workspace_boundary
        && !canon_dir.starts_with(boundary)
    {
        diagnostics.push(AgentDiagnostic {
            path: dir.to_owned(),
            kind: AgentDiagnosticKind::SymlinkEscape,
            message: "agents directory resolves outside workspace boundary".into(),
        });
        return manifest;
    }
    match fs::metadata(&canon_dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            diagnostics.push(AgentDiagnostic {
                path: dir.to_owned(),
                kind: AgentDiagnosticKind::DirectoryUnavailable,
                message: "agents path is not a directory".into(),
            });
            return manifest;
        }
        Err(err) => {
            diagnostics.push(AgentDiagnostic {
                path: dir.to_owned(),
                kind: AgentDiagnosticKind::DirectoryUnavailable,
                message: format!("cannot inspect agents directory: {err}"),
            });
            return manifest;
        }
    }

    let entries = match read_sorted_dir(&canon_dir) {
        Ok(entries) => entries,
        Err(err) => {
            diagnostics.push(AgentDiagnostic {
                path: canon_dir,
                kind: AgentDiagnosticKind::DirectoryUnavailable,
                message: format!("cannot read agents directory: {err}"),
            });
            return manifest;
        }
    };

    for (entry_name, entry_path) in entries {
        let Some(file_name) = entry_name.to_str() else {
            continue;
        };
        if !file_name.ends_with(".md") {
            continue;
        }

        let canon_file = match fs::canonicalize(&entry_path) {
            Ok(path) => path,
            Err(err) => {
                diagnostics.push(AgentDiagnostic {
                    path: entry_path,
                    kind: AgentDiagnosticKind::IoError,
                    message: format!("cannot resolve agent file: {err}"),
                });
                continue;
            }
        };
        let boundary = workspace_boundary.unwrap_or(&canon_dir);
        if !canon_file.starts_with(boundary) || !canon_file.starts_with(&canon_dir) {
            diagnostics.push(AgentDiagnostic {
                path: entry_path,
                kind: AgentDiagnosticKind::SymlinkEscape,
                message: "agent file symlink resolves outside its agents directory".into(),
            });
            continue;
        }

        match fs::metadata(&canon_file) {
            Ok(meta) if meta.is_file() => {}
            Ok(_) => continue,
            Err(err) => {
                diagnostics.push(AgentDiagnostic {
                    path: canon_file,
                    kind: AgentDiagnosticKind::IoError,
                    message: format!("cannot inspect agent file: {err}"),
                });
                continue;
            }
        }

        let bytes = match read_bounded_file(&canon_file, MAX_AGENT_FILE_BYTES) {
            Ok(bytes) => bytes,
            Err(BoundedReadError::TooLarge) => {
                diagnostics.push(AgentDiagnostic {
                    path: canon_file,
                    kind: AgentDiagnosticKind::TooLarge,
                    message: format!("agent file exceeds {MAX_AGENT_FILE_BYTES} bytes"),
                });
                continue;
            }
            Err(BoundedReadError::Io(err)) => {
                diagnostics.push(AgentDiagnostic {
                    path: canon_file,
                    kind: AgentDiagnosticKind::IoError,
                    message: format!("failed to read agent file: {err}"),
                });
                continue;
            }
        };

        let sha256 = sha256_hex(&bytes);
        manifest.push((file_name.to_owned(), sha256.clone()));

        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(_) => {
                diagnostics.push(AgentDiagnostic {
                    path: canon_file,
                    kind: AgentDiagnosticKind::InvalidUtf8,
                    message: "agent file is not valid UTF-8".into(),
                });
                continue;
            }
        };

        let parsed = match parse_agent_document(text) {
            Ok(parsed) => parsed,
            Err((kind, message)) => {
                diagnostics.push(AgentDiagnostic {
                    path: canon_file,
                    kind,
                    message,
                });
                continue;
            }
        };

        registry.insert(AgentPreset {
            name: parsed.meta.name,
            description: parsed.meta.description,
            source,
            tools: parsed.meta.tools,
            model: parsed.meta.model,
            max_model_rounds: parsed.meta.max_model_rounds,
            timeout_secs: parsed.meta.timeout_secs,
            default_interaction: parsed
                .meta
                .default_interaction
                .unwrap_or(AgentInteraction::SingleWork),
            role_prompt: parsed.role_prompt,
            source_file: Some(canon_file),
            sha256: Some(sha256),
        });
    }

    manifest
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

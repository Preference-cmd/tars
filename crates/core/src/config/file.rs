use super::model::{self, DefinitionOverrides, Profile};
use crate::definition::{DefinitionError, McpServerConfig, SessionDefinition};
use crate::external_tools::ToolSource;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::{Path, PathBuf},
};

/// How an application invocation selects shared configuration.
pub enum ConfigSource {
    /// Use TARS_CONFIG, then the platform's global path; no project-local search.
    Discover,
    /// Read this exact file; absence is an error.
    File(PathBuf),
    /// Ignore files and TARS_CONFIG for an isolated invocation.
    Disabled,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileConfig {
    version: u32,
    store: Option<PathBuf>,
    recovery_dir: Option<PathBuf>,
    default_profile: Option<String>,
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
    #[serde(default)]
    mcp_servers: Option<toml::Value>,
    external_tools: Option<FileExternalTools>,
}

/// The `[external_tools]` table of the shared configuration.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileExternalTools {
    /// `managed`, `system`, or an absolute executable path.
    rg: Option<String>,
}

/// Validated, immutable defaults shared by all application entry points.
#[derive(Debug, Default)]
pub struct Config {
    source: Option<PathBuf>,
    mcp_source: Option<PathBuf>,
    store: Option<PathBuf>,
    recovery_dir: Option<PathBuf>,
    default_profile: Option<String>,
    profiles: BTreeMap<String, Profile>,
    mcp_servers: BTreeMap<String, McpServerConfig>,
    ripgrep: ToolSource,
    warnings: Vec<String>,
}

impl Config {
    /// Load and validate startup configuration. TOML supplies profile defaults;
    /// MCP servers are loaded from mcp.json.
    pub fn load(source: ConfigSource) -> Result<Self, ConfigError> {
        let (toml_path, toml_required) = match source {
            ConfigSource::Disabled => return Ok(Self::default()),
            ConfigSource::File(path) => (Some(path), true),
            ConfigSource::Discover => {
                if let Some(path) = std::env::var_os("TARS_CONFIG") {
                    (Some(PathBuf::from(path)), true)
                } else if let Some(path) = default_path() {
                    (Some(path), false)
                } else {
                    (None, false)
                }
            }
        };

        let toml_path = match toml_path {
            Some(path) => {
                let resolved = if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir()
                        .map_err(|error| ConfigError::Io {
                            path: path.clone(),
                            kind: error.kind(),
                        })?
                        .join(path)
                };
                Some(resolved)
            }
            None => None,
        };

        let (mut config, toml_base_dir) = if let Some(ref path) = toml_path {
            match std::fs::File::open(path) {
                Ok(file) => {
                    let mut text = String::new();
                    file.take(1024 * 1024 + 1)
                        .read_to_string(&mut text)
                        .map_err(|error| ConfigError::Io {
                            path: path.clone(),
                            kind: error.kind(),
                        })?;
                    if text.len() > 1024 * 1024 {
                        return Err(ConfigError::TooLarge);
                    }
                    let parsed = Self::parse_toml(&text, path)?;
                    let base = path.parent().unwrap_or(Path::new(".")).to_owned();
                    (parsed, Some(base))
                }
                Err(error) if !toml_required && error.kind() == io::ErrorKind::NotFound => {
                    let base = path.parent().unwrap_or(Path::new(".")).to_owned();
                    (Self::default(), Some(base))
                }
                Err(error) => {
                    return Err(ConfigError::Io {
                        path: path.clone(),
                        kind: error.kind(),
                    });
                }
            }
        } else {
            (Self::default(), None)
        };

        // Without TARS_MCP_CONFIG, mcp.json sits beside the TOML file. A missing
        // TOML path means no MCP file at all: never search the working directory.
        let (mcp_path, mcp_required) = if let Some(path) = std::env::var_os("TARS_MCP_CONFIG") {
            let path = PathBuf::from(path);
            let path = if path.is_absolute() {
                path
            } else {
                std::env::current_dir()
                    .map_err(|error| ConfigError::Io {
                        path: path.clone(),
                        kind: error.kind(),
                    })?
                    .join(path)
            };
            (Some(path), true)
        } else if let Some(base_dir) = toml_base_dir {
            (Some(base_dir.join("mcp.json")), false)
        } else {
            (None, false)
        };

        if let Some(mcp_path) = mcp_path {
            match std::fs::File::open(&mcp_path) {
                Ok(file) => {
                    let mut text = String::new();
                    file.take(1024 * 1024 + 1)
                        .read_to_string(&mut text)
                        .map_err(|error| ConfigError::Io {
                            path: mcp_path.clone(),
                            kind: error.kind(),
                        })?;
                    if text.len() > 1024 * 1024 {
                        return Err(ConfigError::TooLarge);
                    }
                    let (servers, warnings) = super::mcp::parse_mcp_json(&text)?;
                    config.mcp_servers = servers;
                    config.warnings = warnings;
                    config.mcp_source = Some(mcp_path);
                }
                Err(error) if !mcp_required && error.kind() == io::ErrorKind::NotFound => {
                    // Absence of a default mcp.json leaves the MCP catalog empty.
                }
                Err(error) => {
                    return Err(ConfigError::Io {
                        path: mcp_path,
                        kind: error.kind(),
                    });
                }
            }
        }

        for (name, profile) in &config.profiles {
            if name.trim().is_empty() {
                return Err(ConfigError::EmptyProfile);
            }
            model::resolve(
                Some(profile),
                DefinitionOverrides::default(),
                &config.mcp_servers,
            )?;
        }
        config.selected(None)?;
        Ok(config)
    }

    fn parse_toml(text: &str, source: &Path) -> Result<Self, ConfigError> {
        let raw: FileConfig = toml::from_str(text).map_err(|error: toml::de::Error| {
            let offset = error.span().map_or(0, |span| span.start).min(text.len());
            let prefix = &text[..text.floor_char_boundary(offset)];
            ConfigError::Syntax {
                line: prefix.bytes().filter(|byte| *byte == b'\n').count() + 1,
                column: prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
            }
        })?;
        if raw.mcp_servers.is_some() {
            return Err(ConfigError::McpConfiguredInToml);
        }
        if raw.version != 1 {
            return Err(ConfigError::Version(raw.version));
        }
        let base = source.parent().unwrap_or(Path::new("."));
        let resolve = |path: Option<PathBuf>| -> Result<Option<PathBuf>, ConfigError> {
            path.map(|path| {
                if path.as_os_str().is_empty() {
                    return Err(ConfigError::EmptyPath);
                }
                Ok(if path.is_absolute() {
                    path
                } else {
                    base.join(path)
                })
            })
            .transpose()
        };
        let ripgrep = match raw.external_tools.and_then(|tools| tools.rg) {
            Some(value) => {
                ToolSource::parse(&value).map_err(|_| ConfigError::InvalidExternalTool)?
            }
            None => ToolSource::Managed,
        };
        let mut profiles = raw.profiles;
        for profile in profiles.values_mut() {
            profile.workspace_root = profile
                .workspace_root
                .take()
                .map(|path| model::absolute_root(path, Some(base)))
                .transpose()?;
        }
        Ok(Self {
            source: Some(source.to_owned()),
            mcp_source: None,
            store: resolve(raw.store)?,
            recovery_dir: resolve(raw.recovery_dir)?,
            default_profile: raw.default_profile,
            profiles,
            mcp_servers: BTreeMap::new(),
            ripgrep,
            warnings: Vec::new(),
        })
    }

    fn selected(&self, name: Option<&str>) -> Result<Option<&Profile>, ConfigError> {
        name.or(self.default_profile.as_deref())
            .map(|name| {
                self.profiles
                    .get(name)
                    .ok_or_else(|| ConfigError::UnknownProfile(name.to_owned()))
            })
            .transpose()
    }

    /// Read a selected preset, or None when no default/explicit profile exists.
    pub fn preset(&self, name: Option<&str>) -> Result<Option<SessionDefinition>, ConfigError> {
        self.selected(name)?
            .map(|profile| {
                model::resolve(
                    Some(profile),
                    DefinitionOverrides::default(),
                    &self.mcp_servers,
                )
            })
            .transpose()
    }

    /// Resolve explicit per-field choices over the selected preset and documented
    /// defaults. This constructs a new definition and never alters an open session.
    pub fn definition(
        &self,
        name: Option<&str>,
        overrides: DefinitionOverrides,
    ) -> Result<SessionDefinition, ConfigError> {
        model::resolve(self.selected(name)?, overrides, &self.mcp_servers)
    }

    /// File actually read, or None when defaults are empty/disabled.
    pub fn source(&self) -> Option<&Path> {
        self.source.as_deref()
    }

    /// mcp.json file actually read, or None when none was read.
    pub fn mcp_source(&self) -> Option<&Path> {
        self.mcp_source.as_deref()
    }

    /// Warnings generated when loading configuration (e.g. ignored foreign keys).
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Configured store directory, resolved relative to the TOML file.
    pub fn store_dir(&self) -> Option<&Path> {
        self.store.as_deref()
    }

    /// Configured recovery directory, resolved relative to the TOML file.
    pub fn recovery_dir(&self) -> Option<&Path> {
        self.recovery_dir.as_deref()
    }

    /// Every configured MCP server by id, selected or not. Selection happens
    /// only when constructing a new session definition.
    pub fn mcp_servers(&self) -> &BTreeMap<String, McpServerConfig> {
        &self.mcp_servers
    }

    /// Configured source of the `rg` executable; defaults to the managed copy.
    pub fn ripgrep_source(&self) -> &ToolSource {
        &self.ripgrep
    }
}

fn default_path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("TARS/config.toml"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|p| Path::new(p).is_absolute())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|root| root.join("tars/config.toml"))
    }
}

/// Configuration errors omit TOML source snippets and credential values.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// File access failed, without echoing its contents.
    #[error("configuration file {path:?}: {kind:?}")]
    Io {
        /// Requested file.
        path: PathBuf,
        /// I/O category.
        kind: io::ErrorKind,
    },
    /// Invalid TOML, unknown field, or a value with the wrong type.
    #[error("invalid TOML or configuration field at line {line}, column {column}")]
    Syntax {
        /// One-based line.
        line: usize,
        /// One-based column.
        column: usize,
    },
    /// Invalid mcp.json syntax.
    #[error("invalid mcp.json syntax at line {line}, column {column}")]
    McpSyntax {
        /// One-based line.
        line: usize,
        /// One-based column.
        column: usize,
    },
    /// An invalid mcp.json shape or rule violation.
    #[error("{0}")]
    InvalidMcpFile(String),
    /// MCP servers cannot be configured in config.toml.
    #[error("MCP servers are configured in mcp.json")]
    McpConfiguredInToml,
    /// The file format is not supported.
    #[error("unsupported configuration version {0}; expected 1")]
    Version(u32),
    /// File exceeds the startup configuration bound.
    #[error("configuration exceeds 1 MiB")]
    TooLarge,
    /// A present path must not be empty.
    #[error("configuration directory must not be empty")]
    EmptyPath,
    /// Preset names must be nonblank.
    #[error("profile name must not be blank")]
    EmptyProfile,
    /// The `[external_tools] rg` value is not a usable source.
    #[error("[external_tools] rg must be \"managed\", \"system\", or an absolute path")]
    InvalidExternalTool,
    /// The selected preset does not exist.
    #[error("unknown configuration profile {0:?}")]
    UnknownProfile(String),
    /// No explicit protocol or selected preset supplied a protocol.
    #[error("provider is required via --provider or a configuration profile")]
    ProviderRequired,
    /// No explicit model or selected preset supplied a model.
    #[error("model is required via --model or a configuration profile")]
    ModelRequired,
    /// Configured deadlines use the same range as entry-point arguments.
    #[error("timeout must be 1..86400 seconds")]
    InvalidTimeout,
    /// A selected workspace cannot be fixed to a usable directory.
    #[error("workspace root is unavailable or is not a directory: {kind:?}")]
    Workspace {
        /// Original filesystem failure category.
        kind: io::ErrorKind,
    },
    /// Tool names must be known, unique capabilities.
    #[error("tools must be a comma-separated selection of {0} without duplicates")]
    InvalidTools(String),
    /// A configured MCP server is malformed or a selection repeats an id.
    /// The message contains only the id, never environment values.
    #[error("MCP server {0:?} has an invalid definition or is selected more than once")]
    InvalidMcpServer(String),
    /// A selected server id has no top-level definition.
    #[error("unknown MCP server {0:?}")]
    UnknownMcpServer(String),
    /// A fixed-definition value is invalid.
    #[error(transparent)]
    Definition(DefinitionError),
}

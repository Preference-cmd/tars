use super::model::{self, DefinitionOverrides, Profile};
use crate::definition::{
    DefinitionError, McpApproval, McpServerConfig, McpToolSetting, SessionDefinition,
};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    io::{self, Read},
    path::{Path, PathBuf},
};

/// How an application invocation selects shared configuration.
pub enum ConfigSource {
    /// Use TARZ_CONFIG, then the platform's global path; no project-local search.
    Discover,
    /// Read this exact file; absence is an error.
    File(PathBuf),
    /// Ignore files and TARZ_CONFIG for an isolated invocation.
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
    /// Sparse server definitions; defaults are filled during validation.
    #[serde(default)]
    mcp_servers: BTreeMap<String, RawMcpServer>,
}

// Configuration stays sparse while the frozen snapshot is always complete.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMcpServer {
    command: Option<String>,
    args: Option<Vec<String>>,
    url: Option<String>,
    env: Option<BTreeMap<String, String>>,
    env_vars: Option<Vec<String>>,
    bearer_token_env_var: Option<String>,
    startup_timeout_sec: Option<u64>,
    tool_timeout_sec: Option<u64>,
    enabled_tools: Option<Vec<String>>,
    disabled_tools: Option<Vec<String>>,
    approval: Option<McpApproval>,
    tools: Option<BTreeMap<String, RawMcpToolSetting>>,
    required: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMcpToolSetting {
    approval: Option<McpApproval>,
}

impl RawMcpServer {
    fn into_config(self, id: &str) -> Result<McpServerConfig, ConfigError> {
        // A tool table that omits `approval` inherits the server setting rather
        // than silently resetting it to the default.
        let approval = self.approval.unwrap_or_default();
        let config = McpServerConfig {
            id: id.to_owned(),
            command: self.command,
            args: self.args.unwrap_or_default(),
            url: self.url,
            env: self.env.unwrap_or_default(),
            env_vars: self.env_vars.unwrap_or_default(),
            bearer_token_env_var: self.bearer_token_env_var,
            startup_timeout_sec: self.startup_timeout_sec.unwrap_or(10),
            tool_timeout_sec: self.tool_timeout_sec.unwrap_or(60),
            enabled_tools: self.enabled_tools,
            disabled_tools: self.disabled_tools,
            approval,
            tools: self
                .tools
                .unwrap_or_default()
                .into_iter()
                .map(|(name, setting)| {
                    (
                        name,
                        McpToolSetting {
                            approval: setting.approval.unwrap_or(approval),
                        },
                    )
                })
                .collect(),
            required: self.required.unwrap_or(false),
        };
        config
            .validate()
            .map_err(|_| ConfigError::InvalidMcpServer(id.to_owned()))?;
        Ok(config)
    }
}

/// Validated, immutable defaults shared by all application entry points.
#[derive(Default)]
pub struct Config {
    source: Option<PathBuf>,
    store: Option<PathBuf>,
    recovery_dir: Option<PathBuf>,
    default_profile: Option<String>,
    profiles: BTreeMap<String, Profile>,
    mcp_servers: BTreeMap<String, McpServerConfig>,
}
impl Config {
    /// Load and validate up to 1 MiB of TOML. A missing discovered default file
    /// yields empty defaults; explicit paths and TARZ_CONFIG are mandatory.
    pub fn load(source: ConfigSource) -> Result<Self, ConfigError> {
        let (path, required) = match source {
            ConfigSource::Disabled => return Ok(Self::default()),
            ConfigSource::File(path) => (path, true),
            ConfigSource::Discover => {
                if let Some(path) = std::env::var_os("TARZ_CONFIG") {
                    (PathBuf::from(path), true)
                } else if let Some(path) = default_path() {
                    (path, false)
                } else {
                    return Ok(Self::default());
                }
            }
        };
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
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if !required && error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(ConfigError::Io {
                    path,
                    kind: error.kind(),
                });
            }
        };
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
        Self::parse(&text, &path)
    }
    fn parse(text: &str, source: &Path) -> Result<Self, ConfigError> {
        let raw: FileConfig = toml::from_str(text).map_err(|error: toml::de::Error| {
            let offset = error.span().map_or(0, |span| span.start).min(text.len());
            let prefix = &text[..text.floor_char_boundary(offset)];
            ConfigError::Syntax {
                line: prefix.bytes().filter(|byte| *byte == b'\n').count() + 1,
                column: prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
            }
        })?;
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
        let mut profiles = raw.profiles;
        for profile in profiles.values_mut() {
            profile.workspace_root = profile
                .workspace_root
                .take()
                .map(|path| model::absolute_root(path, Some(base)))
                .transpose()?;
        }
        let mut mcp_servers = BTreeMap::new();
        for (id, server) in raw.mcp_servers {
            let server = server.into_config(&id)?;
            mcp_servers.insert(id, server);
        }
        let config = Self {
            source: Some(source.to_owned()),
            store: resolve(raw.store)?,
            recovery_dir: resolve(raw.recovery_dir)?,
            default_profile: raw.default_profile,
            profiles,
            mcp_servers,
        };
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
}

fn default_path() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("Tarz/config.toml"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|p| Path::new(p).is_absolute())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .map(|root| root.join("tarz/config.toml"))
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
    #[error(
        "tools must be a comma-separated selection of list_directory, read_file, search_text without duplicates"
    )]
    InvalidTools,
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

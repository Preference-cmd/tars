use super::model::{self, DefinitionOverrides, Profile};
use crate::definition::{DefinitionError, SessionDefinition};
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
}

/// Validated, immutable defaults shared by all application entry points.
#[derive(Default)]
pub struct Config {
    source: Option<PathBuf>,
    store: Option<PathBuf>,
    recovery_dir: Option<PathBuf>,
    default_profile: Option<String>,
    profiles: BTreeMap<String, Profile>,
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
        let config = Self {
            source: Some(source.to_owned()),
            store: resolve(raw.store)?,
            recovery_dir: resolve(raw.recovery_dir)?,
            default_profile: raw.default_profile,
            profiles,
        };
        for (name, profile) in &config.profiles {
            if name.trim().is_empty() {
                return Err(ConfigError::EmptyProfile);
            }
            model::resolve(Some(profile), DefinitionOverrides::default())?;
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
            .map(|profile| model::resolve(Some(profile), DefinitionOverrides::default()))
            .transpose()
    }
    /// Resolve explicit per-field choices over the selected preset and documented
    /// defaults. This constructs a new definition and never alters an open session.
    pub fn definition(
        &self,
        name: Option<&str>,
        overrides: DefinitionOverrides,
    ) -> Result<SessionDefinition, ConfigError> {
        model::resolve(self.selected(name)?, overrides)
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
    /// A fixed-definition value is invalid.
    #[error(transparent)]
    Definition(DefinitionError),
}

use causa::kernel::{BlockId, Context};
use serde::Deserialize;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use super::{MaterialReference, references};

/// Largest material file this version reads or imports.
pub const MAX_IMPORT_BYTES: usize = 64 * 1024 * 1024;

/// Outcome of writing a new material file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SavedMaterial {
    /// Exact file size in bytes.
    pub bytes: u64,
    /// Whether this version can read the file back through its import limit.
    /// A recovery file remains valid data when this is `false`.
    pub importable: bool,
}

/// A file, Context validation or external-reference failure.
#[derive(Debug, thiserror::Error)]
pub enum MaterialError {
    /// Reading, writing or replacing a file failed.
    #[error("material file I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The JSON is invalid, or imported block identities violate Context rules.
    #[error("invalid material data: {0}")]
    Json(#[from] serde_json::Error),
    /// A session envelope is unsupported or has invalid stored identities.
    #[error("invalid session material envelope: {0}")]
    SessionEnvelope(&'static str),
    /// Material/session import exceeds the bounded input size.
    #[error("material import exceeds 64 MiB")]
    TooLarge,
    /// An asset cannot be considered readable without application validation.
    #[error("external reference validation required for block {block_id:?}")]
    ReferenceValidationRequired {
        /// Block containing the unvalidated reference.
        block_id: BlockId,
    },
    /// The application could not read a required media or artifact reference.
    #[error("external reference unavailable for block {block_id:?}: {source}")]
    ReferenceUnavailable {
        /// Block containing the unreadable reference.
        block_id: BlockId,
        /// Original validation error, available without parsing display text.
        #[source]
        source: io::Error,
    },
}

/// Save Context containing no external references.
///
/// Writes to a temporary file beside the destination, syncs its content, then
/// atomically replaces the destination. An error before replacement leaves the
/// previous destination intact. This is not a cross-file asset transaction or
/// a guarantee of directory-entry durability across a power failure.
/// Parent directories must already exist. Call outside Session's lock.
pub fn save(path: impl AsRef<Path>, context: &Context) -> Result<(), MaterialError> {
    references::validate(context, None)?;
    write(path.as_ref(), context, true).map(|_| ())
}

/// Export a new material file without overwriting an existing destination.
///
/// Uses the same reference validation and content synchronization as [`save`].
/// Useful for recovery exports whose unique path must never replace older data.
/// The parent directory must already exist; no asset or work-history recovery is implied.
///
/// The returned size tells the caller whether this version can import the file:
/// output above [`MAX_IMPORT_BYTES`] is still written completely, because a
/// recovery export must not discard materials, but cannot be read back here.
pub fn save_new(path: impl AsRef<Path>, context: &Context) -> Result<SavedMaterial, MaterialError> {
    save_new_bounded(path.as_ref(), context, MAX_IMPORT_BYTES)
}

fn save_new_bounded(
    path: &Path,
    context: &Context,
    import_limit: usize,
) -> Result<SavedMaterial, MaterialError> {
    references::validate(context, None)?;
    let bytes = write(path, context, false)?;
    Ok(SavedMaterial {
        bytes,
        importable: bytes <= import_limit as u64,
    })
}

#[cfg(test)]
pub(super) fn save_new_with_limit(
    path: impl AsRef<Path>,
    context: &Context,
    import_limit: usize,
) -> Result<SavedMaterial, MaterialError> {
    save_new_bounded(path.as_ref(), context, import_limit)
}

/// Save after verifying each external reference through application code.
///
/// The validator must actually check that selected media/artifact bytes can be
/// read; identifiers are not assumed to be file paths or network URLs. It must
/// not alter Context, and references are recorded exactly as supplied. Their
/// long-term retention and any copying remain the application's responsibility.
pub fn save_with_references(
    path: impl AsRef<Path>,
    context: &Context,
    mut validate: impl FnMut(MaterialReference<'_>) -> io::Result<()>,
) -> Result<(), MaterialError> {
    references::validate(context, Some(&mut validate))?;
    write(path.as_ref(), context, true).map(|_| ())
}

/// Load Context without external references, checking local block uniqueness.
///
/// Plain Context and explicit format 1/2 session files are accepted; session definitions
/// are discarded and never grant tools. The file holds selected materials only.
/// It does not restore request keys,
/// work references, pending tasks, side effects or previously deleted material.
pub fn load(path: impl AsRef<Path>) -> Result<Context, MaterialError> {
    let context = read(path.as_ref())?;
    references::validate(&context, None)?;
    Ok(context)
}

/// Load Context and verify referenced assets before returning it to the caller.
///
/// A reference that was readable when saved may be unavailable now. Validation
/// runs again at load time, including references inside tool results.
pub fn load_with_references(
    path: impl AsRef<Path>,
    mut validate: impl FnMut(MaterialReference<'_>) -> io::Result<()>,
) -> Result<Context, MaterialError> {
    let context = read(path.as_ref())?;
    references::validate(&context, Some(&mut validate))?;
    Ok(context)
}

/// Extract only validated Context from a saved format 1 or 2 session record.
///
/// This reads at most 64 MiB and never modifies the source, restores runtime state,
/// resolves credentials or adopts the saved definition. Export with [`save_new`]
/// or import into a newly configured session with an explicit tool selection.
pub fn load_session_materials(path: impl AsRef<Path>) -> Result<Context, MaterialError> {
    let bytes = read_bounded(path.as_ref())?;
    let record: SessionMaterials = super::decode_stored(&bytes)?;
    let context = record.into_context()?;
    references::validate(&context, None)?;
    Ok(context)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionMaterials {
    format: String,
    conversation_id: String,
    revision: u64,
    last_commit_id: String,
    // Intentionally never construct or default a permission-bearing definition.
    definition: serde::de::IgnoredAny,
    context: Context,
}
impl SessionMaterials {
    fn into_context(self) -> Result<Context, MaterialError> {
        if !matches!(self.format.as_str(), "tars-session-v1" | "tars-session-v2") {
            return Err(MaterialError::SessionEnvelope("unsupported format"));
        }
        for identity in [&self.conversation_id, &self.last_commit_id] {
            if !uuid::Uuid::parse_str(identity)
                .is_ok_and(|uuid| uuid.hyphenated().to_string() == *identity)
            {
                return Err(MaterialError::SessionEnvelope("invalid canonical identity"));
            }
        }
        if self.revision == 0 {
            return Err(MaterialError::SessionEnvelope("invalid revision"));
        }
        let _ = self.definition;
        Ok(self.context)
    }
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, MaterialError> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_IMPORT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_IMPORT_BYTES {
        return Err(MaterialError::TooLarge);
    }
    Ok(bytes)
}

fn read(path: &Path) -> Result<Context, MaterialError> {
    let bytes = read_bounded(path)?;
    #[derive(Deserialize)]
    struct Kind {
        format: Option<String>,
    }
    let kind: Kind = serde_json::from_slice(&bytes)?;
    if kind.format.is_some() {
        let record: SessionMaterials = super::decode_stored(&bytes)?;
        record.into_context()
    } else {
        Ok(super::decode_stored(&bytes)?)
    }
}

fn write(path: &Path, context: &Context, replace: bool) -> Result<u64, MaterialError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut pending = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut pending, context)?;
    pending.write_all(b"\n")?;
    pending.as_file().sync_all()?;
    let bytes = pending.as_file().metadata()?.len();
    if replace {
        pending.persist(path)
    } else {
        pending.persist_noclobber(path)
    }
    .map_err(|error| MaterialError::Io(error.error))?;
    Ok(bytes)
}

#[cfg(test)]
mod session_material_tests {
    use super::*;
    use causa::kernel::{BlockContent, BlockMeta, ContextBlock};
    fn text_block(text: &str) -> ContextBlock {
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(causa::kernel::Role::Assistant, text),
            BlockMeta::default(),
        )
    }

    #[test]
    fn old_record_materials_export_without_adopting_definition_or_touching_source() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("old.json");
        let output = directory.path().join("materials.json");
        let context = Context::from_blocks(vec![text_block("retained actual content")]).unwrap();
        let envelope = serde_json::json!({
            "format":"tars-session-v1", "conversation_id":uuid::Uuid::now_v7().to_string(),
            "revision":1, "last_commit_id":uuid::Uuid::now_v7().to_string(),
            "definition":{"provider":"unknown","tools":["execute_command"],"workspace_root":"/outside"},
            "context":context,
        });
        let bytes = serde_json::to_vec(&envelope).unwrap();
        std::fs::write(&source, &bytes).unwrap();
        let restored = load_session_materials(&source).unwrap();
        assert_eq!(restored.blocks(), context.blocks());
        save_new(&output, &restored).unwrap();
        assert_eq!(load(&output).unwrap().blocks(), context.blocks());
        assert_eq!(load(&source).unwrap().blocks(), context.blocks());
        assert_eq!(std::fs::read(source).unwrap(), bytes);
        assert!(save_new(output, &restored).is_err());
    }

    #[test]
    fn material_record_import_rejects_wrong_versions_identities_and_duplicate_blocks() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("bad.json");
        let envelope = serde_json::json!({
            "format":"tars-session-v1", "conversation_id":uuid::Uuid::now_v7().to_string(),
            "revision":1, "last_commit_id":uuid::Uuid::now_v7().to_string(),
            "definition":{}, "context":Context::default(),
        });
        for (key, value) in [
            ("format", serde_json::json!("future")),
            ("format", serde_json::json!("tarz-session-v1")),
            ("format", serde_json::json!("tarz-session-v2")),
            ("conversation_id", serde_json::json!("../outside")),
            ("revision", serde_json::json!(0)),
        ] {
            let mut invalid = envelope.clone();
            invalid[key] = value;
            std::fs::write(&source, serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(load_session_materials(&source).is_err());
        }
        let block = text_block("duplicate");
        let mut invalid = envelope;
        invalid["context"] = serde_json::json!({"blocks":[block.clone(),block]});
        std::fs::write(&source, serde_json::to_vec(&invalid).unwrap()).unwrap();
        assert!(load_session_materials(source).is_err());
    }
}

#[cfg(test)]
mod size_report_tests {
    use super::*;
    use causa::kernel::{BlockContent, BlockMeta, ContextBlock, Role};

    fn text_block(text: &str) -> ContextBlock {
        ContextBlock::new(
            causa::runtime::new_block_id(),
            BlockContent::text(Role::Assistant, text),
            BlockMeta::default(),
        )
    }

    #[test]
    fn new_material_reports_exact_size_and_importability_at_the_limit() {
        let directory = tempfile::tempdir().unwrap();
        let context = Context::from_blocks(vec![text_block("material")]).unwrap();

        let path = directory.path().join("normal.json");
        let saved = save_new(&path, &context).unwrap();
        assert_eq!(saved.bytes, std::fs::metadata(&path).unwrap().len());
        assert!(saved.importable);
        assert_eq!(load(&path).unwrap().blocks(), context.blocks());

        let exact = directory.path().join("exact.json");
        let saved_at_limit = save_new_with_limit(&exact, &context, saved.bytes as usize).unwrap();
        assert_eq!(saved_at_limit.bytes, saved.bytes);
        assert!(saved_at_limit.importable);
        assert_eq!(load(&exact).unwrap().blocks(), context.blocks());

        // A lower report limit exercises the same boundary as the production
        // 64 MiB constant without allocating and writing a 64 MiB fixture.
        let broad = directory.path().join("broad.json");
        let saved = save_new_with_limit(&broad, &context, 8).unwrap();
        assert!(saved.bytes > 8);
        assert!(!saved.importable);
        assert_eq!(load(&broad).unwrap().blocks(), context.blocks());
    }

    #[test]
    fn oversized_recovery_is_written_completely_and_reported_unimportable() {
        let directory = tempfile::tempdir().unwrap();
        let context =
            Context::from_blocks(vec![text_block(&"x".repeat(MAX_IMPORT_BYTES))]).unwrap();
        let path = directory.path().join("oversized-recovery.json");
        let saved = save_new(&path, &context).unwrap();
        assert!(saved.bytes > MAX_IMPORT_BYTES as u64);
        assert!(!saved.importable);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), saved.bytes);
        assert!(matches!(load(&path), Err(MaterialError::TooLarge)));
    }
}

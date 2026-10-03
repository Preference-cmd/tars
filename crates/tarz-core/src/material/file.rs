use causa::kernel::{BlockId, Context};
use std::fs::File;
use std::io::{self, BufReader, Write};
use std::path::Path;

use super::{MaterialReference, references};

/// A file, Context validation or external-reference failure.
#[derive(Debug, thiserror::Error)]
pub enum MaterialError {
    /// Reading, writing or replacing a file failed.
    #[error("material file I/O failed: {0}")]
    Io(#[from] io::Error),
    /// The JSON is invalid, or imported block identities violate Context rules.
    #[error("invalid material data: {0}")]
    Json(#[from] serde_json::Error),
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
    write(path.as_ref(), context, true)
}

/// Export a new material file without overwriting an existing destination.
///
/// Uses the same reference validation and content synchronization as [`save`].
/// Useful for recovery exports whose unique path must never replace older data.
/// The parent directory must already exist; no asset or work-history recovery is implied.
pub fn save_new(path: impl AsRef<Path>, context: &Context) -> Result<(), MaterialError> {
    references::validate(context, None)?;
    write(path.as_ref(), context, false)
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
    write(path.as_ref(), context, true)
}

/// Load Context without external references, checking local block uniqueness.
///
/// The file holds selected materials only. It does not restore request keys,
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

fn read(path: &Path) -> Result<Context, MaterialError> {
    Ok(serde_json::from_reader(BufReader::new(File::open(path)?))?)
}

fn write(path: &Path, context: &Context, replace: bool) -> Result<(), MaterialError> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut pending = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut pending, context)?;
    pending.write_all(b"\n")?;
    pending.as_file().sync_all()?;
    if replace {
        pending.persist(path)
    } else {
        pending.persist_noclobber(path)
    }
    .map_err(|error| MaterialError::Io(error.error))?;
    Ok(())
}

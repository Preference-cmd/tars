//! Explicit material files, independent of sessions and work records.
//!
//! Save a cloned selection outside the Session coordination lock. Loading
//! validates Context identities but creates no work, receipt or execution.
//! Media and artifact references are opaque to Causa; callers must supply a
//! validator that actually checks their readability. The default functions
//! reject external references rather than assuming that a recorded URI exists.

mod file;
mod references;
mod vocabulary;

pub use file::{
    MAX_IMPORT_BYTES, MaterialError, SavedMaterial, load, load_session_materials,
    load_with_references, save, save_new, save_with_references,
};
pub use references::MaterialReference;
pub(crate) use references::validate as validate_references;
pub(crate) use vocabulary::decode as decode_stored;

#[cfg(test)]
mod tests;

//! Resolve fixed definitions into runtime resources; never persist plaintext keys.

mod provider;

pub(crate) use provider::assemble;

#[cfg(test)]
mod tests;

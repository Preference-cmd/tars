//! Per-call permission handshake for outside-sandbox execution.

use causa::kernel::BlockId;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Grants issued by the permission processor for one batch.
///
/// A grant is keyed by the call block id and consumed once by the command
/// tool. A call that never runs can leave its grant until the next batch
/// starts, which clears the table.
#[derive(Default)]
pub(crate) struct OutsideSandboxGrants {
    granted: Mutex<HashSet<BlockId>>,
}

impl OutsideSandboxGrants {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn grant(&self, call: BlockId) {
        self.granted
            .lock()
            .expect("command grant lock")
            .insert(call);
    }

    pub(crate) fn take(&self, call: BlockId) -> Option<OutsideSandboxGrant> {
        self.granted
            .lock()
            .expect("command grant lock")
            .remove(&call)
            .then_some(OutsideSandboxGrant(()))
    }

    pub(crate) fn clear(&self) {
        self.granted.lock().expect("command grant lock").clear();
    }
}

/// Proof that one call was granted outside-sandbox execution.
pub(crate) struct OutsideSandboxGrant(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_is_single_use_and_scoped_to_its_call() {
        let grants = OutsideSandboxGrants::new();
        let first = BlockId::new(uuid::Uuid::now_v7());
        let second = BlockId::new(uuid::Uuid::now_v7());
        grants.grant(first);
        assert!(grants.take(second).is_none());
        assert!(grants.take(first).is_some());
        assert!(grants.take(first).is_none());
    }

    #[test]
    fn clear_drops_an_unexecuted_call_grant() {
        let grants = OutsideSandboxGrants::new();
        let call = BlockId::new(uuid::Uuid::now_v7());
        grants.grant(call);
        grants.clear();
        assert!(grants.take(call).is_none());
    }
}

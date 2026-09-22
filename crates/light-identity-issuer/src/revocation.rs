use std::collections::HashSet;
use std::sync::RwLock;
use uuid::Uuid;

/// Per-install revocation, checked at renewal (and at replayed first
/// issuance). Deliberately narrow: this crate does not decide whether
/// config-server also distributes this list, per the design doc's open
/// question — it only defines the check a `CaSigner` consults.
pub trait RevocationList: Send + Sync {
    fn is_revoked(&self, install_id: &Uuid) -> bool;
}

/// An in-process revocation list, sufficient for Phase 0/1 where the issuer
/// holds the list itself rather than distributing it through config-server.
impl<T: RevocationList + ?Sized> RevocationList for std::sync::Arc<T> {
    fn is_revoked(&self, install_id: &Uuid) -> bool {
        (**self).is_revoked(install_id)
    }
}

#[derive(Default)]
pub struct InMemoryRevocationList {
    revoked: RwLock<HashSet<Uuid>>,
}

impl InMemoryRevocationList {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn revoke(&self, install_id: Uuid) {
        self.revoked
            .write()
            .expect("revocation list lock poisoned")
            .insert(install_id);
    }
}

impl RevocationList for InMemoryRevocationList {
    fn is_revoked(&self, install_id: &Uuid) -> bool {
        self.revoked
            .read()
            .expect("revocation list lock poisoned")
            .contains(install_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revoking_an_install_id_is_visible_immediately() {
        let list = InMemoryRevocationList::new();
        let install_id = Uuid::new_v4();
        assert!(!list.is_revoked(&install_id));

        list.revoke(install_id);
        assert!(list.is_revoked(&install_id));

        let other_id = Uuid::new_v4();
        assert!(!list.is_revoked(&other_id));
    }
}

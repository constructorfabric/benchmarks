//! A credential store that never resolves.
//!
//! Used when the deployment wires no `credstore` gear: every lookup reports
//! the secret as absent, which the auth plugins surface as an authentication
//! failure instead of a transport error.

use async_trait::async_trait;

/// The stand-in store used in the absence of a credential store gear.
#[derive(Debug, Default, Clone, Copy)]
pub struct AbsentCredStore;

#[async_trait]
impl credstore_sdk::CredStoreClientV1 for AbsentCredStore {
    async fn get(
        &self,
        ctx: &toolkit_security::SecurityContext,
        key: &credstore_sdk::SecretRef,
    ) -> Result<Option<credstore_sdk::GetSecretResponse>, credstore_sdk::CredStoreError> {
        let _ = (ctx, key);
        Ok(None)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use credstore_sdk::CredStoreClientV1 as _;

    #[tokio::test]
    async fn every_lookup_is_empty() {
        let store = AbsentCredStore;
        let reference = credstore_sdk::SecretRef::new("ref").unwrap();
        let security = toolkit_security::SecurityContext::anonymous();
        let resolved = store.get(&security, &reference).await.unwrap();
        assert!(resolved.is_none());
    }
}

//! Plugin infrastructure: the in-process plugin registries and the
//! built-in plugin implementations (DESIGN §3.2 Plugin System, ADR 0002,
//! ADR 0008, ADR 0009).
//!
//! Named plugins are resolved via these registries; UUID-backed custom
//! plugins are resolved by the data plane directly. Catalog-only GTS
//! identifiers (`basic`/`bearer`/`timeout`/`cors`/`logging`/`metrics`)
//! are intentionally *not* registered here — attempting to bind one fails
//! later with `503 PluginNotFound` in the data plane.

pub mod builtins;
pub mod registry;

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::CredStoreClientV1;
use credstore_sdk::SecretRef;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::plugin::SecretResolver;

/// Production [`SecretResolver`] backed by the credstore gear.
///
/// `cred://` references are resolved with a tenant-scoped
/// [`SecurityContext`]; missing or inaccessible secrets map to
/// [`DomainError::SecretNotFound`] (500).
pub struct CredStoreSecretResolver {
    credstore: Arc<dyn CredStoreClientV1>,
}

impl CredStoreSecretResolver {
    /// Wrap a credstore client.
    #[must_use]
    pub fn new(credstore: Arc<dyn CredStoreClientV1>) -> Self {
        Self { credstore }
    }

    fn key_from_ref(reference: &str) -> &str {
        reference.strip_prefix("cred://").unwrap_or(reference)
    }
}

#[async_trait]
impl SecretResolver for CredStoreSecretResolver {
    async fn resolve(&self, tenant_id: Uuid, secret_ref: &str) -> Result<Vec<u8>, DomainError> {
        let key = SecretRef::new(Self::key_from_ref(secret_ref)).map_err(|e| {
            DomainError::Internal(format!("invalid secret reference '{secret_ref}': {e}"))
        })?;
        let ctx = SecurityContext::builder()
            .subject_id(tenant_id)
            .subject_tenant_id(tenant_id)
            .build()
            .map_err(|e| {
                DomainError::Internal(format!("security context build failed: {e}"))
            })?;
        match self.credstore.get(&ctx, &key).await {
            Ok(Some(resp)) => Ok(resp.value.as_bytes().to_vec()),
            Ok(None) => Err(DomainError::SecretNotFound(secret_ref.to_owned())),
            Err(e) => Err(DomainError::Internal(format!("credstore get: {e}"))),
        }
    }
}

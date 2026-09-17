//! Credential-store integration for the data plane.
//!
//! Auth plugins reference secret material through `secret_ref` keys resolved
//! at request time. Material is never logged, never echoed in error details
//! and never returned through the API.

use std::collections::BTreeMap;
use std::sync::Arc;

use credstore_sdk::{CredStoreClientV1, SecretRef};
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Resolves `secret_ref` values to their material.
#[derive(Clone, Default)]
pub struct SecretResolver {
    inner: Option<Arc<dyn CredStoreClientV1>>,
}

impl std::fmt::Debug for SecretResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretResolver")
            .field("wired", &self.inner.is_some())
            .finish()
    }
}

impl SecretResolver {
    /// Resolver backed by the `credstore` gear.
    #[must_use]
    pub fn new(client: Arc<dyn CredStoreClientV1>) -> Self {
        Self { inner: Some(client) }
    }

    /// Resolver that never finds a secret; auth plugins then report
    /// `SecretNotFound`.
    #[must_use]
    pub fn absent() -> Self {
        Self { inner: None }
    }

    /// Wire the resolver to the credential store registered in the gear hub,
    /// falling back to an unwired resolver.
    #[must_use]
    pub fn from_ctx(ctx: &toolkit::GearCtx) -> Self {
        match ctx
            .client_hub()
            .get::<dyn credstore_sdk::CredStoreClientV1>()
        {
            Ok(client) => Self::new(client),
            Err(_) => {
                tracing::info!("credential store not available; secret_ref resolution disabled");
                Self::absent()
            }
        }
    }

    /// `true` when a credential store is wired.
    #[must_use]
    pub fn is_wired(&self) -> bool {
        self.inner.is_some()
    }

    /// Fetch the secret material for `reference`, or `None` when the store is
    /// unwired or the secret is not accessible to the tenant.
    ///
    /// The reference never reaches an error message or log line.
    pub async fn resolve(&self, reference: &str, tenant_id: Uuid) -> Option<String> {
        let client = self.inner.as_ref()?;
        let Ok(key) = SecretRef::new(reference) else {
            return None;
        };
        let ctx = SecurityContext::builder()
            .subject_id(Uuid::nil())
            .subject_tenant_id(tenant_id)
            .build()
            .unwrap_or_else(|_| SecurityContext::anonymous());
        match client.get(&ctx, &key).await {
            Ok(Some(response)) => Some(String::from_utf8_lossy(response.value.as_bytes()).into_owned()),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(tenant_id = %tenant_id, error = %err, "secret lookup failed");
                None
            }
        }
    }

    /// Resolve the references used by an upstream auth configuration into the
    /// plugin-request secret map.
    #[must_use]
    pub async fn resolve_auth_refs(
        &self,
        references: &[String],
        tenant_id: Uuid,
    ) -> BTreeMap<String, String> {
        let mut out = BTreeMap::new();
        for reference in references {
            if let Some(material) = self.resolve(reference, tenant_id).await {
                out.insert(reference.clone(), material);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unwired_resolver_yields_nothing() {
        let r = SecretResolver::absent();
        assert!(!r.is_wired());
        assert!(r.resolve("cred://x", Uuid::new_v4()).await.is_none());
        assert!(r.resolve_auth_refs(&["cred://x".to_owned()], Uuid::new_v4())
            .await
            .is_empty());
    }

    #[test]
    fn debug_output_never_contains_material() {
        let r = SecretResolver::absent();
        let rendered = format!("{r:?}");
        assert!(rendered.contains("wired"));
        assert!(!rendered.contains("secret"));
    }
}

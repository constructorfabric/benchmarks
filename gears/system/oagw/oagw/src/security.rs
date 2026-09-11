//! Security plumbing: the caller's identity, the tenant chain and credential
//! resolution.

use std::sync::Arc;

use async_trait::async_trait;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::error::{ErrorKind, OagwError};

/// Resolves a credential reference to its material at request time.
///
/// Implementations never log the value and never return it in an error message.
#[async_trait]
pub trait CredentialResolver: Send + Sync {
    /// Resolves `reference` for the calling tenant.
    ///
    /// # Errors
    ///
    /// Returns [`ErrorKind::SecretNotFound`] when the reference cannot be resolved and
    /// [`ErrorKind::AuthenticationFailed`] when the caller may not read it.
    async fn resolve(
        &self,
        ctx: &SecurityContext,
        reference: &str,
    ) -> Result<Option<String>, OagwError>;
}

/// Resolver backed by the platform credential store.
pub struct StoreCredentialResolver {
    client: Arc<dyn credstore_sdk::CredStoreClientV1>,
}

impl StoreCredentialResolver {
    /// Builds a resolver over the platform credential store.
    #[must_use]
    pub fn new(client: Arc<dyn credstore_sdk::CredStoreClientV1>) -> Self {
        Self { client }
    }
}

/// Strips the `cred://` scheme from a reference, if present.
#[must_use]
pub fn strip_scheme(reference: &str) -> &str {
    reference.strip_prefix("cred://").unwrap_or(reference)
}

#[async_trait]
impl CredentialResolver for StoreCredentialResolver {
    async fn resolve(
        &self,
        ctx: &SecurityContext,
        reference: &str,
    ) -> Result<Option<String>, OagwError> {
        let key = strip_scheme(reference);
        let reference = credstore_sdk::SecretRef::new(key).map_err(|err| {
            OagwError::new(
                ErrorKind::ValidationError,
                format!("invalid credential reference: {err}"),
            )
        })?;
        let resolved = self.client.get(ctx, &reference).await;
        match resolved {
            Ok(Some(response)) => {
                Ok(Some(String::from_utf8_lossy(response.value.as_bytes()).into_owned()))
            }
            Ok(None) => Err(OagwError::new(
                ErrorKind::SecretNotFound,
                format!("credential `{key}` was not found"),
            )),
            Err(_) => Err(OagwError::new(
                ErrorKind::SecretNotFound,
                format!("credential `{key}` could not be resolved"),
            )),
        }
    }
}

/// A resolver that always fails, used when no credential store is wired.
pub struct NoopCredentialResolver;

#[async_trait]
impl CredentialResolver for NoopCredentialResolver {
    async fn resolve(
        &self,
        _ctx: &SecurityContext,
        reference: &str,
    ) -> Result<Option<String>, OagwError> {
        Err(OagwError::new(
            ErrorKind::SecretNotFound,
            format!("credential `{reference}` could not be resolved: no credential store"),
        ))
    }
}

/// The caller's identity and tenant chain, resolved once per request.
#[derive(Debug, Clone)]
pub struct SecurityContextHolder {
    security: SecurityContext,
    chain: Arc<[Uuid]>,
}

impl SecurityContextHolder {
    /// Builds the holder from the caller's security context and its ancestor chain,
    /// ordered from the caller's tenant towards the root.
    #[must_use]
    pub fn new(security: SecurityContext, chain: Vec<Uuid>) -> Self {
        Self {
            security,
            chain: Arc::from(chain),
        }
    }

    /// The caller's security context.
    #[must_use]
    pub const fn security(&self) -> &SecurityContext {
        &self.security
    }

    /// The caller's own tenant.
    #[must_use]
    pub fn own_tenant(&self) -> Uuid {
        self.security.subject_tenant_id()
    }

    /// The tenant chain, closest tenant first.
    #[must_use]
    pub fn chain(&self) -> crate::store::TenantChain {
        crate::store::TenantChain::new(self.chain.to_vec())
    }
}

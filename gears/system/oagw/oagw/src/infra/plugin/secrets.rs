//! Credential resolution for the auth plugins.
//!
//! `DESIGN` §"Credential isolation" forbids storing credentials in the gear:
//! an upstream names a `cred://` reference and the data plane resolves it
//! through the credential store at request time. The resolution is a port
//! ([`SecretResolver`]) so the plugins stay testable without a cred store and
//! a credential never crosses a module boundary unwrapped.

use std::sync::Arc;

use credstore_sdk::{CredStoreClientV1, SecretRef};
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;

/// Resolves a `cred://` reference to a secret value for one caller identity.
#[async_trait::async_trait]
pub trait SecretResolver: Send + Sync + std::fmt::Debug {
    /// The secret `reference` names, as UTF-8.
    ///
    /// # Errors
    /// Returns [`DomainError::SecretNotFound`] when no accessible secret is
    /// found (a single 404 surface that prevents enumeration) and
    /// [`DomainError::ServiceUnavailable`] when the store cannot be reached.
    async fn resolve(
        &self,
        tenant: uuid::Uuid,
        subject: uuid::Uuid,
        reference: &str,
    ) -> Result<String, DomainError>;
}

/// [`SecretResolver`] over the platform credential store.
#[derive(Clone)]
pub struct CredStoreSecretResolver {
    store: Arc<dyn CredStoreClientV1>,
}

impl std::fmt::Debug for CredStoreSecretResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredStoreSecretResolver")
            .finish_non_exhaustive()
    }
}

impl CredStoreSecretResolver {
    /// Bind the resolver to the credential store client.
    #[must_use]
    pub fn new(store: Arc<dyn CredStoreClientV1>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl SecretResolver for CredStoreSecretResolver {
    async fn resolve(
        &self,
        tenant: uuid::Uuid,
        subject: uuid::Uuid,
        reference: &str,
    ) -> Result<String, DomainError> {
        let key = SecretRef::new(reference).map_err(|error| DomainError::SecretNotFound {
            detail: format!("invalid credential reference '{reference}': {error}"),
        })?;
        let context = SecurityContext::builder()
            .subject_id(subject)
            .subject_tenant_id(tenant)
            .build()
            .map_err(|error| DomainError::ServiceUnavailable {
                detail: "the caller identity cannot be used to resolve a secret".to_owned(),
                cause: Some(Box::new(std::io::Error::other(error))),
            })?;
        match self.store.get(&context, &key).await {
            Ok(Some(secret)) => String::from_utf8(secret.value.as_bytes().to_vec()).map_err(|_| {
                DomainError::SecretNotFound {
                    detail: format!("secret '{reference}' is not valid UTF-8"),
                }
            }),
            Ok(None) => Err(DomainError::SecretNotFound {
                detail: format!("secret '{reference}' is not resolvable for this tenant"),
            }),
            Err(error) => Err(DomainError::ServiceUnavailable {
                detail: format!("credential store rejected the lookup of '{reference}'"),
                cause: Some(Box::new(std::io::Error::other(error.to_string()))),
            }),
        }
    }
}

/// [`SecretResolver`] over an in-process map, for tests.
#[derive(Debug, Default, Clone)]
pub struct StaticSecretResolver {
    secrets: std::collections::BTreeMap<String, String>,
}

impl StaticSecretResolver {
    /// A resolver serving `secrets`.
    #[must_use]
    pub fn new(secrets: std::collections::BTreeMap<String, String>) -> Self {
        Self { secrets }
    }

    /// A resolver with a single entry, for the common one-secret test.
    #[must_use]
    pub fn single(reference: &str, value: &str) -> Self {
        Self {
            secrets: std::collections::BTreeMap::from([(reference.to_owned(), value.to_owned())]),
        }
    }
}

#[async_trait::async_trait]
impl SecretResolver for StaticSecretResolver {
    async fn resolve(
        &self,
        _tenant: uuid::Uuid,
        _subject: uuid::Uuid,
        reference: &str,
    ) -> Result<String, DomainError> {
        self.secrets
            .get(reference)
            .cloned()
            .ok_or_else(|| DomainError::SecretNotFound {
                detail: format!("secret '{reference}' is not resolvable for this tenant"),
            })
    }
}

/// [`SecretResolver`] that picks the platform credential store out of the gear
/// client hub at request time.
///
/// The hub is the only dependency-free way for this gear to reach the
/// credential store without declaring a gear dependency, so the lookup is lazy
/// and fails closed: a deployment that registers no credential store gets a
/// `503` for the upstream that asked for a secret, not a silent fallback.
#[derive(Clone)]
pub struct HubSecretResolver {
    hub: Arc<toolkit::client_hub::ClientHub>,
}

impl std::fmt::Debug for HubSecretResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubSecretResolver").finish_non_exhaustive()
    }
}

impl HubSecretResolver {
    /// Bind the resolver to the gear client hub.
    #[must_use]
    pub fn new(hub: Arc<toolkit::client_hub::ClientHub>) -> Self {
        Self { hub }
    }
}

#[async_trait::async_trait]
impl SecretResolver for HubSecretResolver {
    async fn resolve(
        &self,
        tenant: uuid::Uuid,
        subject: uuid::Uuid,
        reference: &str,
    ) -> Result<String, DomainError> {
        let store = self.hub.get::<dyn CredStoreClientV1>().map_err(|error| {
            DomainError::ServiceUnavailable {
                detail: "no credential store is registered for this deployment".to_owned(),
                cause: Some(Box::new(std::io::Error::other(error.to_string()))),
            }
        })?;
        CredStoreSecretResolver::new(store)
            .resolve(tenant, subject, reference)
            .await
    }
}

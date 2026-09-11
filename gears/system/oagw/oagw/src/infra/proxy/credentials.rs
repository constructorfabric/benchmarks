//! Credential resolution against the credential store.
//!
//! A `secret_ref` is a `cred://` URI. The scheme is stripped before the key is
//! handed to the store, the value is returned wrapped in a [`SecretString`] and
//! nothing about it is ever logged.

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::{CredStoreClientV1, CredStoreError, GetSecretResponse, SecretRef};
use secrecy::{ExposeSecret, SecretString};
use toolkit_security::SecurityContext;

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::services::data_plane::CredentialResolver;

/// Scheme prefix stripped before a key is resolved.
pub const CRED_SCHEME: &str = "cred://";

/// Strip the `cred://` scheme from a reference.
#[must_use]
pub fn strip_scheme(secret_ref: &str) -> &str {
    secret_ref
        .trim()
        .strip_prefix(CRED_SCHEME)
        .unwrap_or_else(|| secret_ref.trim())
}

/// The credential store call the resolver makes.
#[async_trait]
pub trait SecretLookup: Send + Sync {
    /// The value stored under `key`, `None` when it does not exist.
    ///
    /// # Errors
    /// Returns a domain error when the store cannot be reached.
    async fn get(&self, ctx: &SecurityContext, key: &str) -> Result<Option<String>, DomainError>;
}

/// Resolver over a credential-store client.
pub struct CredStoreResolver {
    lookup: Arc<dyn SecretLookup>,
}
impl CredStoreResolver {
    /// A resolver over `lookup`.
    #[must_use]
    pub fn new(lookup: Arc<dyn SecretLookup>) -> Self {
        Self { lookup }
    }

    /// Resolve a reference into its value.
    ///
    /// # Errors
    /// Returns [`crate::domain::error::ErrorKind::SecretNotFound`] when the
    /// reference resolves to nothing.
    pub async fn value_of(
        &self,
        ctx: &SecurityContext,
        secret_ref: &str,
    ) -> Result<SecretString, DomainError> {
        let key = strip_scheme(secret_ref);
        if key.is_empty() {
            return Err(DomainError::validation("secret_ref must not be empty"));
        }
        match self.lookup.get(ctx, key).await? {
            Some(value) if !value.is_empty() => Ok(SecretString::from(value)),
            _ => Err(DomainError::secret_not_found(format!(
                "secret '{secret_ref}' does not exist"
            ))),
        }
    }
}

#[async_trait]
impl CredentialResolver for CredStoreResolver {
    async fn resolve(
        &self,
        ctx: &SecurityContext,
        secret_ref: &str,
    ) -> Result<String, DomainError> {
        self.value_of(ctx, secret_ref)
            .await
            .map(|secret| secret.expose_secret().to_owned())
    }
}

/// [`SecretLookup`] over the platform's credential-store client.
pub struct StoreLookup {
    client: Arc<dyn CredStoreClientV1>,
}

impl StoreLookup {
    /// A lookup over `client`.
    #[must_use]
    pub fn new(client: Arc<dyn CredStoreClientV1>) -> Self {
        Self { client }
    }

    /// A lookup that answers as an absent store would: every reference misses.
    #[must_use]
    pub fn absent() -> Self {
        Self {
            client: Arc::new(crate::infra::proxy::credentials::AbsentClient),
        }
    }
}

/// The client `StoreLookup::absent` fronts: every lookup is a miss.
#[derive(Debug, Clone, Copy, Default)]
pub struct AbsentClient;

#[async_trait]
impl CredStoreClientV1 for AbsentClient {
    async fn get(
        &self,
        _ctx: &SecurityContext,
        _key: &SecretRef,
    ) -> Result<Option<GetSecretResponse>, CredStoreError> {
        Ok(None)
    }
}

#[async_trait]
impl SecretLookup for StoreLookup {
    async fn get(&self, ctx: &SecurityContext, key: &str) -> Result<Option<String>, DomainError> {
        let Ok(reference) = SecretRef::new(key.to_owned()) else {
            return Err(DomainError::validation(format!(
                "'{key}' is not a credential-store key: only letters, digits, '-' and '_' are \
                 allowed"
            )));
        };
        match self.client.get(ctx, &reference).await {
            Ok(Some(response)) => Ok(Some(
                String::from_utf8_lossy(response.value.as_bytes()).into_owned(),
            )),
            Ok(None) => Ok(None),
            Err(error) => {
                // The error names the store's reason, never a value.
                Err(DomainError::new(
                    ErrorKind::Internal,
                    format!("credential store refused the lookup: {error}"),
                ))
            }
        }
    }
}

/// The lookup used when the host registered no credential store: every
/// reference resolves to nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct AbsentStore;

#[async_trait]
impl SecretLookup for AbsentStore {
    async fn get(&self, _ctx: &SecurityContext, key: &str) -> Result<Option<String>, DomainError> {
        tracing::debug!(key = %key, "no credential store registered; resolving to nothing");
        Ok(None)
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod credential_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use parking_lot::Mutex;
    use std::collections::HashMap;

    struct FakeStore {
        secrets: HashMap<String, String>,
        seen: Mutex<Vec<String>>,
    }

    impl FakeStore {
        fn with(secrets: &[(&str, &str)]) -> Self {
            Self {
                secrets: secrets
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn keys_seen(&self) -> Vec<String> {
            self.seen.lock().clone()
        }
    }

    #[async_trait]
    impl SecretLookup for FakeStore {
        async fn get(
            &self,
            _ctx: &SecurityContext,
            key: &str,
        ) -> Result<Option<String>, DomainError> {
            self.seen.lock().push(key.to_owned());
            Ok(self.secrets.get(key).cloned())
        }
    }

    fn resolver(secrets: &[(&str, &str)]) -> (CredStoreResolver, Arc<FakeStore>) {
        let store = Arc::new(FakeStore::with(secrets));
        (CredStoreResolver::new(store.clone()), store)
    }

    #[test]
    fn the_scheme_is_stripped() {
        assert_eq!(strip_scheme("cred://partner-key"), "partner-key");
        assert_eq!(strip_scheme("partner-key"), "partner-key");
        assert_eq!(strip_scheme(" cred://k "), "k");
    }

    #[tokio::test]
    async fn a_reference_resolves_to_its_value() -> Result<(), DomainError> {
        let (resolver, store) = resolver(&[("partner-key", "sk-abc")]);
        let value = resolver
            .value_of(&SecurityContext::anonymous(), "cred://partner-key")
            .await?;
        assert_eq!(value.expose_secret(), "sk-abc");
        assert_eq!(store.keys_seen(), vec!["partner-key".to_owned()]);
        Ok(())
    }

    #[tokio::test]
    async fn a_missing_secret_is_secret_not_found() -> Result<(), DomainError> {
        let (resolver, _) = resolver(&[]);
        let err = resolver
            .value_of(&SecurityContext::anonymous(), "cred://nope")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), crate::domain::error::ErrorKind::SecretNotFound);
        assert_eq!(
            err.http_status(),
            http::StatusCode::INTERNAL_SERVER_ERROR,
            "a missing secret is never a caller error"
        );
        assert!(!err.detail().contains("sk-"), "the detail names no value");
        Ok(())
    }

    #[tokio::test]
    async fn an_empty_secret_is_treated_as_missing() -> Result<(), DomainError> {
        let (resolver, _) = resolver(&[("empty", "")]);
        let err = resolver
            .value_of(&SecurityContext::anonymous(), "cred://empty")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), crate::domain::error::ErrorKind::SecretNotFound);
        Ok(())
    }

    #[tokio::test]
    async fn an_empty_reference_is_rejected() -> Result<(), DomainError> {
        let (resolver, _) = resolver(&[]);
        let err = resolver
            .value_of(&SecurityContext::anonymous(), "cred://")
            .await
            .unwrap_err();
        assert_eq!(err.kind(), crate::domain::error::ErrorKind::Validation);
        Ok(())
    }

    #[tokio::test]
    async fn values_never_reach_the_error_channel() -> Result<(), DomainError> {
        let (resolver, _) = resolver(&[("k", "sk-let-me-through")]);
        let value = resolver
            .resolve(&SecurityContext::anonymous(), "cred://k")
            .await?;
        assert_eq!(value, "sk-let-me-through");
        assert!(
            !format!(
                "{:?}",
                resolver
                    .value_of(&SecurityContext::anonymous(), "cred://missing")
                    .await
            )
            .contains("sk-"),
            "no error path carries a resolved value"
        );
        Ok(())
    }
}

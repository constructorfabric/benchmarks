// Created: 2026-09-01 by Constructor Tech
//! Secret resolution against `cred_store`.
//!
//! `docs/DESIGN.md` §3.2 "Secret Access Control": OAGW never owns secret
//! sharing — it asks `cred_store` for the material behind a `cred://`
//! reference and maps an inaccessible reference to an authentication
//! failure. `cred_store` collapses "unknown" and "not shared with you"
//! into a single `Ok(None)` to prevent enumeration, so both surfaces look
//! the same to the caller.

use std::sync::Arc;

use credstore_sdk::{CredStoreClientV1, SecretRef};
use toolkit_security::SecurityContext;

use crate::domain::errors::OagwError;
use crate::domain::model::strip_cred_prefix;

/// Resolves `cred://` references to secret material.
#[derive(Clone)]
pub struct SecretResolver {
    client: Option<Arc<dyn CredStoreClientV1>>,
}

impl SecretResolver {
    /// A resolver with no backing client: every lookup fails.
    #[must_use]
    pub fn unlinked() -> Self {
        Self { client: None }
    }

    /// A resolver backed by `client`.
    #[must_use]
    pub fn new(client: Arc<dyn CredStoreClientV1>) -> Self {
        Self {
            client: Some(client),
        }
    }

    /// Resolve `reference` to the secret material.
    ///
    /// # Errors
    /// Returns `AuthenticationFailed` when the reference is unknown or not
    /// shared with the caller, `SecretNotFound` when the reference is not a
    /// legal key, and a generic failure when the store itself errors.
    pub async fn resolve(
        &self,
        ctx: &SecurityContext,
        reference: &str,
    ) -> Result<String, OagwError> {
        let Some(client) = &self.client else {
            return Err(OagwError::secret_not_found(reference));
        };
        let key = SecretRef::new(strip_cred_prefix(reference)).map_err(|err| {
            tracing::debug!(reference, error = %err, "invalid secret reference");
            OagwError::secret_not_found(reference)
        })?;
        match client.get(ctx, &key).await {
            Ok(Some(response)) => {
                String::from_utf8(response.value.as_bytes().to_vec()).map_err(|_| {
                    OagwError::authentication_failed(format!(
                        "secret '{reference}' is not valid UTF-8"
                    ))
                })
            }
            Ok(None) => Err(OagwError::authentication_failed(format!(
                "secret '{reference}' is not accessible to this tenant"
            ))),
            Err(credstore_sdk::CredStoreError::NotFound) => {
                Err(OagwError::secret_not_found(reference))
            }
            Err(err) => Err(OagwError::secret_not_found(reference)
                .with_detail(format!("credential store rejected the lookup: {err}"))),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use credstore_sdk::test_util::MockCredStoreClient;

    fn ctx() -> SecurityContext {
        SecurityContext::anonymous()
    }

    #[tokio::test]
    async fn a_known_reference_resolves() {
        let resolver = SecretResolver::new(std::sync::Arc::new(MockCredStoreClient::with_secrets(
            vec![("openai-key".to_owned(), "sk-123".to_owned())],
        )));
        assert_eq!(
            resolver
                .resolve(&ctx(), "cred://openai-key")
                .await
                .expect("resolves"),
            "sk-123"
        );
    }

    #[tokio::test]
    async fn an_unknown_reference_is_an_auth_failure() {
        let resolver = SecretResolver::new(std::sync::Arc::new(MockCredStoreClient::empty()));
        let err = resolver.resolve(&ctx(), "cred://nope").await.unwrap_err();
        assert_eq!(err.status_value(), 401);
        assert!(err.detail().contains("not accessible"), "{}", err.detail());
    }

    #[tokio::test]
    async fn a_malformed_reference_is_a_missing_secret() {
        let resolver = SecretResolver::new(std::sync::Arc::new(MockCredStoreClient::empty()));
        let err = resolver
            .resolve(&ctx(), "cred://has spaces")
            .await
            .unwrap_err();
        assert_eq!(err.status_value(), 500);
    }

    #[tokio::test]
    async fn an_unlinked_resolver_reports_the_missing_secret() {
        let err = SecretResolver::unlinked()
            .resolve(&ctx(), "cred://x")
            .await
            .unwrap_err();
        assert_eq!(err.status_value(), 500);
    }
}

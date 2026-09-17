//! Resolution of the credential material a plugin binding names (DESIGN §2.1
//! "Credential Isolation").
//!
//! A binding never carries a secret inline: the value that names the credential
//! is either a literal (a dev upstream, a non-secret identifier) or a
//! `cred://name` reference into the host's credential store. Only the reference
//! form needs the store, and a store that is not there is a **failure**, never
//! a silent fallthrough to an unauthenticated forward.
//!
//! The literal form is a property of the *resolver*, not a licence every plugin
//! takes: the OAuth2 plugin's `client_secret_ref` is accepted only in the
//! reference form (`domain::types::validate_oauth2_config`), because that value
//! is credential material. Which of a plugin's values may be literals is that
//! plugin's own decision, declared at its write-time validation.

use std::sync::Arc;

use credstore_sdk::api::CredStoreClientV1;
use credstore_sdk::models::SecretRef;
use toolkit_security::SecurityContext;

use crate::domain::types::CREDENTIAL_REFERENCE_SCHEME as CRED_SCHEME;
use crate::error::OagwError;

/// Resolves the values a plugin binding names into credential material.
#[derive(Clone, Default)]
pub struct SecretResolver {
    client: Option<Arc<dyn CredStoreClientV1>>,
}

impl std::fmt::Debug for SecretResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SecretResolver")
            .field("client", &self.client.is_some())
            .finish()
    }
}

impl SecretResolver {
    /// A resolver over `client`, when the host published one.
    #[must_use]
    pub fn new(client: Option<Arc<dyn CredStoreClientV1>>) -> Self {
        Self { client }
    }

    /// Resolve `value` into the credential to use.
    ///
    /// A literal passes through unchanged; a `cred://` reference is resolved
    /// with `security`, so the caller's own authorization decides what it may
    /// read.
    ///
    /// # Errors
    /// [`crate::error::OagwErrorKind::SecretNotFound`] — the host publishes no
    /// credential store, the reference is malformed, the secret does not exist
    /// (or is invisible to the caller), or it cannot be read. Every one of
    /// those is fail-closed: the request is never forwarded without the
    /// credential its binding asked for.
    pub async fn resolve(
        &self,
        security: &SecurityContext,
        value: &str,
    ) -> Result<String, OagwError> {
        let Some(reference) = value.strip_prefix(CRED_SCHEME) else {
            return Ok(value.to_owned());
        };

        let Some(client) = self.client.as_ref() else {
            return Err(OagwError::secret_not_found(format!(
                "the credential reference 'cred://{reference}' cannot be resolved: the host \
                 publishes no credential store"
            )));
        };

        let key = SecretRef::new(reference).map_err(|error| {
            OagwError::secret_not_found(format!(
                "the credential reference 'cred://{reference}' is not a usable secret name: {error}"
            ))
        })?;

        match client.get(security, &key).await {
            Ok(Some(secret)) => {
                let value = String::from_utf8_lossy(secret.value.as_bytes()).into_owned();
                Ok(value)
            }
            // A single 404 surface: the secret is not there or the caller may
            // not see it. Both are the same outcome for the request.
            Ok(None) => Err(OagwError::secret_not_found(format!(
                "the credential reference 'cred://{reference}' resolved to no secret this caller \
                 can read"
            ))),
            Err(error) => Err(OagwError::secret_not_found(format!(
                "the credential reference 'cred://{reference}' could not be read: {error}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use credstore_sdk::error::CredStoreError;
    use credstore_sdk::models::{GetSecretResponse, SecretValue, SharingMode};

    /// A credstore double that answers only one reference.
    struct StubStore {
        value: Option<&'static str>,
        error: bool,
    }

    #[async_trait]
    impl CredStoreClientV1 for StubStore {
        async fn get(
            &self,
            _ctx: &SecurityContext,
            _key: &SecretRef,
        ) -> Result<Option<GetSecretResponse>, CredStoreError> {
            if self.error {
                return Err(CredStoreError::invalid_ref("store is down"));
            }
            Ok(self.value.map(|value| GetSecretResponse {
                value: SecretValue::from(value),
                id: uuid::Uuid::new_v4(),
                owner_tenant_id: tenant_resolver_sdk::TenantId::nil(),
                sharing: SharingMode::Private,
                is_inherited: false,
                version: 1,
                secret_type: "gts.cf.core.credstore.secret_type.v1~token.v1".to_owned(),
                expires_at: None,
            }))
        }
    }

    fn security() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(uuid::Uuid::new_v4())
            .subject_tenant_id(uuid::Uuid::new_v4())
            .build()
            .expect("the security context builds")
    }

    #[tokio::test]
    async fn a_literal_is_not_a_reference_and_passes_through() {
        let resolver = SecretResolver::new(None);

        assert_eq!(
            resolver
                .resolve(&security(), "a-literal-key")
                .await
                .expect("a literal needs no store"),
            "a-literal-key"
        );
    }

    #[tokio::test]
    async fn a_reference_without_a_store_fails_closed() {
        let resolver = SecretResolver::new(None);

        let error = resolver
            .resolve(&security(), "cred://api-keys-vendor")
            .await
            .expect_err("no store published");

        assert_eq!(error.status().as_u16(), 500);
        assert_eq!(error.kind(), crate::error::OagwErrorKind::SecretNotFound);
        assert!(error.detail().contains("cred://api-keys-vendor"));
    }

    #[tokio::test]
    async fn a_reference_is_resolved_through_the_store() {
        let resolver = SecretResolver::new(Some(Arc::new(StubStore {
            value: Some("s3cr3t"),
            error: false,
        })));

        assert_eq!(
            resolver
                .resolve(&security(), "cred://api-keys-vendor")
                .await
                .expect("the store answers"),
            "s3cr3t"
        );
    }

    #[tokio::test]
    async fn a_missing_secret_fails_closed() {
        let resolver = SecretResolver::new(Some(Arc::new(StubStore {
            value: None,
            error: false,
        })));

        let error = resolver
            .resolve(&security(), "cred://api-keys-vendor")
            .await
            .expect_err("no such secret");

        assert_eq!(error.kind(), crate::error::OagwErrorKind::SecretNotFound);
        assert!(error.detail().contains("no secret"));
    }

    #[tokio::test]
    async fn a_store_failure_fails_closed() {
        let resolver = SecretResolver::new(Some(Arc::new(StubStore {
            value: None,
            error: true,
        })));

        let error = resolver
            .resolve(&security(), "cred://api-keys-vendor")
            .await
            .expect_err("the store is down");

        assert_eq!(error.kind(), crate::error::OagwErrorKind::SecretNotFound);
    }

    #[tokio::test]
    async fn a_malformed_reference_fails_closed() {
        let resolver = SecretResolver::new(Some(Arc::new(StubStore {
            value: Some("s3cr3t"),
            error: false,
        })));

        // A space is outside the `[a-zA-Z0-9_-]` alphabet of a `SecretRef`.
        let error = resolver
            .resolve(&security(), "cred://not a ref")
            .await
            .expect_err("not a usable secret name");

        assert_eq!(error.kind(), crate::error::OagwErrorKind::SecretNotFound);
        assert!(error.detail().contains("not a usable secret name"));
    }
}

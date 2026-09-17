//! Credential resolution against `cred_store`
//! ([DESIGN.md](../../../docs/DESIGN.md) "Secret Access Control").
//!
//! [`CredStoreSecretResolver`] is the production
//! [`SecretResolver`](crate::infra::plugins::SecretResolver): it looks a
//! reference up through the `cred_store` client it finds in the toolkit
//! [`ClientHub`], so the gear never stores secret material and the calling
//! tenant's access rules are enforced by the credential store itself.
//!
//! Resolution is fail-closed
//! ([ADR-0008](../../../docs/ADR/0008-oauth2-client-credentials-auth-plugin.md)):
//!
//! - a reference that does not exist or that the caller may not read maps to
//!   the 500 `cf.oagw.secret.not_found.v1` row of the error table;
//! - a store that cannot be consulted at all maps to the 500
//!   `cf.oagw.downstream.secret_error.v1` gateway error.
//!
//! Neither failure names the reference or echoes secret material: the
//! reference is operator data, the value is a credential. Both are gateway
//! errors, so the data plane reports them with
//! `X-OAGW-Error-Source: gateway`
//! ([ADR-0007](../../../docs/ADR/0007-error-source-distinction.md)).

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::{CredStoreClientV1, CredStoreError, SecretRef, SecretValue};
use toolkit::ClientHub;

use crate::domain::error::OagwError;
use crate::infra::plugins::{RequestContext, SecretResolver};

/// `cred://` scheme prefix an auth binding may put in front of a reference.
pub const SECRET_REF_SCHEME: &str = "cred://";

/// Message of the 500 raised when no credential store is wired into the gear.
const CREDSTORE_MISSING: &str = "the credential store is not available";

/// Message of the 500 raised when the credential store cannot be consulted.
const CREDSTORE_FAILED: &str = "the credential store could not be consulted";

/// Strips the optional [`SECRET_REF_SCHEME`] prefix and validates the rest.
///
/// An auth binding may spell a reference as `cred://name` or as the bare name;
/// both resolve identically, mirroring `credstore` itself.
///
/// # Errors
/// [`OagwError::Validation`] when the remainder is not a well-formed secret
/// reference.
pub fn normalize_secret_ref(raw: &str) -> Result<SecretRef, OagwError> {
    let name = raw.strip_prefix(SECRET_REF_SCHEME).unwrap_or(raw);
    SecretRef::new(name).map_err(|_| OagwError::Validation {
        message: format!("'{name}' is not a valid secret reference"),
    })
}

/// [`SecretResolver`] backed by the `cred_store` client of the toolkit
/// [`ClientHub`].
///
/// The client is looked up per resolution rather than once at startup: the
/// gear initializes before its dependencies are wired, and a request that
/// arrives without a credential store fails closed (500) instead of aborting
/// gear startup.
pub struct CredStoreSecretResolver {
    client_hub: Arc<ClientHub>,
}

impl std::fmt::Debug for CredStoreSecretResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredStoreSecretResolver")
            .finish_non_exhaustive()
    }
}

impl CredStoreSecretResolver {
    /// Builds a resolver that reads the credential store client from
    /// `client_hub`.
    #[must_use]
    pub fn new(client_hub: Arc<ClientHub>) -> Self {
        Self { client_hub }
    }

    /// The credential store client of the hub.
    ///
    /// # Errors
    /// [`OagwError::SecretError`] when no client is registered under the
    /// `CredStoreClientV1` contract.
    fn client(&self) -> Result<Arc<dyn CredStoreClientV1>, OagwError> {
        self.client_hub
            .try_get::<dyn CredStoreClientV1>()
            .ok_or_else(|| OagwError::SecretError {
                message: CREDSTORE_MISSING.to_owned(),
            })
    }
}

#[async_trait]
impl SecretResolver for CredStoreSecretResolver {
    async fn resolve(
        &self,
        ctx: &RequestContext,
        secret_ref: &SecretRef,
    ) -> Result<SecretValue, OagwError> {
        let client = self.client()?;
        match client.get(&ctx.security, secret_ref).await {
            Ok(Some(secret)) => Ok(secret.value),
            // `Ok(None)` is the store's single not-found surface: the reference
            // does not exist or the caller may not read it. A client that
            // reports that surface as an error maps onto it too, so both
            // spellings of "missing" fail identically.
            Ok(None) => Err(OagwError::SecretNotFound),
            Err(CredStoreError::NotFound) => Err(OagwError::SecretNotFound),
            Err(_) => Err(OagwError::SecretError {
                message: CREDSTORE_FAILED.to_owned(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use std::sync::Arc;

    use credstore_sdk::CredStoreClientV1;
    use credstore_sdk::test_util::MockCredStoreClient;
    use toolkit::ClientHub;

    use super::normalize_secret_ref;
    use crate::domain::error::{OagwError, SECRET_ERROR_GTS_ID, SECRET_NOT_FOUND_GTS_ID};
    use crate::infra::plugins::SecretResolver;
    use crate::infra::secrets::{CredStoreSecretResolver, SECRET_REF_SCHEME};
    use crate::infra::test_support::{context, hub_with_credstore, hub_without_credstore};

    const REFERENCE: &str = "partner-openai-key";
    const VALUE: &str = "sk-123";

    /// A hub holding a store with the secrets of this module's tests.
    fn store(secrets: &[(&str, &str)]) -> Arc<ClientHub> {
        hub_with_credstore(MockCredStoreClient::with_secrets(
            secrets
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
        ))
    }

    /// A hub holding a store that always fails.
    fn failing_hub() -> Arc<ClientHub> {
        hub_with_credstore(MockCredStoreClient::always_failing())
    }

    /// A hub holding a store that reports not-found as an error.
    fn not_found_erroring_hub() -> Arc<ClientHub> {
        hub_with_credstore(MockCredStoreClient::erroring_not_found())
    }

    #[test]
    fn normalize_strips_the_cred_scheme_prefix() {
        let with_scheme = normalize_secret_ref(&format!("{SECRET_REF_SCHEME}{REFERENCE}"))
            .expect("a valid reference");
        let bare = normalize_secret_ref(REFERENCE).expect("a valid reference");

        assert_eq!(with_scheme.as_ref(), REFERENCE);
        assert_eq!(bare.as_ref(), REFERENCE);
    }

    #[test]
    fn normalize_rejects_a_malformed_reference() {
        let error = normalize_secret_ref("has:colons").expect_err("not a valid reference");

        assert!(matches!(error, OagwError::Validation { .. }), "{error}");
        assert_eq!(error.http_status(), 400);
    }

    #[tokio::test]
    async fn resolve_returns_the_stored_value() {
        let resolver = CredStoreSecretResolver::new(store(&[(REFERENCE, VALUE)]));
        let reference =
            normalize_secret_ref(&format!("{SECRET_REF_SCHEME}{REFERENCE}")).expect("valid");

        let secret = resolver
            .resolve(&context(), &reference)
            .await
            .expect("the store holds the secret");

        assert_eq!(secret.as_bytes(), VALUE.as_bytes());
    }

    #[tokio::test]
    async fn resolve_cref_strips_the_cred_scheme_prefix() {
        let resolver = CredStoreSecretResolver::new(store(&[(REFERENCE, VALUE)]));

        let secret = resolver
            .resolve_cref(&context(), &format!("{SECRET_REF_SCHEME}{REFERENCE}"))
            .await
            .expect("resolvable reference");

        assert_eq!(secret.as_bytes(), VALUE.as_bytes());
    }

    #[tokio::test]
    async fn a_missing_reference_is_a_500_secret_not_found() {
        let hub = Arc::new(ClientHub::new());
        let client: Arc<dyn CredStoreClientV1> = Arc::new(MockCredStoreClient::empty());
        hub.register(client);
        let resolver = CredStoreSecretResolver::new(hub);

        let error = resolver
            .resolve_cref(&context(), REFERENCE)
            .await
            .unwrap_err();

        assert!(matches!(error, OagwError::SecretNotFound), "{error}");
        assert_eq!(error.http_status(), 500);
        assert_eq!(error.gts_id(), SECRET_NOT_FOUND_GTS_ID);
    }

    #[tokio::test]
    async fn a_failing_store_is_a_500_gateway_error() {
        let resolver = CredStoreSecretResolver::new(failing_hub());

        let error = resolver
            .resolve_cref(&context(), REFERENCE)
            .await
            .unwrap_err();

        assert!(matches!(error, OagwError::SecretError { .. }), "{error}");
        assert_eq!(error.http_status(), 500);
        assert_eq!(error.gts_id(), SECRET_ERROR_GTS_ID);
    }

    #[tokio::test]
    async fn a_store_that_reports_not_found_as_an_error_maps_to_500() {
        let resolver = CredStoreSecretResolver::new(not_found_erroring_hub());

        let error = resolver
            .resolve_cref(&context(), REFERENCE)
            .await
            .unwrap_err();

        assert!(matches!(error, OagwError::SecretNotFound), "{error}");
        assert_eq!(error.http_status(), 500);
        assert_eq!(error.gts_id(), SECRET_NOT_FOUND_GTS_ID);
    }

    #[tokio::test]
    async fn without_a_credstore_client_resolution_fails_closed() {
        let resolver = CredStoreSecretResolver::new(hub_without_credstore());

        let error = resolver
            .resolve_cref(&context(), REFERENCE)
            .await
            .unwrap_err();

        assert!(matches!(error, OagwError::SecretError { .. }), "{error}");
        assert_eq!(error.http_status(), 500);
        assert_eq!(error.gts_id(), SECRET_ERROR_GTS_ID);
    }

    #[tokio::test]
    async fn failures_never_name_the_reference_or_the_value() {
        let missing = CredStoreSecretResolver::new(store(&[]))
            .resolve_cref(&context(), REFERENCE)
            .await
            .unwrap_err();
        let failing = CredStoreSecretResolver::new(failing_hub())
            .resolve_cref(&context(), REFERENCE)
            .await
            .unwrap_err();

        for error in [missing, failing] {
            let rendered = error.to_string();
            assert!(
                !rendered.contains(REFERENCE) && !rendered.contains(VALUE),
                "the failure leaks credential data: {rendered}"
            );
        }
    }
}

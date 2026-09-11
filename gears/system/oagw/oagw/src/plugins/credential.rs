//! Credential resolution via `cred_store`
//! (`cpt-cf-oagw-algo-plugin-cred-resolve`).
//!
//! OAGW stores references only (`secret_ref`/`cred://...`); this module
//! resolves one such reference per call through the injected
//! [`CredStoreClientV1`], never persisting or memoizing the result itself
//! (a token cache entry, when one exists, is [`super::token_cache`]'s
//! concern, not this module's).
//!
//! RF-001: reached for real from `crate::proxy::engine` (via
//! `super::oauth2`/`super::auth`'s production code, in turn reached from
//! `super::execute`'s chain executor) whenever a request's merged
//! `AuthConfig` names `apikey`/`oauth2_client_cred`/`oauth2_client_cred_basic`.

use credstore_sdk::{CredStoreClientV1, CredStoreError, SecretRef};
use toolkit_auth::oauth2::SecretString;
use toolkit_security::SecurityContext;

/// A credential-reference resolution failure
/// (`cpt-cf-oagw-algo-plugin-cred-resolve`'s failure output). Every
/// variant is safe to log or embed in an RFC 9457 `detail`: none carries
/// secret material, only the reference's *shape* or the failure kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CredentialError {
    /// The reference was absent, empty, or blank where the plugin
    /// requires one (`inst-cred-resolve-01`).
    MissingReference,
    /// The reference string is not a well-formed `SecretRef`.
    InvalidReference,
    /// `cred_store` reports the secret does not exist or is not
    /// accessible to the calling tenant (`inst-cred-resolve-03`/`-04`).
    NotAccessible,
    /// A `cred_store` transport/service failure.
    Unavailable,
}

/// Resolve one credential reference through `cred_store`
/// (`inst-cred-resolve-01` through `-06`).
///
/// Accepts the bare reference or the `cred://`-prefixed spelling
/// interchangeably. The returned [`SecretString`] is a redacting,
/// zero-on-drop wrapper (`cpt-cf-oagw-dod-plugin-secret-nondisclosure`):
/// its `Debug`/`Display` never render the value, and its buffer is zeroed
/// when it drops.
// @cpt-algo:cpt-cf-oagw-algo-plugin-cred-resolve:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-cred-injection:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-secret-nondisclosure:p1
// @cpt-begin:cpt-cf-oagw-algo-plugin-cred-resolve:p1:inst-cred-resolve-01
// @cpt-begin:cpt-cf-oagw-algo-plugin-cred-resolve:p1:inst-cred-resolve-02
// @cpt-begin:cpt-cf-oagw-algo-plugin-cred-resolve:p1:inst-cred-resolve-03
// @cpt-begin:cpt-cf-oagw-algo-plugin-cred-resolve:p1:inst-cred-resolve-04
// @cpt-begin:cpt-cf-oagw-algo-plugin-cred-resolve:p1:inst-cred-resolve-05
// @cpt-begin:cpt-cf-oagw-algo-plugin-cred-resolve:p1:inst-cred-resolve-06
pub(crate) async fn resolve_secret(
    reference: &str,
    ctx: &SecurityContext,
    credstore: &dyn CredStoreClientV1,
) -> Result<SecretString, CredentialError> {
    let trimmed = reference.trim();
    if trimmed.is_empty() {
        return Err(CredentialError::MissingReference);
    }
    let bare = trimmed.strip_prefix("cred://").unwrap_or(trimmed);
    let secret_ref = SecretRef::new(bare).map_err(|_| CredentialError::InvalidReference)?;

    match credstore.get(ctx, &secret_ref).await {
        Ok(Some(response)) => {
            // Copy once into our own redacting wrapper; `response` (and its
            // own `SecretValue`) drops -- and zeroes -- at the end of this
            // scope.
            let text = String::from_utf8_lossy(response.value.as_bytes()).into_owned();
            Ok(SecretString::new(text))
        }
        Ok(None) => Err(CredentialError::NotAccessible),
        // A client implementation that reports the not-found surface as an
        // error (instead of `Ok(None)`) is still "the secret does not
        // exist", not a transport outage.
        Err(CredStoreError::NotFound) => Err(CredentialError::NotAccessible),
        Err(_) => Err(CredentialError::Unavailable),
    }
}
// @cpt-end:cpt-cf-oagw-algo-plugin-cred-resolve:p1:inst-cred-resolve-06
// @cpt-end:cpt-cf-oagw-algo-plugin-cred-resolve:p1:inst-cred-resolve-05
// @cpt-end:cpt-cf-oagw-algo-plugin-cred-resolve:p1:inst-cred-resolve-04
// @cpt-end:cpt-cf-oagw-algo-plugin-cred-resolve:p1:inst-cred-resolve-03
// @cpt-end:cpt-cf-oagw-algo-plugin-cred-resolve:p1:inst-cred-resolve-02
// @cpt-end:cpt-cf-oagw-algo-plugin-cred-resolve:p1:inst-cred-resolve-01

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use credstore_sdk::test_util::MockCredStoreClient;
    use uuid::Uuid;

    fn ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn resolves_a_secret_from_a_cred_url_reference() {
        let store = MockCredStoreClient::with_secrets(vec![(
            "partner-key".to_owned(),
            "sk-live-xyz".to_owned(),
        )]);
        let secret = resolve_secret("cred://partner-key", &ctx(), &store)
            .await
            .unwrap();
        assert_eq!(secret.expose(), "sk-live-xyz");
    }

    #[tokio::test]
    async fn resolves_a_secret_from_a_bare_reference() {
        let store =
            MockCredStoreClient::with_secrets(vec![("partner-key".to_owned(), "v".to_owned())]);
        let secret = resolve_secret("partner-key", &ctx(), &store).await.unwrap();
        assert_eq!(secret.expose(), "v");
    }

    #[tokio::test]
    async fn missing_secret_maps_to_not_accessible() {
        let store = MockCredStoreClient::empty();
        let err = resolve_secret("cred://absent", &ctx(), &store)
            .await
            .unwrap_err();
        assert_eq!(err, CredentialError::NotAccessible);
    }

    #[tokio::test]
    async fn empty_reference_is_a_missing_reference_error() {
        let store = MockCredStoreClient::empty();
        let err = resolve_secret("", &ctx(), &store).await.unwrap_err();
        assert_eq!(err, CredentialError::MissingReference);
    }

    #[tokio::test]
    async fn transport_failure_maps_to_unavailable() {
        let store = MockCredStoreClient::always_failing();
        let err = resolve_secret("cred://x", &ctx(), &store)
            .await
            .unwrap_err();
        assert_eq!(err, CredentialError::Unavailable);
    }

    #[tokio::test]
    async fn not_found_reported_as_error_still_maps_to_not_accessible() {
        let store = MockCredStoreClient::erroring_not_found();
        let err = resolve_secret("cred://x", &ctx(), &store)
            .await
            .unwrap_err();
        assert_eq!(err, CredentialError::NotAccessible);
    }

    #[test]
    fn debug_and_display_never_expose_the_resolved_value() {
        let secret = SecretString::new("super-secret-value");
        let debug = format!("{secret:?}");
        let display = format!("{secret}");
        assert!(!debug.contains("super-secret-value"));
        assert!(!display.contains("super-secret-value"));
    }
}

//! Shared secret-resolution helper for auth plugins.
//!
//! OAGW never stores secret material; auth plugins resolve
//! `cred://...` references against the credstore SDK exactly once per
//! credential need (token fetches are cached separately — see ADR-0008).

use std::sync::Arc;

use credstore_sdk::{CredStoreClientV1, SecretRef};
use toolkit_auth::oauth2::SecretString;
use toolkit_security::SecurityContext;

use crate::domain::plugin::PluginError;

/// Strip an optional `cred://` prefix from a secret reference.
#[must_use]
pub fn strip_cred_prefix(reference: &str) -> &str {
    reference.strip_prefix("cred://").unwrap_or(reference)
}

/// Resolve a `cred://...` reference to its material.
///
/// # Errors
///
/// Returns [`PluginError::Config`] for a malformed reference,
/// [`PluginError::Secret`] when the secret does not exist / is inaccessible,
/// and [`PluginError::Internal`] on unexpected resolver failures.
pub async fn lookup_secret(
    credstore: &Arc<dyn CredStoreClientV1>,
    ctx: &SecurityContext,
    reference: &str,
) -> Result<SecretString, PluginError> {
    let key = strip_cred_prefix(reference);
    let secret_ref = SecretRef::new(key).map_err(|e| PluginError::Config {
        detail: format!("invalid secret reference {reference:?}: {e}"),
    })?;
    let found = credstore
        .get(ctx, &secret_ref)
        .await
        .map_err(|e| PluginError::Internal {
            diagnostic: format!("credstore lookup failed for {reference:?}: {e}"),
        })?;
    let Some(resp) = found else {
        return Err(PluginError::Secret {
            reference: reference.to_owned(),
        });
    };
    // Secrets are byte material; silently lossy-mangling them could corrupt
    // credentials. Non-UTF-8 material is a hard secret-resolution failure.
    let material =
        String::from_utf8(resp.value.as_bytes().to_vec()).map_err(|_| PluginError::Secret {
            reference: reference.to_owned(),
        })?;
    Ok(SecretString::new(material))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn security() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(uuid::Uuid::new_v4())
            .subject_tenant_id(uuid::Uuid::new_v4())
            .build()
            .expect("valid security context")
    }

    #[tokio::test]
    async fn resolves_utf8_secret_with_or_without_cred_prefix() {
        let store: Arc<dyn CredStoreClientV1> =
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::with_secrets(
                vec![("api-key".to_owned(), "plain-secret".to_owned())],
            ));
        let ctx = security();
        let bare = lookup_secret(&store, &ctx, "api-key")
            .await
            .expect("bare ref");
        assert_eq!(bare.expose(), "plain-secret");
        let prefixed = lookup_secret(&store, &ctx, "cred://api-key")
            .await
            .expect("prefixed ref");
        assert_eq!(prefixed.expose(), "plain-secret");
    }

    #[tokio::test]
    async fn non_utf8_secret_material_is_a_hard_secret_failure() {
        // Secret material is bytes; silently lossy-mangling it could corrupt
        // credentials (e.g. a binary session key). This must be an explicit
        // `Secret` error, never a mangled string handed to a plugin.
        let store: Arc<dyn CredStoreClientV1> = Arc::new(
            credstore_sdk::test_util::MockCredStoreClient::returning_raw_value(vec![
                0xff, 0xfe, b'k', b'e', b'y',
            ]),
        );
        let err = lookup_secret(&store, &security(), "cred://hsm-key")
            .await
            .expect_err("non-UTF-8 material must fail resolution");
        assert!(
            matches!(err, PluginError::Secret { ref reference } if reference == "cred://hsm-key")
        );
    }

    #[tokio::test]
    async fn unresolvable_secret_and_resolver_failure_are_distinct() {
        let empty: Arc<dyn CredStoreClientV1> =
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty());
        let err = lookup_secret(&empty, &security(), "cred://missing")
            .await
            .expect_err("missing secret");
        assert!(matches!(err, PluginError::Secret { .. }));

        let failing: Arc<dyn CredStoreClientV1> =
            Arc::new(credstore_sdk::test_util::MockCredStoreClient::always_failing());
        let err = lookup_secret(&failing, &security(), "cred://boom")
            .await
            .expect_err("resolver failure");
        assert!(matches!(err, PluginError::Internal { .. }));
    }

    #[test]
    fn strip_prefix_removes_only_cred_prefix() {
        assert_eq!(strip_cred_prefix("cred://a"), "a");
        assert_eq!(strip_cred_prefix("a"), "a");
    }
}

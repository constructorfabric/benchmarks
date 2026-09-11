//! `cred://` reference resolution.
//!
//! `cpt-cf-oagw-principle-cred-isolation`: OAGW never stores secret material.
//! Auth plugins hold a reference and resolve it per request against the
//! credstore, which decides visibility across the tenant hierarchy.

use credstore_sdk::{CredStoreClientV1, SecretRef};
use std::sync::Arc;
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::plugin::PluginError;

/// Strip the `cred://` scheme from a secret reference. A bare reference is
/// accepted so that operators are not forced into the URI spelling.
#[must_use]
pub fn strip_scheme(reference: &str) -> &str {
    reference
        .trim()
        .strip_prefix("cred://")
        .unwrap_or_else(|| reference.trim())
}

/// Resolve `reference` for the calling tenant.
///
/// # Errors
///
/// * [`PluginError::Rejected`] with a `500 SecretNotFound` when the reference
///   is malformed or resolves to nothing.
/// * [`PluginError::Rejected`] with a `401` when the credstore refuses access
///   — `docs/DESIGN.md` §"Secret Access Control" step 4.
pub async fn resolve_secret(
    credstore: &Arc<dyn CredStoreClientV1>,
    ctx: &SecurityContext,
    reference: &str,
) -> Result<String, PluginError> {
    let key = SecretRef::new(strip_scheme(reference)).map_err(|_| {
        // The reference itself is operator-supplied configuration, not secret
        // material, so echoing it back is safe and makes the error actionable.
        PluginError::Rejected(DomainError::secret_not_found(format!(
            "secret reference '{reference}' is not a valid credstore key"
        )))
    })?;

    match credstore.get(ctx, &key).await {
        Ok(Some(secret)) => String::from_utf8(secret.value.as_bytes().to_vec()).map_err(|_| {
            PluginError::Rejected(DomainError::secret_not_found(format!(
                "secret '{reference}' is not valid UTF-8"
            )))
        }),
        Ok(None) => Err(PluginError::Rejected(DomainError::secret_not_found(
            format!("secret '{reference}' was not found"),
        ))),
        Err(err) => Err(PluginError::Rejected(
            DomainError::authentication_failed(format!(
                "credential store refused '{reference}': {err}"
            )),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_is_optional() {
        assert_eq!(strip_scheme("cred://openai-key"), "openai-key");
        assert_eq!(strip_scheme("  openai-key "), "openai-key");
    }

    fn store(pairs: Vec<(&str, &str)>) -> Arc<dyn CredStoreClientV1> {
        Arc::new(credstore_sdk::test_util::MockCredStoreClient::with_secrets(
            pairs
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        ))
    }

    #[tokio::test]
    async fn a_known_reference_resolves() {
        let store = store(vec![("openai-key", "sk-test")]);
        let value = resolve_secret(&store, &SecurityContext::anonymous(), "cred://openai-key")
            .await
            .expect("resolves");
        assert_eq!(value, "sk-test");
    }

    #[tokio::test]
    async fn missing_secret_maps_to_500_secret_not_found() {
        let store = store(Vec::new());
        let err = resolve_secret(&store, &SecurityContext::anonymous(), "cred://absent")
            .await
            .expect_err("absent secret");
        match err {
            PluginError::Rejected(domain) => assert_eq!(domain.status(), 500),
            PluginError::Internal(msg) => panic!("unexpected internal error: {msg}"),
        }
    }

    #[tokio::test]
    async fn invalid_reference_is_rejected_before_any_lookup() {
        let store = store(Vec::new());
        let err = resolve_secret(&store, &SecurityContext::anonymous(), "cred://not valid!")
            .await
            .expect_err("invalid reference");
        assert!(matches!(err, PluginError::Rejected(_)));
    }
}

//! Credential resolution for auth plugins.
//!
//! OAGW never stores secret material: a plugin holds a `cred://` reference
//! and resolves it through the credential store at request time, with the
//! caller's own [`SecurityContext`] so `cred_store`'s sharing policy decides
//! what is reachable (`cpt-cf-oagw-principle-cred-isolation`).

use credstore_sdk::{CredStoreClientV1, CredStoreError, SecretRef};
use toolkit_auth::oauth2::SecretString;
use toolkit_security::SecurityContext;

use crate::domain::error::PluginError;

/// Scheme prefix an operator writes in configuration.
pub const CRED_SCHEME: &str = "cred://";

/// Strip the `cred://` scheme from a reference, if present.
#[must_use]
pub fn strip_cred_scheme(raw: &str) -> &str {
    let trimmed = raw.trim();
    trimmed.strip_prefix(CRED_SCHEME).unwrap_or(trimmed)
}

/// Resolve `raw_ref` through the credential store.
///
/// # Errors
///
/// * [`PluginError::Config`] — the reference is not a legal secret key;
/// * [`PluginError::SecretNotFound`] — no accessible secret with that key;
/// * [`PluginError::AccessDenied`] — the caller lacks read permission;
/// * [`PluginError::Internal`] — the credential store is unreachable.
pub async fn resolve_secret(
    credstore: &dyn CredStoreClientV1,
    ctx: &SecurityContext,
    raw_ref: &str,
) -> Result<SecretString, PluginError> {
    let key = SecretRef::new(strip_cred_scheme(raw_ref)).map_err(|err| {
        // The reference itself is safe to echo; the value never is.
        PluginError::Config(format!("invalid credential reference '{raw_ref}': {err}"))
    })?;
    match credstore.get(ctx, &key).await {
        Ok(Some(response)) => {
            let bytes = response.value.as_bytes();
            let text = std::str::from_utf8(bytes).map_err(|_| {
                PluginError::Config(format!(
                    "credential '{raw_ref}' is not valid UTF-8 and cannot be injected as a \
                     header or query value"
                ))
            })?;
            Ok(SecretString::new(text.to_owned()))
        }
        Ok(None) => Err(PluginError::SecretNotFound(format!(
            "credential '{raw_ref}' was not found or is not accessible to this tenant"
        ))),
        Err(CredStoreError::AccessDenied) => Err(PluginError::AccessDenied(format!(
            "read access to credential '{raw_ref}' was denied"
        ))),
        Err(CredStoreError::NotFound) => Err(PluginError::SecretNotFound(format!(
            "credential '{raw_ref}' was not found"
        ))),
        Err(err) => Err(PluginError::Internal(format!(
            "credential store failure resolving '{raw_ref}': {err}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::{resolve_secret, strip_cred_scheme};
    use crate::domain::error::PluginError;
    use credstore_sdk::test_util::MockCredStoreClient;
    use toolkit_security::SecurityContext;
    use uuid::Uuid;

    fn ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .expect("security context")
    }

    #[test]
    fn scheme_is_optional() {
        assert_eq!(strip_cred_scheme("cred://openai-key"), "openai-key");
        assert_eq!(strip_cred_scheme("  openai-key "), "openai-key");
    }

    #[tokio::test]
    async fn resolves_a_stored_secret_through_either_spelling() {
        let store = MockCredStoreClient::with_secrets(vec![(
            "openai-key".to_owned(),
            "sk-test-e2e-fake-key".to_owned(),
        )]);
        let ctx = ctx();
        for reference in ["cred://openai-key", "openai-key"] {
            let resolved = resolve_secret(&store, &ctx, reference)
                .await
                .expect("resolved");
            assert_eq!(resolved.expose(), "sk-test-e2e-fake-key");
        }
    }

    #[tokio::test]
    async fn missing_secret_maps_to_secret_not_found() {
        let store = MockCredStoreClient::empty();
        let err = resolve_secret(&store, &ctx(), "cred://absent")
            .await
            .expect_err("missing");
        assert!(matches!(err, PluginError::SecretNotFound(_)));
    }

    #[tokio::test]
    async fn a_malformed_reference_is_a_config_error() {
        let store = MockCredStoreClient::empty();
        let err = resolve_secret(&store, &ctx(), "cred://has spaces")
            .await
            .expect_err("malformed");
        assert!(matches!(err, PluginError::Config(_)));
    }

    #[tokio::test]
    async fn a_store_failure_is_internal_not_a_missing_secret() {
        let store = MockCredStoreClient::always_failing();
        let err = resolve_secret(&store, &ctx(), "openai-key")
            .await
            .expect_err("unreachable");
        assert!(matches!(err, PluginError::Internal(_)));
    }

    #[tokio::test]
    async fn a_non_utf8_value_is_refused_rather_than_lossily_injected() {
        let store = MockCredStoreClient::returning_raw_value(vec![0xff, 0xfe]);
        let err = resolve_secret(&store, &ctx(), "openai-key")
            .await
            .expect_err("not injectable");
        assert!(matches!(err, PluginError::Config(_)));
    }
}

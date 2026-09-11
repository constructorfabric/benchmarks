//! CredStore reference resolution shared by the auth plugins.
//!
//! `cpt-cf-oagw-principle-cred-isolation`: OAGW stores no secret material, it
//! only carries `cred://<reference>` pointers. This module is the one place
//! that turns such a pointer into a value, and it never puts the value into an
//! error message or a log line.

use credstore_sdk::{CredStoreClientV1, CredStoreError, SecretRef};
use toolkit_auth::oauth2::SecretString;
use toolkit_security::SecurityContext;

use crate::domain::plugin::PluginError;

/// Strip the `cred://` scheme from a secret reference.
#[must_use]
pub fn strip_scheme(secret_ref: &str) -> &str {
    let trimmed = secret_ref.trim();
    trimmed
        .strip_prefix("cred://")
        .unwrap_or(trimmed)
        .trim_start_matches('/')
}

/// Resolve a `cred://` reference to its value.
///
/// # Errors
///
/// * [`PluginError::InvalidConfig`] — the reference is malformed.
/// * [`PluginError::SecretNotFound`] — no accessible secret with that
///   reference exists for the calling tenant.
/// * [`PluginError::Unauthenticated`] — CredStore refused the read.
/// * [`PluginError::Internal`] — CredStore is unavailable, or the value is not
///   valid UTF-8.
pub async fn resolve_secret_ref(
    credstore: &dyn CredStoreClientV1,
    ctx: &SecurityContext,
    secret_ref: &str,
) -> Result<SecretString, PluginError> {
    let bare = strip_scheme(secret_ref);
    let key = SecretRef::new(bare).map_err(|err| {
        PluginError::InvalidConfig(format!("invalid secret reference {secret_ref:?}: {err}"))
    })?;
    let response = credstore.get(ctx, &key).await.map_err(|err| match err {
        CredStoreError::NotFound => {
            PluginError::SecretNotFound(format!("secret {secret_ref:?} not found"))
        }
        CredStoreError::AccessDenied => PluginError::Unauthenticated(format!(
            "secret {secret_ref:?} is not accessible to this tenant"
        )),
        other => PluginError::Internal(format!("credential store failure: {other}")),
    })?;
    let Some(response) = response else {
        return Err(PluginError::SecretNotFound(format!(
            "secret {secret_ref:?} not found"
        )));
    };
    let value = String::from_utf8(response.value.as_bytes().to_vec()).map_err(|_| {
        PluginError::Internal(format!(
            "secret {secret_ref:?} is not valid UTF-8 and cannot be injected as a header"
        ))
    })?;
    Ok(SecretString::new(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use credstore_sdk::test_util::MockCredStoreClient;
    use uuid::Uuid;

    fn security() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .expect("context")
    }

    #[test]
    fn scheme_is_optional() {
        assert_eq!(strip_scheme("cred://openai-key"), "openai-key");
        assert_eq!(strip_scheme("openai-key"), "openai-key");
        assert_eq!(strip_scheme("  cred://openai-key "), "openai-key");
    }

    #[tokio::test]
    async fn resolves_a_known_reference() {
        let store = MockCredStoreClient::with_secrets(vec![(
            "openai-key".to_owned(),
            "sk-test".to_owned(),
        )]);
        let value = resolve_secret_ref(&store, &security(), "cred://openai-key")
            .await
            .expect("resolved");
        assert_eq!(value.expose(), "sk-test");
    }

    #[tokio::test]
    async fn absent_reference_is_secret_not_found() {
        let store = MockCredStoreClient::empty();
        let err = resolve_secret_ref(&store, &security(), "cred://absent")
            .await
            .expect_err("absent");
        assert!(matches!(err, PluginError::SecretNotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn erroring_not_found_is_also_secret_not_found() {
        let store = MockCredStoreClient::erroring_not_found();
        let err = resolve_secret_ref(&store, &security(), "cred://absent")
            .await
            .expect_err("absent");
        assert!(matches!(err, PluginError::SecretNotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn backend_failure_is_internal() {
        let store = MockCredStoreClient::always_failing();
        let err = resolve_secret_ref(&store, &security(), "cred://x")
            .await
            .expect_err("failing backend");
        assert!(matches!(err, PluginError::Internal(_)), "{err:?}");
    }

    #[tokio::test]
    async fn non_utf8_value_is_rejected_without_echoing_it() {
        let store = MockCredStoreClient::returning_raw_value(vec![0xff, 0xfe]);
        let err = resolve_secret_ref(&store, &security(), "cred://binary")
            .await
            .expect_err("binary value");
        let rendered = err.to_string();
        assert!(rendered.contains("not valid UTF-8"), "{rendered}");
        assert!(!rendered.contains("\u{fffd}"), "value must not be echoed");
    }

    #[tokio::test]
    async fn malformed_reference_is_a_config_error() {
        let store = MockCredStoreClient::empty();
        let err = resolve_secret_ref(&store, &security(), "cred://not a ref")
            .await
            .expect_err("malformed");
        assert!(matches!(err, PluginError::InvalidConfig(_)), "{err:?}");
    }
}

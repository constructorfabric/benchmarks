//! Credential resolution for the built-in auth plugins.
//!
//! Secrets are read from the `credstore` dependency when the hub exposes a
//! [`CredStoreClientV1`]; otherwise an inline `value` in the plugin
//! configuration is honoured. Resolved values are returned to the caller and
//! never logged: every failure path reports only the *reference name*, never
//! the secret bytes.

use std::sync::Arc;

use credstore_sdk::{CredStoreClientV1, SecretRef};
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;
use crate::domain::plugin::PluginContext;

/// Where an auth plugin obtains its credentials.
#[derive(Clone, Default)]
pub struct CredentialSource {
    /// In-process credstore client, when the dependency is wired.
    client: Option<Arc<dyn CredStoreClientV1>>,
}

impl std::fmt::Debug for CredentialSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialSource")
            .field("credstore", &self.client.is_some())
            .finish()
    }
}

/// Configuration keys accepted for a credstore reference.
///
/// `client_secret_ref` is the OAuth2 client-credentials spelling of the same
/// thing: without it the plugin could never reach the credstore.
pub const SECRET_REF_KEYS: [&str; 5] = [
    "secret_ref",
    "credential_ref",
    "secret",
    "cred_ref",
    "client_secret_ref",
];
/// Configuration keys accepted for an inline (non-production) secret.
///
/// `client_secret` is the OAuth2 client-credentials spelling: the plugin's own
/// configuration documents it, so an inline credential must resolve through it.
pub const INLINE_VALUE_KEYS: [&str; 5] = [
    "value",
    "token",
    "secret_value",
    "api_key",
    "client_secret",
];

impl CredentialSource {
    /// Build a source around an optional credstore client.
    #[must_use]
    pub fn new(client: Option<Arc<dyn CredStoreClientV1>>) -> Self {
        Self { client }
    }

    /// A source with no backing store (inline configuration only).
    #[must_use]
    pub fn inline_only() -> Self {
        Self::new(None)
    }

    /// Resolve the credential named by `config`.
    ///
    /// The credstore reference is preferred; the inline value is the fallback
    /// so that a deployment without a credstore dependency still works.
    ///
    /// # Errors
    /// [`DomainError::SecretNotFound`] when neither source yields a value.
    pub async fn resolve(
        &self,
        ctx: &PluginContext,
        config: &serde_json::Value,
    ) -> Result<String, DomainError> {
        if let Some(value) = self.resolve_optional(ctx, config).await? {
            return Ok(value);
        }
        Err(DomainError::SecretNotFound)
    }

    /// Like [`Self::resolve`] but returns `None` when no source is configured
    /// at all (the plugin may then no-op).
    ///
    /// # Errors
    /// [`DomainError::SecretNotFound`] when a reference is configured but the
    /// credstore cannot satisfy it.
    pub async fn resolve_optional(
        &self,
        ctx: &PluginContext,
        config: &serde_json::Value,
    ) -> Result<Option<String>, DomainError> {
        let reference = SECRET_REF_KEYS
            .iter()
            .find_map(|key| config.get(*key).and_then(serde_json::Value::as_str))
            .filter(|r| !r.trim().is_empty());
        if let Some(reference) = reference {
            return self
                .read_secret(ctx, reference.trim())
                .await
                .map(Some);
        }
        Ok(
            INLINE_VALUE_KEYS
                .iter()
                .find_map(|key| config.get(*key).and_then(serde_json::Value::as_str))
                .filter(|v| !v.is_empty())
                .map(str::to_owned),
        )
    }

    async fn read_secret(
        &self,
        ctx: &PluginContext,
        reference: &str,
    ) -> Result<String, DomainError> {
        let bare = reference
            .strip_prefix("cred://")
            .unwrap_or(reference)
            .to_owned();
        let Some(client) = self.client.as_ref() else {
            tracing::debug!("credential reference present but credstore is not wired");
            return Err(DomainError::SecretNotFound);
        };
        let secret_ref = SecretRef::new(bare.clone())
            .map_err(|_| DomainError::InvalidTargetHost(format!("credential reference '{bare}'")))?;
        // Credstore secrets are owned by a (subject, tenant) pair, so the lookup
        // must carry the *downstream caller's* security context. Falling back to
        // a tenant-derived context keeps system-initiated invocations (no HTTP
        // caller behind the request) working.
        let security = ctx.security_context.clone().unwrap_or_else(|| {
            SecurityContext::builder()
                .subject_id(ctx.tenant_id)
                .subject_type("service")
                .subject_tenant_id(ctx.tenant_id)
                .build()
                .unwrap_or_else(|_| SecurityContext::anonymous())
        });
        match client.get(&security, &secret_ref).await {
            Ok(Some(response)) => match std::str::from_utf8(response.value.as_bytes()) {
                Ok(text) if !text.is_empty() => Ok(text.trim_end_matches('\n').to_owned()),
                _ => Err(DomainError::SecretNotFound),
            },
            Ok(None) => Err(DomainError::SecretNotFound),
            Err(err) => {
                // Never log the secret; the error carries only the reference.
                tracing::debug!(reference = %bare, error = %err, "credential lookup failed");
                Err(DomainError::SecretNotFound)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use credstore_sdk::test_util::MockCredStoreClient;

    fn ctx() -> PluginContext {
        PluginContext {
            tenant_id: uuid::Uuid::nil(),
            ..PluginContext::default()
        }
    }

    #[tokio::test]
    async fn inline_value_is_used_when_present() {
        let source = CredentialSource::inline_only();
        let config = serde_json::json!({ "value": "sk-inline" });
        let resolved = source
            .resolve(&ctx(), &config)
            .await
            .expect("inline credential");
        assert_eq!(resolved, "sk-inline");
    }

    #[tokio::test]
    async fn credstore_reference_wins_over_inline() {
        let source = CredentialSource::new(Some(Arc::new(MockCredStoreClient::with_secrets(
            vec![("openai".to_owned(), "sk-from-store".to_owned())],
        ))));
        let config = serde_json::json!({ "secret_ref": "openai", "value": "sk-inline" });
        let resolved = source
            .resolve(&ctx(), &config)
            .await
            .expect("credential from credstore");
        assert_eq!(resolved, "sk-from-store");
    }

    #[tokio::test]
    async fn missing_reference_is_a_secret_error() {
        let source = CredentialSource::new(Some(Arc::new(MockCredStoreClient::empty())));
        let config = serde_json::json!({ "secret_ref": "nope" });
        let err = source.resolve(&ctx(), &config).await.expect_err("missing");
        assert_eq!(err.status(), 500);
    }

    #[tokio::test]
    async fn nothing_configured_is_optional() {
        let source = CredentialSource::inline_only();
        let resolved = source
            .resolve_optional(&ctx(), &serde_json::json!({}))
            .await
            .expect("no credential configured");
        assert!(resolved.is_none());
    }

    /// Credstore double that records the security context it was handed, so the
    /// tests can assert the *downstream caller's* context reaches the store.
    struct RecordingCredStore {
        store: std::collections::HashMap<String, Vec<u8>>,
        seen: parking_lot::Mutex<Vec<SecurityContext>>,
    }

    impl RecordingCredStore {
        fn with_secret(reference: &str, value: &str) -> Self {
            Self {
                store: std::collections::HashMap::from([(
                    reference.to_owned(),
                    value.as_bytes().to_vec(),
                )]),
                seen: parking_lot::Mutex::new(Vec::new()),
            }
        }

        fn seen(&self) -> Vec<SecurityContext> {
            self.seen.lock().clone()
        }
    }

    #[async_trait::async_trait]
    impl CredStoreClientV1 for RecordingCredStore {
        async fn get(
            &self,
            ctx: &SecurityContext,
            key: &SecretRef,
        ) -> Result<Option<credstore_sdk::GetSecretResponse>, credstore_sdk::CredStoreError> {
            self.seen.lock().push(ctx.clone());
            let reference = key.as_ref();
            Ok(self.store.get(reference).map(|value| {
                credstore_sdk::GetSecretResponse {
                    value: credstore_sdk::SecretValue::new(value.clone()),
                    id: uuid::Uuid::nil(),
                    owner_tenant_id: credstore_sdk::TenantId::nil(),
                    sharing: credstore_sdk::SharingMode::Private,
                    is_inherited: false,
                    version: 1,
                    secret_type: "gts.cf.core.credstore.secret_type.v1~generic.v1".to_owned(),
                    expires_at: None,
                }
            }))
        }
    }

    fn caller_context() -> PluginContext {
        PluginContext {
            tenant_id: uuid::Uuid::from_u128(0x42),
            security_context: Some(
                SecurityContext::builder()
                    .subject_id(uuid::Uuid::from_u128(0x1111))
                    .subject_type("user")
                    .subject_tenant_id(uuid::Uuid::from_u128(0x22))
                    .build()
                    .expect("caller security context"),
            ),
            ..PluginContext::default()
        }
    }

    #[tokio::test]
    async fn the_callers_security_context_reaches_the_credstore() {
        let store = Arc::new(RecordingCredStore::with_secret("openai", "sk-caller"));
        let source = CredentialSource::new(Some(Arc::clone(&store) as Arc<dyn CredStoreClientV1>));
        let config = serde_json::json!({ "secret_ref": "openai" });

        let resolved = source
            .resolve(&caller_context(), &config)
            .await
            .expect("caller-owned secret");
        assert_eq!(resolved, "sk-caller");

        let seen = store.seen();
        assert_eq!(seen.len(), 1, "exactly one credstore lookup");
        let used = &seen[0];
        assert_eq!(
            used.subject_id(),
            uuid::Uuid::from_u128(0x1111),
            "the caller's subject id must be used, not the gateway tenant"
        );
        assert_eq!(
            used.subject_tenant_id(),
            uuid::Uuid::from_u128(0x22),
            "the caller's tenant must be used, not the gateway tenant"
        );
    }

    #[tokio::test]
    async fn without_a_caller_context_a_tenant_derived_one_is_used() {
        let store = Arc::new(RecordingCredStore::with_secret("openai", "sk-tenant"));
        let source = CredentialSource::new(Some(Arc::clone(&store) as Arc<dyn CredStoreClientV1>));
        let config = serde_json::json!({ "secret_ref": "openai" });

        let fallback = PluginContext {
            tenant_id: uuid::Uuid::from_u128(0x42),
            ..PluginContext::default()
        };
        let resolved = source
            .resolve(&fallback, &config)
            .await
            .expect("tenant-derived lookup");
        assert_eq!(resolved, "sk-tenant");

        let seen = store.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].subject_id(), uuid::Uuid::from_u128(0x42));
        assert_eq!(seen[0].subject_tenant_id(), uuid::Uuid::from_u128(0x42));
    }

    #[test]
    fn resolved_secrets_are_never_logged_or_debug_printed() {
        let secret = credstore_sdk::SecretValue::from("sk-never-log-me");
        // The value type itself redacts, and the error paths of `read_secret`
        // only ever carry the reference name.
        assert_eq!(format!("{secret:?}"), "[REDACTED]");
        assert_eq!(format!("{secret}"), "[REDACTED]");
        let err = DomainError::SecretNotFound;
        let rendered = err.to_string();
        assert!(!rendered.contains("sk-never-log-me"));
    }
}

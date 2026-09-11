//! Credential resolution for the built-in auth plugins.
//!
//! References are credential-store keys. A `cred://` prefix is accepted and
//! stripped so operators can paste a full URI; the store itself only admits
//! `[a-zA-Z0-9_-]`.

use toolkit_security::SecurityContext;

/// Resolves credential references to their secret values.
#[derive(Clone)]
pub struct SecretResolver {
    store: std::sync::Arc<dyn credstore_sdk::CredStoreClientV1>,
}

/// Error produced while resolving a credential reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveError(pub String);

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ResolveError {}

impl SecretResolver {
    /// Creates a resolver over the credential store.
    #[must_use]
    pub fn new(store: std::sync::Arc<dyn credstore_sdk::CredStoreClientV1>) -> Self {
        Self { store }
    }

    /// Resolves a reference to a UTF-8 secret.
    ///
    /// # Errors
    ///
    /// Returns [`ResolveError`] when the reference is malformed, the secret is
    /// absent, unreadable or not valid UTF-8.
    pub async fn resolve(
        &self,
        ctx: &crate::domain::plugin::RequestContext,
        secret_ref: &str,
    ) -> Result<String, ResolveError> {
        let key = strip_scheme(secret_ref);
        let reference = credstore_sdk::SecretRef::new(key)
            .map_err(|error| ResolveError(format!("invalid credential reference: {error}")))?;

        let security = SecurityContext::builder()
            .subject_id(ctx.subject_id)
            .subject_tenant_id(ctx.subject_tenant_id)
            .build()
            .map_err(|error| ResolveError(format!("invalid request context: {error}")))?;

        let found = self
            .store
            .get(&security, &reference)
            .await
            .map_err(|error| ResolveError(format!("credential lookup failed: {error}")))?;

        let Some(response) = found else {
            return Err(ResolveError(secret_ref.to_owned()));
        };
        String::from_utf8(response.value.as_bytes().to_vec())
            .map(|value| value.trim().to_owned())
            .map_err(|_| ResolveError(secret_ref.to_owned()))
    }
}

/// Strips a leading `cred://` (or `cred:`) scheme.
#[must_use]
pub fn strip_scheme(secret_ref: &str) -> &str {
    secret_ref
        .strip_prefix("cred://")
        .unwrap_or_else(|| secret_ref.strip_prefix("cred:").unwrap_or(secret_ref))
}

/// A stable, non-secret fingerprint of a plugin config object, used as part of
/// the `OAuth2` token cache key.
#[must_use]
pub fn hash_config(config: &serde_json::Value) -> String {
    use std::hash::{Hash, Hasher};

    let canonical = serde_json::to_string(config).unwrap_or_default();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn strips_credential_schemes() {
        assert_eq!(strip_scheme("cred://openai-key"), "openai-key");
        assert_eq!(strip_scheme("cred:openai-key"), "openai-key");
        assert_eq!(strip_scheme("openai-key"), "openai-key");
    }

    #[test]
    fn hash_is_stable_and_secret_sensitive() {
        let first = serde_json::json!({"a": 1});
        let second = serde_json::json!({"a": 1});
        let third = serde_json::json!({"a": 2});
        assert_eq!(hash_config(&first), hash_config(&second));
        assert_ne!(hash_config(&first), hash_config(&third));
    }

    #[test]
    fn reject_unrepresentable_references() {
        assert!(credstore_sdk::SecretRef::new("cred://x").is_err());
        assert!(credstore_sdk::SecretRef::new("with space").is_err());
        assert!(credstore_sdk::SecretRef::new("ok_ref-1").is_ok());
    }
}

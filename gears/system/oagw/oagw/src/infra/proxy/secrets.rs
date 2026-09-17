//! Secret resolution for the data plane (PRD "Credential storage").
//!
//! Plugins never touch `credstore-sdk` directly: they see the narrow
//! [`SecretSource`] port below, so identical plugin code runs against the real
//! credstore in production and against an in-memory double in tests.
//!
//! Every resolved value is wrapped in [`SecretValue`], whose `Debug`/`Display`
//! impls print `[REDACTED]`, so credential material cannot reach a log line,
//! an error message, or an API response through accidental formatting.

use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::{CredStoreClientV1, SecretRef};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;

/// The credstore's redacting secret wrapper, re-exported so plugins never need
/// a direct `credstore-sdk` dependency to type their credential material.
pub use credstore_sdk::SecretValue;

/// A credential resolved by reference.
pub struct ResolvedSecret {
    reference: String,
    value: SecretValue,
}

impl Clone for ResolvedSecret {
    fn clone(&self) -> Self {
        // `SecretValue` is deliberately not `Clone` (it zeroises on drop), so a
        // copy re-wraps the bytes instead of sharing them.
        Self {
            reference: self.reference.clone(),
            value: SecretValue::new(self.value.as_bytes().to_vec()),
        }
    }
}

impl ResolvedSecret {
    /// Wrap raw bytes resolved from `reference`.
    #[must_use]
    pub fn new(reference: impl Into<String>, value: SecretValue) -> Self {
        Self {
            reference: reference.into(),
            value,
        }
    }

    /// Build a secret from a UTF-8 string.
    #[must_use]
    pub fn from_str(reference: impl Into<String>, value: impl Into<String>) -> Self {
        Self::new(reference, SecretValue::from(value.into()))
    }

    /// The (sanitised) reference the value was resolved from.
    #[must_use]
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// The raw secret bytes.
    #[must_use]
    pub fn value(&self) -> &SecretValue {
        &self.value
    }

    /// The secret as UTF-8, when it is text.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(self.value.as_bytes()).ok()
    }

    /// The secret as UTF-8 bytes; non-UTF-8 material is rejected by the
    /// caller-specific validators.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.value.as_bytes()
    }
}

impl std::fmt::Debug for ResolvedSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedSecret")
            .field("reference", &self.reference)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

/// Port over which plugins obtain credential material.
#[async_trait]
pub trait SecretSource: Send + Sync {
    /// Resolve `reference` on behalf of `tenant_id`.
    ///
    /// # Errors
    /// [`DomainError::SecretNotFound`] when the reference does not resolve,
    /// [`DomainError::Validation`] when the reference is malformed and
    /// [`DomainError::Internal`] when the backing store is unavailable.
    async fn resolve(
        &self,
        tenant_id: Uuid,
        security: &SecurityContext,
        reference: &str,
    ) -> Result<ResolvedSecret, DomainError>;
}

/// Normalise a user-supplied secret reference.
///
/// The `cred://` scheme prefix is accepted (and stripped) for operator
/// convenience; the credstore's own reference grammar is `[a-zA-Z0-9_-]+`.
///
/// # Errors
/// [`DomainError::Validation`] when nothing usable remains.
pub fn normalize_reference(reference: &str) -> Result<String, DomainError> {
    let trimmed = reference.trim();
    let stripped = trimmed
        .strip_prefix("cred://")
        .unwrap_or(trimmed)
        .trim_start_matches("//");
    if stripped.is_empty() {
        return Err(DomainError::validation(
            "secret_ref",
            "secret reference must not be empty",
        ));
    }
    if !stripped
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(DomainError::validation(
            "secret_ref",
            "secret reference may only contain [a-zA-Z0-9_-]",
        ));
    }
    Ok(stripped.to_owned())
}

/// The production [`SecretSource`]: the credstore gear.
pub struct CredStoreSecretSource {
    client: Arc<dyn CredStoreClientV1>,
}

impl CredStoreSecretSource {
    /// Wrap a credstore client handle.
    #[must_use]
    pub fn new(client: Arc<dyn CredStoreClientV1>) -> Self {
        Self { client }
    }
}

impl std::fmt::Debug for CredStoreSecretSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredStoreSecretSource")
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl SecretSource for CredStoreSecretSource {
    async fn resolve(
        &self,
        tenant_id: Uuid,
        security: &SecurityContext,
        reference: &str,
    ) -> Result<ResolvedSecret, DomainError> {
        let key = normalize_reference(reference)?;
        let secret_ref = SecretRef::new(key.clone()).map_err(|err| {
            DomainError::validation(
                "secret_ref",
                format!("secret reference `{key}` is not usable: {err}"),
            )
        })?;
        let response = self
            .client
            .get(security, &secret_ref)
            .await
            .map_err(|err| match err {
                credstore_sdk::CredStoreError::NotFound
                | credstore_sdk::CredStoreError::AccessDenied => DomainError::SecretNotFound {
                    detail: format!("secret `{key}` is not available to tenant {tenant_id}"),
                },
                other => DomainError::Internal {
                    diagnostic: format!("credstore lookup for `{key}` failed: {other}"),
                },
            })?;
        match response {
            Some(found) => Ok(ResolvedSecret::new(key, found.value)),
            None => Err(DomainError::SecretNotFound {
                detail: format!("secret `{key}` is not available to tenant {tenant_id}"),
            }),
        }
    }
}

/// In-memory [`SecretSource`] used by tests and by the `test-utils` feature.
#[derive(Debug, Default, Clone)]
pub struct InMemorySecretSource {
    secrets: Arc<parking_lot::RwLock<std::collections::BTreeMap<String, Vec<u8>>>>,
}

impl InMemorySecretSource {
    /// An empty source.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install `value` under `reference` (the `cred://` prefix is stripped).
    pub fn insert(&self, reference: &str, value: impl Into<Vec<u8>>) {
        let key = normalize_reference(reference).unwrap_or_else(|_| reference.to_owned());
        self.secrets.write().insert(key, value.into());
    }
}

#[async_trait]
impl SecretSource for InMemorySecretSource {
    async fn resolve(
        &self,
        _tenant_id: Uuid,
        _security: &SecurityContext,
        reference: &str,
    ) -> Result<ResolvedSecret, DomainError> {
        let key = normalize_reference(reference)?;
        match self.secrets.read().get(&key) {
            Some(value) => Ok(ResolvedSecret::new(key, SecretValue::new(value.clone()))),
            None => Err(DomainError::SecretNotFound {
                detail: format!("secret `{key}` is not available"),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_strip_the_scheme_prefix() {
        assert_eq!(
            normalize_reference("cred://openai-key").unwrap(),
            "openai-key"
        );
        assert_eq!(normalize_reference("  openai-key  ").unwrap(), "openai-key");
        assert!(normalize_reference("cred://").is_err());
        assert!(normalize_reference("a/b").is_err());
    }

    #[tokio::test]
    async fn in_memory_source_resolves_and_hides() {
        let source = InMemorySecretSource::new();
        source.insert("cred://openai-key", "sk-test");
        let ctx = SecurityContext::anonymous();
        let secret = source
            .resolve(Uuid::nil(), &ctx, "cred://openai-key")
            .await
            .unwrap();
        assert_eq!(secret.as_str(), Some("sk-test"));
        assert_eq!(
            format!("{secret:?}"),
            "ResolvedSecret { reference: \"openai-key\", value: \"[REDACTED]\" }"
        );
        assert!(source.resolve(Uuid::nil(), &ctx, "nope").await.is_err());
    }
}

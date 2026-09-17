//! CredStore-backed [`SecretResolver`].

use std::sync::Arc;

use credstore_sdk::{CredStoreClientV1, SecretRef};
use toolkit_security::SecurityContext;

use crate::domain::plugin::{PluginError, ResolvedSecret, SecretResolver};

/// Resolves `cred://` references (and bare keys) through the CredStore.
pub struct CredStoreSecretResolver {
    credstore: Arc<dyn CredStoreClientV1>,
}

impl std::fmt::Debug for CredStoreSecretResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CredStoreSecretResolver")
    }
}

/// Fallback [`SecretResolver`] used when no CredStore client is wired.
///
/// Every lookup fails closed with a static reason code; the reference value is
/// never echoed back.
pub struct UnresolvedSecretResolver;

#[async_trait::async_trait]
impl SecretResolver for UnresolvedSecretResolver {
    async fn resolve(
        &self,
        _ctx: &SecurityContext,
        _reference: &str,
    ) -> Result<Option<ResolvedSecret>, PluginError> {
        Err(PluginError::new(
            "SECRET_RESOLVE_FAILED",
            "no credential store is available",
        ))
    }
}

/// Strips the `cred://` scheme and validates the remaining key.
///
/// # Errors
///
/// Returns a [`PluginError`] when the reference is malformed. The message is
/// static and never echoes the reference value.
fn secret_reference(reference: &str) -> Result<SecretRef, PluginError> {
    let key = reference.strip_prefix("cred://").unwrap_or(reference);
    SecretRef::new(key)
        .map_err(|_| PluginError::new("INVALID_SECRET_REF", "secret reference is malformed"))
}

impl CredStoreSecretResolver {
    /// Wraps a CredStore client.
    #[must_use]
    pub fn new(credstore: Arc<dyn CredStoreClientV1>) -> Self {
        Self { credstore }
    }
}

#[async_trait::async_trait]
impl SecretResolver for CredStoreSecretResolver {
    async fn resolve(
        &self,
        ctx: &SecurityContext,
        reference: &str,
    ) -> Result<Option<ResolvedSecret>, PluginError> {
        let secret_ref = secret_reference(reference)?;
        match self.credstore.get(ctx, &secret_ref).await {
            Ok(Some(response)) => {
                let raw = response.value.as_bytes();
                let value = String::from_utf8_lossy(raw).to_string();
                Ok(Some(ResolvedSecret::new(value)))
            }
            Ok(None) => Ok(None),
            Err(_) => Err(PluginError::new(
                "SECRET_RESOLVE_FAILED",
                "credential store rejected the lookup",
            )),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn reference_parsing_strips_scheme_and_rejects_bad_values() {
        assert!(secret_reference("cred://partner-key").is_ok());
        assert!(secret_reference("partner-key").is_ok());
        let err = secret_reference("cred://a/b").expect_err("invalid");
        assert_eq!(err.code, "INVALID_SECRET_REF");
    }
}

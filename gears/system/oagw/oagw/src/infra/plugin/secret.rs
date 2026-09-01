//! Secret resolution port for the auth plugins (DESIGN "Secret Access
//! Control").
//!
//! Auth configuration never carries credential material: it carries a
//! `cred://` reference that has to be resolved at request time against the
//! credential store, which owns the tenant-scoped sharing policy. The plugins
//! depend on the [`SecretResolver`] port instead of on `credstore-sdk`
//! directly, so tests (and a deployment without a credstore dependency) can
//! substitute another implementation.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::{CredStoreClientV1, SecretRef};
use toolkit_security::SecurityContext;

use crate::domain::error::OagwError;

/// URI scheme of a credential store reference (DESIGN "Secret Access Control").
pub const SECRET_REF_SCHEME: &str = "cred://";

/// Resolves a `cred://` reference to its secret material.
///
/// The returned value is consumed immediately by the caller to build the
/// outbound credential and is never logged or persisted.
#[async_trait]
pub trait SecretResolver: Send + Sync {
    /// Resolves `secret_ref` in the scope of `ctx`.
    ///
    /// `Ok(None)` means the reference is well-formed but no secret is
    /// accessible to the caller; the caller decides which error to surface.
    ///
    /// # Errors
    ///
    /// Returns [`OagwError::SecretNotFound`] when the reference is malformed
    /// or the credential store itself fails.
    async fn resolve(
        &self,
        ctx: &SecurityContext,
        secret_ref: &str,
    ) -> Result<Option<String>, OagwError>;
}

/// Strips the `cred://` scheme from a reference, returning the bare key.
///
/// Surrounding whitespace is trimmed as well: the credential store validates
/// keys against `[a-zA-Z0-9_-]`, so the URI decoration must not be handed to
/// it.
#[must_use]
pub fn strip_secret_scheme(secret_ref: &str) -> &str {
    let trimmed = secret_ref.trim();
    trimmed
        .strip_prefix(SECRET_REF_SCHEME)
        .or_else(|| trimmed.strip_prefix("cred:"))
        .unwrap_or(trimmed)
        .trim()
}

/// Builds a credential store reference from a bare key.
///
/// # Errors
///
/// Returns [`OagwError::secret_not_found`] when the key is not a valid
/// [`SecretRef`] (empty, too long, or containing characters outside the
/// credential store alphabet).
pub fn secret_ref(secret_ref: &str) -> Result<SecretRef, OagwError> {
    SecretRef::new(strip_secret_scheme(secret_ref)).map_err(|error| {
        OagwError::secret_not_found(format!("invalid secret reference '{secret_ref}': {error}"))
    })
}

/// Resolver backed by the `credstore` dependency.
pub struct CredStoreSecretResolver {
    client: Arc<dyn CredStoreClientV1>,
}

impl CredStoreSecretResolver {
    /// Wraps a credential store client.
    #[must_use]
    pub fn new(client: Arc<dyn CredStoreClientV1>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl SecretResolver for CredStoreSecretResolver {
    async fn resolve(
        &self,
        ctx: &SecurityContext,
        secret_ref: &str,
    ) -> Result<Option<String>, OagwError> {
        let reference = self::secret_ref(secret_ref)?;
        match self.client.get(ctx, &reference).await {
            Ok(Some(response)) => {
                let value = String::from_utf8_lossy(response.value.as_bytes()).into_owned();
                Ok(Some(value))
            }
            Ok(None) => Ok(None),
            Err(error) => Err(OagwError::secret_not_found(format!(
                "credential store lookup for '{secret_ref}' failed: {error}"
            ))),
        }
    }
}

/// Fail-closed resolver installed when the `credstore` dependency is absent.
///
/// A gateway that cannot resolve credentials must not silently fall back to an
/// unauthenticated upstream call, so every resolution fails with
/// [`OagwError::SecretNotFound`] (500) at use time.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableSecretResolver;

#[async_trait]
impl SecretResolver for UnavailableSecretResolver {
    async fn resolve(
        &self,
        _ctx: &SecurityContext,
        secret_ref: &str,
    ) -> Result<Option<String>, OagwError> {
        Err(OagwError::secret_not_found(format!(
            "no credential store is available to resolve '{secret_ref}'"
        )))
    }
}

/// In-memory resolver for tests and for operators who pin a credential
/// statically.
#[derive(Debug, Default, Clone)]
pub struct StaticSecretResolver {
    secrets: HashMap<String, String>,
}

impl StaticSecretResolver {
    /// Creates a resolver serving `secrets`, keyed by the *bare* reference key
    /// (without the `cred://` scheme).
    #[must_use]
    pub fn new(secrets: HashMap<String, String>) -> Self {
        Self { secrets }
    }

    /// Adds one secret.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<String>) {
        self.secrets.insert(key.into(), value.into());
    }
}

#[async_trait]
impl SecretResolver for StaticSecretResolver {
    async fn resolve(
        &self,
        _ctx: &SecurityContext,
        secret_ref: &str,
    ) -> Result<Option<String>, OagwError> {
        Ok(self.secrets.get(strip_secret_scheme(secret_ref)).cloned())
    }
}

#[cfg(test)]
#[path = "secret_tests.rs"]
mod tests;

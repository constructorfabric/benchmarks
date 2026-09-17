//! `cred://` secret resolution (DESIGN.md §3.1 "Secret Access Control").
//!
//! OAGW never stores secret material: every auth plugin configuration
//! references secrets by URI and resolution is delegated to the `cred_store`
//! gear, which applies tenant visibility rules itself.

use std::sync::Arc;

use credstore_sdk::CredStoreClientV1;
use credstore_sdk::models::{GetSecretResponse, SecretRef};
use toolkit_security::SecurityContext;

use crate::domain::error::DomainError;

/// URI scheme every secret reference uses on the wire.
pub const SECRET_SCHEME: &str = "cred://";

/// Resolver for `cred://` references backed by the `cred_store` gear.
#[derive(Clone)]
pub struct SecretResolver {
    credstore: Arc<dyn CredStoreClientV1>,
}

impl SecretResolver {
    /// Bind the resolver to a credstore client.
    #[must_use]
    pub fn new(credstore: Arc<dyn CredStoreClientV1>) -> Self {
        Self { credstore }
    }

    /// Resolve a `cred://` reference (or bare key) for `context`.
    ///
    /// # Errors
    ///
    /// Returns `cf.oagw.secret.not_found.v1` when the reference is malformed
    /// or the store has no accessible secret under it, and surfaces credstore
    /// access failures as `cf.oagw.secret.not_found.v1` too — the contract
    /// has a single 404 surface for secret resolution.
    pub async fn resolve(
        &self,
        context: &SecurityContext,
        reference: &str,
    ) -> Result<String, DomainError> {
        let key = parse_secret_ref(reference)?;
        match self.credstore.get(context, &key).await {
            Ok(Some(response)) => Ok(secret_string(&response)),
            Ok(None) => Err(secret_missing(reference)),
            Err(error) => {
                tracing::debug!(reference = %reference, error = %error, "secret resolution failed");
                Err(secret_missing(reference))
            }
        }
    }
}

/// Turn a wire reference into a credstore key.
fn parse_secret_ref(reference: &str) -> Result<SecretRef, DomainError> {
    let bare = reference
        .strip_prefix(SECRET_SCHEME)
        .unwrap_or(reference)
        .trim();
    SecretRef::new(bare).map_err(|error| {
        DomainError::secret_not_found(format!("invalid secret reference: {error}"))
    })
}

fn secret_string(response: &GetSecretResponse) -> String {
    String::from_utf8_lossy(response.value.as_bytes()).into_owned()
}

fn secret_missing(reference: &str) -> DomainError {
    DomainError::secret_not_found(format!(
        "secret `{reference}` is not accessible to this tenant"
    ))
}

#[cfg(test)]
#[path = "credentials_tests.rs"]
mod credentials_tests;

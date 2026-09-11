//! `cred://` resolution over the credstore client.
//!
//! The resolver is the only place credential material is handled, and it is
//! wrapped so that nothing it produces can reach a log record, an error detail
//! or a response body: [`CredCredential`] has a redacting `Debug`, and every
//! error path carries only the reference's *key*, never its value.

use crate::domain::error::OagwError;
use crate::domain::plugin::{Credential, CredentialResolver};
use async_trait::async_trait;
use credstore_sdk::{CredStoreClientV1, SecretRef};
use std::sync::Arc;
use toolkit_security::SecurityContext;
use uuid::Uuid;

/// Credential resolver backed by the credstore client.
pub struct CredStoreResolver {
    client: Option<Arc<dyn CredStoreClientV1>>,
}

impl CredStoreResolver {
    /// Builds a resolver over an optional client; without one every lookup
    /// fails closed with [`OagwError::AuthenticationFailed`].
    #[must_use]
    pub fn new(client: Option<Arc<dyn CredStoreClientV1>>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl CredentialResolver for CredStoreResolver {
    async fn resolve(&self, tenant_id: Uuid, reference: &str) -> Result<Credential, OagwError> {
        let key = secret_key(reference).ok_or_else(|| {
            OagwError::ValidationError(format!(
                "credential reference '{reference}' is not a valid cred:// reference"
            ))
        })?;
        let Some(client) = self.client.as_ref() else {
            return Err(OagwError::AuthenticationFailed(format!(
                "credential '{}' is unavailable: no credential store is configured",
                describe(reference)
            )));
        };
        let reference = SecretRef::new(key.clone()).map_err(|_| {
            OagwError::ValidationError(format!("credential reference '{reference}' is invalid"))
        })?;
        let context = SecurityContext::builder()
            .subject_id(Uuid::nil())
            .subject_tenant_id(tenant_id)
            .build()
            .map_err(|_| {
                OagwError::AuthenticationFailed("credential context could not be built".to_owned())
            })?;
        match client.get(&context, &reference).await {
            Ok(Some(response)) => Ok(Credential::new(response.value.as_bytes().to_vec())),
            Ok(None) => Err(OagwError::SecretNotFound(format!(
                "credential '{key}' does not exist"
            ))),
            Err(credstore_sdk::CredStoreError::AccessDenied) => Err(
                OagwError::AuthenticationFailed(format!("credential '{key}' is not accessible")),
            ),
            Err(_) => Err(OagwError::AuthenticationFailed(format!(
                "credential '{key}' could not be read"
            ))),
        }
    }
}

/// Extracts the secret key from a `cred://` reference.
///
/// A reference may name a tenant segment before the key
/// (`cred://tenant/openai-key`); the credstore performs its own hierarchical
/// resolution, so only the final segment is used.
#[must_use]
pub fn secret_key(reference: &str) -> Option<String> {
    let path = reference.strip_prefix("cred://")?;
    let key = path.split('/').next_back().unwrap_or(path);
    if key.is_empty() {
        None
    } else {
        Some(key.to_owned())
    }
}

/// A reference description safe for logs: the key only, never a value.
#[must_use]
fn describe(reference: &str) -> String {
    secret_key(reference).unwrap_or_else(|| "unparsable".to_owned())
}

#[cfg(test)]
#[path = "credstore_tests.rs"]
mod tests;

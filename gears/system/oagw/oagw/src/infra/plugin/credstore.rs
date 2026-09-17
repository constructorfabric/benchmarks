//! `credstore`-backed [`SecretResolver`].
use std::sync::Arc;

use async_trait::async_trait;
use credstore_sdk::{CredStoreClientV1, SecretRef};
use toolkit_security::SecurityContext;

use crate::domain::plugin::{ResolvedSecret, SecretError, SecretResolver, strip_cred_scheme};

/// Resolves `cred://<name>` (or bare `<name>`) references through the
/// `credstore` gear's client.
///
/// The resolver reads with an *anonymous* security context: the credential
/// store's hierarchical resolution is what decides visibility, and OAGW has
/// no principal of its own to authenticate with — the caller's tenant is
/// already the one the upstream's configuration belongs to.
pub struct CredStoreSecretResolver {
    client: Arc<dyn CredStoreClientV1>,
}

impl CredStoreSecretResolver {
    /// Wrap a `credstore` client.
    #[must_use]
    pub fn new(client: Arc<dyn CredStoreClientV1>) -> Self {
        Self { client }
    }
}

#[async_trait]
impl SecretResolver for CredStoreSecretResolver {
    async fn resolve(&self, reference: &str) -> Result<Option<ResolvedSecret>, SecretError> {
        let name = strip_cred_scheme(reference);
        if name.is_empty() {
            return Ok(None);
        }
        let Ok(secret_ref) = SecretRef::new(name) else {
            return Err(SecretError::Invalid {
                reference: reference.to_owned(),
                reason: "must be [a-zA-Z0-9_-] after the cred:// scheme".to_owned(),
            });
        };
        let context = SecurityContext::anonymous();
        match self.client.get(&context, &secret_ref).await {
            Ok(Some(response)) => {
                let bytes = response.value.as_bytes();
                let Ok(value) = std::str::from_utf8(bytes) else {
                    return Err(SecretError::Invalid {
                        reference: reference.to_owned(),
                        reason: "secret is not valid UTF-8".to_owned(),
                    });
                };
                Ok(Some(ResolvedSecret::new(value)))
            }
            Ok(None) => Err(SecretError::NotFound {
                reference: reference.to_owned(),
            }),
            Err(error) => Err(SecretError::Unavailable(error.to_string())),
        }
    }
}

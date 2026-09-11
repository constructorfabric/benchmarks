//! Credential-store access.
//!
//! Everything in this module enforces the PRD §6.1 isolation rule: a resolved
//! secret is only ever held in a [`credstore_sdk::SecretValue`] (zeroizing,
//! redacted `Debug`) and is injected straight into the outbound header map.
//! It is never logged, never serialized into an error, and never returned to
//! the caller.

use std::sync::Arc;

use credstore_sdk::{CredStoreClientV1, SecretRef, SecretValue};
use toolkit_security::SecurityContext;

use crate::error::{DomainError, ErrorKind};

/// A shared handle to the credential store client.
pub type SharedCredentialStore = Arc<dyn CredStoreClientV1>;

/// Strip a credential-URI scheme so `cred://openai-key` and `openai-key` both
/// resolve to the same stored reference.
#[must_use]
pub fn normalize_secret_ref(secret_ref: &str) -> &str {
    for prefix in ["cred://", "secret://", "credstore://"] {
        if let Some(rest) = secret_ref.strip_prefix(prefix) {
            return rest;
        }
    }
    secret_ref
}

/// Resolve a `secret_ref` to its secret value.
///
/// # Errors
///
/// [`ErrorKind::SecretNotFound`] when the store reports the reference as
/// absent or inaccessible. The error message names the *reference*, never the
/// value.
pub async fn resolve(
    credential_store: &SharedCredentialStore,
    security: &SecurityContext,
    secret_ref: &str,
) -> Result<SecretValue, DomainError> {
    let key = normalize_secret_ref(secret_ref);
    let reference = SecretRef::new(key).map_err(|err| {
        DomainError::new(
            ErrorKind::Validation,
            format!("secret_ref {secret_ref:?} is not a valid credential reference: {err}"),
        )
    })?;
    match credential_store.get(security, &reference).await {
        Ok(Some(response)) => Ok(response.value),
        Ok(None) => Err(DomainError::new(
            ErrorKind::SecretNotFound,
            format!("credential {secret_ref:?} could not be resolved from the credential store"),
        )),
        Err(err) => Err(DomainError::new(
            ErrorKind::SecretNotFound,
            format!("credential {secret_ref:?} could not be read from the credential store: {err}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_credential_uri_prefixes() {
        assert_eq!(normalize_secret_ref("cred://openai-key"), "openai-key");
        assert_eq!(normalize_secret_ref("secret://openai-key"), "openai-key");
        assert_eq!(normalize_secret_ref("credstore://openai-key"), "openai-key");
        assert_eq!(normalize_secret_ref("openai-key"), "openai-key");
    }

    #[test]
    fn invalid_references_are_rejected_before_the_store_is_touched() {
        assert!(SecretRef::new("bad ref with spaces").is_err());
    }
}

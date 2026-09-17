//! Credential resolution through the credential store.

use std::sync::Arc;

use credstore_sdk::{CredStoreClientV1, GetSecretResponse, SecretRef};
use toolkit_security::SecurityContext;

use crate::domain::error::ErrorKind;

/// Credential store that resolves nothing.
///
/// Used when the deployment does not ship the credential store gear: the auth
/// plugins that dereference a `cred://` pointer then fail closed with
/// [`ErrorKind::SecretNotFound`] instead of the gear refusing to start.
pub struct NullCredStore;

#[async_trait::async_trait]
impl CredStoreClientV1 for NullCredStore {
    async fn get(
        &self,
        ctx: &SecurityContext,
        key: &SecretRef,
    ) -> Result<Option<GetSecretResponse>, credstore_sdk::CredStoreError> {
        let _ = (ctx, key);
        Ok(None)
    }
}

/// Extract the credential-store key from a reference.
///
/// Accepts a bare key (`partner-openai-key`) or a `cred://` URI
/// (`cred://partner-openai-key`); any path component after the scheme is
/// treated as the key.
#[must_use]
pub fn credential_key(reference: &str) -> Option<String> {
    let key = reference
        .trim()
        .strip_prefix("cred://")
        .unwrap_or_else(|| reference.trim());
    let key = key.split('/').next_back().unwrap_or(key).trim();
    if key.is_empty() {
        None
    } else {
        Some(key.to_owned())
    }
}

/// Resolve a credential reference to its raw bytes.
///
/// The reference is a pointer, not the secret itself; the value is never
/// logged and never rendered into an error message. A failure is logged with
/// the reference NAME and a cause class only.
///
/// # Errors
///
/// Returns [`ErrorKind::SecretNotFound`] when the reference cannot be resolved
/// or the credential store reports an infrastructure fault, and
/// [`ErrorKind::AuthenticationFailed`] when the store refuses the lookup: a
/// denial is an authorization failure the caller may act on, while a missing or
/// unresolved credential is this deployment's configuration gap (`500`).
pub async fn resolve_secret(
    credstore: &Arc<dyn CredStoreClientV1>,
    ctx: &SecurityContext,
    reference: &str,
) -> Result<Vec<u8>, crate::domain::error::OagwError> {
    let Some(key) = credential_key(reference) else {
        return Err(crate::domain::error::OagwError::new(
            ErrorKind::Validation,
            "credential reference is malformed",
        ));
    };
    let parsed = credstore_sdk::SecretRef::new(key.clone()).map_err(|_| {
        crate::domain::error::OagwError::new(
            ErrorKind::Validation,
            "credential reference is malformed",
        )
    })?;
    match credstore.get(ctx, &parsed).await {
        Ok(Some(response)) => Ok(response.value.as_bytes().to_vec()),
        Ok(None) => Err(credential_failure(
            &key,
            None,
            "referenced credential could not be resolved",
            ErrorKind::SecretNotFound,
        )),
        Err(error) => {
            let denied = matches!(error, credstore_sdk::CredStoreError::AccessDenied);
            let kind = if denied {
                ErrorKind::AuthenticationFailed
            } else {
                ErrorKind::SecretNotFound
            };
            Err(credential_failure(
                &key,
                Some(&error),
                "referenced credential could not be resolved",
                kind,
            ))
        }
    }
}

/// Log a credential resolution failure and build the problem the caller sees.
///
/// The log carries the reference NAME and a short cause class, never a secret
/// value and never the store's own message verbatim: a backend error string may
/// quote material it was handed.
fn credential_failure(
    key: &str,
    cause: Option<&credstore_sdk::CredStoreError>,
    detail: &'static str,
    kind: ErrorKind,
) -> crate::domain::error::OagwError {
    let classification = cause.map_or_else(|| "not_found".to_owned(), cause_class);
    tracing::warn!(
        credential_reference = %key,
        cause = %classification,
        "credential resolution failed"
    );
    crate::domain::error::OagwError::new(kind, detail)
}

/// Short, value-free classification of a credential-store failure.
fn cause_class(error: &credstore_sdk::CredStoreError) -> String {
    use credstore_sdk::CredStoreError;
    match error {
        CredStoreError::AccessDenied => "access_denied".to_owned(),
        CredStoreError::NotFound => "not_found".to_owned(),
        CredStoreError::InvalidSecretRef { .. } => "invalid_reference".to_owned(),
        CredStoreError::ServiceUnavailable { .. } | CredStoreError::NoPluginAvailable => {
            "unavailable".to_owned()
        }
        CredStoreError::Internal(_) => "internal".to_owned(),
        CredStoreError::Conflict
        | CredStoreError::UnsupportedTransition { .. }
        | CredStoreError::TypeViolation { .. } => "rejected".to_owned(),
    }
}

/// Resolve a credential reference to a UTF-8 string.
///
/// # Errors
///
/// Returns [`ErrorKind::SecretNotFound`] when the secret is missing or not
/// valid UTF-8.
pub async fn resolve_secret_string(
    credstore: &Arc<dyn CredStoreClientV1>,
    ctx: &SecurityContext,
    reference: &str,
) -> Result<String, crate::domain::error::OagwError> {
    let bytes = resolve_secret(credstore, ctx, reference).await?;
    String::from_utf8(bytes).map_err(|_| {
        crate::domain::error::OagwError::new(
            ErrorKind::SecretNotFound,
            "referenced credential is not valid UTF-8",
        )
    })
}

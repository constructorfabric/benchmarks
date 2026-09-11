//! Resolution of `cred://` references through the credential store.
//!
//! OAGW never stores secret material; every credential is a reference resolved
//! at request time (`cpt-cf-oagw-principle-cred-isolation`). Failures never
//! carry the resolved value, only the reference.

use std::sync::Arc;

use credstore_sdk::{CredStoreClientV1, SecretRef};
use toolkit_auth::oauth2::SecretString;
use toolkit_security::SecurityContext;

use crate::domain::plugin::PluginError;

/// Strip the `cred://` scheme from a secret reference.
#[must_use]
pub fn strip_scheme(reference: &str) -> &str {
    let trimmed = reference.trim();
    trimmed
        .strip_prefix("cred://")
        .or_else(|| trimmed.strip_prefix("credstore://"))
        .unwrap_or(trimmed)
}

/// Resolve `reference` for the caller's tenant.
///
/// # Errors
///
/// * [`PluginError::InvalidConfig`] — the reference is not a well-formed key.
/// * [`PluginError::SecretNotFound`] — nothing accessible resolves.
/// * [`PluginError::Unauthenticated`] — the credential store refused access.
pub async fn resolve_secret(
    credstore: &Arc<dyn CredStoreClientV1>,
    ctx: &SecurityContext,
    reference: &str,
) -> Result<SecretString, PluginError> {
    let key = SecretRef::new(strip_scheme(reference))
        .map_err(|err| PluginError::InvalidConfig(format!("invalid secret_ref: {err}")))?;

    match credstore.get(ctx, &key).await {
        Ok(Some(response)) => {
            let value = String::from_utf8(response.value.as_bytes().to_vec()).map_err(|_| {
                PluginError::InvalidConfig(format!(
                    "secret '{reference}' is not valid UTF-8 and cannot be injected as a header"
                ))
            })?;
            Ok(SecretString::new(value))
        }
        Ok(None) => Err(PluginError::SecretNotFound(reference.to_owned())),
        Err(err) => Err(PluginError::Unauthenticated(format!(
            "credential store refused '{reference}': {err}"
        ))),
    }
}

#[cfg(test)]
#[path = "credref_tests.rs"]
mod tests;

//! `cred://` reference resolution.
//!
//! OAGW never stores secret material; auth plugins resolve references through
//! the `credstore` gear at request time (`cpt-cf-oagw-principle-cred-isolation`).

use std::sync::Arc;

use credstore_sdk::{CredStoreClientV1, SecretRef};
use toolkit_security::SecurityContext;

use crate::domain::plugin::{PluginError, PluginResult};

/// Strip the `cred://` scheme from a secret reference.
#[must_use]
pub fn strip_scheme(reference: &str) -> &str {
    reference
        .trim()
        .strip_prefix("cred://")
        .unwrap_or_else(|| reference.trim())
}

/// Resolve a `cred://` reference to its value.
///
/// # Errors
/// * [`PluginError::SecretNotFound`] when the reference does not resolve.
/// * [`PluginError::Unauthenticated`] when the tenant may not read it.
/// * [`PluginError::Config`] when the reference is malformed.
pub async fn resolve_secret(
    credstore: &Arc<dyn CredStoreClientV1>,
    ctx: &SecurityContext,
    reference: &str,
) -> PluginResult<String> {
    let name = strip_scheme(reference);
    let key = SecretRef::new(name).map_err(|e| {
        PluginError::Config(format!("invalid secret reference '{reference}': {e}"))
    })?;
    match credstore.get(ctx, &key).await {
        Ok(Some(found)) => String::from_utf8(found.value.as_bytes().to_vec())
            .map_err(|_| PluginError::Config("secret value is not valid UTF-8".to_owned())),
        Ok(None) => Err(PluginError::SecretNotFound(format!(
            "secret '{name}' was not found or is not accessible"
        ))),
        Err(credstore_sdk::CredStoreError::AccessDenied) => Err(PluginError::Unauthenticated(
            format!("access to secret '{name}' was denied"),
        )),
        Err(credstore_sdk::CredStoreError::NotFound) => Err(PluginError::SecretNotFound(format!(
            "secret '{name}' was not found"
        ))),
        Err(other) => Err(PluginError::Internal(format!(
            "credential store failure while resolving '{name}': {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::strip_scheme;

    #[test]
    fn the_cred_scheme_is_optional() {
        assert_eq!(strip_scheme("cred://openai-key"), "openai-key");
        assert_eq!(strip_scheme("openai-key"), "openai-key");
        assert_eq!(strip_scheme("  cred://openai-key "), "openai-key");
    }
}

//! Credential-reference resolution: the `cred://` routine.
//!
//! Realizes `cpt-cf-oagw-algo-credential-resolution` and the
//! `cpt-cf-oagw-principle-cred-isolation` obligations it carries: this module
//! is the only thing in the gear that turns a credential reference into
//! material, it validates the reference for shape only, and it resolves the
//! material through nothing but the credential store, at request time, never
//! at management time.
//!
//! No failure value this module returns carries the reference it failed on or
//! the material it failed to resolve: [`PluginFailure`] names a shape, a
//! missing secret, a decline, an unreachable store, or an unusable
//! configuration, and names nothing else.
//!
//! Realizes `cpt-cf-oagw-dod-credential-isolation`.

// @cpt-dod:cpt-cf-oagw-dod-credential-isolation:p1

use std::sync::Arc;

use credstore_sdk::error::CredStoreError;
use credstore_sdk::models::SecretRef;
use credstore_sdk::CredStoreClientV1;
use toolkit_auth::SecretString;
use toolkit_security::SecurityContext;

use crate::domain::plugin_contract::PluginFailure;

/// The only credential scheme the gear accepts.
pub const CREDENTIAL_SCHEME: &str = "cred://";

/// Whether one string is a credential reference the store could be asked to
/// resolve: the `cred://` scheme, a non-empty remainder, no surrounding
/// whitespace, no fragment, and a remainder the credential store accepts as
/// its own key spelling.
///
/// A reference that fails this check fails before any credential-store call,
/// and the answer carries no reason that names the reference.
#[must_use]
pub fn is_credential_reference(reference: &str) -> bool {
    if reference != reference.trim() {
        return false;
    }
    let Some(remainder) = reference.strip_prefix(CREDENTIAL_SCHEME) else {
        return false;
    };
    if remainder.is_empty() || remainder.contains('#') {
        return false;
    }
    SecretRef::new(remainder).is_ok()
}

/// Validates one reference for shape and returns the key the store resolves.
///
/// # Errors
///
/// Returns [`PluginFailure::CredentialShape`] for a reference that
/// [`is_credential_reference`] declines, before any store call is made.
pub fn credential_key(reference: &str) -> Result<SecretRef, PluginFailure> {
    // @cpt-begin:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-shape
    // The shape check is the whole of what the gear asks of a reference
    // before the store: anything else is the store's own sharing policy.
    let remainder = reference
        .strip_prefix(CREDENTIAL_SCHEME)
        .ok_or(PluginFailure::CredentialShape)?;
    let key = SecretRef::new(remainder).map_err(|_| PluginFailure::CredentialShape)?;
    Ok(key)
    // @cpt-end:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-shape
}

/// Resolves one credential reference into its material.
///
/// The store applies its own sharing policy, including the ancestor sharing
/// the hierarchical resolution performs, so this routine asks it nothing about
/// who may read what: it hands over the calling tenant and subject and maps
/// the answer onto the typed failures the caller maps onto the catalogue.
///
/// # Errors
///
/// Returns [`PluginFailure::CredentialShape`] before any store call when the
/// reference is malformed, [`PluginFailure::SecretNotFound`] when the store
/// answers no material, [`PluginFailure::AuthenticationFailed`] when the store
/// declines the reference for the calling tenant or subject,
/// [`PluginFailure::Unavailable`] when the store is unreachable or fails, and
/// [`PluginFailure::Configuration`] when the material it returned cannot be
/// carried as text.
pub async fn resolve_credential(
    store: Arc<dyn CredStoreClientV1>,
    security_context: &SecurityContext,
    reference: &str,
) -> Result<SecretString, PluginFailure> {
    let key = credential_key(reference)?;

    // @cpt-begin:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-try
    // @cpt-begin:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-call
    let answer = store.get(security_context, &key).await;
    // @cpt-end:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-call
    // @cpt-begin:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-catch
    // @cpt-begin:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-catch-handle
    let response = answer.map_err(store_failure)?;
    // @cpt-end:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-catch-handle
    // @cpt-end:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-catch
    // @cpt-begin:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-missing-if
    // @cpt-begin:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-missing-return
    let value = response.ok_or(PluginFailure::SecretNotFound)?;
    // @cpt-end:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-missing-return
    // @cpt-end:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-missing-if
    // @cpt-begin:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-ok-else
    let material = String::from_utf8(value.value.as_bytes().to_vec())
        .map_err(|_| PluginFailure::Configuration {
            reason: String::from("credential material is not valid UTF-8"),
        })?;
    // @cpt-begin:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-ok
    // @cpt-begin:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-return
    Ok(SecretString::new(material))
    // @cpt-end:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-return
    // @cpt-end:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-ok
    // @cpt-end:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-ok-else
    // @cpt-end:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-try
}

/// Maps a store failure onto the typed failures the catalogue names.
///
/// The mapping is one-directional and carries no detail from the store: a
/// decline is an authentication failure, an absent secret is a missing secret,
/// and everything else is a store the gear could not reach.
fn store_failure(error: CredStoreError) -> PluginFailure {
    match error {
        // @cpt-begin:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-declined-if
        // @cpt-begin:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-declined-return
        CredStoreError::AccessDenied => PluginFailure::AuthenticationFailed,
        // @cpt-end:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-declined-return
        // @cpt-end:cpt-cf-oagw-algo-credential-resolution:p1:inst-cred-declined-if
        CredStoreError::NotFound => PluginFailure::SecretNotFound,
        CredStoreError::InvalidSecretRef { .. } => PluginFailure::CredentialShape,
        _ => PluginFailure::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_with_an_inner_space_is_malformed() {
        assert!(!is_credential_reference("cred://api key"));
        assert!(!is_credential_reference("cred://api-key\n"));
    }

    #[test]
    fn a_shape_failure_names_nothing() {
        assert!(matches!(
            credential_key("https://api-key"),
            Err(PluginFailure::CredentialShape)
        ));
    }
}

//! Shared plugin-binding helpers: credential-reference parsing, bounded
//! credential-store resolution, and the deterministic configuration hash
//! the `OAuth2` token-cache key uses
//! (`cpt-cf-oagw-dod-credential-isolation`, `cpt-cf-oagw-algo-token-cache-lookup`).

use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use credstore_sdk::{CredStoreClientV1, CredStoreError, SecretRef};
use toolkit_auth::oauth2::SecretString;
use toolkit_security::SecurityContext;

use crate::error::OagwError;

/// The scheme prefix a credential reference must carry: an inline secret
/// value (missing this prefix) is always rejected
/// (`cpt-cf-oagw-dod-credential-isolation`).
pub const CRED_REF_PREFIX: &str = "cred://";

/// Parses a `cred://`-prefixed credential reference out of a plugin
/// binding's configuration value. Returns `None` — treated as a
/// structurally invalid binding by every caller — when the value is
/// missing, not a string, lacks the `cred://` prefix (an inline secret
/// value), or fails [`SecretRef`]'s character-set validation
/// (`cpt-cf-oagw-algo-apikey-injection`, `cpt-cf-oagw-algo-oauth2-token-acquisition`).
#[must_use]
pub fn parse_cred_ref(value: Option<&serde_json::Value>) -> Option<SecretRef> {
    let raw = value?.as_str()?;
    let stripped = raw.strip_prefix(CRED_REF_PREFIX)?;
    SecretRef::new(stripped).ok()
}

/// Reads a string-valued key from a plugin binding's configuration object.
#[must_use]
pub fn config_str<'a>(config: Option<&'a serde_json::Value>, key: &str) -> Option<&'a str> {
    config?.get(key)?.as_str()
}

/// A deterministic hash of `config`'s key/value pairs, sorted by key
/// (`cpt-cf-oagw-algo-token-cache-lookup`). Absent or non-object
/// configuration hashes to a fixed sentinel.
#[must_use]
pub fn config_hash(config: Option<&serde_json::Value>) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    if let Some(obj) = config.and_then(serde_json::Value::as_object) {
        let sorted: BTreeMap<&String, &serde_json::Value> = obj.iter().collect();
        for (key, value) in sorted {
            key.hash(&mut hasher);
            value.to_string().hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Resolves `secret_ref` through the credential store for `security_context`,
/// bounded by `proxy_timeout`
/// (`cpt-cf-oagw-dod-credential-isolation`).
///
/// # Errors
///
/// Returns [`OagwError::authentication_failed`] (`401`) when the lookup
/// exceeds `proxy_timeout`, access is denied, or any other store failure
/// occurs; [`OagwError::secret_not_found`] (`500`) when the reference does
/// not exist, including a client whose `get` reports the not-found surface
/// as an error rather than `Ok(None)`.
pub async fn resolve_secret(
    credstore: &dyn CredStoreClientV1,
    security_context: &SecurityContext,
    secret_ref: &SecretRef,
    proxy_timeout: Duration,
) -> Result<SecretString, OagwError> {
    let outcome =
        tokio::time::timeout(proxy_timeout, credstore.get(security_context, secret_ref)).await;
    match outcome {
        Err(_) => Err(OagwError::authentication_failed(
            "credential-store lookup exceeded the configured proxy timeout",
        )),
        Ok(Err(CredStoreError::AccessDenied)) => Err(OagwError::authentication_failed(
            "credential store denied access to the referenced secret",
        )),
        Ok(Err(CredStoreError::NotFound) | Ok(None)) => Err(OagwError::secret_not_found(
            "referenced secret does not exist",
        )),
        Ok(Err(other)) => Err(OagwError::authentication_failed(format!(
            "credential store lookup failed: {other}"
        ))),
        Ok(Ok(Some(response))) => String::from_utf8(response.value.as_bytes().to_vec())
            .map(SecretString::new)
            .map_err(|_| OagwError::authentication_failed("resolved secret is not valid UTF-8")),
    }
}

#[cfg(test)]
mod tests {
    use super::{config_hash, config_str, parse_cred_ref};
    use serde_json::json;

    #[test]
    fn parses_a_well_formed_cred_reference() {
        let value = json!("cred://my-secret_1");
        let parsed = parse_cred_ref(Some(&value)).expect("must parse");
        assert_eq!(parsed.as_ref(), "my-secret_1");
    }

    // @cpt-begin:cpt-cf-oagw-dod-credential-isolation:p2:inst-cred-ref-inline-test-01
    #[test]
    fn an_inline_value_without_the_cred_scheme_is_rejected() {
        let value = json!("sk-not-a-reference");
        assert!(parse_cred_ref(Some(&value)).is_none());
    }
    // @cpt-end:cpt-cf-oagw-dod-credential-isolation:p2:inst-cred-ref-inline-test-01

    #[test]
    fn a_missing_reference_is_rejected() {
        assert!(parse_cred_ref(None).is_none());
    }

    #[test]
    fn config_str_reads_a_string_field() {
        let value = json!({"name": "X-Api-Key"});
        assert_eq!(config_str(Some(&value), "name"), Some("X-Api-Key"));
        assert_eq!(config_str(Some(&value), "missing"), None);
    }

    #[test]
    fn config_hash_is_deterministic_and_order_independent() {
        let a = json!({"b": 1, "a": 2});
        let b = json!({"a": 2, "b": 1});
        assert_eq!(config_hash(Some(&a)), config_hash(Some(&b)));
    }

    #[test]
    fn config_hash_differs_for_differing_configs() {
        let a = json!({"scope": "read"});
        let b = json!({"scope": "write"});
        assert_ne!(config_hash(Some(&a)), config_hash(Some(&b)));
    }

    #[test]
    fn absent_config_hashes_to_a_fixed_sentinel() {
        assert_eq!(config_hash(None), config_hash(None));
    }
}

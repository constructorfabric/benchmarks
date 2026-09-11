//! Token-cache value objects and the shared `cred://` resolution
//! (`cpt-cf-oagw-dod-credential-isolation`).
//!
//! [`TokenCacheConfig`] is the parameter object that threads the two
//! gear-configuration keys of `cpt-cf-oagw-feature-gear-wiring` into the OAuth2
//! plugin constructors, [`CachedToken`] is the cache-entry wrapper whose key is
//! verified on every hit, and [`resolve_secret`] is the one place the plugin
//! chain turns a `cred://` reference into a [`SecretString`] — the single §1.5
//! failure mapping every auth-plugin failure is born from.
//!
//! No secret material ever leaves a [`SecretString`]: configuration carries only
//! `cred://` references, and no failure message of this module echoes a
//! resolved value.
// @cpt-begin:cpt-cf-oagw-dod-credential-isolation:p1:inst-full

use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::Duration;

use credstore_sdk::{CredStoreClientV1, CredStoreError, SecretRef};
use serde_json::Value;
use toolkit_auth::oauth2::SecretString;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::domain::error::OagwError;
use crate::domain::model::CRED_REF_SCHEME;

/// The 30-second safety margin ADR 0008 subtracts from the IdP-reported
/// `expires_in` before caching a token.
pub const EXPIRY_SAFETY_MARGIN_SECS: u64 = 30;

/// The gear-level token-cache settings of ADR 0008, threaded from
/// [`OagwConfig`] into the OAuth2 plugin constructors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenCacheConfig {
    /// Ceiling for a cached access token's TTL, in seconds: the
    /// `token_cache_ttl_secs` value of the gear configuration.
    pub ttl_secs: Duration,
    /// Maximum number of entries the token cache holds: the
    /// `token_cache_capacity` value of the gear configuration.
    pub capacity: usize,
}

impl TokenCacheConfig {
    /// Bundles a TTL ceiling and a cache capacity, exactly as ADR 0008's
    /// "Gear-Level Configuration" section defines them.
    #[must_use]
    pub fn new(ttl_secs: u64, capacity: usize) -> Self {
        Self {
            ttl_secs: Duration::from_secs(ttl_secs),
            capacity,
        }
    }

    /// Reads the two gear-configuration keys the gear-wiring feature owns.
    #[must_use]
    pub fn from(config: &OagwConfig) -> Self {
        Self::new(config.token_cache_ttl_secs, config.token_cache_capacity)
    }
}

/// One token-cache entry: the key it was written under and the token it holds.
///
/// The key is verified on every hit, so a hash collision of the underlying
/// cache degrades to a miss and never to another tenant's token. `Debug` is
/// implemented by hand and redacts the token, which is held as a
/// [`SecretString`] zeroed on eviction.
#[derive(Clone)]
pub struct CachedToken {
    /// The four-component cache key the entry was written under.
    pub key: String,
    /// The bearer value, held as a `SecretString`.
    pub token: SecretString,
}

impl fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachedToken")
            .field("key", &self.key)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

/// Builds the ADR 0008 four-component cache key:
/// subject tenant id, subject id, auth-method tag, and the deterministic hash
/// of the sorted config key/value pairs.
///
/// `config` is a `BTreeMap`, so the walk is in sorted key order and two
/// configurations that differ only in a value — for example in `scopes` — hash
/// differently, while the same pairs in a different insertion order hash
/// identically.
#[must_use]
pub fn cache_key(
    tenant: Uuid,
    subject: Uuid,
    auth_method_tag: &str,
    config: &BTreeMap<String, Value>,
) -> String {
    format!(
        "{tenant}:{subject}:{auth_method_tag}:{}",
        hash_config(config)
    )
}

/// The deterministic `DefaultHasher` walk over the sorted config key/value
/// pairs, rendered as a fixed-width hex string.
fn hash_config(config: &BTreeMap<String, Value>) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (key, value) in config {
        hasher.write(key.as_bytes());
        value.hash(&mut hasher);
    }
    format!("{:016x}", hasher.finish())
}

/// Computes the cache TTL of a fetched token:
/// `min(config_ttl, expires_in − 30s safety margin)`.
///
/// `None` means the token is at or below the 30-second margin and must be used
/// for its own request only, never cached.
#[must_use]
pub fn ttl(config_ttl: Duration, expires_in: Duration) -> Option<Duration> {
    let margin = Duration::from_secs(EXPIRY_SAFETY_MARGIN_SECS);
    // @cpt-begin:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-14
    // The cache TTL is the lower of the gear-configured ceiling and the
    // IdP-reported lifetime minus the 30-second safety margin.
    if expires_in <= margin {
        return None;
    }
    Some(config_ttl.min(expires_in - margin))
    // @cpt-end:cpt-cf-oagw-algo-oauth2-token-acquisition:p1:inst-tk-14
    // @cpt-begin:cpt-cf-oagw-state-token-cache-entry:p1:inst-te-03
    // The value above is the TTL the entry is written with, so expiry is lazy:
    // an entry is observed as absent on the first lookup after its TTL elapses,
    // or earlier when capacity eviction removes it, and it never outlives the
    // lifetime the IdP reported.
    // @cpt-end:cpt-cf-oagw-state-token-cache-entry:p1:inst-te-03
}

/// Resolves one `cred://` reference through the cred-store client into a
/// [`SecretString`] at request time.
///
/// This is the single §1.5 failure mapping every auth-plugin failure is born
/// from: a reference that does not resolve is a [`OagwError::SecretNotFound`]
/// (500), an unreachable credential store is a [`OagwError::LinkUnavailable`]
/// (503) and a malformed reference is a [`OagwError::ValidationError`] (400) —
/// the same `cred://` form `cpt-cf-oagw-algo-shape-validation` of the
/// domain-model feature enforces at persist time, re-applied here without
/// re-declaring the rule. No failure message echoes the resolved value.
///
/// # Errors
/// Returns the mapped typed failure for the four outcomes above.
pub async fn resolve_secret(
    ctx: &SecurityContext,
    cred_store: &Arc<dyn CredStoreClientV1>,
    reference: &str,
) -> Result<SecretString, OagwError> {
    // @cpt-begin:cpt-cf-oagw-flow-auth-phase:p1:inst-au-09
    // The plugin failure of the auth phase is born here, mapped onto the
    // existing rows of the closed mapping table: no new row, no new variant.
    let key = cred_reference(reference)?;
    match cred_store.get(ctx, &key).await {
        // A secret that resolves to no usable credential value is an
        // unresolvable reference: the single 404 surface of the cred-store
        // contract, indistinguishable from an absent secret.
        Ok(Some(response)) => usable_value(response.value.as_bytes()),
        Ok(None) => Err(OagwError::secret_not_found(format!(
            "oagw.plugins: the '{CRED_REF_SCHEME}' reference does not resolve to a secret in the credential store"
        ))),
        Err(CredStoreError::NotFound | CredStoreError::AccessDenied) => {
            Err(OagwError::secret_not_found(format!(
                "oagw.plugins: the '{CRED_REF_SCHEME}' reference does not resolve to a secret in the credential store"
            )))
        }
        Err(CredStoreError::InvalidSecretRef { .. }) => Err(OagwError::validation_error(
            "oagw.plugins: the reference is not a usable credential-store reference",
        )),
        Err(_) => Err(OagwError::link_unavailable(
            "oagw.plugins: the credential store is unavailable",
        )),
    }
    // @cpt-end:cpt-cf-oagw-flow-auth-phase:p1:inst-au-09
}

/// The credential-store key of one `cred://` reference.
///
/// A reference that does not carry the `cred://` scheme, or whose remainder is
/// not a key the credential store accepts, is a malformed reference.
fn cred_reference(reference: &str) -> Result<SecretRef, OagwError> {
    let Some(key) = reference.strip_prefix(CRED_REF_SCHEME) else {
        return Err(OagwError::validation_error(format!(
            "oagw.plugins: the reference must be a '{CRED_REF_SCHEME}' reference that the credential store resolves at request time"
        )));
    };
    SecretRef::new(key).map_err(|_| {
        OagwError::validation_error(format!(
            "oagw.plugins: the '{CRED_REF_SCHEME}' reference does not name a credential-store secret"
        ))
    })
}

/// The `SecretString` of one resolved secret value.
fn usable_value(value: &[u8]) -> Result<SecretString, OagwError> {
    std::str::from_utf8(value)
        .map(SecretString::new)
        .map_err(|_| {
            OagwError::secret_not_found(
                "oagw.plugins: the credential store does not resolve the reference to a usable secret value",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use credstore_sdk::test_util::MockCredStoreClient;

    fn security_context(tenant: Uuid, subject: Uuid) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(subject)
            .subject_tenant_id(tenant)
            .build()
            .expect("a test context carries a subject and a tenant")
    }

    const TENANT: Uuid = Uuid::from_u128(0xa11ce);
    const SUBJECT: Uuid = Uuid::from_u128(0xbeef);

    #[test]
    fn the_cache_key_has_the_four_adr_0008_components() {
        let config = BTreeMap::from([(
            "client_id_ref".to_owned(),
            Value::String("cred://client".to_owned()),
        )]);
        let key = cache_key(TENANT, SUBJECT, "form", &config);

        let expected_hash = hash_config(&config);
        assert_eq!(
            key,
            format!("{TENANT}:{SUBJECT}:form:{expected_hash}"),
            "tenant, subject, auth-method tag and the config hash, colon-separated"
        );
        assert_eq!(key.split(':').count(), 4);
    }

    #[test]
    fn two_tenants_and_two_subjects_never_share_a_key() {
        let config = BTreeMap::new();

        assert_ne!(
            cache_key(TENANT, SUBJECT, "form", &config),
            cache_key(Uuid::from_u128(0xb0b), SUBJECT, "form", &config)
        );
        assert_ne!(
            cache_key(TENANT, SUBJECT, "form", &config),
            cache_key(TENANT, Uuid::from_u128(0xc0de), "form", &config)
        );
    }

    #[test]
    fn two_configs_differing_only_in_scopes_never_share_a_key() {
        let without_scopes = BTreeMap::from([(
            "client_id_ref".to_owned(),
            Value::String("cred://client".to_owned()),
        )]);
        let with_scopes = BTreeMap::from([
            (
                "client_id_ref".to_owned(),
                Value::String("cred://client".to_owned()),
            ),
            ("scopes".to_owned(), Value::String("read".to_owned())),
        ]);

        assert_ne!(
            cache_key(TENANT, SUBJECT, "form", &without_scopes),
            cache_key(TENANT, SUBJECT, "form", &with_scopes)
        );
    }

    #[test]
    fn the_config_hash_is_deterministic_and_order_independent() {
        let first = BTreeMap::from([
            ("scopes".to_owned(), Value::String("read write".to_owned())),
            (
                "client_id_ref".to_owned(),
                Value::String("cred://client".to_owned()),
            ),
        ]);
        let second = BTreeMap::from([
            (
                "client_id_ref".to_owned(),
                Value::String("cred://client".to_owned()),
            ),
            ("scopes".to_owned(), Value::String("read write".to_owned())),
        ]);

        assert_eq!(hash_config(&first), hash_config(&second));
        assert_eq!(
            cache_key(TENANT, SUBJECT, "form", &first),
            cache_key(TENANT, SUBJECT, "form", &second),
            "the same pairs in a different insertion order hash identically"
        );
        assert_ne!(
            hash_config(&first),
            hash_config(&BTreeMap::from([(
                "client_id_ref".to_owned(),
                Value::String("cred://other".to_owned())
            )]))
        );
    }

    #[test]
    fn the_ttl_is_the_minimum_of_the_ceiling_and_the_lifetime_margin() {
        let config_ttl = Duration::from_secs(300);

        assert_eq!(
            ttl(config_ttl, Duration::from_secs(3600)),
            Some(config_ttl),
            "a long-lived token is bounded by the gear-configured ceiling"
        );
        assert_eq!(
            ttl(Duration::from_secs(60), Duration::from_secs(1800)),
            Some(Duration::from_secs(60)),
            "a short ceiling wins over a long IdP lifetime"
        );
        assert_eq!(
            ttl(config_ttl, Duration::from_secs(31)),
            Some(Duration::from_secs(1)),
            "the 30-second margin is subtracted from the IdP lifetime"
        );
    }

    #[test]
    fn a_token_at_or_below_the_margin_is_not_cached() {
        for expires_in in [0, 1, 29, 30] {
            assert_eq!(
                ttl(Duration::from_secs(300), Duration::from_secs(expires_in)),
                None,
                "expires_in of {expires_in}s is at or below the safety margin"
            );
        }
        assert_eq!(
            ttl(Duration::from_secs(300), Duration::from_secs(31)),
            Some(Duration::from_secs(1)),
            "a lifetime one second above the margin is still cacheable"
        );
    }

    #[test]
    fn the_cache_config_threads_the_two_gear_keys() {
        let config = TokenCacheConfig::from(&crate::config::OagwConfig::default());
        assert_eq!(config.ttl_secs, Duration::from_secs(300));
        assert_eq!(config.capacity, 10_000);

        let overridden = crate::config::OagwConfig {
            token_cache_ttl_secs: 60,
            token_cache_capacity: 7,
            ..crate::config::OagwConfig::default()
        };
        let config = TokenCacheConfig::from(&overridden);
        assert_eq!(config.ttl_secs, Duration::from_secs(60));
        assert_eq!(config.capacity, 7);
        assert_eq!(TokenCacheConfig::new(60, 7), config);
    }

    #[tokio::test]
    async fn a_resolved_reference_becomes_a_secret_string() {
        let store: Arc<dyn CredStoreClientV1> =
            Arc::new(MockCredStoreClient::with_secrets(vec![(
                "api-key".to_owned(),
                "s3cr3t-value".to_owned(),
            )]));

        let resolved = resolve_secret(&security_context(TENANT, SUBJECT), &store, "cred://api-key")
            .await
            .expect("the reference resolves");
        assert_eq!(resolved.expose(), "s3cr3t-value");
    }

    #[tokio::test]
    async fn an_unresolvable_reference_is_a_secret_not_found() {
        let store: Arc<dyn CredStoreClientV1> = Arc::new(MockCredStoreClient::empty());

        let error = resolve_secret(&security_context(TENANT, SUBJECT), &store, "cred://missing")
            .await
            .unwrap_err();
        assert_eq!(error.mapping().variant, "SecretNotFound");
        assert_eq!(error.status(), 500);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
        );
    }

    #[tokio::test]
    async fn a_client_reporting_not_found_is_a_secret_not_found() {
        let store: Arc<dyn CredStoreClientV1> = Arc::new(MockCredStoreClient::erroring_not_found());
        let error = resolve_secret(&security_context(TENANT, SUBJECT), &store, "cred://key")
            .await
            .unwrap_err();
        assert_eq!(error.mapping().variant, "SecretNotFound");
    }

    #[tokio::test]
    async fn a_malformed_reference_is_a_validation_error() {
        let store: Arc<dyn CredStoreClientV1> = Arc::new(MockCredStoreClient::empty());

        for reference in ["api-key", "cred://", "cred://not a key", "http://api-key"] {
            let error = resolve_secret(&security_context(TENANT, SUBJECT), &store, reference)
                .await
                .unwrap_err();
            assert_eq!(error.mapping().variant, "ValidationError", "{reference}");
            assert_eq!(error.status(), 400);
            assert!(
                error.detail().contains("cred://"),
                "the failure names the expected reference form, got: {}",
                error.detail()
            );
        }
    }

    #[tokio::test]
    async fn an_unreachable_store_is_a_link_unavailable() {
        let store: Arc<dyn CredStoreClientV1> = Arc::new(MockCredStoreClient::always_failing());
        let error = resolve_secret(&security_context(TENANT, SUBJECT), &store, "cred://key")
            .await
            .unwrap_err();

        assert_eq!(error.mapping().variant, "LinkUnavailable");
        assert_eq!(error.status(), 503);
        assert_eq!(
            error.gts_type(),
            "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
        );
    }

    #[tokio::test]
    async fn a_non_utf8_secret_value_is_an_unresolvable_reference() {
        let store: Arc<dyn CredStoreClientV1> =
            Arc::new(MockCredStoreClient::returning_raw_value(vec![
                0xff, 0xfe, 0x00,
            ]));
        let error = resolve_secret(&security_context(TENANT, SUBJECT), &store, "cred://key")
            .await
            .unwrap_err();

        assert_eq!(error.mapping().variant, "SecretNotFound");
    }

    #[tokio::test]
    async fn no_failure_message_carries_the_resolved_value() {
        let store: Arc<dyn CredStoreClientV1> = Arc::new(MockCredStoreClient::always_failing());
        let error = resolve_secret(&security_context(TENANT, SUBJECT), &store, "cred://key")
            .await
            .unwrap_err();

        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains("backend failure"), "{rendered}");
    }

    #[test]
    fn a_cached_token_is_redacted_in_its_debug_output() {
        let entry = CachedToken {
            key: cache_key(TENANT, SUBJECT, "form", &BTreeMap::new()),
            token: SecretString::new("bearer-value"),
        };
        let rendered = format!("{entry:?}");
        assert!(rendered.contains(&entry.key), "{rendered}");
        assert!(!rendered.contains("bearer"), "{rendered}");
        assert!(rendered.contains("[REDACTED]"), "{rendered}");
    }
}

// @cpt-end:cpt-cf-oagw-dod-credential-isolation:p1:inst-full

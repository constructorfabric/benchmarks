//! Credential resolution seam (DESIGN §2.1 `principle-cred-isolation`).
//!
//! OAGW never stores secret material: an upstream references its credential
//! through a `cred://` URI that `cred_store` resolves. The gear declares
//! `credstore` as a dependency, so a deployment that wires the
//! [`SecretResolver`] gets the real lookup; every plugin that handles secret
//! material goes through this trait instead of touching configuration values
//! directly.

use async_trait::async_trait;


use crate::domain::error::DomainError;
use crate::domain::plugin::RequestContext;

/// URI scheme that marks a value as a `cred_store` reference.
pub const CREDENTIAL_SCHEME: &str = "cred://";

/// Resolves a credential reference into the material that must be sent
/// upstream.
impl std::fmt::Debug for dyn SecretResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Secret material is never rendered; only the resolver kind is.
        write!(f, "{}", self.describe())
    }
}
#[async_trait]
pub trait SecretResolver: Send + Sync {
    /// Human-readable family of the resolver, for diagnostics only.
    fn describe(&self) -> &'static str {
        "secret-resolver"
    }

    /// Resolves `reference`, which may be a `cred://` URI or a literal.
    ///
    /// # Errors
    ///
    /// Returns [`DomainError::SecretNotFound`] when the reference cannot be
    /// resolved for the calling tenant.
    async fn resolve(
        &self,
        ctx: &RequestContext,
        reference: &str,
    ) -> Result<String, DomainError>;
}

/// Fallback resolver used when no `cred_store` client is wired into the
/// runtime (unit tests, offline usage).
///
/// Review evidence (privilege boundary — credential isolation):
/// * Guardrail: DESIGN §2.1 `principle-cred-isolation` — "never stores or logs
///   secret material"; ADR 0008 resolves `client_id_ref`/`client_secret_ref`
///   through `cred_store`.
/// * Rationale: a `cred://` value is a *locator*, not a credential. Sending it
///   to the upstream would leak the tenant's secret namespace, so the
///   fallback fails closed with `500 SecretNotFound` instead of degrading into
///   a bogus `Authorization` header. Only an explicit literal configured by
///   the operator is forwarded, and the resolved value is never logged.
/// * Validation performed: `secret_resolver_*` tests in
///   `infra/plugin/tests.rs` assert that literals resolve, that `cred://`
///   references fail closed and that failures never echo the reference into
///   an outbound header.
#[derive(Debug, Default)]
pub struct LiteralSecretResolver;

#[async_trait]
impl SecretResolver for LiteralSecretResolver {
    async fn resolve(
        &self,
        _ctx: &RequestContext,
        reference: &str,
    ) -> Result<String, DomainError> {
        let trimmed = reference.trim();
        if trimmed.is_empty() {
            return Err(DomainError::SecretNotFound {
                detail: "credential reference is empty".to_owned(),
                upstream_id: None,
            });
        }
        if starts_with_credential_scheme(trimmed) {
            return Err(DomainError::SecretNotFound {
                detail: "credential reference requires a configured credential store".to_owned(),
                upstream_id: None,
            });
        }
        Ok(trimmed.to_owned())
    }
}

/// Whether `value` is a `cred://` locator rather than a literal credential.
pub(crate) fn starts_with_credential_scheme(value: &str) -> bool {
    let prefix_len = CREDENTIAL_SCHEME.len();
    value.len() >= prefix_len
        && value[..prefix_len].eq_ignore_ascii_case(CREDENTIAL_SCHEME)
}

/// Reads a string member of a plugin configuration object.
pub(crate) fn config_string(config: &serde_json::Value, key: &str) -> Option<String> {
    config.get(key).and_then(serde_json::Value::as_str).map(str::to_owned)
}

/// Reads the first present member among `keys`.
pub(crate) fn config_any(config: &serde_json::Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| config_string(config, key))
}

/// Reads a boolean member of a plugin configuration object.
pub(crate) fn config_flag(config: &serde_json::Value, key: &str, default: bool) -> bool {
    config
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(default)
}

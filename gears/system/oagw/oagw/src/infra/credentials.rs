//! Credential resolution for the auth plugins.
//!
//! Secrets are referenced by `cred://…` and resolved through the credential store at request time
//! (ADR-0008). The `cred://` prefix is *not* part of the store's key space, so it is stripped
//! before lookup. Resolved values are wrapped in [`SecretString`] so they never reach a log line or
//! a problem body.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use toolkit_security::SecurityContext;

use crate::domain::plugin::PluginError;

/// Opaque secret value; `Debug` and `Display` both redact it.
#[derive(Clone, Default)]
pub struct SecretValue(Arc<str>);

impl SecretValue {
    /// Wrap a plain value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(Arc::from(value.into()))
    }

    /// Controlled read access for building headers or form bodies.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// Backend the plugins resolve references against.
#[async_trait]
pub trait SecretResolverBackend: Send + Sync {
    /// Resolve `reference` for the caller, or fail.
    async fn resolve(
        &self,
        ctx: Option<Arc<SecurityContext>>,
        reference: &str,
    ) -> Result<SecretValue, PluginError>;
}

/// Handle to a credential resolver.
pub type SecretResolver = Arc<dyn SecretResolverBackend>;

/// Resolver backed by `CredStoreClientV1`.
pub struct CredStoreResolver {
    store: Arc<dyn credstore_sdk::CredStoreClientV1>,
}

impl CredStoreResolver {
    /// Resolver over the given credential store client.
    #[must_use]
    pub fn new(store: Arc<dyn credstore_sdk::CredStoreClientV1>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl SecretResolverBackend for CredStoreResolver {
    async fn resolve(
        &self,
        ctx: Option<Arc<SecurityContext>>,
        reference: &str,
    ) -> Result<SecretValue, PluginError> {
        let key = strip_cred_prefix(reference);
        let ctx = Arc::unwrap_or_clone(ctx.unwrap_or_else(|| Arc::new(SecurityContext::anonymous())));
        let secret_ref = credstore_sdk::SecretRef::new(key.to_string()).map_err(|e| {
            PluginError::SecretNotFound(format!(
                "'{reference}' is not a valid secret reference: {e}"
            ))
        })?;
        match self.store.get(&ctx, &secret_ref).await {
            Ok(Some(response)) => {
                let text = String::from_utf8_lossy(response.value.as_bytes()).to_string();
                Ok(SecretValue::new(text))
            }
            Ok(None) => Err(PluginError::SecretNotFound(reference.to_string())),
            Err(e) => Err(PluginError::AuthFailed(format!(
                "credential store rejected the request for '{reference}': {e}"
            ))),
        }
    }
}

/// Resolver that always fails, used when no credential store is wired.
#[derive(Debug, Default)]
pub struct MissingResolver;

#[async_trait]
impl SecretResolverBackend for MissingResolver {
    async fn resolve(
        &self,
        _ctx: Option<Arc<SecurityContext>>,
        reference: &str,
    ) -> Result<SecretValue, PluginError> {
        Err(PluginError::SecretNotFound(reference.to_string()))
    }
}

/// Strip the `cred://` scheme from a secret reference.
#[must_use]
pub fn strip_cred_prefix(reference: &str) -> &str {
    reference
        .strip_prefix("cred://")
        .unwrap_or(reference)
        .trim_start_matches('/')
}

/// Read a `cred://` reference out of a plugin config object.
#[must_use]
pub fn reference_of(config: &Value, key: &str) -> Option<String> {
    config.get(key).and_then(Value::as_str).map(str::to_string)
}

/// Resolve `reference` through `resolver`, mapping failures onto [`PluginError`].
pub async fn resolve_secret(
    resolver: &SecretResolver,
    ctx: Option<Arc<SecurityContext>>,
    reference: &str,
) -> Result<SecretValue, PluginError> {
    resolver.resolve(ctx, reference).await
}

/// Config reader for the OAuth2 plugins.
#[derive(Debug, Clone)]
pub struct OAuth2PluginConfig {
    /// Direct token endpoint URL.
    pub token_endpoint: Option<String>,
    /// OIDC issuer URL for discovery.
    pub issuer_url: Option<String>,
    /// `cred://` reference for the client id.
    pub client_id_ref: String,
    /// `cred://` reference for the client secret.
    pub client_secret_ref: String,
    /// Space-separated scopes.
    pub scopes: Option<String>,
}

impl OAuth2PluginConfig {
    /// Parse the plugin config, failing when a required key is missing.
    pub fn parse(config: &Value) -> Result<Self, PluginError> {
        let token_endpoint = reference_of(config, "token_endpoint");
        let issuer_url = reference_of(config, "issuer_url");
        if token_endpoint.is_none() && issuer_url.is_none() {
            return Err(PluginError::Config(
                "oauth2_client_cred requires 'token_endpoint' or 'issuer_url'".to_string(),
            ));
        }
        let Some(client_id_ref) = reference_of(config, "client_id_ref") else {
            return Err(PluginError::Config(
                "oauth2_client_cred requires 'client_id_ref'".to_string(),
            ));
        };
        let Some(client_secret_ref) = reference_of(config, "client_secret_ref") else {
            return Err(PluginError::Config(
                "oauth2_client_cred requires 'client_secret_ref'".to_string(),
            ));
        };
        Ok(Self {
            token_endpoint,
            issuer_url,
            client_id_ref,
            client_secret_ref,
            scopes: reference_of(config, "scopes"),
        })
    }
}

/// Deterministic hash of a plugin config, used in the token cache key (ADR-0008).
#[must_use]
pub fn hash_config(config: &Value) -> String {
    let mut pairs: Vec<String> = match config {
        Value::Object(map) => map
            .iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| {
                let scalar = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                format!("{k}={scalar}")
            })
            .collect(),
        _ => Vec::new(),
    };
    pairs.sort();
    pairs.join("&")
}

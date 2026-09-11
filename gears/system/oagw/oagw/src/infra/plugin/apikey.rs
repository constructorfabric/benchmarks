// Created: 2026-09-01 by Constructor Tech
//! The API-key auth plugin.
//!
//! `docs/PRD.md` §5.2 "Authentication Injection": API key injection by
//! header or query parameter. The key material is resolved from
//! `cred_store` at request time.

use async_trait::async_trait;

use crate::domain::errors::OagwError;
use crate::domain::model::builtin_plugins;
use crate::infra::context::PluginRequest;
use crate::infra::credstore::SecretResolver;
use crate::infra::plugin::traits::AuthPlugin;

/// Injects an API key into the outbound request.
pub struct ApiKeyAuthPlugin {
    resolver: crate::infra::credstore::SecretResolver,
}

/// Config keys read from the upstream `auth.config` block.
pub mod keys {
    /// `cred://` reference holding the key material.
    pub const KEY_REF: &str = "key_ref";
    /// Header the key is injected into.
    pub const HEADER: &str = "header";
    /// Query parameter the key is injected into.
    pub const QUERY: &str = "query";
}

/// The header used when the configuration names neither a header nor a
/// query parameter.
pub const DEFAULT_HEADER: &str = "x-api-key";

impl ApiKeyAuthPlugin {
    /// A plugin resolving keys through `resolver`.
    #[must_use]
    pub fn new(resolver: SecretResolver) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &str {
        builtin_plugins::AUTH_APIKEY
    }

    async fn authenticate(&self, request: &mut PluginRequest) -> Result<(), OagwError> {
        let config = &request.auth_config;
        let value_of = |key: &str| -> Option<String> {
            config.get(key).and_then(|v| v.as_str()).map(str::to_owned)
        };
        let Some(reference) = value_of(keys::KEY_REF) else {
            return Err(OagwError::authentication_failed(
                "api key plugin requires a 'key_ref' credential reference",
            ));
        };
        let key = self.resolver.resolve(&request.security, &reference).await?;
        if let Some(header) = value_of(keys::HEADER) {
            request.set_header(header, key);
        } else if let Some(query) = value_of(keys::QUERY) {
            let path = match request.path.split_once('?') {
                Some((base, existing)) => format!("{base}?{existing}&{query}={key}"),
                None => format!("{}?{query}={key}", request.path),
            };
            request.path = path;
        } else {
            request.set_header(DEFAULT_HEADER, key);
        }
        Ok(())
    }
}

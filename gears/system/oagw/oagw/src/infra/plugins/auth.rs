//! Built-in auth plugins and the `AuthPlugin` trait
//! ([ADR-0002](../../../../docs/ADR/0002-plugin-system.md),
//! [ADR-0008](../../../../docs/ADR/0008-oauth2-client-credentials-auth-plugin.md)).
//!
//! An auth plugin injects credentials into the outbound request. It never
//! touches credstore itself: it names a `cred://` reference in its
//! configuration and resolves it through the [`SecretResolver`] handle it was
//! built with, so the credential store is an
//! [`infra::secrets`](crate::infra::secrets) concern and the plugins stay
//! testable against a mock store.
//!
//! Every built-in type has its own module: [`apikey`] injects a key into a
//! configured header, [`basic`] and [`bearer`] build an `Authorization` value,
//! and [`oauth2`] exchanges client credentials for an access token and caches
//! it per [ADR-0008](../../../../docs/ADR/0008-oauth2-client-credentials-auth-plugin.md).

pub mod apikey;
pub mod basic;
pub mod bearer;
pub mod oauth2;

use async_trait::async_trait;
use credstore_sdk::SecretValue;
use http::HeaderValue;
use serde_json::Value;

use crate::domain::error::OagwError;
use crate::domain::model::AuthType;

pub use apikey::ApiKeyAuthPlugin;
pub use basic::BasicAuthPlugin;
pub use bearer::BearerAuthPlugin;
pub use oauth2::{OAuth2ClientAuthMethod, OAuth2ClientCredAuthPlugin, TokenCacheConfig};

use super::{PluginType, RequestContext};

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// Credential injection, executed once per request before any guard.
#[async_trait]
pub trait AuthPlugin: Send + Sync {
    /// The GTS identifier the plugin is registered under.
    fn id(&self) -> &str;

    /// The kind of the plugin.
    fn plugin_type(&self) -> PluginType;

    /// Injects the credentials of the binding currently executing.
    ///
    /// # Errors
    /// [`OagwError::Validation`] when the binding's configuration is invalid,
    /// [`OagwError::SecretNotFound`] / [`OagwError::SecretError`] when a
    /// referenced secret is missing or the credential store fails,
    /// [`OagwError::AuthenticationFailed`] when the credential itself is
    /// rejected, and [`OagwError::DownstreamError`] when a token endpoint
    /// rejects the request.
    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError>;
}

// ---------------------------------------------------------------------------
// Configuration keys and helpers
// ---------------------------------------------------------------------------

/// `basic` — `cred://` reference of the username.
pub const BASIC_USERNAME_REF: &str = "username_ref";
/// `basic` — `cred://` reference of the password.
pub const BASIC_PASSWORD_REF: &str = "password_ref";
/// `bearer` — `cred://` reference of the bearer token.
pub const BEARER_SECRET_REF: &str = "secret_ref";

/// The configuration object of the binding currently executing.
///
/// The value is owned, so a plugin can read its configuration and mutate the
/// request context in the same breath.
///
/// # Errors
/// [`OagwError::Validation`] when the binding carries no configuration object.
pub(crate) fn config_of(ctx: &RequestContext) -> Result<Value, OagwError> {
    match &ctx.config {
        Some(config) if config.is_object() => Ok(config.clone()),
        Some(_) => Err(OagwError::Validation {
            message: "the binding configuration must be a JSON object".to_owned(),
        }),
        None => Err(OagwError::Validation {
            message: "the binding carries no configuration".to_owned(),
        }),
    }
}

/// The `cred://` reference named by `config[key]`, as spelled by the binding.
///
/// # Errors
/// [`OagwError::Validation`] when the key is absent or not a string. The
/// reference itself is validated by
/// [`normalize_secret_ref`](crate::infra::secrets::normalize_secret_ref) once
/// it is resolved.
pub(crate) fn required_ref<'a>(config: &'a Value, key: &str) -> Result<&'a str, OagwError> {
    config
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| OagwError::Validation {
            message: format!("'{key}' is required and must be a string"),
        })
}

/// An optional string configuration value.
fn optional_str<'a>(config: &'a Value, key: &str) -> Option<&'a str> {
    config.get(key).and_then(Value::as_str)
}

/// The resolved credential as a header value.
///
/// # Errors
/// [`OagwError::Validation`] when the credential is not valid header text.
pub(crate) fn header_value_of(credential: &str) -> Result<HeaderValue, OagwError> {
    HeaderValue::from_str(credential).map_err(|_| OagwError::Validation {
        message: "the resolved credential is not a valid header value".to_owned(),
    })
}

/// `Bearer <credential>` as an `Authorization` header value.
///
/// # Errors
/// [`OagwError::Validation`] when the resolved credential is not valid header
/// text.
pub(crate) fn bearer_value_of(credential: &str) -> Result<HeaderValue, OagwError> {
    header_value_of(&format!("Bearer {credential}"))
}

/// The credential bytes of a resolved secret as header-safe text.
///
/// A credential that is not UTF-8 cannot be carried in a header value; the
/// lossy conversion keeps the round trip well defined without ever logging the
/// material. The result is short-lived request state, never logged.
#[must_use]
pub(crate) fn credential_text(secret: &SecretValue) -> String {
    String::from_utf8_lossy(secret.as_bytes()).into_owned()
}

// ---------------------------------------------------------------------------
// Built-ins
// ---------------------------------------------------------------------------

/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1` — no authentication.
#[derive(Debug, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        AuthType::NOOP
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Auth
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), OagwError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use super::*;
    use crate::domain::model::AuthType;
    use crate::infra::plugins::PluginType;
    use crate::infra::test_support::context_with;

    #[tokio::test]
    async fn noop_authenticates_without_touching_the_request() {
        let plugin = NoopAuthPlugin;
        let mut context = context_with(serde_json::json!({}));

        plugin
            .authenticate(&mut context)
            .await
            .expect("noop allows");

        assert!(context.request_headers.is_empty());
        assert!(context.attributes.is_empty());
        assert_eq!(plugin.id(), AuthType::NOOP);
        assert_eq!(plugin.plugin_type(), PluginType::Auth);
    }
}

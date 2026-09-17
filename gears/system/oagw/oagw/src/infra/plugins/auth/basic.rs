//! The `basic` auth plugin: HTTP Basic injection
//! ([PRD.md](../../../../../docs/PRD.md) `cpt-cf-oagw-fr-auth-injection`).
//!
//! Both halves of the credential come from the credential store at request
//! time and are written as `Authorization: Basic base64(<username>:<password>)`,
//! replacing any `Authorization` header the caller supplied.

use std::sync::Arc;

use async_trait::async_trait;
use http::header::AUTHORIZATION;

use crate::domain::error::OagwError;
use crate::domain::model::AuthType;
use crate::infra::plugins::{PluginType, RequestContext, SecretResolver};

use super::{AuthPlugin, config_of, header_value_of, required_ref};

/// `basic` — `cred://` reference of the username.
pub const BASIC_USERNAME_REF: &str = "username_ref";
/// `basic` — `cred://` reference of the password.
pub const BASIC_PASSWORD_REF: &str = "password_ref";

/// `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1` — HTTP Basic
/// authentication.
///
/// Configuration: `username_ref` and `password_ref` (both required, `cred://`
/// references).
pub struct BasicAuthPlugin {
    resolver: Arc<dyn SecretResolver>,
}

impl std::fmt::Debug for BasicAuthPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BasicAuthPlugin").finish_non_exhaustive()
    }
}

impl BasicAuthPlugin {
    /// Builds the plugin, resolving both halves through `resolver`.
    #[must_use]
    pub fn new(resolver: Arc<dyn SecretResolver>) -> Self {
        Self { resolver }
    }

    /// `base64(<username>:<password>)` of the resolved credentials.
    ///
    /// The bytes are joined before the conversion, so a credential that is not
    /// UTF-8 still round-trips exactly.
    #[must_use]
    pub fn credentials(username: &[u8], password: &[u8]) -> String {
        let mut credentials = Vec::with_capacity(username.len() + password.len() + 1);
        credentials.extend_from_slice(username);
        credentials.push(b':');
        credentials.extend_from_slice(password);
        base64(&credentials)
    }
}

/// The standard base64 alphabet
/// ([RFC 4648 §4](https://datatracker.ietf.org/doc/html/rfc4648#section-4)).
const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encodes `data` in standard base64 with padding.
///
/// The crate carries no base64 dependency; this is the smallest correct
/// encoder, exercised against the RFC 4648 vectors in the tests.
#[must_use]
fn base64(data: &[u8]) -> String {
    let mut encoded = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        encoded.push(symbol(first >> 2));
        encoded.push(symbol((first & 0x03) << 4 | second >> 4));
        if chunk.len() > 1 {
            encoded.push(symbol((second & 0x0f) << 2 | third >> 6));
        } else {
            encoded.push('=');
        }
        if chunk.len() > 2 {
            encoded.push(symbol(third & 0x3f));
        } else {
            encoded.push('=');
        }
    }
    encoded
}

/// The base64 symbol of a six-bit `index`.
#[must_use]
fn symbol(index: u8) -> char {
    char::from(BASE64_ALPHABET[usize::from(index)])
}

#[async_trait]
impl AuthPlugin for BasicAuthPlugin {
    fn id(&self) -> &str {
        AuthType::BASIC
    }

    fn plugin_type(&self) -> PluginType {
        PluginType::Auth
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        let config = config_of(ctx)?;
        let username_ref = required_ref(&config, BASIC_USERNAME_REF)?;
        let password_ref = required_ref(&config, BASIC_PASSWORD_REF)?;
        let username = self.resolver.resolve_cref(ctx, username_ref).await?;
        let password = self.resolver.resolve_cref(ctx, password_ref).await?;
        let encoded = Self::credentials(username.as_bytes(), password.as_bytes());
        // Replaces any client-supplied Authorization header.
        ctx.request_headers
            .insert(AUTHORIZATION, header_value_of(&format!("Basic {encoded}"))?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]
    use http::{HeaderMap, HeaderValue};

    use super::{BASIC_PASSWORD_REF, BASIC_USERNAME_REF, BasicAuthPlugin, base64};
    use crate::domain::error::{OagwError, SECRET_NOT_FOUND_GTS_ID};
    use crate::domain::model::AuthType;
    use crate::infra::plugins::{AuthPlugin, PluginType};
    use crate::infra::test_support::{context_with, secret_store};

    const USERNAME: &str = "integration-user";
    const PASSWORD: &str = "hunter2";
    const ENCODED: &str = "aW50ZWdyYXRpb24tdXNlcjpodW50ZXIy";

    fn plugin(secrets: &[(&str, &str)]) -> BasicAuthPlugin {
        BasicAuthPlugin::new(secret_store(secrets.to_vec()))
    }

    fn config(username: &str, password: &str) -> serde_json::Value {
        serde_json::json!({ BASIC_USERNAME_REF: username, BASIC_PASSWORD_REF: password })
    }

    #[test]
    fn base64_matches_the_standard_vectors() {
        for (input, expected) in [
            (&b""[..], ""),
            (&b"f"[..], "Zg=="),
            (&b"fo"[..], "Zm8="),
            (&b"foo"[..], "Zm9v"),
            (&b"foob"[..], "Zm9vYg=="),
            (&b"fooba"[..], "Zm9vYmE="),
            (&b"foobar"[..], "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input), expected, "base64 of {input:?}");
        }
    }

    #[test]
    fn the_credential_pair_is_joined_before_it_is_encoded() {
        assert_eq!(
            BasicAuthPlugin::credentials(USERNAME.as_bytes(), PASSWORD.as_bytes()),
            ENCODED
        );
    }

    #[tokio::test]
    async fn injects_the_base64_encoded_credential_pair() {
        let plugin = plugin(&[("username", USERNAME), ("password", PASSWORD)]);
        let mut context = context_with(config("cred://username", "password"));

        plugin.authenticate(&mut context).await.expect("injected");

        let expected = format!("Basic {ENCODED}");
        assert_eq!(
            context
                .request_headers
                .get("authorization")
                .and_then(|value| value.to_str().ok()),
            Some(expected.as_str())
        );
        assert_eq!(plugin.id(), AuthType::BASIC);
        assert_eq!(plugin.plugin_type(), PluginType::Auth);
    }

    #[tokio::test]
    async fn replaces_a_client_supplied_authorization_header() {
        let plugin = plugin(&[("username", USERNAME), ("password", PASSWORD)]);
        let mut context = context_with(config("username", "password"));
        let mut headers = HeaderMap::new();
        headers.insert("authorization", HeaderValue::from_static("Bearer stale"));
        context.request_headers = headers;

        plugin.authenticate(&mut context).await.expect("injected");

        let expected = format!("Basic {ENCODED}");
        let injected: Vec<_> = context
            .request_headers
            .get_all("authorization")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        assert_eq!(injected, [expected], "the client header must be replaced");
    }

    #[tokio::test]
    async fn a_missing_password_is_a_500_secret_not_found() {
        let plugin = plugin(&[("username", USERNAME)]);
        let mut context = context_with(config("username", "unknown-password"));

        let error = plugin.authenticate(&mut context).await.unwrap_err();

        assert!(matches!(error, OagwError::SecretNotFound), "{error}");
        assert_eq!(error.http_status(), 500);
        assert_eq!(error.gts_id(), SECRET_NOT_FOUND_GTS_ID);
    }

    #[tokio::test]
    async fn a_missing_username_ref_is_a_400() {
        let plugin = plugin(&[]);
        let mut context = context_with(serde_json::json!({ BASIC_PASSWORD_REF: "password" }));

        let error = plugin.authenticate(&mut context).await.unwrap_err();

        assert!(matches!(error, OagwError::Validation { .. }), "{error}");
        assert_eq!(error.http_status(), 400);
    }
}

//! Built-in plugins (ADR `0002-plugin-system`, "Built-in Plugins").
//!
//! * [`NoopAuthPlugin`] — injects nothing (the documented no-op auth plugin);
//! * [`ApiKeyAuthPlugin`] — injects a `cred://`-resolved API key;
//! * [`OAuth2ClientCredAuthPlugin`] — the two client-credentials variants,
//!   with an internal token cache (ADR 0008);
//! * [`RequiredHeadersGuardPlugin`] — header presence enforcement (ADR 0009);
//! * [`RequestIdTransformPlugin`] — `X-Request-ID` propagation.
//!
//! `register_builtins` returns the shared [`CredentialStore`] so the gear can
//! wire the credstore client into it after `ctx` is available.

use async_trait::async_trait;
use std::sync::Arc;

use crate::domain::error::DomainError;
use crate::domain::plugin::{
    AUTH_PLUGIN_NOOP, ErrorContext, GuardDecision, GuardPlugin, PluginRegistry, RequestContext,
    ResponseContext, TransformPlugin,
};
use crate::infra::plugin::credential::{
    ApiKeyAuthPlugin, CredentialStore, OAuth2ClientCredAuthPlugin,
};

/// Comma-separated header names checked on the inbound request (ADR 0009).
pub const REQUIRED_REQUEST_HEADERS: &str = "required_request_headers";
/// Comma-separated header names checked on the upstream response (ADR 0009).
pub const REQUIRED_RESPONSE_HEADERS: &str = "required_response_headers";
/// Error code carried by a required-headers rejection (ADR 0009).
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// Assembles the built-in plugin set into `registry`.
///
/// Returns the shared credential store the auth plugins were built over, so
/// the gear can wire the credstore client into it once `ctx` is available.
pub fn register_builtins(registry: &mut PluginRegistry) -> Arc<CredentialStore> {
    let credentials = CredentialStore::new();
    let ttl = crate::config::OagwConfig::default().token_cache_ttl();
    let capacity = crate::config::OagwConfig::default().token_cache_capacity;

    registry.register_auth(Arc::new(NoopAuthPlugin));
    registry.register_auth(Arc::new(ApiKeyAuthPlugin::new(Arc::clone(&credentials))));
    registry.register_auth(Arc::new(OAuth2ClientCredAuthPlugin::client_secret_post(
        Arc::clone(&credentials),
        ttl,
        capacity,
    )));
    registry.register_auth(Arc::new(OAuth2ClientCredAuthPlugin::client_secret_basic(
        Arc::clone(&credentials),
        ttl,
        capacity,
    )));
    registry.register_guard(Arc::new(RequiredHeadersGuardPlugin));
    registry.register_transform(Arc::new(RequestIdTransformPlugin));
    credentials
}

/// No-op auth plugin (`cf.core.oagw.noop.v1`).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopAuthPlugin;

#[async_trait]
impl crate::domain::plugin::AuthPlugin for NoopAuthPlugin {
    fn id(&self) -> &str {
        "cf.core.oagw.noop.v1"
    }

    fn plugin_type(&self) -> &str {
        AUTH_PLUGIN_NOOP
    }

    async fn authenticate(&self, _ctx: &mut RequestContext) -> Result<(), DomainError> {
        Ok(())
    }
}

/// Required-headers guard plugin (`cf.core.oagw.required_headers.v1`).
///
/// ADR 0009: header **presence** enforcement, opt-in per upstream, failing
/// open when unconfigured. Configuration keys
/// (`plugins.configs[<this plugin>]`):
///
/// | Key | Required | Description |
/// |---|---|---|
/// | `required_request_headers` | no | comma-separated names checked on the request |
/// | `required_response_headers` | no | comma-separated names checked on the response |
///
/// Both phases fail open when their key is absent or blank, match
/// case-insensitively, and report only the *first* missing header.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequiredHeadersGuardPlugin;

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        "cf.core.oagw.required_headers.v1"
    }

    fn plugin_type(&self) -> &str {
        crate::domain::plugin::GUARD_PLUGIN_REQUIRED_HEADERS
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, DomainError> {
        let Some(required) = ctx
            .plugin_config
            .as_ref()
            .and_then(|config| config.get(REQUIRED_REQUEST_HEADERS))
            .and_then(serde_json::Value::as_str)
        else {
            return Ok(GuardDecision::Continue);
        };
        for name in split_header_names(required) {
            if ctx.headers.get(&name).is_none() {
                return Ok(GuardDecision::reject(DomainError::Validation(format!(
                    "required request header '{name}' is missing ({REQUIRED_HEADER_MISSING})"
                ))));
            }
        }
        Ok(GuardDecision::Continue)
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, DomainError> {
        let Some(required) = ctx
            .plugin_config
            .as_ref()
            .and_then(|config| config.get(REQUIRED_RESPONSE_HEADERS))
            .and_then(serde_json::Value::as_str)
        else {
            return Ok(GuardDecision::Continue);
        };
        for name in split_header_names(required) {
            if ctx.headers.get(&name).is_none() {
                return Ok(GuardDecision::reject(DomainError::ProtocolError(
                    ctx.upstream_alias.clone(),
                    format!("required response header '{name}' is missing ({REQUIRED_HEADER_MISSING})"),
                )));
            }
        }
        Ok(GuardDecision::Continue)
    }
}

/// Splits a comma-separated header-name list, lowercased, empties dropped.
fn split_header_names(raw: &str) -> Vec<http::HeaderName> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| http::HeaderName::try_from(entry.to_ascii_lowercase()).ok())
        .collect()
}

/// Request-id transform plugin (`cf.core.oagw.request_id.v1`).
///
/// Propagates an inbound `X-Request-ID` or mints one; the value is applied to
/// the upstream request headers by the caller that drains the injected headers.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestIdTransformPlugin;

/// Header the plugin propagates.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

#[async_trait]
impl TransformPlugin for RequestIdTransformPlugin {
    fn id(&self) -> &str {
        "cf.core.oagw.request_id.v1"
    }

    fn plugin_type(&self) -> &str {
        crate::domain::plugin::TRANSFORM_PLUGIN_REQUEST_ID
    }

    async fn transform_request(&self, ctx: &mut RequestContext) -> Result<(), DomainError> {
        let existing = ctx
            .headers
            .get(REQUEST_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let id = existing.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if let Ok(value) = http::HeaderValue::from_str(&id) {
            ctx.headers
                .insert(http::HeaderName::from_static("x-request-id"), value);
        }
        Ok(())
    }

    async fn transform_response(&self, ctx: &mut ResponseContext) -> Result<(), DomainError> {
        // Echo the propagated `X-Request-ID` back to the caller so the caller
        // can correlate the exchange even when the upstream mints its own.
        if let Some(injected) = ctx
            .request_id
            .take()
            .and_then(|id| http::HeaderValue::from_str(&id).ok())
        {
            ctx.headers
                .insert(http::HeaderName::from_static("x-request-id"), injected);
        }
        Ok(())
    }

    async fn transform_error(&self, ctx: &mut ErrorContext) -> Result<(), DomainError> {
        let _ = ctx;
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::domain::plugin::{
        AUTH_PLUGIN_API_KEY, AuthPlugin, GUARD_PLUGIN_REQUIRED_HEADERS, PluginRegistry,
        TRANSFORM_PLUGIN_REQUEST_ID,
    };
    use uuid::Uuid;

    fn request_ctx() -> RequestContext {
        let mut ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        ctx.plugin_config = Some(serde_json::json!({
            REQUIRED_REQUEST_HEADERS: "x-correlation-id, accept"
        }));
        ctx
    }

    #[test]
    fn builtins_register_under_their_short_ids() {
        let mut registry = PluginRegistry::new();
        register_builtins(&mut registry);
        assert!(registry.auth_plugin(AUTH_PLUGIN_NOOP).is_some());
        assert!(registry.auth_plugin(AUTH_PLUGIN_API_KEY).is_some());
        assert!(
            registry
                .auth_plugin(crate::domain::plugin::AUTH_PLUGIN_OAUTH2_CLIENT_CRED)
                .is_some()
        );
        assert!(
            registry
                .auth_plugin(crate::domain::plugin::AUTH_PLUGIN_OAUTH2_CLIENT_CRED_BASIC)
                .is_some()
        );
        assert!(registry.guard_plugin(GUARD_PLUGIN_REQUIRED_HEADERS).is_some());
        assert!(registry.transform_plugin(TRANSFORM_PLUGIN_REQUEST_ID).is_some());
        assert!(!registry.knows("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1"));
    }

    #[tokio::test]
    async fn noop_auth_plugin_injects_nothing() {
        let mut registry = PluginRegistry::new();
        register_builtins(&mut registry);
        let mut ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        NoopAuthPlugin.authenticate(&mut ctx).await.expect("noop");
        assert!(ctx.take_injected_headers().is_empty());
    }

    #[tokio::test]
    async fn request_id_plugin_propagates_or_mints() {
        let mut ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        ctx.headers.insert(
            REQUEST_ID_HEADER,
            http::HeaderValue::from_static("req-1"),
        );
        RequestIdTransformPlugin.transform_request(&mut ctx).await.expect("ok");
        assert_eq!(ctx.headers.get(REQUEST_ID_HEADER).unwrap(), "req-1");

        let mut fresh = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        RequestIdTransformPlugin.transform_request(&mut fresh).await.expect("ok");
        assert!(!fresh.headers.get(REQUEST_ID_HEADER).unwrap().is_empty());
    }

    #[tokio::test]
    async fn required_headers_guard_fails_open_when_unconfigured() {
        let ctx = RequestContext::new(Uuid::nil(), "a.example", http::Method::GET, "/v1");
        let decision = RequiredHeadersGuardPlugin
            .guard_request(&ctx)
            .await
            .expect("ok");
        assert!(decision.is_continue());
    }

    #[tokio::test]
    async fn required_headers_guard_rejects_the_first_missing_header() {
        let mut ctx = request_ctx();
        // `accept` is present, `x-correlation-id` is not, so the first missing
        // entry wins.
        ctx.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        let decision = RequiredHeadersGuardPlugin
            .guard_request(&ctx)
            .await
            .expect("resolved");
        match decision {
            GuardDecision::Reject(error) => {
                assert_eq!(error.http_status(), 400);
                assert!(format!("{error}").contains("x-correlation-id"));
                assert!(format!("{error}").contains(REQUIRED_HEADER_MISSING));
            }
            GuardDecision::Continue => panic!("a missing header must be rejected"),
        }
    }

    #[tokio::test]
    async fn required_headers_guard_accepts_a_complete_request() {
        let mut ctx = request_ctx();
        ctx.headers.insert(
            http::HeaderName::from_static("x-correlation-id"),
            http::HeaderValue::from_static("abc"),
        );
        ctx.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        assert!(
            RequiredHeadersGuardPlugin
                .guard_request(&ctx)
                .await
                .expect("resolved")
                .is_continue()
        );
    }

    #[tokio::test]
    async fn required_headers_guard_rejects_a_bad_upstream_response() {
        let mut ctx = crate::domain::plugin::ResponseContext::new(
            Uuid::nil(),
            "a.example",
            http::StatusCode::OK,
        );
        ctx.plugin_config = Some(serde_json::json!({ REQUIRED_RESPONSE_HEADERS: "content-type" }));
        ctx.headers.insert(
            http::header::CONTENT_LENGTH,
            http::HeaderValue::from_static("0"),
        );
        let decision = RequiredHeadersGuardPlugin
            .guard_response(&ctx)
            .await
            .expect("resolved");
        match decision {
            GuardDecision::Reject(error) => {
                assert_eq!(error.http_status(), 502);
                assert!(format!("{error}").contains("content-type"));
            }
            GuardDecision::Continue => panic!("a missing header must be rejected"),
        }
    }

    #[test]
    fn blank_header_lists_are_dropped() {
        let names: Vec<String> = split_header_names(" , , Accept , ,")
            .into_iter()
            .map(|name| name.as_str().to_owned())
            .collect();
        assert_eq!(names, vec!["accept"]);
    }
}

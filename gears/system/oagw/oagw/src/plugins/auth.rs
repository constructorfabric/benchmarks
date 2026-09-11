//! Built-in auth plugins.
//!
//! Each plugin resolves its credential by reference at request time and produces a
//! header injection for the outbound request. Credential material never reaches a log
//! record, an error message or an API response: references are carried in configuration,
//! material is fetched just before the upstream call and dropped afterwards.

use bytes::Bytes;
use serde_json::Value;
use toolkit_auth::{ClientAuthMethod, OAuthClientConfig, SecretString, fetch_token};
use url::Url;

use crate::error::{ErrorKind, OagwError};
use crate::plugins::{InjectedCredential, PluginContext};

/// Auth plugins that exist in the catalog but fail when selected.
pub const UNIMPLEMENTED_AUTH_PLUGINS: [&str; 2] = ["basic", "bearer"];

/// The auth plugins the gateway binds and executes.
pub const BINDABLE_AUTH_PLUGINS: [&str; 4] = [
    "noop",
    "apikey",
    "oauth2_client_cred",
    "oauth2_client_cred_basic",
];

/// The client-auth method an `OAuth2` plugin uses.
#[must_use]
pub fn auth_method(plugin_name: &str) -> Option<ClientAuthMethod> {
    match plugin_name {
        "oauth2_client_cred" => Some(ClientAuthMethod::Form),
        "oauth2_client_cred_basic" => Some(ClientAuthMethod::Basic),
        _ => None,
    }
}

/// Runs one auth plugin and returns the credential it wants injected.
///
/// # Errors
///
/// Returns an unknown-auth-plugin error for a catalog-only identifier, a validation error
/// when a reference is missing and an authentication failure when an exchange fails.
pub async fn execute(
    plugin: &crate::plugins::BoundPlugin,
    ctx: &PluginContext<'_>,
) -> Result<InjectedCredential, OagwError> {
    match plugin.name.as_str() {
        "oauth2_client_cred" | "oauth2_client_cred_basic" => execute_oauth2(plugin, ctx).await,
        other if UNIMPLEMENTED_AUTH_PLUGINS.contains(&other) => Err(OagwError::new(
            ErrorKind::PluginNotFound,
            format!("auth plugin `{other}` is catalogued but not implemented"),
        )
        .with_extensions(ctx.extensions())),
        // `noop` and `apikey` both read a single material reference; the config key is
        // `credential` unless the binding overrides it.
        other if BINDABLE_AUTH_PLUGINS.contains(&other) => {
            let key = plugin
                .config
                .get("credential_key")
                .and_then(Value::as_str)
                .unwrap_or("credential");
            let material = resolve_material(plugin, ctx, key).await?;
            inject_static(plugin, ctx, &material)
        }
        other => Err(OagwError::new(
            ErrorKind::PluginNotFound,
            format!("unknown auth plugin `{other}`"),
        )
        .with_extensions(ctx.extensions())),
    }
}

/// Resolves a credential reference for a plugin binding.
///
/// # Errors
///
/// Returns a validation error when the binding carries no reference and propagates the
/// resolver's error otherwise.
pub async fn resolve_material(
    plugin: &crate::plugins::BoundPlugin,
    ctx: &PluginContext<'_>,
    key: &str,
) -> Result<String, OagwError> {
    let reference = plugin
        .config
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default();
    if reference.is_empty() {
        return Err(OagwError::new(
            ErrorKind::ValidationError,
            format!("auth plugin `{}` requires a `{key}` credential reference", plugin.name),
        )
        .with_extensions(ctx.extensions()));
    }
    ctx.credentials
        .resolve(ctx.security.security(), reference)
        .await
        .map_err(|err| err.with_extensions(ctx.extensions()))
        .and_then(|value| {
            value.ok_or_else(|| {
                OagwError::new(
                    ErrorKind::SecretNotFound,
                    format!("credential `{reference}` was not found"),
                )
                .with_extensions(ctx.extensions())
            })
        })
}

/// Builds the header injection for a static-material auth plugin.
///
/// # Errors
///
/// Returns an unknown-auth-plugin error for a catalog-only identifier.
pub fn inject_static(
    plugin: &crate::plugins::BoundPlugin,
    ctx: &PluginContext<'_>,
    material: &str,
) -> Result<InjectedCredential, OagwError> {
    match plugin.name.as_str() {
        "noop" => Ok(InjectedCredential::header(
            "authorization",
            Bytes::copy_from_slice(material.as_bytes()),
        )),
        "apikey" => {
            let header = plugin
                .config
                .get("header")
                .and_then(Value::as_str)
                .unwrap_or("authorization");
            let prefix = plugin
                .config
                .get("prefix")
                .and_then(Value::as_str)
                .unwrap_or_default();
            Ok(InjectedCredential::header(
                header,
                Bytes::from(format!("{prefix}{material}")),
            ))
        }
        other if UNIMPLEMENTED_AUTH_PLUGINS.contains(&other) => Err(OagwError::new(
            ErrorKind::PluginNotFound,
            format!("auth plugin `{other}` is catalogued but not implemented"),
        )
        .with_extensions(ctx.extensions())),
        other => Err(OagwError::new(
            ErrorKind::PluginNotFound,
            format!("unknown auth plugin `{other}`"),
        )
        .with_extensions(ctx.extensions())),
    }
}

/// Executes an `OAuth2` client-credentials plugin: resolves the client id and secret by
/// reference, exchanges them at the configured token endpoint and returns the bearer
/// header injection.
///
/// Tokens are served from the shared cache keyed by tenant, subject, client-auth method
/// and a digest of the binding's configuration (ADR-0008); a failed exchange is never
/// cached.
///
/// # Errors
///
/// Returns an authentication failure when the exchange fails.
pub async fn execute_oauth2(
    plugin: &crate::plugins::BoundPlugin,
    ctx: &PluginContext<'_>,
) -> Result<InjectedCredential, OagwError> {
    let method = auth_method(&plugin.name).ok_or_else(|| {
        OagwError::new(
            ErrorKind::PluginNotFound,
            format!("unknown auth plugin `{}`", plugin.name),
        )
        .with_extensions(ctx.extensions())
    })?;

    let key = format!(
        "{}:{}:{}:{}",
        ctx.security.security().subject_tenant_id(),
        ctx.security.security().subject_id(),
        method_tag(method),
        super::token_cache::hash_config(&plugin.config),
    );

    if let Some(token) = ctx.token_cache.get(&key) {
        return Ok(InjectedCredential::header(
            "authorization",
            Bytes::from(format!("Bearer {}", token.expose())),
        ));
    }

    let client_id = resolve_material(plugin, ctx, "client_id").await?;
    let client_secret = resolve_material(plugin, ctx, "client_secret").await?;

    let endpoint = plugin
        .config
        .get("token_endpoint")
        .and_then(Value::as_str)
        .or_else(|| ctx.config.get("token_endpoint").and_then(Value::as_str))
        .unwrap_or_default();
    let scopes: Vec<String> = plugin
        .config
        .get("scopes")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();

    let config = OAuthClientConfig {
        token_endpoint: Some(Url::parse(endpoint).map_err(|err| {
            OagwError::new(
                ErrorKind::ValidationError,
                format!("oauth2 token_endpoint is not a valid URL: {err}"),
            )
            .with_extensions(ctx.extensions())
        })?),
        issuer_url: None,
        client_id,
        client_secret: SecretString::new(client_secret),
        scopes,
        auth_method: method,
        extra_headers: Vec::new(),
        refresh_offset: std::time::Duration::from_secs(30),
        jitter_max: std::time::Duration::ZERO,
        min_refresh_period: std::time::Duration::from_secs(1),
        default_ttl: std::time::Duration::from_secs(
            ctx.config
                .get("default_ttl_secs")
                .and_then(Value::as_u64)
                .unwrap_or(300),
        ),
        http_config: None,
    };

    let fetched = fetch_token(config).await.map_err(|err| {
        // Never include the client credentials in the message.
        OagwError::new(
            ErrorKind::AuthenticationFailed,
            format!("oauth2 token exchange failed: {err}"),
        )
        .with_extensions(ctx.extensions())
    })?;

    ctx.token_cache
        .put(&key, SecretString::new(fetched.bearer.expose().to_owned()), fetched.expires_in);

    Ok(InjectedCredential::header(
        "authorization",
        Bytes::from(format!("Bearer {}", fetched.bearer.expose())),
    ))
}

/// Stable tag distinguishing the two client-auth methods in a cache key.
#[must_use]
fn method_tag(method: ClientAuthMethod) -> &'static str {
    match method {
        ClientAuthMethod::Basic => "basic",
        ClientAuthMethod::Form => "form",
    }
}

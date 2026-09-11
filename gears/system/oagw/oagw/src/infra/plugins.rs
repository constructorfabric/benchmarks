//! Built-in plugin registries and their implementations.
//!
//! Three plugin kinds run in a deterministic order: authentication injects
//! outbound credentials, guards may reject a request, and transforms mutate a
//! request or a response. Upstream-level bindings run before route-level ones.

use crate::domain::error::{DomainError, DomainResult, ErrorKind};
use crate::domain::model::gts_instance;
use credstore_sdk::CredStoreClientV1;
use credstore_sdk::models::SecretRef;
use http::HeaderMap;
use std::collections::BTreeMap;
use std::sync::Arc;
use toolkit_security::SecurityContext;

/// Resolvable authentication plugin identifiers.
pub const AUTH_NOOP: &str = "cf.core.oagw.noop.v1";
/// API key injection.
pub const AUTH_APIKEY: &str = "cf.core.oagw.apikey.v1";
/// `OAuth2` client credentials with a form-encoded token request.
pub const AUTH_OAUTH2_FORM: &str = "cf.core.oagw.oauth2_client_cred.v1";
/// `OAuth2` client credentials with a Basic-authenticated token request.
pub const AUTH_OAUTH2_BASIC: &str = "cf.core.oagw.oauth2_client_cred_basic.v1";

/// Catalogue-only authentication identifiers with no backing implementation.
pub const AUTH_CATALOG_ONLY: [&str; 2] = ["cf.core.oagw.basic.v1", "cf.core.oagw.bearer.v1"];

/// The only bindable guard identifier.
pub const GUARD_REQUIRED_HEADERS: &str = "cf.core.oagw.required_headers.v1";
/// Catalogue-only guard identifiers; these name core data-plane behaviour.
pub const GUARD_CATALOG_ONLY: [&str; 2] = ["cf.core.oagw.timeout.v1", "cf.core.oagw.cors.v1"];

/// The only resolvable transform identifier.
pub const TRANSFORM_REQUEST_ID: &str = "cf.core.oagw.request_id.v1";
/// Catalogue-only transform identifiers; these name core instrumentation.
pub const TRANSFORM_CATALOG_ONLY: [&str; 2] =
    ["cf.core.oagw.logging.v1", "cf.core.oagw.metrics.v1"];

/// Plugin configuration as stored on an upstream or a route.
pub type PluginConfig = BTreeMap<String, serde_json::Value>;

/// Read a configuration value as a string.
fn config_str<'a>(config: &'a PluginConfig, key: &str) -> Option<&'a str> {
    config.get(key).and_then(serde_json::Value::as_str)
}

// @cpt-begin:cpt-cf-oagw-dod-policy-auth-registry:p2:inst-authreg
/// Resolve and apply an authentication plugin to the outbound headers.
///
/// # Errors
/// Returns `PluginNotFound` when the identifier names no resolvable plugin,
/// `SecretNotFound` when a referenced secret is missing, and
/// `AuthenticationFailed` when a credential cannot be prepared.
pub async fn apply_auth_plugin(
    plugin_ref: &str,
    config: &PluginConfig,
    ctx: &SecurityContext,
    cred_store: Option<&Arc<dyn CredStoreClientV1>>,
    headers: &mut HeaderMap,
) -> DomainResult<()> {
    let instance = gts_instance(plugin_ref);
    if AUTH_CATALOG_ONLY.contains(&instance) {
        return Err(DomainError::new(
            ErrorKind::PluginNotFound,
            format!("unknown auth plugin `{instance}`"),
        ));
    }
    match instance {
        AUTH_NOOP => Ok(()),
        AUTH_APIKEY => apply_api_key(config, ctx, cred_store, headers).await,
        AUTH_OAUTH2_FORM | AUTH_OAUTH2_BASIC => {
            // The bearer value is prepared by the token cache and injected by
            // the caller; nothing is added here when no token was resolved.
            Ok(())
        }
        other => Err(DomainError::new(
            ErrorKind::PluginNotFound,
            format!("unknown auth plugin `{other}`"),
        )),
    }
}

/// Inject an API key into a header or a query parameter.
async fn apply_api_key(
    config: &PluginConfig,
    ctx: &SecurityContext,
    cred_store: Option<&Arc<dyn CredStoreClientV1>>,
    headers: &mut HeaderMap,
) -> DomainResult<()> {
    let header_name = config_str(config, "header").unwrap_or("authorization");
    let prefix = config_str(config, "prefix").unwrap_or("");
    let secret = resolve_secret(config, ctx, cred_store).await?;
    let value = if prefix.is_empty() {
        secret
    } else {
        format!("{prefix} {secret}")
    };
    let name = http::HeaderName::try_from(header_name).map_err(|_| {
        DomainError::validation(format!("auth header name `{header_name}` is not valid"))
    })?;
    let value = http::HeaderValue::from_str(&value).map_err(|_| {
        DomainError::new(
            ErrorKind::AuthenticationFailed,
            "credential is not a valid header value",
        )
    })?;
    headers.insert(name, value);
    Ok(())
}

/// Resolve the credential a plugin should inject.
///
/// A literal `value` is used when present, otherwise `secret_ref` is fetched
/// from the credential store. The secret material is never logged.
async fn resolve_secret(
    config: &PluginConfig,
    ctx: &SecurityContext,
    cred_store: Option<&Arc<dyn CredStoreClientV1>>,
) -> DomainResult<String> {
    if let Some(literal) = config_str(config, "value") {
        return Ok(literal.to_owned());
    }
    let Some(reference) = config_str(config, "secret_ref") else {
        return Err(DomainError::new(
            ErrorKind::AuthenticationFailed,
            "auth plugin config must carry `value` or `secret_ref`",
        ));
    };
    // A `cred://` prefix is the documented reference form.
    let reference = reference.strip_prefix("cred://").unwrap_or(reference);
    let Some(store) = cred_store else {
        return Err(DomainError::new(
            ErrorKind::SecretNotFound,
            "credential store is unavailable",
        ));
    };
    let key = SecretRef::new(reference)
        .map_err(|e| DomainError::validation(format!("invalid secret_ref: {e}")))?;
    match store.get(ctx, &key).await {
        Ok(Some(found)) => String::from_utf8(found.value.as_bytes().to_vec())
            .map_err(|_| DomainError::new(ErrorKind::SecretNotFound, "secret is not valid UTF-8")),
        Ok(None) => Err(DomainError::new(
            ErrorKind::SecretNotFound,
            format!("secret `{reference}` was not found"),
        )),
        Err(_) => Err(DomainError::new(
            ErrorKind::AuthenticationFailed,
            "credential store rejected the request",
        )),
    }
}
// @cpt-end:cpt-cf-oagw-dod-policy-auth-registry:p2:inst-authreg

// @cpt-begin:cpt-cf-oagw-dod-policy-required-headers-guard:p2:inst-guard
/// Which phase a guard rejection happened in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardPhase {
    /// Before the upstream call.
    Request,
    /// After the upstream responded.
    Response,
}

/// Error code the required-headers guard reports.
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// Parse a comma-separated header-name list into normalized names.
///
/// Entries are trimmed, lowercased, and empty entries are dropped. A list that
/// is blank after trimming yields no names, which makes the guard a no-op.
#[must_use]
pub fn parse_required_headers(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|part| part.trim().to_ascii_lowercase())
        .filter(|part| !part.is_empty())
        .collect()
}

/// Evaluate the required-headers guard for one phase.
///
/// Reports only the first missing header. A request-phase rejection is a `400`
/// and a response-phase rejection is a `502`.
///
/// # Errors
/// Returns a validation error naming the first missing header.
pub fn evaluate_required_headers(
    config: &PluginConfig,
    headers: &HeaderMap,
    phase: GuardPhase,
) -> DomainResult<()> {
    let key = match phase {
        GuardPhase::Request => "required_request_headers",
        GuardPhase::Response => "required_response_headers",
    };
    let Some(raw) = config_str(config, key) else {
        // Absent configuration is a no-op; the guard fails open.
        return Ok(());
    };
    for name in parse_required_headers(raw) {
        let present = headers
            .keys()
            .any(|k| k.as_str().eq_ignore_ascii_case(&name));
        if !present {
            let kind = match phase {
                GuardPhase::Request => ErrorKind::ValidationError,
                GuardPhase::Response => ErrorKind::ProtocolError,
            };
            return Err(
                DomainError::new(kind, format!("required header `{name}` is missing"))
                    .with_context(serde_json::json!({
                        "error_code": REQUIRED_HEADER_MISSING,
                        "header": name,
                    })),
            );
        }
    }
    Ok(())
}

/// Resolve and evaluate a guard plugin.
///
/// # Errors
/// Returns `PluginNotFound` for an identifier no registry resolves, or the
/// guard's own rejection.
pub fn apply_guard_plugin(
    plugin_ref: &str,
    config: &PluginConfig,
    headers: &HeaderMap,
    phase: GuardPhase,
) -> DomainResult<()> {
    let instance = gts_instance(plugin_ref);
    if instance == GUARD_REQUIRED_HEADERS {
        return evaluate_required_headers(config, headers, phase);
    }
    if GUARD_CATALOG_ONLY.contains(&instance) {
        return Err(DomainError::new(
            ErrorKind::PluginNotFound,
            format!("guard `{instance}` is catalogue-only and cannot be bound"),
        ));
    }
    Err(DomainError::new(
        ErrorKind::PluginNotFound,
        format!("unknown guard plugin `{instance}`"),
    ))
}
// @cpt-end:cpt-cf-oagw-dod-policy-required-headers-guard:p2:inst-guard

/// Apply a transform plugin to the outbound request headers.
///
/// # Errors
/// Returns `PluginNotFound` for an identifier no registry resolves.
pub fn apply_transform_plugin(plugin_ref: &str, headers: &mut HeaderMap) -> DomainResult<()> {
    let instance = gts_instance(plugin_ref);
    if instance == TRANSFORM_REQUEST_ID {
        if !headers.contains_key("x-request-id") {
            let id = uuid::Uuid::new_v4().to_string();
            if let Ok(value) = http::HeaderValue::from_str(&id) {
                headers.insert("x-request-id", value);
            }
        }
        return Ok(());
    }
    if TRANSFORM_CATALOG_ONLY.contains(&instance) {
        return Err(DomainError::new(
            ErrorKind::PluginNotFound,
            format!("transform `{instance}` is catalogue-only and cannot be bound"),
        ));
    }
    Err(DomainError::new(
        ErrorKind::PluginNotFound,
        format!("unknown transform plugin `{instance}`"),
    ))
}

/// Classify a plugin reference so the chain can dispatch it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginClass {
    /// An authentication plugin.
    Auth,
    /// A guard plugin.
    Guard,
    /// A transform plugin.
    Transform,
    /// A reference whose kind cannot be determined.
    Unknown,
}

/// Determine which registry a plugin reference belongs to.
#[must_use]
pub fn classify_plugin(plugin_ref: &str) -> PluginClass {
    if plugin_ref.contains("auth_plugin.v1") {
        PluginClass::Auth
    } else if plugin_ref.contains("guard_plugin.v1") {
        PluginClass::Guard
    } else if plugin_ref.contains("transform_plugin.v1") {
        PluginClass::Transform
    } else {
        PluginClass::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AUTH_CATALOG_ONLY, GUARD_REQUIRED_HEADERS, GuardPhase, PluginClass, PluginConfig,
        apply_guard_plugin, apply_transform_plugin, classify_plugin, evaluate_required_headers,
        parse_required_headers,
    };
    use http::HeaderMap;

    fn config(pairs: &[(&str, &str)]) -> PluginConfig {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), serde_json::Value::String((*v).to_owned())))
            .collect()
    }

    #[test]
    fn header_lists_are_split_trimmed_lowercased_and_compacted() {
        assert_eq!(
            parse_required_headers(" X-A , ,x-b ,, "),
            vec!["x-a".to_owned(), "x-b".to_owned()]
        );
    }

    #[test]
    fn a_blank_list_makes_the_guard_a_no_op() {
        let cfg = config(&[("required_request_headers", " , , ")]);
        let headers = HeaderMap::new();
        assert!(evaluate_required_headers(&cfg, &headers, GuardPhase::Request).is_ok());
    }

    #[test]
    fn absent_configuration_makes_the_guard_a_no_op() {
        let cfg = PluginConfig::new();
        let headers = HeaderMap::new();
        assert!(evaluate_required_headers(&cfg, &headers, GuardPhase::Request).is_ok());
        assert!(evaluate_required_headers(&cfg, &headers, GuardPhase::Response).is_ok());
    }

    #[test]
    fn a_missing_request_header_is_four_hundred() {
        let cfg = config(&[("required_request_headers", "x-needed")]);
        let err = evaluate_required_headers(&cfg, &HeaderMap::new(), GuardPhase::Request)
            .expect_err("guard rejects");
        assert_eq!(err.status(), 400);
        assert_eq!(err.context["error_code"], "REQUIRED_HEADER_MISSING");
    }

    #[test]
    fn a_missing_response_header_is_five_hundred_and_two() {
        let cfg = config(&[("required_response_headers", "x-needed")]);
        let err = evaluate_required_headers(&cfg, &HeaderMap::new(), GuardPhase::Response)
            .expect_err("guard rejects");
        assert_eq!(err.status(), 502);
        assert_eq!(err.context["error_code"], "REQUIRED_HEADER_MISSING");
    }

    #[test]
    fn header_matching_is_case_insensitive() {
        let cfg = config(&[("required_request_headers", "X-Needed")]);
        let mut headers = HeaderMap::new();
        headers.insert("x-needed", http::HeaderValue::from_static("1"));
        assert!(evaluate_required_headers(&cfg, &headers, GuardPhase::Request).is_ok());
    }

    #[test]
    fn only_the_first_missing_header_is_reported() {
        let cfg = config(&[("required_request_headers", "x-first,x-second")]);
        let err = evaluate_required_headers(&cfg, &HeaderMap::new(), GuardPhase::Request)
            .expect_err("guard rejects");
        assert_eq!(err.context["header"], "x-first");
    }

    #[test]
    fn catalogue_only_guards_are_not_bindable() {
        let err = apply_guard_plugin(
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
            &PluginConfig::new(),
            &HeaderMap::new(),
            GuardPhase::Request,
        )
        .expect_err("catalogue-only guard is rejected");
        assert_eq!(err.status(), 503);
    }

    #[test]
    fn the_required_headers_guard_is_bindable() {
        let plugin_ref = format!("gts.cf.core.oagw.guard_plugin.v1~{GUARD_REQUIRED_HEADERS}");
        assert!(
            apply_guard_plugin(
                &plugin_ref,
                &PluginConfig::new(),
                &HeaderMap::new(),
                GuardPhase::Request
            )
            .is_ok()
        );
    }

    #[test]
    fn catalogue_only_auth_identifiers_are_listed() {
        assert!(AUTH_CATALOG_ONLY.contains(&"cf.core.oagw.basic.v1"));
        assert!(AUTH_CATALOG_ONLY.contains(&"cf.core.oagw.bearer.v1"));
    }

    #[test]
    fn the_request_id_transform_injects_a_header() {
        let mut headers = HeaderMap::new();
        apply_transform_plugin(
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1",
            &mut headers,
        )
        .expect("transform applies");
        assert!(headers.contains_key("x-request-id"));
    }

    #[test]
    fn catalogue_only_transforms_are_not_bindable() {
        let err = apply_transform_plugin(
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
            &mut HeaderMap::new(),
        )
        .expect_err("catalogue-only transform is rejected");
        assert_eq!(err.status(), 503);
    }

    #[test]
    fn plugin_references_are_classified_by_their_base() {
        assert_eq!(
            classify_plugin("gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"),
            PluginClass::Auth
        );
        assert_eq!(
            classify_plugin("gts.cf.core.oagw.guard_plugin.v1~x"),
            PluginClass::Guard
        );
        assert_eq!(
            classify_plugin("gts.cf.core.oagw.transform_plugin.v1~x"),
            PluginClass::Transform
        );
        assert_eq!(classify_plugin("nonsense"), PluginClass::Unknown);
    }
}

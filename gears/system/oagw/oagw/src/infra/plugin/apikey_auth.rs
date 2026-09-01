// Created: 2026-08-31 by Constructor Tech
//! `ApiKeyAuthPlugin` (ADR-0002 "Built-in Plugins", DESIGN §3.2 "Secret Access
//! Control").
//!
//! Static API-key injection: the binding names a `cred://` reference, the data
//! plane resolves it as the caller at request time and writes it into the
//! outbound request — as a header (default `x-api-key`) or as a query
//! parameter, the two being mutually exclusive.
//!
//! # Residual plaintext
//!
//! As in ADR-0008 ("Known Residual Plaintext"), the rendered credential value
//! is a plain `String` for the few instructions it takes to become a header
//! value. The credential never reaches a log line, a problem document or a
//! `Debug` impl; the store's zeroized buffer is released when the request ends.

use std::fmt;

use async_trait::async_trait;
use http::HeaderValue;

use crate::error::{OagwError, OagwErrorKind};
use crate::infra::plugin::secrets::{CredStore, resolve_secret};
use crate::infra::plugin::traits::{AuthPlugin, PluginConfig, RequestContext};

/// GTS id of the built-in API-key auth plugin.
pub const APIKEY_AUTH_PLUGIN_ID: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";

/// Header the credential is written into when the binding names none.
pub const DEFAULT_HEADER_NAME: &str = "x-api-key";

/// Configuration members of the plugin.
const KEY_REF: &str = "key_ref";
const HEADER_NAME: &str = "header_name";
const QUERY_PARAM: &str = "query_param";
const PREFIX: &str = "prefix";

/// Injects a static API key resolved from the credential store.
pub struct ApiKeyAuthPlugin {
    credstore: CredStore,
}

/// `Debug` names no field: the plugin holds a credential store handle, and a
/// panic message must never carry it (PRD `cpt-cf-oagw-fr-auth-injection`).
impl fmt::Debug for ApiKeyAuthPlugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiKeyAuthPlugin").finish()
    }
}

impl ApiKeyAuthPlugin {
    /// Plugin over `credstore`.
    #[must_use]
    pub fn new(credstore: CredStore) -> Self {
        Self { credstore }
    }
}

#[async_trait]
impl AuthPlugin for ApiKeyAuthPlugin {
    fn id(&self) -> &'static str {
        "apikey"
    }

    fn plugin_type(&self) -> &'static str {
        APIKEY_AUTH_PLUGIN_ID
    }

    async fn authenticate(&self, ctx: &mut RequestContext) -> Result<(), OagwError> {
        let target = Target::of(&ctx.config)?;
        let key = resolve_secret(&self.credstore, &ctx.security, &target.key_ref).await?;
        target.inject(ctx, key.expose())
    }
}

/// Where the credential goes.
#[derive(Debug)]
struct Target {
    key_ref: String,
    kind: Kind,
    prefix: String,
}

/// Header or query-parameter injection; the two are mutually exclusive.
#[derive(Debug)]
enum Kind {
    /// Write the credential into this header.
    Header(String),
    /// Append the credential as this query parameter.
    Query(String),
}

impl Target {
    /// Read the binding, rejecting an incomplete or contradictory one.
    ///
    /// A binding the write path would have rejected (the control plane
    /// validates every auth binding) is still refused here: a record may have
    /// been seeded directly into the store, and silently forwarding a request
    /// without its credential is the one failure the gateway must never make.
    fn of(config: &PluginConfig) -> Result<Target, OagwError> {
        let key_ref = config
            .string(KEY_REF)
            .filter(|member| !member.trim().is_empty())
            .ok_or_else(missing_key_ref)?
            .to_owned();
        let header = config.string(HEADER_NAME);
        let query = config.string(QUERY_PARAM);
        let kind = match (header, query) {
            (Some(_), Some(_)) => {
                return Err(OagwError::validation(format!(
                    "auth binding '{APIKEY_AUTH_PLUGIN_ID}' accepts only one of \
                     '{HEADER_NAME}' or '{QUERY_PARAM}'"
                )));
            }
            (Some(name), None) => Kind::Header(name.to_owned()),
            (None, Some(name)) => Kind::Query(name.to_owned()),
            (None, None) => Kind::Header(DEFAULT_HEADER_NAME.to_owned()),
        };
        Ok(Self {
            key_ref,
            kind,
            prefix: config.string(PREFIX).unwrap_or_default().to_owned(),
        })
    }

    /// Write the credential into the outbound request.
    fn inject(&self, ctx: &mut RequestContext, key: &str) -> Result<(), OagwError> {
        let value = format!("{}{key}", self.prefix);
        match &self.kind {
            Kind::Header(name) => {
                let header = http::HeaderName::try_from(name.as_str())
                    .map_err(|error| unusable_member(HEADER_NAME, name, &error))?;
                let header_value = HeaderValue::from_str(&value)
                    .map_err(|_| unusable_credential(&self.key_ref))?;
                ctx.headers.insert(header, header_value);
            }
            Kind::Query(name) => ctx.query = append_query_parameter(&ctx.query, name, &value),
        }
        Ok(())
    }
}

/// 400 `validation.error.v1`: the binding names no credential reference.
fn missing_key_ref() -> OagwError {
    OagwError::validation(format!(
        "auth binding '{APIKEY_AUTH_PLUGIN_ID}' requires the '{KEY_REF}' cred:// reference"
    ))
}

/// 400 for a member the outbound request cannot carry.
fn unusable_member(member: &str, value: &str, error: &impl std::fmt::Display) -> OagwError {
    OagwError::validation(format!(
        "auth binding member '{member}' carries an unusable value '{value}': {error}"
    ))
}

/// 500 for a resolved credential that cannot become a header value.
fn unusable_credential(reference: &str) -> OagwError {
    OagwError::new(
        OagwErrorKind::Internal,
        format!("referenced secret '{reference}' cannot be sent as a header value"),
    )
}

/// Append `name=value` to a query string, keeping the existing parameters.
fn append_query_parameter(query: &str, name: &str, value: &str) -> String {
    let mut rendered = form_urlencoded::Serializer::new(String::new());
    rendered.extend_pairs(form_urlencoded::parse(query.as_bytes()));
    rendered.append_pair(name, value);
    rendered.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(raw: &serde_json::Value) -> PluginConfig {
        PluginConfig::new(
            raw.as_object()
                .cloned()
                .unwrap_or_else(serde_json::Map::new),
        )
    }

    #[test]
    fn the_plugin_is_the_documented_built_in() {
        let plugin = ApiKeyAuthPlugin::new(test_credstore());
        assert_eq!(plugin.id(), "apikey");
        assert_eq!(plugin.plugin_type(), APIKEY_AUTH_PLUGIN_ID);
        assert_eq!(
            plugin.plugin_type(),
            crate::domain::plugin::PluginKind::Auth.built_in_id("apikey")
        );
    }

    /// A store no test of this module reads; the resolution paths are covered
    /// by the data-plane integration tests.
    fn test_credstore() -> CredStore {
        std::sync::Arc::new(crate::infra::plugin::secrets::stub::StubCredStore(
            crate::infra::plugin::secrets::stub::Behaviour::Empty,
        ))
    }

    #[test]
    fn a_binding_without_a_reference_is_rejected() {
        let error = Target::of(&config(&json!({}))).unwrap_err();
        assert_eq!(*error.kind(), OagwErrorKind::Validation);
    }

    #[test]
    fn a_blank_reference_is_rejected() {
        let error = Target::of(&config(&json!({ "key_ref": "  " }))).unwrap_err();
        assert_eq!(*error.kind(), OagwErrorKind::Validation);
    }

    #[test]
    fn a_header_and_a_query_parameter_are_exclusive() {
        let error = Target::of(&config(&json!({
            "key_ref": "cred://partner-key",
            "header_name": "x-key",
            "query_param": "api_key"
        })))
        .unwrap_err();
        assert_eq!(*error.kind(), OagwErrorKind::Validation);
    }

    #[test]
    fn a_binding_without_a_member_defaults_to_the_documented_header() {
        let target = Target::of(&config(&json!({ "key_ref": "cred://partner-key" }))).unwrap();
        match target.kind {
            Kind::Header(name) => assert_eq!(name, DEFAULT_HEADER_NAME),
            Kind::Query(_) => panic!("the default target must be a header"),
        }
    }

    #[test]
    fn a_prefix_is_taken_verbatim() {
        let target = Target::of(&config(&json!({
            "key_ref": "cred://partner-key",
            "prefix": "Bearer "
        })))
        .unwrap();
        assert_eq!(target.prefix, "Bearer ");
    }

    #[test]
    fn a_query_parameter_is_appended_to_the_existing_query() {
        let rendered = append_query_parameter("a=1&b=2", "api_key", "s3cr3t");
        assert_eq!(rendered, "a=1&b=2&api_key=s3cr3t");
    }

    #[test]
    fn an_empty_query_gains_only_the_credential() {
        let rendered = append_query_parameter("", "api_key", "s3cr3t");
        assert_eq!(rendered, "api_key=s3cr3t");
    }

    #[test]
    fn a_query_value_is_percent_encoded() {
        // `form_urlencoded` renders in the `application/x-www-form-urlencoded`
        // form, where a space is a `+`.
        let rendered = append_query_parameter("", "api_key", "a b/c");
        assert_eq!(rendered, "api_key=a+b%2Fc");
    }
}

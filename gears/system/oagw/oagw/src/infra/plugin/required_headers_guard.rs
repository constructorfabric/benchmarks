//! The required-headers guard plugin (ADR-0009).
//!
//! A stateless guard that checks for the *presence* of configured header names
//! on the request and/or on the upstream's response, and rejects with a
//! phase-specific status on the first missing one:
//!
//! | Phase | Status | `error_code` |
//! |---|---|---|
//! | `guard_request` | 400 | `REQUIRED_HEADER_MISSING` |
//! | `guard_response` | 502 | `REQUIRED_HEADER_MISSING` |
//!
//! The configuration is fail-open by decision of ADR-0009: an absent or blank
//! key means the phase is a no-op, so registering the plugin in the process
//! changes nothing for an upstream that did not configure it. Header names are
//! matched case-insensitively and only for presence, never for value.
//!
//! Only the first missing header is reported (ADR-0009 "Consequences": a
//! deliberate trade against a round-trip per missing header).

use http::HeaderMap;

use super::registry::REQUIRED_HEADERS_GUARD_PLUGIN_REF;
use super::traits::{GuardPlugin, PluginContext};
use crate::error::OagwError;

/// The machine-readable code a rejection carries (ADR-0009 "Decision Flow").
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// The configuration key of the request phase.
const REQUEST_KEY: &str = "required_request_headers";
/// The configuration key of the response phase.
const RESPONSE_KEY: &str = "required_response_headers";

/// Checks the presence of configured headers on the request and the response.
#[derive(Debug, Default)]
pub struct RequiredHeadersGuardPlugin;

impl RequiredHeadersGuardPlugin {
    /// The required names of `key`, as ADR-0009 parses them: split on commas,
    /// trimmed, lowercased, empty entries dropped.
    ///
    /// `None` when the key is absent or every entry is blank: the phase is then
    /// a no-op rather than an error.
    fn required(context: &PluginContext<'_>, key: &str) -> Option<Vec<String>> {
        let raw = context.string(key)?;
        let names: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(|name| name.to_ascii_lowercase())
            .collect();

        (!names.is_empty()).then_some(names)
    }

    /// The first name of `required` that is missing from `headers`.
    fn first_missing(required: &[String], headers: &HeaderMap) -> Option<String> {
        required
            .iter()
            .find(|name| !headers.contains_key(name.as_str()))
            .cloned()
    }

    /// The rejection of the request phase (ADR-0009: 400).
    fn request_rejection(header: String) -> OagwError {
        OagwError::validation(format!("required header '{header}' is missing"))
            .with_extension("error_code", serde_json::json!(REQUIRED_HEADER_MISSING))
            .with_extension("header", serde_json::json!(header))
    }

    /// The rejection of the response phase (ADR-0009: 502).
    fn response_rejection(header: String) -> OagwError {
        OagwError::downstream_error(format!(
            "the upstream response is missing the required header '{header}'"
        ))
        .with_extension("error_code", serde_json::json!(REQUIRED_HEADER_MISSING))
        .with_extension("header", serde_json::json!(header))
    }
}

#[async_trait::async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn plugin_ref(&self) -> &str {
        REQUIRED_HEADERS_GUARD_PLUGIN_REF
    }

    async fn guard_request(
        &self,
        context: &PluginContext<'_>,
        headers: &HeaderMap,
    ) -> Result<(), OagwError> {
        let Some(required) = Self::required(context, REQUEST_KEY) else {
            return Ok(());
        };

        match Self::first_missing(&required, headers) {
            Some(header) => Err(Self::request_rejection(header)),
            None => Ok(()),
        }
    }

    async fn guard_response(
        &self,
        context: &PluginContext<'_>,
        headers: &mut HeaderMap,
    ) -> Result<(), OagwError> {
        let Some(required) = Self::required(context, RESPONSE_KEY) else {
            return Ok(());
        };

        match Self::first_missing(&required, headers) {
            Some(header) => Err(Self::response_rejection(header)),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::plugin::test_context;
    use serde_json::json;

    #[tokio::test]
    async fn an_unconfigured_phase_is_a_no_op() {
        let plugin = RequiredHeadersGuardPlugin;
        let mut response = HeaderMap::new();

        plugin
            .guard_request(
                &PluginContext {
                    config: None,
                    request: &test_context(),
                },
                &HeaderMap::new(),
            )
            .await
            .expect("fail-open without configuration");
        plugin
            .guard_response(
                &PluginContext {
                    config: None,
                    request: &test_context(),
                },
                &mut response,
            )
            .await
            .expect("fail-open without configuration");

        assert!(
            response.is_empty(),
            "the guard transforms nothing on either phase"
        );
    }

    #[tokio::test]
    async fn a_blank_list_is_a_no_op() {
        // ADR-0009 "Risks": a malformed list silently no-ops rather than
        // erroring; this is the documented behaviour.
        let config = json!({"required_request_headers": " , , "});
        let plugin = RequiredHeadersGuardPlugin;

        plugin
            .guard_request(
                &PluginContext {
                    config: Some(&config),
                    request: &test_context(),
                },
                &HeaderMap::new(),
            )
            .await
            .expect("a blank list enforces nothing");
    }

    #[tokio::test]
    async fn a_missing_request_header_is_a_400() {
        let config = json!({"required_request_headers": "X-Correlation-Id, accept"});
        let mut inbound = HeaderMap::new();
        inbound.insert("accept", "application/json".parse().expect("valid"));

        let error = RequiredHeadersGuardPlugin
            .guard_request(
                &PluginContext {
                    config: Some(&config),
                    request: &test_context(),
                },
                &inbound,
            )
            .await
            .expect_err("x-correlation-id is missing");

        assert_eq!(error.status().as_u16(), 400);
        assert_eq!(
            error
                .extensions()
                .get("error_code")
                .and_then(|value| value.as_str()),
            Some(REQUIRED_HEADER_MISSING)
        );
        assert_eq!(
            error.extensions().get("header").and_then(|v| v.as_str()),
            Some("x-correlation-id"),
            "only the first missing header is reported"
        );
    }

    #[tokio::test]
    async fn a_present_request_header_passes() {
        let config = json!({"required_request_headers": "x-correlation-id"});
        let mut inbound = HeaderMap::new();
        inbound.insert("X-Correlation-Id", "01J".parse().expect("valid"));

        RequiredHeadersGuardPlugin
            .guard_request(
                &PluginContext {
                    config: Some(&config),
                    request: &test_context(),
                },
                &inbound,
            )
            .await
            .expect("presence is matched case-insensitively");
    }

    #[tokio::test]
    async fn a_missing_response_header_is_a_502() {
        let config = json!({"required_response_headers": "content-type"});
        let mut response = HeaderMap::new();
        response.insert("x-request-id", "01J".parse().expect("valid"));

        let error = RequiredHeadersGuardPlugin
            .guard_response(
                &PluginContext {
                    config: Some(&config),
                    request: &test_context(),
                },
                &mut response,
            )
            .await
            .expect_err("content-type is missing");

        assert_eq!(error.status().as_u16(), 502);
        assert_eq!(
            error.extensions().get("header").and_then(|v| v.as_str()),
            Some("content-type")
        );
    }

    #[tokio::test]
    async fn the_two_phases_are_independent() {
        let config = json!({"required_response_headers": "content-type"});
        let mut response = HeaderMap::new();
        response.insert("content-type", "application/json".parse().expect("valid"));

        RequiredHeadersGuardPlugin
            .guard_response(
                &PluginContext {
                    config: Some(&config),
                    request: &test_context(),
                },
                &mut response,
            )
            .await
            .expect("only the response phase is configured");
    }

    #[test]
    fn the_plugin_is_registered_under_the_required_headers_identifier() {
        assert_eq!(
            RequiredHeadersGuardPlugin.plugin_ref(),
            REQUIRED_HEADERS_GUARD_PLUGIN_REF
        );
    }
}

// Created: 2026-09-01 by Constructor Tech
//! The required-headers guard plugin.
//!
//! `docs/ADR/0009-required-headers-guard-plugin.md`. Stateless, fail-open
//! when unconfigured, and symmetric across the request and response
//! phases. Only header *presence* is checked, never values, and only the
//! first missing header is reported per rejection.

use crate::domain::errors::OagwError;
use crate::domain::model::builtin_plugins;
use crate::infra::context::{PluginRequest, PluginResponse};
use crate::infra::plugin::traits::GuardPlugin;

/// Config keys read from the binding's `config` block.
pub mod keys {
    /// Comma-separated header names required on the request.
    pub const REQUIRED_REQUEST_HEADERS: &str = "required_request_headers";
    /// Comma-separated header names required on the response.
    pub const REQUIRED_RESPONSE_HEADERS: &str = "required_response_headers";
}

/// Status returned when a required request header is absent.
pub const REQUEST_REJECT_STATUS: u16 = 400;

/// Status returned when a required response header is missing.
pub const RESPONSE_REJECT_STATUS: u16 = 502;

/// The error code carried in the rejection detail.
pub const ERROR_CODE: &str = "REQUIRED_HEADER_MISSING";

/// Checks that configured headers are present.
#[derive(Debug, Default)]
pub struct RequiredHeadersGuardPlugin;

/// Parse a comma-separated header list: trim, lowercase, drop empties.
///
/// `", , ,"` therefore yields an empty set and the phase no-ops.
#[must_use]
pub fn parse_header_list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

fn missing<'a>(
    headers: impl Iterator<Item = &'a str> + Clone,
    required: &[String],
) -> Option<String> {
    for name in required {
        if !headers.clone().any(|h| h.eq_ignore_ascii_case(name)) {
            return Some(name.clone());
        }
    }
    None
}

impl RequiredHeadersGuardPlugin {
    fn required(&self, request: &PluginRequest, key: &str) -> Vec<String> {
        request
            .config_str(key)
            .map(|v| parse_header_list(&v))
            .unwrap_or_default()
    }

    fn required_response(&self, response: &PluginResponse, key: &str) -> Vec<String> {
        response
            .config_str(key)
            .map(|v| parse_header_list(&v))
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        builtin_plugins::GUARD_REQUIRED_HEADERS
    }

    async fn guard_request(&self, request: &mut PluginRequest) -> Result<(), OagwError> {
        let required = self.required(request, keys::REQUIRED_REQUEST_HEADERS);
        if required.is_empty() {
            return Ok(());
        }
        let present: Vec<&str> = request.headers.iter().map(|(n, _)| n.as_str()).collect();
        if let Some(name) = missing(present.into_iter(), &required) {
            return Err(OagwError::validation_error(format!(
                "{ERROR_CODE}: required request header '{name}' is missing"
            )));
        }
        Ok(())
    }

    async fn guard_response(&self, response: &mut PluginResponse) -> Result<(), OagwError> {
        let required = self.required_response(response, keys::REQUIRED_RESPONSE_HEADERS);
        if required.is_empty() {
            return Ok(());
        }
        if let Some(name) = missing(response.headers.iter().map(|(n, _)| n.as_str()), &required) {
            return Err(OagwError::protocol_error(format!(
                "{ERROR_CODE}: required response header '{name}' is missing"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn request(headers: &[(&str, &str)], required: &str) -> PluginRequest {
        let mut config = BTreeMap::new();
        config.insert(
            keys::REQUIRED_REQUEST_HEADERS.to_owned(),
            serde_json::json!(required),
        );
        PluginRequest {
            method: "GET".to_owned(),
            path: "/".to_owned(),
            headers: headers
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect(),
            body: Vec::new(),
            target: crate::domain::model::Target {
                host: "h".to_owned(),
                port: 443,
                secure: true,
            },
            alias: "h".to_owned(),
            upstream_id: "u".to_owned(),
            route_id: None,
            tenant_id: "t".to_owned(),
            subject: None,
            request_id: "r".to_owned(),
            content_type: None,
            auth_config: BTreeMap::new(),
            plugin_config: config,
            security: toolkit_security::SecurityContext::anonymous(),
        }
    }

    fn response(headers: &[(&str, &str)], required: &str) -> PluginResponse {
        let mut config = BTreeMap::new();
        config.insert(
            keys::REQUIRED_RESPONSE_HEADERS.to_owned(),
            serde_json::json!(required),
        );
        PluginResponse {
            status: 200,
            headers: headers
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect(),
            plugin_config: config,
        }
    }

    #[tokio::test]
    async fn a_blank_config_is_a_no_op() {
        for blank in ["", ",", ", , ,"] {
            let mut r = request(&[], blank);
            RequiredHeadersGuardPlugin
                .guard_request(&mut r)
                .await
                .expect("fail-open");
        }
    }

    #[tokio::test]
    async fn a_present_header_passes() {
        let mut r = request(&[("X-Correlation-Id", "abc")], "x-correlation-id");
        RequiredHeadersGuardPlugin
            .guard_request(&mut r)
            .await
            .expect("present");
    }

    #[tokio::test]
    async fn the_first_missing_header_is_reported() {
        let mut r = request(&[("accept", "application/json")], "x-correlation-id,accept");
        let err = RequiredHeadersGuardPlugin
            .guard_request(&mut r)
            .await
            .unwrap_err();
        assert_eq!(err.status_value(), REQUEST_REJECT_STATUS);
        assert!(err.detail().contains(ERROR_CODE), "{}", err.detail());
        assert!(
            err.detail().contains("x-correlation-id"),
            "{}",
            err.detail()
        );
    }

    #[tokio::test]
    async fn a_missing_response_header_rejects_with_502() {
        let mut r = response(&[("content-length", "3")], "content-type");
        let err = RequiredHeadersGuardPlugin
            .guard_response(&mut r)
            .await
            .unwrap_err();
        assert_eq!(err.status_value(), RESPONSE_REJECT_STATUS);
        assert!(err.detail().contains("content-type"), "{}", err.detail());
    }

    #[tokio::test]
    async fn a_present_response_header_passes() {
        let mut r = response(&[("Content-Type", "application/json")], "content-type");
        RequiredHeadersGuardPlugin
            .guard_response(&mut r)
            .await
            .expect("present");
    }

    #[test]
    fn parsing_trims_and_lowercases() {
        assert_eq!(
            parse_header_list(" X-A , x-b,, "),
            vec!["x-a".to_owned(), "x-b".to_owned()]
        );
        assert!(parse_header_list(" , ").is_empty());
    }
}

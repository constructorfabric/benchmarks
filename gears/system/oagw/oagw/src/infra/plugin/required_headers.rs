//! Built-in required-headers guard plugin
//! (`gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1`).
//!
//! Stateless presence check on the request (before proxying) and on the
//! upstream response (before returning to the caller), per ADR-0009:
//!
//! * config keys `required_request_headers` / `required_response_headers`,
//!   comma-separated, trimmed, lower-cased, empty entries dropped;
//! * an absent or blank list makes the phase a no-op (fail-open, so binding
//!   the plugin changes nothing for an upstream that does not opt in);
//! * a request-phase rejection is `400`, a response-phase rejection `502`;
//! * the error code is always `REQUIRED_HEADER_MISSING`;
//! * only the **first** missing header is reported per rejection;
//! * only presence is checked, never the value.

use async_trait::async_trait;
use axum::http::StatusCode;

use crate::domain::error::OagwError;
use crate::domain::plugin::{
    GUARD_PLUGIN_TYPE_ID, GuardDecision, GuardPlugin, RequestContext, ResponseContext, builtin,
};
use crate::infra::plugin::parse_header_names;

/// Stable error code of every rejection of this plugin (ADR-0009).
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// Status of a request-phase rejection.
pub const REQUEST_REJECT_STATUS: u16 = 400;

/// Status of a response-phase rejection.
pub const RESPONSE_REJECT_STATUS: u16 = 502;

/// Required header enforcement plugin.
#[derive(Debug, Default, Clone)]
pub struct RequiredHeadersGuardPlugin {
    required_request: Vec<String>,
    required_response: Vec<String>,
}

impl RequiredHeadersGuardPlugin {
    /// Registry key of this plugin.
    pub const PLUGIN_ID: &'static str = builtin::REQUIRED_HEADERS_GUARD;

    /// GTS base type of this plugin.
    pub const PLUGIN_TYPE: &'static str = GUARD_PLUGIN_TYPE_ID;

    /// Builds the plugin from a binding configuration payload.
    ///
    /// Unparseable or blank payloads degrade to a no-op plugin (fail-open),
    /// matching the ADR-0009 behaviour for an unconfigured phase rather than
    /// failing the request.
    #[must_use]
    pub fn new(config: &serde_json::Value) -> Self {
        Self {
            required_request: required_list(config, "required_request_headers"),
            required_response: required_list(config, "required_response_headers"),
        }
    }

    /// Request headers this plugin requires, for diagnostics and tests.
    #[must_use]
    pub fn required_request_headers(&self) -> &[String] {
        &self.required_request
    }

    /// Response headers this plugin requires, for diagnostics and tests.
    #[must_use]
    pub fn required_response_headers(&self) -> &[String] {
        &self.required_response
    }

    fn first_missing<'a>(
        &self,
        required: &'a [String],
        present: &dyn Fn(&str) -> bool,
    ) -> Option<&'a str> {
        required
            .iter()
            .find(|name| !present(name.as_str()))
            .map(String::as_str)
    }
}

fn required_list(config: &serde_json::Value, key: &str) -> Vec<String> {
    let Some(raw) = config.get(key).and_then(serde_json::Value::as_str) else {
        return Vec::new();
    };
    parse_header_names(raw)
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        builtin::REQUIRED_HEADERS_GUARD
    }

    fn plugin_type(&self) -> &str {
        GUARD_PLUGIN_TYPE_ID
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, OagwError> {
        if self.required_request.is_empty() {
            return Ok(GuardDecision::allow());
        }
        match self.first_missing(&self.required_request, &|name| ctx.has_header(name)) {
            Some(missing) => Ok(GuardDecision::reject(
                StatusCode::from_u16(REQUEST_REJECT_STATUS).unwrap_or(StatusCode::BAD_REQUEST),
                REQUIRED_HEADER_MISSING,
                format!("required request header '{missing}' is missing"),
            )),
            None => Ok(GuardDecision::allow()),
        }
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, OagwError> {
        if self.required_response.is_empty() {
            return Ok(GuardDecision::allow());
        }
        match self.first_missing(&self.required_response, &|name| ctx.has_header(name)) {
            Some(missing) => Ok(GuardDecision::reject(
                StatusCode::from_u16(RESPONSE_REJECT_STATUS).unwrap_or(StatusCode::BAD_GATEWAY),
                REQUIRED_HEADER_MISSING,
                format!("required response header '{missing}' is missing"),
            )),
            None => Ok(GuardDecision::allow()),
        }
    }
}

#[cfg(test)]
#[path = "required_headers_tests.rs"]
mod tests;

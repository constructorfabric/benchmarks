//! `cf.core.oagw.required_headers.v1` — presence enforcement for named
//! headers on the request and/or the upstream response (ADR 0009).

use async_trait::async_trait;
use http::{HeaderMap, StatusCode};

use crate::domain::gts;
use crate::domain::plugin::{
    GuardDecision, GuardPlugin, PluginError, RequestContext, ResponseContext, config_str,
};

/// Error code reported on either phase.
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// Stateless presence check. Fail-open when unconfigured: adding the plugin to
/// the registry changes nothing for upstreams that do not opt in.
#[derive(Debug, Default, Clone, Copy)]
pub struct RequiredHeadersGuardPlugin;

/// Split a comma-separated config value into lowercase header names, dropping
/// blanks. An all-blank list yields nothing, which is a no-op phase.
fn parse_names(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect()
}

/// First configured name absent from `headers`, if any.
fn first_missing(headers: &HeaderMap, names: &[String]) -> Option<String> {
    names
        .iter()
        .find(|name| !headers.keys().any(|key| key.as_str() == name.as_str()))
        .cloned()
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        "required_headers"
    }

    fn plugin_type(&self) -> &str {
        gts::REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    async fn guard_request(
        &self,
        ctx: &RequestContext<'_>,
    ) -> Result<GuardDecision, PluginError> {
        let Some(raw) = config_str(ctx.config, "required_request_headers") else {
            return Ok(GuardDecision::Allow);
        };
        let names = parse_names(&raw);
        match first_missing(&ctx.request.headers, &names) {
            Some(missing) => Ok(GuardDecision::reject(
                StatusCode::BAD_REQUEST,
                REQUIRED_HEADER_MISSING,
                format!("required request header '{missing}' is missing"),
            )),
            None => Ok(GuardDecision::Allow),
        }
    }

    async fn guard_response(
        &self,
        ctx: &ResponseContext<'_>,
    ) -> Result<GuardDecision, PluginError> {
        let Some(raw) = config_str(ctx.config, "required_response_headers") else {
            return Ok(GuardDecision::Allow);
        };
        let names = parse_names(&raw);
        match first_missing(&ctx.response.headers, &names) {
            Some(missing) => Ok(GuardDecision::reject(
                StatusCode::BAD_GATEWAY,
                REQUIRED_HEADER_MISSING,
                format!("required response header '{missing}' is missing"),
            )),
            None => Ok(GuardDecision::Allow),
        }
    }
}

#[cfg(test)]
#[path = "required_headers_guard_tests.rs"]
mod tests;

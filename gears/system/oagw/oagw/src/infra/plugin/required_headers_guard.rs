//! The built-in `required_headers` guard plugin (ADR 0009,
//! `cpt-cf-oagw-algo-plugin-system-required-headers-decision`).
//!
//! `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1` is the
//! only guard plugin `GuardPluginRegistry::with_builtins()` registers. It is
//! stateless — no cache, no security-sensitive material — and enforces
//! **presence-only** header checks from the two independent configuration keys
//! `required_request_headers` and `required_response_headers`. Constraint 7 of
//! the DECOMPOSITION entry: header *values* are never validated.
//!
//! Rejection statuses: `400` in the request phase, `502` in the response
//! phase, both reporting only the first missing header name.

use async_trait::async_trait;

use crate::domain::plugin::{
    GuardDecision, GuardPlugin, PluginError, RequestContext, ResponseContext,
};

/// `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1`
pub const REQUIRED_HEADERS_PLUGIN_TYPE: &str =
    crate::domain::gts_helpers::REQUIRED_HEADERS_GUARD_PLUGIN_ID;

/// The request-phase reject status (ADR 0009).
pub const REQUEST_REJECT_STATUS: u16 = 400;
/// The response-phase reject status (ADR 0009).
pub const RESPONSE_REJECT_STATUS: u16 = 502;
/// The machine-readable reason both phases carry.
pub const REQUIRED_HEADER_MISSING: &str = "REQUIRED_HEADER_MISSING";

/// The built-in required-headers guard.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequiredHeadersGuardPlugin;

impl RequiredHeadersGuardPlugin {
    /// `inst-ps-hdr-1`/`-2`: read the configuration key for the phase under
    /// evaluation — `required_request_headers` for the request phase and
    /// `required_response_headers` for the response phase, independently of
    /// each other. An absent or blank key fails open, so an upstream that does
    /// not opt in sees no behavior change.
    #[must_use]
    pub fn required_names(config: Option<&serde_json::Value>, key: &str) -> Vec<String> {
        config
            .and_then(|config| config.get(key))
            .map(parse_names)
            .unwrap_or_default()
    }

    /// `inst-ps-hdr-4` .. `-9`: scan the phase's headers for each required
    /// name case-insensitively, in configuration order, checking presence
    /// only, and report only the first missing name.
    // @cpt-begin:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-1
    // `inst-ps-hdr-1`/`-2`: the configuration key is read for the phase under
    // evaluation and an absent or blank key fails open. `inst-ps-hdr-4` .. `-8`:
    // the entries are split, trimmed and lowercased, the phase's headers are
    // scanned case-insensitively in configuration order, presence only, and
    // only the first missing name is reported. `inst-ps-hdr-10`: the plugin
    // stays stateless, with no cache and no security-sensitive material.
    #[must_use]
    pub fn decide(
        config: Option<&serde_json::Value>,
        key: &str,
        headers: &[(String, String)],
        status: u16,
    ) -> GuardDecision {
        let required = Self::required_names(config, key);
        if required.is_empty() {
            return GuardDecision::allow();
        }
        let present: Vec<String> =
            headers.iter().map(|(name, _)| name.to_ascii_lowercase()).collect();
        for name in required {
            if !present.contains(&name) {
                return GuardDecision::Reject {
                    status,
                    reason: format!("{REQUIRED_HEADER_MISSING}: {name}"),
                };
            }
        }
        GuardDecision::allow()
    }
}
// @cpt-end:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-1

// @cpt-begin:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-10
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-2
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-3
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-4
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-5
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-6
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-7
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-8
// @cpt-begin:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-9
/// Parse one configured name list: an entry list is split on the comma
/// separator, each entry is trimmed and lowercased, and empty entries are
/// dropped. A JSON array of names is accepted as well, each element parsed the
/// same way.
fn parse_names(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(text) => parse_comma_list(text),
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(serde_json::Value::as_str)
            .flat_map(parse_comma_list)
            .collect(),
        _ => Vec::new(),
    }
}
//
// @cpt-end:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-9
// @cpt-end:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-8
// @cpt-end:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-7
// @cpt-end:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-6
// @cpt-end:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-5
// @cpt-end:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-4
// @cpt-end:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-3
// @cpt-end:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-2
// @cpt-end:cpt-cf-oagw-algo-plugin-system-required-headers-decision:p1:inst-ps-hdr-10
//

fn parse_comma_list(text: &str) -> Vec<String> {
    text.split(',')
        .map(|entry| entry.trim().to_ascii_lowercase())
        .filter(|entry| !entry.is_empty())
        .collect()
}

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &str {
        "required_headers"
    }

    fn plugin_type(&self) -> &str {
        REQUIRED_HEADERS_PLUGIN_TYPE
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, PluginError> {
        Ok(Self::decide(
            ctx.config.as_ref(),
            "required_request_headers",
            &ctx.headers,
            REQUEST_REJECT_STATUS,
        ))
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, PluginError> {
        Ok(Self::decide(
            ctx.config.as_ref(),
            "required_response_headers",
            &ctx.headers,
            RESPONSE_REJECT_STATUS,
        ))
    }
}

#[cfg(test)]
#[path = "required_headers_guard_tests.rs"]
mod required_headers_guard_tests;

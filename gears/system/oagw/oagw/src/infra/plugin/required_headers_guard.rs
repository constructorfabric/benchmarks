// Created: 2026-08-31 by Constructor Tech
//! `RequiredHeadersGuardPlugin` (ADR-0009).
//!
//! Presence-only enforcement of configured header names, independently in the
//! request and the response phase. Fail-open: a binding without configuration —
//! or with a configuration that is blank after splitting, trimming and dropping
//! empties — allows everything, so adding the plugin to a chain changes nothing
//! for an upstream that did not opt in.
//!
//! # Which map the two phases read (documented choice)
//!
//! The request phase reads the **outbound** headers, the set that is about to
//! be dialled. The response phase reads
//! [`ResponseContext::upstream_headers`](crate::infra::plugin::traits::ResponseContext::upstream_headers),
//! the headers the upstream actually answered with: the response transformation
//! rules have already been applied to `headers`, so a rule that drops a header
//! would otherwise turn a signed upstream response into a 502 the upstream is
//! not responsible for. ADR-0009 requires the *upstream* to have sent the
//! header, not the gateway to have kept it.
//!
//! # Status mapping (documented choice)
//!
//! ADR-0009 fixes only the HTTP status and the `error_code` of a rejection
//! (400 / `REQUIRED_HEADER_MISSING` for a request, 502 for a response). The
//! problem `type` of a rejection is taken from the DESIGN §3.3 error table,
//! which is where every gateway problem type comes from:
//! `validation.error.v1` for the request phase (`OagwErrorKind::Validation`)
//! and `protocol.error.v1` for the response phase (`OagwErrorKind::
//! ProtocolError`). Both rejections carry the `error_code` and the
//! `missing_header` extension members.

use async_trait::async_trait;

use crate::error::{OagwError, OagwErrorKind};
use crate::infra::plugin::traits::{
    GuardDecision, GuardPlugin, PluginConfig, RequestContext, ResponseContext,
};

/// GTS id of the built-in required-headers guard plugin.
pub const REQUIRED_HEADERS_GUARD_PLUGIN_ID: &str =
    "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";

/// `error_code` of every rejection (ADR-0009).
pub const ERROR_CODE: &str = "REQUIRED_HEADER_MISSING";

/// Request-phase configuration member.
const REQUEST_MEMBER: &str = "required_request_headers";
/// Response-phase configuration member.
const RESPONSE_MEMBER: &str = "required_response_headers";

/// Extension member naming the header whose absence rejected the phase.
pub const MISSING_HEADER: &str = "missing_header";

/// Presence-only enforcement of required headers (ADR-0009).
#[derive(Debug, Default)]
pub struct RequiredHeadersGuardPlugin;

#[async_trait]
impl GuardPlugin for RequiredHeadersGuardPlugin {
    fn id(&self) -> &'static str {
        "required_headers"
    }

    fn plugin_type(&self) -> &'static str {
        REQUIRED_HEADERS_GUARD_PLUGIN_ID
    }

    async fn guard_request(&self, ctx: &RequestContext) -> Result<GuardDecision, OagwError> {
        Ok(
            reject_first_missing(&ctx.config, REQUEST_MEMBER, &ctx.headers)
                .map_or(GuardDecision::Allow, |missing| {
                    GuardDecision::Reject(request_rejection(&missing))
                }),
        )
    }

    async fn guard_response(&self, ctx: &ResponseContext) -> Result<GuardDecision, OagwError> {
        // The upstream's own headers, not the client-bound set: a response rule
        // that strips a header must not turn into a 502 (see the module docs).
        Ok(
            reject_first_missing(&ctx.config, RESPONSE_MEMBER, &ctx.upstream_headers)
                .map_or(GuardDecision::Allow, |missing| {
                    GuardDecision::Reject(response_rejection(&missing))
                }),
        )
    }
}

/// Reject with the first required header the map does not carry.
///
/// Only the first missing name is reported (ADR-0009), in the order the
/// configuration listed them. Header names are matched case-insensitively and
/// only for presence, never for a value.
fn reject_first_missing(
    config: &PluginConfig,
    member: &str,
    headers: &http::HeaderMap,
) -> Option<String> {
    required(config, member).find(|name| headers.get(name.as_str()).is_none())
}

/// The configured header names of one phase, already normalised.
///
/// ADR-0009: split on `,`, trim, lowercase, drop empties; absent or blank means
/// the phase is a no-op.
fn required(config: &PluginConfig, member: &str) -> impl Iterator<Item = String> {
    config
        .string(member)
        .into_iter()
        .flat_map(|raw| raw.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
}

/// 400 `validation.error.v1` for a request that misses a required header.
fn request_rejection(missing: &str) -> crate::infra::plugin::traits::Rejection {
    crate::infra::plugin::traits::Rejection::new(missing_header_error(
        OagwErrorKind::Validation,
        missing,
        "this request does not carry the required header",
    ))
}

/// 502 `protocol.error.v1` for an upstream response that misses a header.
fn response_rejection(missing: &str) -> crate::infra::plugin::traits::Rejection {
    crate::infra::plugin::traits::Rejection::new(missing_header_error(
        OagwErrorKind::ProtocolError,
        missing,
        "the upstream response does not carry the required header",
    ))
}

/// The problem document of a rejection, with the ADR-0009 extension members.
fn missing_header_error(kind: OagwErrorKind, missing: &str, detail: &str) -> OagwError {
    OagwError::new(kind, format!("{detail} '{missing}'")).with_extension(|ext| {
        ext.error_code = Some(ERROR_CODE.to_owned());
        ext.missing_header = Some(missing.to_owned());
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(raw: &serde_json::Value) -> PluginConfig {
        PluginConfig::new(
            raw.as_object()
                .cloned()
                .unwrap_or_else(serde_json::Map::new),
        )
    }

    fn headers(entries: &[&str]) -> http::HeaderMap {
        let mut map = http::HeaderMap::new();
        for name in entries {
            map.insert(
                http::HeaderName::try_from(*name).unwrap_or(http::header::ACCEPT),
                http::HeaderValue::from_static("x"),
            );
        }
        map
    }

    #[test]
    fn the_plugin_is_the_documented_built_in() {
        let plugin = RequiredHeadersGuardPlugin;
        assert_eq!(plugin.id(), "required_headers");
        assert_eq!(plugin.plugin_type(), REQUIRED_HEADERS_GUARD_PLUGIN_ID);
        assert_eq!(
            plugin.plugin_type(),
            crate::domain::plugin::PluginKind::Guard.built_in_id("required_headers")
        );
    }

    #[test]
    fn the_configuration_is_split_trimmed_and_lowercased() {
        let names: Vec<String> = required(
            &config(&serde_json::json!({ REQUEST_MEMBER: " X-Correlation-Id, accept ,," })),
            REQUEST_MEMBER,
        )
        .collect();
        assert_eq!(
            names,
            Vec::from(["x-correlation-id".to_owned(), "accept".to_owned()])
        );
    }

    #[test]
    fn an_absent_or_blank_configuration_is_a_no_op() {
        for raw in [
            serde_json::json!({}),
            serde_json::json!({ REQUEST_MEMBER: " , , " }),
        ] {
            assert!(reject_first_missing(&config(&raw), REQUEST_MEMBER, &headers(&[])).is_none());
        }
    }

    #[test]
    fn only_the_first_missing_header_is_reported() {
        let config =
            config(&serde_json::json!({ REQUEST_MEMBER: "accept,x-tenant-id,x-signature" }));
        let missing = reject_first_missing(&config, REQUEST_MEMBER, &headers(&["accept"]));
        assert_eq!(missing.as_deref(), Some("x-tenant-id"));
    }

    #[test]
    fn header_names_are_matched_case_insensitively() {
        let config = config(&serde_json::json!({ REQUEST_MEMBER: "X-Correlation-Id" }));
        let present =
            reject_first_missing(&config, REQUEST_MEMBER, &headers(&["x-correlation-id"]));
        assert_eq!(present, None);
    }

    #[test]
    fn a_phase_is_independent_of_the_other() {
        let config = config(&serde_json::json!({ REQUEST_MEMBER: "x-request-id" }));
        assert!(reject_first_missing(&config, RESPONSE_MEMBER, &headers(&[])).is_none());
    }
}

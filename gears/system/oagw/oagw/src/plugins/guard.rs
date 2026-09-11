//! The `required_headers` guard (`cpt-cf-oagw-adr-required-headers-guard-plugin`,
//! `cpt-cf-oagw-algo-plugin-guard-evaluate`).
//!
//! Stateless: two independent, optional, comma-separated configuration
//! keys, one per phase; case-insensitive, presence-only matching; each
//! phase fails open independently when its own key is absent, empty, or
//! blank after trimming every entry; only the first missing header is
//! reported.
//!
//! RF-001: reached for real from `crate::proxy::engine` (via
//! `super::execute`'s chain executor) on every request, not just this
//! module's own tests. Note the structural limit this exposes: the frozen
//! `upstream.v1.schema.json`/`route.v1.schema.json` declare
//! `plugins.items[]` as bare identifier strings with no per-item `config`
//! slot (unlike `auth.config`, which the schema does carry), so a
//! `required_headers` guard bound through `Upstream`/`Route`
//! `plugins.items[]` today always observes an absent `config` here and
//! fails open -- see `crate::proxy::merge::merge_plugins`'s doc comment.
//! Closing that gap needs a wire-format change to `src/model/{upstream,route}.rs`,
//! outside this pass's file-ownership.

use axum::http::HeaderMap;
use serde_json::Value;

const REQUIRED_REQUEST_HEADERS_KEY: &str = "required_request_headers";
const REQUIRED_RESPONSE_HEADERS_KEY: &str = "required_response_headers";

/// `cpt-cf-oagw-dod-plugin-required-headers-guard`'s error code, reused
/// verbatim on both phases (only the HTTP status differs).
pub(crate) const REQUIRED_HEADER_MISSING_CODE: &str = "REQUIRED_HEADER_MISSING";

/// Which phase `evaluate_required_headers` is being asked to check
/// (`cpt-cf-oagw-algo-plugin-guard-evaluate`'s `phase` input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GuardPhase {
    Request,
    Response,
}

impl GuardPhase {
    fn config_key(self) -> &'static str {
        match self {
            Self::Request => REQUIRED_REQUEST_HEADERS_KEY,
            Self::Response => REQUIRED_RESPONSE_HEADERS_KEY,
        }
    }

    /// `inst-guard-evaluate-08`: `400` on the request phase, `502` on the
    /// response phase.
    fn rejection_status(self) -> u16 {
        match self {
            Self::Request => 400,
            Self::Response => 502,
        }
    }
}

/// The guard's decision for one phase (`cpt-cf-oagw-algo-plugin-guard-evaluate`'s
/// output shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GuardDecision {
    Allow,
    Reject {
        status: u16,
        code: &'static str,
        missing_header: String,
    },
}

fn parse_required_names(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Evaluate the `required_headers` guard for one `phase`
/// (`inst-guard-evaluate-01` through `-09`).
// @cpt-algo:cpt-cf-oagw-algo-plugin-guard-evaluate:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-required-headers-guard:p2
// @cpt-begin:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-01
// @cpt-begin:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-02
// @cpt-begin:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-03
pub(crate) fn evaluate_required_headers(
    config: &Value,
    phase: GuardPhase,
    headers: &HeaderMap,
) -> GuardDecision {
    let Some(raw) = config.get(phase.config_key()).and_then(Value::as_str) else {
        return GuardDecision::Allow;
    };
    // @cpt-end:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-03
    // @cpt-end:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-02
    // @cpt-end:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-01

    // @cpt-begin:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-04
    let names = parse_required_names(raw);
    if names.is_empty() {
        // Absent key handled above; this covers `""` and `", , ,"`.
        return GuardDecision::Allow;
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-04

    // @cpt-begin:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-05
    // @cpt-begin:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-06
    // @cpt-begin:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-07
    // @cpt-begin:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-08
    for name in names {
        // `HeaderMap` keys are always lowercased by construction, and
        // `name` was lowercased above, so this is presence-only,
        // case-insensitive matching without ever inspecting a value.
        if !headers.contains_key(name.as_str()) {
            return GuardDecision::Reject {
                status: phase.rejection_status(),
                code: REQUIRED_HEADER_MISSING_CODE,
                missing_header: name,
            };
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-08
    // @cpt-end:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-07
    // @cpt-end:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-06
    // @cpt-end:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-05

    // @cpt-begin:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-09
    GuardDecision::Allow
    // @cpt-end:cpt-cf-oagw-algo-plugin-guard-evaluate:p1:inst-guard-evaluate-09
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn allows_when_all_required_request_headers_present_case_insensitively() {
        let config = json!({"required_request_headers": "x-correlation-id,accept"});
        let hdrs = headers(&[("x-correlation-id", ""), ("ACCEPT", "*/*")]);
        assert_eq!(
            evaluate_required_headers(&config, GuardPhase::Request, &hdrs),
            GuardDecision::Allow
        );
    }

    #[test]
    fn rejects_naming_only_the_first_missing_header() {
        let config = json!({"required_request_headers": "x-correlation-id,accept"});
        let hdrs = headers(&[]);
        let decision = evaluate_required_headers(&config, GuardPhase::Request, &hdrs);
        assert_eq!(
            decision,
            GuardDecision::Reject {
                status: 400,
                code: REQUIRED_HEADER_MISSING_CODE,
                missing_header: "x-correlation-id".to_owned(),
            }
        );
    }

    #[test]
    fn response_phase_rejection_uses_502() {
        let config = json!({"required_response_headers": "content-type"});
        let hdrs = HeaderMap::new();
        let decision = evaluate_required_headers(&config, GuardPhase::Response, &hdrs);
        assert_eq!(
            decision,
            GuardDecision::Reject {
                status: 502,
                code: REQUIRED_HEADER_MISSING_CODE,
                missing_header: "content-type".to_owned(),
            }
        );
    }

    #[test]
    fn absent_key_fails_open() {
        let config = json!({});
        assert_eq!(
            evaluate_required_headers(&config, GuardPhase::Request, &HeaderMap::new()),
            GuardDecision::Allow
        );
    }

    #[test]
    fn empty_string_value_fails_open() {
        let config = json!({"required_request_headers": ""});
        assert_eq!(
            evaluate_required_headers(&config, GuardPhase::Request, &HeaderMap::new()),
            GuardDecision::Allow
        );
    }

    #[test]
    fn blank_entries_after_trimming_fail_open() {
        let config = json!({"required_request_headers": ", , ,"});
        assert_eq!(
            evaluate_required_headers(&config, GuardPhase::Request, &HeaderMap::new()),
            GuardDecision::Allow
        );
    }

    #[test]
    fn phases_fail_open_independently() {
        let config = json!({"required_response_headers": "content-type"});
        // Request phase has no key at all: allow, regardless of the
        // response phase's own configuration.
        assert_eq!(
            evaluate_required_headers(&config, GuardPhase::Request, &HeaderMap::new()),
            GuardDecision::Allow
        );
        // Response phase is still enforced.
        assert!(matches!(
            evaluate_required_headers(&config, GuardPhase::Response, &HeaderMap::new()),
            GuardDecision::Reject { .. }
        ));
    }

    #[test]
    fn header_value_is_never_inspected_empty_value_still_satisfies_presence() {
        let config = json!({"required_request_headers": "x-correlation-id"});
        let hdrs = headers(&[("x-correlation-id", "")]);
        assert_eq!(
            evaluate_required_headers(&config, GuardPhase::Request, &hdrs),
            GuardDecision::Allow
        );
    }
}

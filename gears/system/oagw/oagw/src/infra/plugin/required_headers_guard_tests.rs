//! The built-in required-headers guard
//! (`cpt-cf-oagw-dod-plugin-system-required-headers-guard`).
//!
//! The guard checks **presence only** and never compares header values
//! (ADR 0009), reads its own binding's configuration key for the phase under
//! evaluation, and fails open on an absent or blank key.
// @cpt-dod:cpt-cf-oagw-dod-plugin-system-required-headers-guard:p1

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::domain::plugin::{RequestContext, ResponseContext};
use serde_json::json;

/// Run an immediately-completing plugin call to its allow decision.
fn allow_of<F: std::future::Future<Output = Result<GuardDecision, crate::domain::plugin::PluginError>>>(
    future: F,
) -> bool {
    futures_util::future::FutureExt::now_or_never(future)
        .expect("the guard completes immediately")
        .expect("the guard runs")
        .is_allowed()
}

/// Run an immediately-completing plugin call to its decision.
fn decision_of<
    F: std::future::Future<Output = Result<GuardDecision, crate::domain::plugin::PluginError>>,
>(
    future: F,
) -> GuardDecision {
    futures_util::future::FutureExt::now_or_never(future)
        .expect("the guard completes immediately")
        .expect("the guard runs")
}

fn request_config(value: serde_json::Value) -> RequestContext {
    RequestContext { config: Some(value), ..RequestContext::default() }
}

/// An absent or blank key allows, so an upstream that does not opt in sees no
/// behavior change.
#[test]
fn an_absent_or_blank_key_fails_open() {
    let plugin = RequiredHeadersGuardPlugin;
    let decision = allow_of(plugin.guard_request(&RequestContext::default()));
    assert!(decision);
    let decision = allow_of(plugin.guard_request(&request_config(json!({}))));
    assert!(decision);
    let decision = allow_of(plugin.guard_request(&request_config(json!({
        "required_request_headers": "   "
    }))));
    assert!(decision);
    // A non-string, non-array value is no requirement at all.
    let decision = allow_of(plugin.guard_request(&request_config(json!({
        "required_request_headers": 7
    }))));
    assert!(decision);
}

/// Every required name present is an allow decision.
#[test]
fn every_required_name_present_allows() {
    let plugin = RequiredHeadersGuardPlugin;
    let ctx = RequestContext {
        headers: vec![
            ("x-tenant-id".to_owned(), "t".to_owned()),
            ("X-Request-ID".to_owned(), "r".to_owned()),
        ],
        config: Some(json!({ "required_request_headers": "x-tenant-id, x-request-id" })),
        ..RequestContext::default()
    };
    assert!(allow_of(plugin.guard_request(&ctx)));
}

/// One missing name is a `400` reject naming only the first missing header.
#[test]
fn the_first_missing_name_is_a_400_reject() {
    let plugin = RequiredHeadersGuardPlugin;
    let ctx = RequestContext {
        headers: vec![("x-tenant-id".to_owned(), "t".to_owned())],
        config: Some(json!({ "required_request_headers": "x-tenant-id, x-trace-id" })),
        ..RequestContext::default()
    };
    let decision = decision_of(plugin.guard_request(&ctx));
    assert_eq!(
        decision,
        GuardDecision::Reject {
            status: REQUEST_REJECT_STATUS,
            reason: "REQUIRED_HEADER_MISSING: x-trace-id".to_owned()
        }
    );
}

/// The response phase reads `required_response_headers` and rejects with
/// `502`, independently of the request key.
#[test]
fn the_response_phase_reads_its_own_key_and_rejects_with_502() {
    let plugin = RequiredHeadersGuardPlugin;
    let ctx = ResponseContext {
        status: 200,
        headers: Vec::new(),
        config: Some(json!({
            "required_request_headers": "x-absent",
            "required_response_headers": "x-request-id"
        })),
        ..ResponseContext::default()
    };
    // The request key is irrelevant here: only the response key is consulted.
    let decision = decision_of(plugin.guard_response(&ctx));
    assert_eq!(
        decision,
        GuardDecision::Reject {
            status: RESPONSE_REJECT_STATUS,
            reason: "REQUIRED_HEADER_MISSING: x-request-id".to_owned()
        }
    );
    let satisfied = ResponseContext {
        headers: vec![("X-Request-Id".to_owned(), "r".to_owned())],
        config: Some(json!({ "required_response_headers": "x-request-id" })),
        ..ResponseContext::default()
    };
    assert!(allow_of(plugin.guard_response(&satisfied)));
}

/// Names are split on the comma, trimmed, lowercased and emptied out; a JSON
/// array is accepted as well.
#[test]
fn name_lists_are_normalised() {
    assert_eq!(
        RequiredHeadersGuardPlugin::required_names(Some(&json!({ "k": " A , b,, " })), "k"),
        ["a", "b"]
    );
    assert_eq!(
        RequiredHeadersGuardPlugin::required_names(Some(&json!({ "k": ["A", " b "] })), "k"),
        ["a", "b"]
    );
}

/// Presence only: a required name that is present with an empty value is
/// still present, and no value is ever compared.
#[test]
fn the_check_is_presence_only() {
    let plugin = RequiredHeadersGuardPlugin;
    let ctx = RequestContext {
        headers: vec![("x-tenant-id".to_owned(), String::new())],
        config: Some(json!({ "required_request_headers": "x-tenant-id" })),
        ..RequestContext::default()
    };
    assert!(allow_of(plugin.guard_request(&ctx)));
}

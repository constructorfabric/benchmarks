//! Quota rejection mapping tests: 429 `resource_exhausted` with the quota scope as the violation
//! subject, 400 `failed_precondition` `FEATURE_DISABLED` for the web search kill switch, 500
//! `internal` for a policy failure, the Problem shape and the rule that a quota rejection is
//! never an SSE error code.
//!
//! Slice S1-all: algorithm `cpt-cf-mini-chat-algo-quota-and-billing-rejection-mapping`,
//! flows `cpt-cf-mini-chat-flow-quota-and-billing-turn-admission` and
//! `cpt-cf-mini-chat-flow-quota-and-billing-tool-quota-request` (error scenarios), definition of
//! done `cpt-cf-mini-chat-dod-quota-and-billing-rejection-contract`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::response::IntoResponse;
use serde_json::Value;
use toolkit_canonical_errors::{CanonicalError, Problem};

use crate::domain::error::{DomainError, QuotaScope, StreamErrorCode};

fn problem(err: DomainError) -> Value {
    serde_json::to_value(Problem::from(CanonicalError::from(err))).expect("problem serializes")
}

const SCOPES: [(QuotaScope, &str); 3] = [
    (QuotaScope::Tokens, "tokens"),
    (QuotaScope::WebSearch, "web_search"),
    (QuotaScope::CodeInterpreter, "code_interpreter"),
];

#[test]
fn a_quota_rejection_is_a_429_resource_exhausted_with_the_scope_as_the_violation_subject() {
    for (scope, wire) in SCOPES {
        let value = problem(DomainError::QuotaExceeded { scope });
        assert_eq!(value["status"], 429, "{wire}");
        assert!(
            value["type"]
                .as_str()
                .unwrap()
                .contains("resource_exhausted"),
            "{value}"
        );
        let violations = value["context"]["violations"].as_array().unwrap();
        assert_eq!(violations.len(), 1, "{value}");
        assert_eq!(violations[0]["subject"], wire);
        assert_eq!(violations[0]["description"], "quota_exceeded");
    }
}

#[test]
fn the_scope_wire_values_are_the_documented_subjects() {
    for (scope, wire) in SCOPES {
        assert_eq!(scope.as_str(), wire);
    }
}

#[test]
fn clients_can_branch_on_the_first_violation_subject_alone() {
    let tokens = problem(DomainError::QuotaExceeded {
        scope: QuotaScope::Tokens,
    });
    let web = problem(DomainError::QuotaExceeded {
        scope: QuotaScope::WebSearch,
    });
    assert_ne!(
        tokens["context"]["violations"][0]["subject"],
        web["context"]["violations"][0]["subject"]
    );
}

#[test]
fn a_quota_rejection_problem_has_the_contract_fields_and_no_top_level_code() {
    for (scope, wire) in SCOPES {
        let value = problem(DomainError::QuotaExceeded { scope });
        for key in ["type", "title", "status", "detail", "context"] {
            assert!(
                value.get(key).is_some(),
                "{wire}: missing `{key}` in {value}"
            );
        }
        assert!(value.get("code").is_none(), "{wire}: {value}");
        assert!(value["type"].as_str().unwrap().starts_with("gts://"));
    }
}

#[test]
fn a_quota_rejection_is_served_as_problem_json_before_any_stream() {
    for (scope, _) in SCOPES {
        let response = CanonicalError::from(DomainError::QuotaExceeded { scope }).into_response();
        assert_eq!(response.status().as_u16(), 429);
        let content_type = response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(content_type, "application/problem+json");
    }
}

#[test]
fn the_web_search_kill_switch_is_a_400_failed_precondition_with_feature_disabled() {
    let value = problem(DomainError::FeatureDisabled {
        subject: "web_search",
    });
    assert_eq!(value["status"], 400);
    assert!(
        value["type"]
            .as_str()
            .unwrap()
            .contains("failed_precondition"),
        "{value}"
    );
    let violations = value["context"]["violations"].as_array().unwrap();
    assert_eq!(violations.len(), 1, "{value}");
    assert_eq!(violations[0]["subject"], "web_search");
    assert_eq!(violations[0]["type"], "FEATURE_DISABLED");
    assert!(value.get("code").is_none());
}

#[test]
fn a_policy_failure_is_a_generic_500_that_never_leaks_the_cause() {
    let value = problem(DomainError::PolicyUnavailable {
        cause: "plugin down: provider-secret-detail".to_owned(),
    });
    assert_eq!(value["status"], 500);
    assert!(
        !value.to_string().contains("provider-secret-detail"),
        "{value}"
    );
}

#[test]
fn an_oversized_usage_payload_is_a_generic_500() {
    let value = problem(DomainError::OutboxPayloadTooLarge {
        site: crate::domain::error::EnqueueSite::TurnFinalize,
    });
    assert_eq!(value["status"], 500);
}

#[test]
fn quota_exceeded_is_never_an_sse_error_code_and_the_unimplemented_scopes_do_not_exist() {
    for code in StreamErrorCode::ALL {
        assert_ne!(code.as_str(), "quota_exceeded");
        assert_ne!(code.as_str(), "image_bytes_exceeded");
    }
    // the mid-turn tool codes are SSE events, never 429 scopes
    for (_, wire) in SCOPES {
        assert_ne!(wire, "web_search_calls_exceeded");
        assert_ne!(wire, "code_interpreter_calls_exceeded");
        assert_ne!(wire, "image_bytes");
        assert_ne!(wire, "image_inputs");
    }
}

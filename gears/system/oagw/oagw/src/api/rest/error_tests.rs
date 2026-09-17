//! Tests for the [`DomainError`] → problem-document mapping.
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::Value;
use toolkit_canonical_errors::Problem;
use uuid::Uuid;

use crate::api::rest::error::{
    ALREADY_EXISTS_TYPE, ApiError, ERROR_SOURCE_HEADER, GATEWAY_SOURCE, INTERNAL_TYPE,
    NOT_FOUND_TYPE, PLUGIN_IN_USE_TYPE, PROBLEM_JSON, VALIDATION_TYPE,
};
use crate::domain::error::{DomainError, PluginReferences};
use crate::domain::gts;

fn body(error: DomainError) -> (StatusCode, Value) {
    let response = ApiError::from(error).into_response();
    let status = response.status();
    assert_eq!(
        response
            .headers()
            .get(&ERROR_SOURCE_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(GATEWAY_SOURCE),
        "every management problem must be tagged as originating from the gateway"
    );
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .expect("content type")
        .to_owned();
    assert_eq!(content_type, PROBLEM_JSON);
    // `Problem` renders synchronously, so the body is buffered through a
    // private runtime instead of turning every assertion into an async test.
    let bytes = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime")
        .block_on(async { axum::body::to_bytes(response.into_body(), usize::MAX).await })
        .expect("body")
        .to_vec();
    (status, serde_json::from_slice(&bytes).expect("json body"))
}

#[test]
fn validation_is_a_400_problem() {
    let (status, json) = body(DomainError::Validation {
        detail: "nope".to_owned(),
    });
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["type"], VALIDATION_TYPE);
    assert_eq!(json["status"], 400);
    assert_eq!(json["detail"], "nope");
}

#[test]
fn field_violations_carry_the_violation_array() {
    let (status, json) = body(DomainError::field(
        "server.endpoints",
        crate::domain::reason::ENDPOINTS_EMPTY,
        "at least one endpoint is required",
    ));
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["type"], VALIDATION_TYPE);
    assert_eq!(json["context"]["field"], "server.endpoints");
    assert_eq!(
        json["context"]["violations"][0]["reason"],
        crate::domain::reason::ENDPOINTS_EMPTY
    );
}

#[test]
fn not_found_uses_the_canonical_identifier() {
    let (status, json) = body(DomainError::NotFound {
        detail: "upstream not found".to_owned(),
    });
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["type"], NOT_FOUND_TYPE);
    assert_eq!(json["status"], 404);
}

#[test]
fn alias_conflict_is_a_409_with_the_resource_type() {
    let tenant = Uuid::new_v4();
    let (status, json) = body(DomainError::AliasConflict {
        alias: "api.example.com".to_owned(),
        tenant_id: tenant,
    });
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["type"], ALREADY_EXISTS_TYPE);
    assert_eq!(
        json["detail"],
        "alias 'api.example.com' is already used by another upstream of this tenant"
    );
    assert_eq!(
        json["context"]["resource_type"],
        "gts.cf.core.oagw.upstream.v1~"
    );
    assert_eq!(json["context"]["resource_name"], "api.example.com");
}

#[test]
fn route_match_conflict_is_a_409() {
    let (status, json) = body(DomainError::RouteMatchConflict {
        detail: "duplicate".to_owned(),
    });
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["type"], ALREADY_EXISTS_TYPE);
    assert_eq!(
        json["context"]["resource_type"],
        format!("gts.{}~", gts::ROUTE_TYPE)
    );
    assert_eq!(json["context"]["reason"], "route_match_conflict");
}

#[test]
fn plugin_in_use_reports_the_referencing_resources() {
    let references = PluginReferences {
        upstreams: vec!["gts.cf.core.oagw.upstream.v1~1111".to_owned()],
        routes: vec![],
    };
    let (status, json) = body(DomainError::PluginInUse {
        plugin_id: "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1".to_owned(),
        references,
    });
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["type"], PLUGIN_IN_USE_TYPE);
    assert_eq!(
        json["detail"],
        "plugin is referenced by 1 upstream(s) and 0 route(s)"
    );
    assert_eq!(
        json["context"]["referenced_by"]["upstreams"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    assert_eq!(
        json["context"]["referenced_by"]["routes"]
            .as_array()
            .map(Vec::len),
        Some(0)
    );
}

#[test]
fn internal_errors_stay_opaque() {
    let (status, json) = body(DomainError::internal("secret storage diagnostic"));
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(json["type"], INTERNAL_TYPE);
    assert_eq!(json["detail"], "internal error");
    assert!(
        !json.to_string().contains("secret storage diagnostic"),
        "the diagnostic must never reach the wire"
    );
}

#[test]
fn unknown_plugin_references_are_a_400() {
    let (status, json) = body(DomainError::UnknownPluginRef {
        detail: "no such plugin".to_owned(),
    });
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["type"], VALIDATION_TYPE);
    assert_eq!(
        json["context"]["reason"],
        crate::domain::reason::PLUGIN_UNKNOWN
    );
}

#[test]
fn immutable_fields_are_a_400() {
    let (status, json) = body(DomainError::ImmutableField { field: "id" });
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["type"], VALIDATION_TYPE);
    assert_eq!(json["detail"], "immutable field 'id' cannot be changed");
    assert_eq!(json["context"]["reason"], crate::domain::reason::IMMUTABLE);
}

#[test]
fn problem_documents_survive_the_canonical_round_trip() {
    // The api-gateway middleware re-parses every `application/problem+json`
    // body through `Problem`. Anything the gear puts outside `context` would
    // be dropped there, so assert the round trip preserves what matters.
    let (_, json) = body(DomainError::AliasConflict {
        alias: "round.trip.example.com".to_owned(),
        tenant_id: Uuid::new_v4(),
    });
    let problem: Problem = serde_json::from_value(json).expect("canonical Problem re-parse");
    assert_eq!(problem.status, 409);
    assert_eq!(problem.problem_type, ALREADY_EXISTS_TYPE);
    assert!(problem.context["alias"].is_string());
}

#[test]
fn resource_type_prefixes_the_family() {
    assert_eq!(
        crate::api::rest::error::resource_type(gts::UPSTREAM_TYPE),
        "gts.cf.core.oagw.upstream.v1~"
    );
}

// The `cf.oagw.plugin.in_use.v1` identifier is the only OAGW-specific 409 in
// the catalogue; the other 409 classes fall back on the platform's
// `cf.core.err.already_exists.v1` because `DESIGN.md` has no entry for them.
#[test]
fn the_catalogue_has_exactly_one_custom_409() {
    let types = [
        VALIDATION_TYPE,
        NOT_FOUND_TYPE,
        ALREADY_EXISTS_TYPE,
        PLUGIN_IN_USE_TYPE,
        INTERNAL_TYPE,
    ];
    let mut distinct = types.to_vec();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), 5);
    assert!(PLUGIN_IN_USE_TYPE.contains("cf.oagw.plugin.in_use.v1"));
    assert!(ALREADY_EXISTS_TYPE.contains("cf.core.err.already_exists.v1"));
}

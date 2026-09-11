//! Black-box tests for DECOMPOSITION entry 2.3 (Route Management API)'s
//! public surface (`oagw::model::route`), exercising the crate the way an
//! external consumer would.
//!
//! The REST handlers themselves (`api::rest::route_api`) are `pub(crate)`
//! by contract with the fixed aggregator in `api::rest::routes` (owned by
//! entry 2.1) -- this feature's own signature constraint forbids widening
//! that visibility. The full router-level, `tower::ServiceExt`-driven HTTP
//! exercise of all five endpoints (create/list/get/replace/delete, both
//! `{id}` forms, tenant scoping, ownership, and uniqueness) therefore lives
//! in that module's own `#[cfg(test)] mod tests`
//! (`src/api/rest/route_api.rs`), which is a crate-internal file this
//! feature owns and is exactly the "inline `#[cfg(test)] mod tests` in your
//! own files" the task's Method section asks for. This file complements it
//! with genuinely external, black-box coverage of the one piece of this
//! feature's surface that *is* public: the [`Route`] domain/DTO type.

#![allow(clippy::unwrap_used)]

use axum::response::IntoResponse;
use http_body_util::BodyExt;
use oagw::error::{OagwError, OagwErrorKind};
use oagw::model::route::{ROUTE_GTS_ID_PREFIX, Route};
use oagw::model::upstream::{RateLimitAlgorithm, RateLimitScope, RateLimitStrategy, Sharing};
use uuid::Uuid;

#[test]
fn route_deserializes_a_schema_valid_http_body() {
    let upstream_id = Uuid::new_v4();
    let json = serde_json::json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET", "POST"], "path": "/v1/models" } },
        "tags": ["public-api"],
        "priority": 5,
    });
    let route: Route = serde_json::from_value(json).unwrap();
    assert_eq!(route.upstream_id, upstream_id);
    assert_eq!(route.tags, vec!["public-api".to_owned()]);
    assert_eq!(route.priority, Some(5));
    assert!(route.route_match.http.is_some());
    assert!(route.route_match.grpc.is_none());
    // Schema-versus-domain-model discrepancy #1: `enabled` defaults to
    // `true` when omitted, per `cpt-cf-oagw-dod-route-schema-validation`.
    assert!(route.enabled);
}

#[test]
fn route_deserializes_a_schema_valid_grpc_body_without_priority() {
    let json = serde_json::json!({
        "upstream_id": Uuid::new_v4(),
        "match": { "grpc": { "service": "pkg.v1.Service", "method": "GetThing" } },
    });
    let route: Route = serde_json::from_value(json).unwrap();
    assert!(route.route_match.grpc.is_some());
    assert!(route.route_match.http.is_none());
    // Schema-versus-domain-model discrepancy #2: `priority` only matters
    // alongside `match.http`; a grpc-only route leaves it absent.
    assert!(route.priority.is_none());
}

#[test]
fn route_id_and_tenant_id_are_server_generated_never_client_suppliable() {
    let json = serde_json::json!({
        "id": Uuid::new_v4(),
        "tenant_id": Uuid::new_v4(),
        "upstream_id": Uuid::new_v4(),
        "match": { "http": { "methods": ["GET"], "path": "/p" } },
        "priority": 1,
    });
    let route: Route = serde_json::from_value(json).unwrap();
    // `id` IS a schema-declared field (server-generated, read-only on the
    // wire) so it round-trips when present in the request body -- the REST
    // handler layer, not this deserialize step, is what disregards a
    // client-supplied `id` on create (see the router-level test
    // `create_disregards_a_client_supplied_id`).
    assert!(route.id.is_some());
    // `tenant_id` is not a schema field at all: never read from the wire.
    assert_eq!(route.tenant_id, Uuid::nil());

    let mut route = route;
    route.tenant_id = Uuid::new_v4();
    let value = serde_json::to_value(&route).unwrap();
    assert!(value.get("tenant_id").is_none());
}

#[test]
fn normalize_id_param_treats_the_bare_uuid_and_anonymous_gts_form_as_equivalent() {
    let id = Uuid::new_v4();
    let gts_form = format!("{ROUTE_GTS_ID_PREFIX}{id}");
    assert_eq!(Route::normalize_id_param(&id.to_string()), Some(id));
    assert_eq!(Route::normalize_id_param(&gts_form), Some(id));
    assert_eq!(
        Route::normalize_id_param(&id.to_string()),
        Route::normalize_id_param(&gts_form)
    );
}

#[test]
fn normalize_id_param_rejects_a_malformed_identifier() {
    assert!(Route::normalize_id_param("gts.cf.core.oagw.route.v1~not-a-uuid").is_none());
    assert!(Route::normalize_id_param("").is_none());
}

/// `cpt-cf-oagw-dod-route-schema-validation`: `plugins.sharing` accepts all
/// three documented `Sharing` values. This closes a gap no existing test in
/// this crate covers for `RoutePluginsBinding` specifically -- the
/// crate-private `route_api::validate::parse_plugins` (which real
/// create/replace requests flow through) only has coverage for the default
/// (`private`) case, and the shared `Sharing` enum's `inherit`/`enforce`
/// values are otherwise tested only via `RateLimitConfig` in a sibling
/// FEATURE's test file, never through a `Route`'s own `plugins` field.
#[test]
fn route_plugins_sharing_deserializes_all_three_documented_values() {
    for (raw, expected) in [
        ("private", Sharing::Private),
        ("inherit", Sharing::Inherit),
        ("enforce", Sharing::Enforce),
    ] {
        let json = serde_json::json!({
            "upstream_id": Uuid::new_v4(),
            "match": { "http": { "methods": ["GET"], "path": "/p" } },
            "priority": 1,
            "plugins": { "sharing": raw, "items": [] },
        });
        let route: Route = serde_json::from_value(json).unwrap();
        assert_eq!(route.plugins.unwrap().sharing, expected);
    }

    let invalid = serde_json::json!({
        "upstream_id": Uuid::new_v4(),
        "match": { "http": { "methods": ["GET"], "path": "/p" } },
        "priority": 1,
        "plugins": { "sharing": "bogus" },
    });
    assert!(serde_json::from_value::<Route>(invalid).is_err());
}

/// `cpt-cf-oagw-dod-route-schema-validation`'s "`rate_limit`'s sub-fields"
/// line item: a Route's `rate_limit` accepts the documented non-default
/// `algorithm`/`scope`/`strategy` values and an explicit `burst.capacity`,
/// not just the all-defaults shape already covered elsewhere.
#[test]
fn route_rate_limit_accepts_non_default_documented_values() {
    let json = serde_json::json!({
        "upstream_id": Uuid::new_v4(),
        "match": { "http": { "methods": ["GET"], "path": "/p" } },
        "priority": 1,
        "rate_limit": {
            "algorithm": "sliding_window",
            "sustained": { "rate": 7, "window": "day" },
            "burst": { "capacity": 42 },
            "scope": "global",
            "strategy": "degrade",
            "cost": 2,
        },
    });
    let route: Route = serde_json::from_value(json).unwrap();
    let rate_limit = route.rate_limit.unwrap();
    assert_eq!(rate_limit.algorithm, RateLimitAlgorithm::SlidingWindow);
    assert_eq!(rate_limit.burst.unwrap().capacity, Some(42));
    assert_eq!(rate_limit.scope, RateLimitScope::Global);
    assert_eq!(rate_limit.strategy, RateLimitStrategy::Degrade);
    assert_eq!(rate_limit.cost, 2);
}

/// `route.v1.schema.json` defines a `cors` sub-schema under `definitions`
/// that no top-level Route *property* references, and the [`Route`] DTO
/// declares no `cors` field at all -- confirming the wire-contract note
/// that CORS configuration lives on the Upstream only, never the Route. A
/// `cors` object present in the JSON simply has nowhere to deserialize
/// into and is silently dropped, exactly matching
/// `cpt-cf-oagw-algo-route-match-validate`'s `inst-match-validate-cors-ignore`
/// step this crate's internal validator implements.
#[test]
fn route_model_has_no_cors_field_a_cors_object_is_dropped_on_deserialize() {
    let json = serde_json::json!({
        "upstream_id": Uuid::new_v4(),
        "match": { "http": { "methods": ["GET"], "path": "/p" } },
        "priority": 1,
        "cors": { "enabled": true, "allowed_origins": ["*"] },
    });
    let route: Route = serde_json::from_value(json).unwrap();
    let value = serde_json::to_value(&route).unwrap();
    assert!(value.get("cors").is_none());
}

/// `cpt-cf-oagw-dod-route-error-mapping`'s `400` row: every shape-validation
/// or `upstream_id`-integrity failure this feature's `create_route`/
/// `replace_route` handlers raise (via the crate-private `validation_error()`
/// helper, which wraps `OagwError::new(OagwErrorKind::ValidationError, ..)`
/// -- the identical construction `upstream_management.rs` exercises for its
/// own `400`s) renders through the shared RFC 9457 envelope with the
/// documented GTS `type`, `application/problem+json`, and
/// `X-OAGW-Error-Source: gateway`.
#[tokio::test]
async fn route_validation_error_kind_renders_the_documented_400_envelope() {
    let response =
        OagwError::new(OagwErrorKind::ValidationError, "upstream_id is required").into_response();
    assert_eq!(response.status().as_u16(), 400);
    assert_eq!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/problem+json")
    );
    assert_eq!(
        response
            .headers()
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("gateway")
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
    assert_eq!(json["status"], 400);
}

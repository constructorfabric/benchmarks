//! The transport DTO tests of the upstream management surface (FEATURE entry
//! 2.2, DoD `cpt-cf-oagw-dod-upstream-management-schema-shapes`).
//!
//! The write body is its own DTO, so the tests assert the three things the
//! domain record cannot express: no `id`/`tenant_id` on the wire, unknown
//! properties rejected, and a sub-configuration block that is absent staying
//! absent.

use serde_json::json;

use super::dto::{CorsRequestDto, UpstreamListResponse, UpstreamRequest, UpstreamResponse};
use crate::domain::dto::Upstream;
use crate::domain::services::management::EffectiveEnablement;

fn https_pool() -> serde_json::Value {
    json!({
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "server": { "endpoints": [ { "host": "api.vendor.com" } ] }
    })
}

/// The scalar structural defaults are materialized by the body itself:
/// `enabled` is true when omitted.
#[test]
fn the_body_materializes_the_scalar_defaults() {
    let body: UpstreamRequest =
        serde_json::from_value(https_pool()).expect("the minimal body is well formed");
    assert!(body.enabled, "enabled defaults to true");
    assert_eq!(body.alias, "", "an absent alias is the empty convention");
    assert_eq!(body.tags, Vec::<String>::new());
    assert!(body.supplied_alias().is_none());
    assert!(body.auth.is_none() && body.headers.is_none() && body.rate_limit.is_none());
    assert!(body.cors.is_none() && body.plugins.is_none());
}

/// A body that omits every sub-configuration block deserializes into a domain
/// record carrying no block at all.
#[test]
fn an_omitted_block_stays_absent_through_into_upstream() {
    let body: UpstreamRequest = serde_json::from_value(https_pool()).expect("body");
    let record = body.into_upstream();
    assert!(record.auth.is_none(), "no implicit auth block");
    assert!(record.headers.is_none(), "no implicit headers block");
    assert!(record.rate_limit.is_none(), "no implicit rate_limit block");
    assert!(record.cors.is_none(), "no implicit cors block");
    assert!(record.plugins.is_none(), "no implicit plugins block");
    assert_eq!(record.id, uuid::Uuid::nil(), "the identifier is server-generated");
    assert_eq!(record.tenant_id, uuid::Uuid::nil(), "the tenant is server-assigned");
    assert_eq!(record.enabled, true);
}

/// `id` and `tenant_id` are unknown properties of the write body, so the
/// caller can never dictate them.
#[test]
fn a_supplied_identifier_or_tenant_is_an_unknown_property() {
    for mut body in [https_pool(), https_pool()] {
        body["id"] = json!(uuid::Uuid::new_v4().to_string());
        assert!(
            serde_json::from_value::<UpstreamRequest>(body).is_err(),
            "a supplied `id` is rejected"
        );
    }
    let mut body = https_pool();
    body["tenant_id"] = json!(uuid::Uuid::new_v4().to_string());
    assert!(
        serde_json::from_value::<UpstreamRequest>(body).is_err(),
        "a supplied `tenant_id` is rejected"
    );
}

/// Any key outside the schema property set is rejected.
#[test]
fn an_unknown_property_is_rejected() {
    let mut body = https_pool();
    body["scopes"] = json!(["read"]);
    assert!(serde_json::from_value::<UpstreamRequest>(body).is_err());
}

/// A `cors` block without `enabled` is rejected: the schema default
/// `cors.enabled: false` belongs to the merge engine, not to the write body.
#[test]
fn a_cors_block_without_enabled_is_rejected() {
    let mut body = https_pool();
    body["cors"] = json!({ "allowed_origins": ["*"] });
    assert!(
        serde_json::from_value::<UpstreamRequest>(body).is_err(),
        "the required `enabled` flag has no default on the write path"
    );
}

/// A `cors` block with `enabled` is accepted and converted with the flag
/// carried through.
#[test]
fn a_cors_block_with_enabled_is_carried_through() {
    let mut body = https_pool();
    body["cors"] = json!({ "enabled": true, "allowed_origins": ["https://app.vendor.com"] });
    let body: UpstreamRequest = serde_json::from_value(body).expect("body");
    let enabled = body.cors.as_ref().expect("the block is present").enabled;
    assert!(enabled);
    let record = body.into_upstream();
    assert_eq!(record.cors.expect("block").enabled, true);
}

/// An unknown key inside a sub-configuration block is rejected, so a
/// misspelled control cannot silently disable itself.
#[test]
fn an_unknown_key_inside_a_block_is_rejected() {
    let mut body = https_pool();
    body["plugins"] = json!({ "sharing": "private", "itemz": [] });
    assert!(serde_json::from_value::<UpstreamRequest>(body).is_err());
    let mut body = https_pool();
    body["cors"] = json!({ "enabled": true, "origin": "*" });
    assert!(serde_json::from_value::<UpstreamRequest>(body).is_err());
}

/// A supplied alias is trimmed and carried; an empty one is "not supplied".
#[test]
fn an_explicit_alias_is_trimmed() {
    let mut body = https_pool();
    body["alias"] = json!("  api.vendor.com  ");
    let body: UpstreamRequest = serde_json::from_value(body).expect("body");
    assert_eq!(body.supplied_alias(), Some("api.vendor.com"));
    let record = body.into_upstream();
    assert_eq!(record.alias, "api.vendor.com");
}

/// The read-back is the stored record plus the effective enablement, and
/// never the owner tenant.
#[test]
fn the_response_carries_the_effective_enablement_and_not_the_tenant() {
    let record = Upstream {
        id: uuid::Uuid::new_v4(),
        tenant_id: uuid::Uuid::new_v4(),
        protocol: "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1".to_owned(),
        enabled: true,
        ..serde_json::from_str::<Upstream>(
            "{\"alias\":\"api.vendor.com\",\"protocol\":\"gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1\",\"server\":{\"endpoints\":[]}}",
        )
        .expect("record")
    };
    let effective = EffectiveEnablement {
        enabled: false,
        disabling_tenant_id: Some(uuid::Uuid::new_v4()),
    };
    let response = UpstreamResponse::from_record(&record, effective);
    let wire = serde_json::to_value(&response).expect("wire");
    assert!(wire.get("tenant_id").is_none(), "the owner tenant is never on the wire");
    assert!(wire["effective_enablement"]["enabled"] == json!(false));
    assert!(wire["effective_enablement"]["disabling_tenant_id"].is_string());
}

/// The list body carries the count of the records actually returned.
#[test]
fn the_list_body_reports_the_count_actually_returned() {
    let response = UpstreamListResponse { count: 0, items: Vec::new() };
    let wire = serde_json::to_value(&response).expect("wire");
    assert_eq!(wire["count"], json!(0));
    assert_eq!(wire["items"], json!([]));
}

/// The CORS request block keeps the schema property set.
#[test]
fn the_cors_request_block_keeps_the_schema_shape() {
    let cors: CorsRequestDto = serde_json::from_value(json!({ "enabled": true })).expect("block");
    assert_eq!(cors.allowed_methods, vec!["GET".to_owned(), "POST".to_owned()]);
    assert!(!cors.allow_credentials);
    assert_eq!(cors.sharing, crate::domain::dto::SharingMode::Private);
}

// ---------------------------------------------------------------------------
// Routes (FEATURE entry 2.3)
// ---------------------------------------------------------------------------

use super::dto::{RouteRequest, RouteResponse, RouteUpdateRequest};
use crate::domain::dto::{MatchConfig, Route};

fn route_body(upstream_id: uuid::Uuid) -> serde_json::Value {
    json!({
        "upstream_id": upstream_id.to_string(),
        "match": { "http": { "path": "/v1/orders", "methods": ["GET", "POST"] } }
    })
}

/// The route write body materializes the declared scalar defaults.
#[test]
fn the_route_body_materializes_the_scalar_defaults() {
    let body: RouteRequest =
        serde_json::from_value(route_body(uuid::Uuid::new_v4())).expect("the minimal body");
    assert_eq!(body.priority, 0, "the declared default priority");
    assert!(body.enabled, "the declared default enablement");
    assert_eq!(body.tags, Vec::<String>::new());
    assert!(body.rate_limit.is_none() && body.cors.is_none() && body.plugins.is_none());
}

/// `upstream_id` is part of the create body and is not ignorable.
#[test]
fn the_create_body_requires_the_upstream_reference() {
    let mut body = route_body(uuid::Uuid::new_v4());
    body.as_object_mut().expect("object").remove("upstream_id");
    assert!(
        serde_json::from_value::<RouteRequest>(body).is_err(),
        "the owning upstream is required"
    );
}

/// `id` and `tenant_id` are unknown properties of the create body, so the
/// caller can never dictate them. The both-alternatives and match-shape rules
/// are the domain validation, not the transport shape.
#[test]
fn the_route_bodies_reject_the_server_assigned_and_immutable_fields() {
    let upstream_id = uuid::Uuid::new_v4();
    let mut body = route_body(upstream_id);
    body["id"] = json!(upstream_id.to_string());
    assert!(serde_json::from_value::<RouteRequest>(body).is_err(), "a supplied `id`");
    let mut body = route_body(upstream_id);
    body["tenant_id"] = json!(upstream_id.to_string());
    assert!(serde_json::from_value::<RouteRequest>(body).is_err(), "a supplied tenant");
    let mut body = route_body(upstream_id);
    body["match_type"] = json!("http");
    assert!(serde_json::from_value::<RouteRequest>(body).is_err(), "a supplied match_type");
}

/// The replacement body has no `upstream_id` field, so the wire shape of the
/// update differs from the create shape by exactly that one property.
#[test]
fn the_update_body_has_no_upstream_reference() {
    let update: RouteUpdateRequest = serde_json::from_value(json!({
        "match": { "http": { "path": "/v1/orders", "methods": ["GET"] } }
    }))
    .expect("the replacement body");
    let record = update.into_route();
    assert_eq!(record.upstream_id, uuid::Uuid::nil(), "not supplied, retain the stored one");
    assert_eq!(record.priority, 0, "the declared default priority");
    assert!(record.enabled, "the declared default re-enables a disabled route");
    assert!(record.rate_limit.is_none(), "an omitted override is cleared");
    assert!(record.cors.is_none());
    assert!(record.plugins.is_none());
    assert_eq!(record.tags, Vec::<String>::new(), "an omitted tag list is cleared");
    // A replacement body that supplies `upstream_id` is an unknown property.
    let mut body = json!({ "match": { "http": { "path": "/v1", "methods": ["GET"] } } });
    body["upstream_id"] = json!(uuid::Uuid::new_v4().to_string());
    assert!(serde_json::from_value::<RouteUpdateRequest>(body).is_err());
}

/// `match` is the wire name of the match block, and the stored match block is
/// returned as it was given.
#[test]
fn the_match_block_round_trips_under_its_wire_name() {
    let body: RouteRequest = serde_json::from_value(route_body(uuid::Uuid::new_v4())).expect("body");
    let record = body.into_route();
    assert_eq!(record.match_type, crate::domain::dto::RouteMatchType::Http);
    let response = RouteResponse::from_record(&Route {
        id: uuid::Uuid::new_v4(),
        tenant_id: uuid::Uuid::new_v4(),
        upstream_id: uuid::Uuid::new_v4(),
        match_type: crate::domain::dto::RouteMatchType::Http,
        priority: 0,
        enabled: true,
        match_: record.match_.clone(),
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: vec!["orders".to_owned()],
    });
    let wire = serde_json::to_value(&response).expect("wire");
    assert!(wire.get("tenant_id").is_none(), "the owner tenant is never on the wire");
    assert!(wire.get("match_type").is_none(), "the derived match type is never on the wire");
    assert!(wire["match"]["http"]["path"] == json!("/v1/orders"));
    assert!(wire["match"]["http"]["path_suffix_mode"] == json!("append"), "the default mode");
    assert_eq!(wire["tags"], json!(["orders"]));
}

/// A `grpc` match block is the other alternative of the wire shape.
#[test]
fn the_grpc_alternative_is_the_wire_shape_of_a_grpc_route() {
    let body: RouteRequest = serde_json::from_value(json!({
        "upstream_id": uuid::Uuid::new_v4().to_string(),
        "match": { "grpc": { "service": "cf.inventory.v1.Inventory", "method": "Reserve" } }
    }))
    .expect("body");
    let match_: MatchConfig = body.match_;
    assert!(match_.grpc.is_some() && match_.http.is_none());
}

//! Crate-internal HTTP-level tests for the Upstream Management API,
//! exercising a real `axum::Router` via `tower::ServiceExt`.
//!
//! These live here (rather than in the external `tests/` integration-test
//! crate) because `api::rest::upstreams` is only reachable from inside the
//! `oagw` crate: `src/api/rest/mod.rs` (owned by DECOMPOSITION entry 2.1,
//! out of this entry's scope to edit) declares `mod upstreams;` as
//! crate-private, so no external test crate can reach `register_routes` or
//! the handlers no matter how they are marked inside this file. See this
//! module's parent doc comment for the tenant-hierarchy design this choice
//! also documents.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use uuid::Uuid;

use crate::config::OagwConfig;
use crate::store::OagwState;
use toolkit::api::OpenApiRegistryImpl;
use toolkit_security::SecurityContext;

use super::{NoTenantHierarchy, TenantHierarchyProvider};

fn security_context(tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_id)
        .build()
        .unwrap()
}

fn security_context_with_scopes(tenant_id: Uuid, scopes: Vec<String>) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_id)
        .token_scopes(scopes)
        .build()
        .unwrap()
}

/// Build a router with the given `SecurityContext` and `TenantHierarchyProvider`
/// pre-injected -- production auth middleware (outside this gear) is what
/// injects `SecurityContext` for real requests; here the test stands in for
/// it directly, as `gear_foundation.rs`'s own tests do for other Extensions.
fn router_for(
    state: Arc<OagwState>,
    ctx: SecurityContext,
    hierarchy: Arc<dyn TenantHierarchyProvider>,
) -> axum::Router {
    let registry = OpenApiRegistryImpl::new();
    let router = axum::Router::new();
    super::register_routes_with_hierarchy(router, &registry, state, hierarchy)
        .layer(axum::Extension(ctx))
}

fn default_router_for(state: Arc<OagwState>, ctx: SecurityContext) -> axum::Router {
    router_for(state, ctx, Arc::new(NoTenantHierarchy))
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn post_request(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn put_request(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn get_request(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

fn delete_request(uri: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

/// Acceptance criterion: a single HTTPS hostname endpoint (standard port
/// 443) with no explicit `alias` returns `201 Created` with a
/// server-generated `id` and `alias` auto-derived to the hostname.
#[tokio::test]
async fn create_upstream_derives_alias_and_returns_201() {
    let state = Arc::new(OagwState::new(OagwConfig::default()));
    let tenant = Uuid::new_v4();
    let router = default_router_for(state, security_context(tenant));

    let body = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "api.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    let response = router
        .oneshot(post_request("/oagw/v1/upstreams", body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let json = body_json(response).await;
    assert_eq!(json["alias"], "api.example.com");
    assert!(json["id"].is_string());
}

/// `http_scheme_check`: the graded configuration's exact acceptance
/// criterion -- `{"scheme": "http", "port": 80}` MUST return `201 Created`,
/// independent of `allow_http_upstream`.
#[tokio::test]
async fn create_upstream_accepts_plaintext_http_scheme_and_returns_201() {
    let state = Arc::new(OagwState::new(OagwConfig::default()));
    let tenant = Uuid::new_v4();
    let router = default_router_for(state, security_context(tenant));

    let body = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "http", "host": "example-plaintext.internal", "port": 80 } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        "alias": "example-plaintext-svc",
    });
    let response = router
        .oneshot(post_request("/oagw/v1/upstreams", body))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let json = body_json(response).await;
    assert_eq!(json["alias"], "example-plaintext-svc");
}

/// `id_form_check`: `GET .../{id}` succeeds identically whether `{id}` is
/// the bare UUID or the anonymous GTS form.
#[tokio::test]
async fn get_upstream_resolves_both_bare_uuid_and_anonymous_gts_id_forms() {
    let state = Arc::new(OagwState::new(OagwConfig::default()));
    let tenant = Uuid::new_v4();
    let router = default_router_for(state.clone(), security_context(tenant));

    let body = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "dual-form.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    let created = router
        .clone()
        .oneshot(post_request("/oagw/v1/upstreams", body))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_json = body_json(created).await;
    let id = created_json["id"].as_str().unwrap().to_owned();

    let by_bare_uuid = router
        .clone()
        .oneshot(get_request(&format!("/oagw/v1/upstreams/{id}")))
        .await
        .unwrap();
    assert_eq!(by_bare_uuid.status(), StatusCode::OK);
    let by_bare_uuid_json = body_json(by_bare_uuid).await;

    let gts_form = format!("gts.cf.core.oagw.upstream.v1~{id}");
    let by_gts_form = router
        .oneshot(get_request(&format!(
            "/oagw/v1/upstreams/{}",
            urlencoding_slash_safe(&gts_form)
        )))
        .await
        .unwrap();
    assert_eq!(by_gts_form.status(), StatusCode::OK);
    let by_gts_form_json = body_json(by_gts_form).await;

    assert_eq!(by_bare_uuid_json, by_gts_form_json);
    assert_eq!(by_bare_uuid_json["id"], id);
}

/// The anonymous GTS form contains no characters axum's router treats
/// specially in a single path segment (`~` and `.` are both valid in a
/// path segment), so no percent-encoding is actually required -- this
/// helper exists purely so the test above reads as "the wire form", not to
/// perform any real encoding.
fn urlencoding_slash_safe(s: &str) -> String {
    s.to_owned()
}

/// Two `POST`s from the same tenant resolving to the same
/// `(tenant_id, alias)` return `201` then `409`.
#[tokio::test]
async fn duplicate_alias_within_same_tenant_returns_409() {
    let state = Arc::new(OagwState::new(OagwConfig::default()));
    let tenant = Uuid::new_v4();
    let router = default_router_for(state, security_context(tenant));

    let body = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "dup.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    let first = router
        .clone()
        .oneshot(post_request("/oagw/v1/upstreams", body.clone()))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::CREATED);

    let second = router
        .oneshot(post_request("/oagw/v1/upstreams", body))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::CONFLICT);
    let json = body_json(second).await;
    assert_eq!(json["type"], "about:blank");
}

/// `GET`/`PUT`/`DELETE` never see another tenant's upstream: `404`,
/// identical shape for "does not exist" and "belongs to another tenant".
#[tokio::test]
async fn cross_tenant_visibility_is_404_not_found() {
    let state = Arc::new(OagwState::new(OagwConfig::default()));
    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();

    let router_a = default_router_for(state.clone(), security_context(tenant_a));
    let body = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "owned-by-a.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    let created = router_a
        .oneshot(post_request("/oagw/v1/upstreams", body))
        .await
        .unwrap();
    let created_json = body_json(created).await;
    let id = created_json["id"].as_str().unwrap().to_owned();

    let router_b = default_router_for(state.clone(), security_context(tenant_b));
    let get_resp = router_b
        .clone()
        .oneshot(get_request(&format!("/oagw/v1/upstreams/{id}")))
        .await
        .unwrap();
    assert_eq!(get_resp.status(), StatusCode::NOT_FOUND);

    let random_id = Uuid::new_v4();
    let get_missing = router_b
        .oneshot(get_request(&format!("/oagw/v1/upstreams/{random_id}")))
        .await
        .unwrap();
    assert_eq!(get_missing.status(), StatusCode::NOT_FOUND);
}

/// `GET /oagw/v1/upstreams` never returns ancestor/foreign rows, and
/// defaults/caps `$top`.
#[tokio::test]
async fn list_upstreams_is_tenant_scoped_and_respects_top_default_and_cap() {
    let state = Arc::new(OagwState::new(OagwConfig::default()));
    let tenant_a = Uuid::new_v4();
    let tenant_b = Uuid::new_v4();

    let router_a = default_router_for(state.clone(), security_context(tenant_a));
    for i in 0..3 {
        let body = serde_json::json!({
            "server": { "endpoints": [ { "scheme": "https", "host": format!("svc-{i}.example.com") } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        let resp = router_a
            .clone()
            .oneshot(post_request("/oagw/v1/upstreams", body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    let router_b = default_router_for(state.clone(), security_context(tenant_b));
    let list_b = router_b
        .oneshot(get_request("/oagw/v1/upstreams"))
        .await
        .unwrap();
    let list_b_json = body_json(list_b).await;
    assert_eq!(list_b_json.as_array().unwrap().len(), 0);

    let router_a2 = default_router_for(state, security_context(tenant_a));
    let list_a = router_a2
        .oneshot(get_request("/oagw/v1/upstreams"))
        .await
        .unwrap();
    let list_a_json = body_json(list_a).await;
    assert_eq!(list_a_json.as_array().unwrap().len(), 3);
}

/// RF-005: `$top`/`$skip` now share one parser with `GET /oagw/v1/routes`
/// (`src/api/rest/page_params.rs`); both endpoints must behave identically
/// on absent, valid, over-max, and malformed input. See
/// `src/api/rest/route_api.rs`'s
/// `list_top_and_skip_malformed_absent_and_over_max_match_the_upstreams_contract`
/// for the identical assertions against the routes endpoint.
#[tokio::test]
async fn list_upstreams_top_and_skip_malformed_absent_and_over_max_are_handled_per_the_shared_contract()
 {
    let state = Arc::new(OagwState::new(OagwConfig::default()));
    let tenant_id = Uuid::new_v4();
    let router = default_router_for(state.clone(), security_context(tenant_id));

    for i in 0..3 {
        let body = serde_json::json!({
            "server": { "endpoints": [ { "scheme": "https", "host": format!("svc-{i}.example.com") } ] },
            "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
        });
        let resp = router
            .clone()
            .oneshot(post_request("/oagw/v1/upstreams", body))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
    }

    // Absent: default $top = 50, all 3 rows returned.
    let absent = router
        .clone()
        .oneshot(get_request("/oagw/v1/upstreams"))
        .await
        .unwrap();
    assert_eq!(absent.status(), StatusCode::OK);
    assert_eq!(body_json(absent).await.as_array().unwrap().len(), 3);

    // Valid: $top=1 respected.
    let valid = router
        .clone()
        .oneshot(get_request("/oagw/v1/upstreams?%24top=1"))
        .await
        .unwrap();
    assert_eq!(valid.status(), StatusCode::OK);
    assert_eq!(body_json(valid).await.as_array().unwrap().len(), 1);

    // Over-max: $top clamped to 100, not rejected (only 3 rows exist).
    let over_max = router
        .clone()
        .oneshot(get_request("/oagw/v1/upstreams?%24top=1000"))
        .await
        .unwrap();
    assert_eq!(over_max.status(), StatusCode::OK);
    assert_eq!(body_json(over_max).await.as_array().unwrap().len(), 3);

    // Malformed $top: rejected with 400 -- already this endpoint's
    // documented behaviour, and now also `routes`'.
    let bad_top = router
        .clone()
        .oneshot(get_request("/oagw/v1/upstreams?%24top=notanumber"))
        .await
        .unwrap();
    assert_eq!(bad_top.status(), StatusCode::BAD_REQUEST);

    // Malformed $skip: rejected with 400.
    let bad_skip = router
        .oneshot(get_request("/oagw/v1/upstreams?%24skip=notanumber"))
        .await
        .unwrap();
    assert_eq!(bad_skip.status(), StatusCode::BAD_REQUEST);
}

/// `DELETE` returns `204`, and a subsequent `GET` for that id returns `404`.
#[tokio::test]
async fn delete_then_get_returns_404() {
    let state = Arc::new(OagwState::new(OagwConfig::default()));
    let tenant = Uuid::new_v4();
    let router = default_router_for(state, security_context(tenant));

    let body = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "deleteme.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    let created = router
        .clone()
        .oneshot(post_request("/oagw/v1/upstreams", body))
        .await
        .unwrap();
    let created_json = body_json(created).await;
    let id = created_json["id"].as_str().unwrap().to_owned();

    let deleted = router
        .clone()
        .oneshot(delete_request(&format!("/oagw/v1/upstreams/{id}")))
        .await
        .unwrap();
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);

    let get_after = router
        .oneshot(get_request(&format!("/oagw/v1/upstreams/{id}")))
        .await
        .unwrap();
    assert_eq!(get_after.status(), StatusCode::NOT_FOUND);
}

/// `PUT` resubmitting the same endpoints unchanged succeeds and preserves
/// the existing alias; changing the host so the derived alias would change
/// is rejected with `400`.
#[tokio::test]
async fn replace_upstream_preserves_alias_and_rejects_alias_changing_endpoint_edits() {
    let state = Arc::new(OagwState::new(OagwConfig::default()));
    let tenant = Uuid::new_v4();
    let router = default_router_for(state, security_context(tenant));

    let body = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "stable.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    let created = router
        .clone()
        .oneshot(post_request("/oagw/v1/upstreams", body.clone()))
        .await
        .unwrap();
    let created_json = body_json(created).await;
    let id = created_json["id"].as_str().unwrap().to_owned();

    let unchanged = router
        .clone()
        .oneshot(put_request(&format!("/oagw/v1/upstreams/{id}"), body))
        .await
        .unwrap();
    assert_eq!(unchanged.status(), StatusCode::OK);
    let unchanged_json = body_json(unchanged).await;
    assert_eq!(unchanged_json["alias"], "stable.example.com");

    let changed_body = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "changed.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    let changed = router
        .oneshot(put_request(
            &format!("/oagw/v1/upstreams/{id}"),
            changed_body,
        ))
        .await
        .unwrap();
    assert_eq!(changed.status(), StatusCode::BAD_REQUEST);
}

/// State machine: `PUT enabled: false` succeeds; a descendant-tenant
/// upstream sharing the same alias then cannot be set `enabled: true` while
/// the ancestor copy remains disabled.
#[tokio::test]
async fn descendant_cannot_re_enable_an_ancestor_disabled_upstream() {
    let state = Arc::new(OagwState::new(OagwConfig::default()));
    let ancestor_tenant = Uuid::new_v4();
    let child_tenant = Uuid::new_v4();

    #[derive(Debug)]
    struct FixedHierarchy(Uuid);
    impl TenantHierarchyProvider for FixedHierarchy {
        fn ancestors(&self, _tenant_id: Uuid) -> Vec<Uuid> {
            vec![self.0]
        }
    }

    let router_ancestor = router_for(
        state.clone(),
        security_context(ancestor_tenant),
        Arc::new(NoTenantHierarchy),
    );
    let body = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "shared.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    let created = router_ancestor
        .clone()
        .oneshot(post_request("/oagw/v1/upstreams", body.clone()))
        .await
        .unwrap();
    let created_json = body_json(created).await;
    let ancestor_id = created_json["id"].as_str().unwrap().to_owned();

    let mut disable_body = body.clone();
    disable_body["enabled"] = serde_json::json!(false);
    let disabled = router_ancestor
        .oneshot(put_request(
            &format!("/oagw/v1/upstreams/{ancestor_id}"),
            disable_body,
        ))
        .await
        .unwrap();
    assert_eq!(disabled.status(), StatusCode::OK);

    let router_child = router_for(
        state,
        security_context_with_scopes(child_tenant, vec!["oagw:upstream:bind".to_owned()]),
        Arc::new(FixedHierarchy(ancestor_tenant)),
    );
    let child_created = router_child
        .clone()
        .oneshot(post_request("/oagw/v1/upstreams", body.clone()))
        .await
        .unwrap();
    let child_json = body_json(child_created).await;
    let child_id = child_json["id"].as_str().unwrap().to_owned();

    let mut re_enable_body = body;
    re_enable_body["enabled"] = serde_json::json!(true);
    let blocked = router_child
        .oneshot(put_request(
            &format!("/oagw/v1/upstreams/{child_id}"),
            re_enable_body,
        ))
        .await
        .unwrap();
    assert_eq!(blocked.status(), StatusCode::BAD_REQUEST);
}

/// A create whose alias matches an ancestor's alias, submitted by a
/// principal holding `oagw:upstream:bind`, is created as a tenant-local
/// bind record; without that permission, it is `409 Conflict`.
#[tokio::test]
async fn ancestor_alias_bind_requires_the_bind_permission() {
    let state = Arc::new(OagwState::new(OagwConfig::default()));
    let ancestor_tenant = Uuid::new_v4();
    let child_tenant = Uuid::new_v4();

    #[derive(Debug)]
    struct FixedHierarchy(Uuid);
    impl TenantHierarchyProvider for FixedHierarchy {
        fn ancestors(&self, _tenant_id: Uuid) -> Vec<Uuid> {
            vec![self.0]
        }
    }

    let router_ancestor = router_for(
        state.clone(),
        security_context(ancestor_tenant),
        Arc::new(NoTenantHierarchy),
    );
    let body = serde_json::json!({
        "server": { "endpoints": [ { "scheme": "https", "host": "bindable.example.com" } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    let created = router_ancestor
        .oneshot(post_request("/oagw/v1/upstreams", body.clone()))
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::CREATED);

    let router_child_no_perm = router_for(
        state.clone(),
        security_context(child_tenant),
        Arc::new(FixedHierarchy(ancestor_tenant)),
    );
    let denied = router_child_no_perm
        .oneshot(post_request("/oagw/v1/upstreams", body.clone()))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::CONFLICT);

    let router_child_with_perm = router_for(
        state,
        security_context_with_scopes(child_tenant, vec!["oagw:upstream:bind".to_owned()]),
        Arc::new(FixedHierarchy(ancestor_tenant)),
    );
    let allowed = router_child_with_perm
        .oneshot(post_request("/oagw/v1/upstreams", body))
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::CREATED);
}

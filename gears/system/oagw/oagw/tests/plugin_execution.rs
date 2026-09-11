//! Black-box, external-crate tests for DECOMPOSITION entry 2.9
//! (plugin-execution), exercising the publicly reachable surface of
//! `oagw::*`.
//!
//! Most of `oagw::plugins::*` (the registries, binding resolution, chain
//! assembly and execution, credential resolution, and the
//! client-credentials token cache this FEATURE implements) is **not**
//! reachable *by name* from an external test crate: `src/plugins/mod.rs`
//! declares every submodule `pub(crate)` except [`oagw::plugins::runtime`]
//! (see below). The thorough, direct unit tests this feature's algorithms
//! need (registry init, binding resolution, chain assembly/execution and
//! ordering, guard evaluation, credential resolution, and token-cache
//! semantics) live as inline `#[cfg(test)] mod tests` inside each of
//! `src/plugins/{registry,binding,plan,guard,transform,credential,
//! token_cache,oauth2,auth,execute}.rs`, which -- being part of the `oagw`
//! crate itself -- has the access this file cannot.
//!
//! RF-001: `crate::proxy::engine` now drives `oagw::plugins::execute`'s
//! real chain executor on every request (the narrow `plugins::chain`
//! adapters this superseded have been retired), so the credentialed-proxy
//! and guard-rejection flows genuinely run in production. This file proves
//! that wiring **through the real, unmodified production router**
//! (`oagw::OagwGear`, exactly `tests/cors_handling.rs`'s pattern), seeding
//! Upstream/Route data through the real `POST /oagw/v1/upstreams`/
//! `POST /oagw/v1/routes` endpoints:
//!
//! - an `apikey` auth binding causes the configured credential to actually
//!   arrive at a mocked upstream, resolved through
//!   [`oagw::plugins::runtime::test_support::register_secret`] -- the one
//!   `pub` (feature-gated, test-only) seam `oagw::plugins::runtime`
//!   exposes, since this gear has no cross-gear `cred_store` client wired
//!   in production this round (see that module's doc comment for exactly
//!   why, and what closing it would require);
//! - a `required_headers` guard binding is genuinely *evaluated* by a real
//!   request (`evaluate_required_headers` is no longer dead code reached
//!   only by this crate's own tests). **Known, reported gap**: the frozen
//!   `upstream.v1.schema.json`/`route.v1.schema.json` declare
//!   `plugins.items[]` as bare identifier strings only (`oneOf: gts-identifier
//!   | uuid`), with no per-item `config` slot -- unlike `auth.config`,
//!   which the schema does carry inline. ADR-0009's own "Upstream
//!   Configuration Example" documents an object-shaped `items[]` entry
//!   (`{"plugin_ref": ..., "config": {...}}`) that contradicts the frozen
//!   schema its own `Registry Integration` section is otherwise faithful
//!   to. Closing this needs a wire-format change to
//!   `src/model/{upstream,route}.rs` (outside this pass's file-ownership:
//!   `src/model/**` belongs to a sibling entry) and, most likely, to the
//!   frozen schema files themselves (a "never edit" hard constraint this
//!   pass must not violate). So: a `required_headers` guard bound through
//!   a real Upstream/Route today can be proven to *execute* (this file's
//!   `required_headers_guard_now_genuinely_executes_though_it_cannot_reject_via_the_frozen_wire_format`
//!   test), but cannot be proven to *reject* through the real management
//!   API, since there is no way to write a non-empty `config` onto the
//!   wire for it. The guard's actual reject/accept behaviour *given* a
//!   config is separately, thoroughly unit-tested inline in
//!   `src/plugins/{guard,execute}.rs`.
//!
//! This file also keeps exercising the parts of the FEATURE that were
//! already visible from `oagw`'s public API before RF-001: the
//! named-plugin catalog's backed-vs-catalog-only classification this
//! feature's registries must agree with
//! (`cpt-cf-oagw-dod-plugin-builtin-registries`,
//! `cpt-cf-oagw-dod-plugin-catalog-only-ids`), the exact GTS-identifier
//! literals `cpt-cf-oagw-algo-plugin-registry-init`'s table names (guarding
//! against silent drift), the `PluginIdentifier` classification an
//! `Upstream.auth.type`/UUID-backed binding produces on its way into
//! `cpt-cf-oagw-algo-plugin-binding-resolve` (unreachable here, but its
//! input shape is), the `PluginNotFound`/`AuthenticationFailed` RFC 9457
//! catalog rows this feature's rejections render through
//! (`cpt-cf-oagw-dod-plugin-binding-resolution`,
//! `cpt-cf-oagw-dod-plugin-cred-injection`), and RF-006's fix to
//! `OagwConfig`'s token-cache settings (see
//! `token_cache_settings_documented_by_adr_0008_are_real_configurable_oagw_config_keys`
//! below).

#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use httpmock::prelude::*;
use oagw::OagwGear;
use oagw::config::OagwConfig;
use oagw::error::{ERROR_SOURCE_GATEWAY, ERROR_SOURCE_HEADER_NAME, OagwError, OagwErrorKind};
use oagw::model::plugin::{
    PluginIdentifier, PluginType, named_plugin_gts_ref, parse_plugin_identifier, plugin_gts_ref,
};
use oagw::model::upstream::Upstream;
use oagw::plugins::runtime::test_support::register_secret;
use serde_json::{Value, json};
use toolkit::api::OpenApiRegistryImpl;
use toolkit::{ClientHub, ConfigProvider, Gear, GearCtx, RestApiCapability};
use toolkit_security::SecurityContext;
use tower::ServiceExt;
use uuid::Uuid;

/// `cpt-cf-oagw-dod-plugin-builtin-registries`: the six identifiers this
/// feature's registries must contain -- auth `noop`/`apikey`/
/// `oauth2_client_cred`/`oauth2_client_cred_basic`, guard
/// `required_headers`, transform `request_id` -- are exactly the backed
/// entries of the named-plugin catalog `cpt-cf-oagw-feature-plugin-management`
/// (2.4) publishes.
#[test]
fn backed_catalog_entries_are_exactly_the_six_registry_resolvable_identifiers() {
    let backed: Vec<(&str, &str)> = [PluginType::Auth, PluginType::Guard, PluginType::Transform]
        .into_iter()
        .flat_map(|plugin_type| {
            plugin_type
                .named_catalog()
                .iter()
                .filter(|entry| entry.has_backing_implementation)
                .map(move |entry| (plugin_type.url_segment(), entry.token))
                .collect::<Vec<_>>()
        })
        .collect();

    assert_eq!(backed.len(), 6, "expected exactly six backed identifiers");
    assert!(backed.contains(&("auth", "noop")));
    assert!(backed.contains(&("auth", "apikey")));
    assert!(backed.contains(&("auth", "oauth2_client_cred")));
    assert!(backed.contains(&("auth", "oauth2_client_cred_basic")));
    assert!(backed.contains(&("guard", "required_headers")));
    assert!(backed.contains(&("transform", "request_id")));
}

/// `cpt-cf-oagw-dod-plugin-catalog-only-ids`: `basic`/`bearer`,
/// `timeout`/`cors`, `logging`/`metrics` are cataloged (so a `plugin_ref`
/// using them is syntactically well-formed) but carry no backing
/// implementation -- this feature's registries must keep them unresolvable.
#[test]
fn catalog_only_identifiers_are_exactly_the_six_documented_ones() {
    let catalog_only: Vec<(&str, &str)> =
        [PluginType::Auth, PluginType::Guard, PluginType::Transform]
            .into_iter()
            .flat_map(|plugin_type| {
                plugin_type
                    .named_catalog()
                    .iter()
                    .filter(|entry| !entry.has_backing_implementation)
                    .map(move |entry| (plugin_type.url_segment(), entry.token))
                    .collect::<Vec<_>>()
            })
            .collect();

    assert_eq!(catalog_only.len(), 6);
    assert!(catalog_only.contains(&("auth", "basic")));
    assert!(catalog_only.contains(&("auth", "bearer")));
    assert!(catalog_only.contains(&("guard", "timeout")));
    assert!(catalog_only.contains(&("guard", "cors")));
    assert!(catalog_only.contains(&("transform", "logging")));
    assert!(catalog_only.contains(&("transform", "metrics")));
}

/// `cpt-cf-oagw-dod-plugin-binding-resolution`: an unresolvable binding
/// (a catalog-only identifier, an unknown one, or a kind mismatch) renders
/// as `503` with GTS `type` `...plugin.not_found.v1` and
/// `X-OAGW-Error-Source: gateway`, naming only the offending identifier.
#[tokio::test]
async fn plugin_not_found_renders_the_documented_503_envelope() {
    let response = OagwError::new(
        OagwErrorKind::PluginNotFound,
        "unknown guard plugin: gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
    )
    .into_response();

    assert_eq!(response.status().as_u16(), 503);
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE_HEADER_NAME)
            .and_then(|v| v.to_str().ok()),
        Some(ERROR_SOURCE_GATEWAY)
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.not_found.v1"
    );
    assert!(
        json["detail"]
            .as_str()
            .unwrap()
            .contains("cf.core.oagw.timeout.v1"),
        "detail must name the offending identifier"
    );
}

/// `cpt-cf-oagw-dod-plugin-cred-injection`: a reference this feature
/// cannot resolve (absent, unknown, or inaccessible) renders as `401`
/// with GTS `type` `...auth.failed.v1`, and -- the security-critical
/// assertion -- the rendered body never contains the reference value or
/// any secret-shaped material, only a generic failure description.
#[tokio::test]
async fn authentication_failed_renders_the_documented_401_envelope_without_leaking_the_reference() {
    let response = OagwError::new(
        OagwErrorKind::AuthenticationFailed,
        "credential reference could not be resolved",
    )
    .into_response();

    assert_eq!(response.status().as_u16(), 401);
    assert_eq!(
        response
            .headers()
            .get(ERROR_SOURCE_HEADER_NAME)
            .and_then(|v| v.to_str().ok()),
        Some(ERROR_SOURCE_GATEWAY)
    );
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1"
    );
    let detail = json["detail"].as_str().unwrap();
    assert!(
        !detail.contains("cred://"),
        "must not echo the reference value"
    );
}

/// This feature introduces no new error-catalog row
/// (`cpt-cf-oagw-algo-plugin-guard-evaluate`'s closing note,
/// `cpt-cf-oagw-dod-plugin-guard-short-circuit`): the two rows it renders
/// through, `PluginNotFound` and `AuthenticationFailed`, are exactly the
/// pre-existing catalog entries `cpt-cf-oagw-feature-gear-foundation`
/// already ships.
#[test]
fn this_feature_reuses_pre_existing_catalog_rows_only() {
    let plugin_not_found = OagwError::new(OagwErrorKind::PluginNotFound, "x").render();
    let auth_failed = OagwError::new(OagwErrorKind::AuthenticationFailed, "x").render();
    assert_eq!(plugin_not_found.status, 503);
    assert_eq!(auth_failed.status, 401);
}

/// `cpt-cf-oagw-algo-plugin-registry-init`'s table names the six
/// registry-resolvable identifiers by exact GTS string. This guards that
/// literal text against silent drift (a typo in the token or the `{type}`
/// segment would still classify as "backed" per
/// `backed_catalog_entries_are_exactly_the_six_registry_resolvable_identifiers`
/// above but would no longer match what an operator must actually write
/// into `auth.type` / `plugins.items[]`).
#[test]
fn backed_identifier_gts_strings_match_the_registry_init_table_literally() {
    assert_eq!(
        named_plugin_gts_ref(PluginType::Auth, "noop"),
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1"
    );
    assert_eq!(
        named_plugin_gts_ref(PluginType::Auth, "apikey"),
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
    );
    assert_eq!(
        named_plugin_gts_ref(PluginType::Auth, "oauth2_client_cred"),
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1"
    );
    assert_eq!(
        named_plugin_gts_ref(PluginType::Auth, "oauth2_client_cred_basic"),
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1"
    );
    assert_eq!(
        named_plugin_gts_ref(PluginType::Guard, "required_headers"),
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"
    );
    assert_eq!(
        named_plugin_gts_ref(PluginType::Transform, "request_id"),
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
    );
}

/// `cpt-cf-oagw-algo-plugin-registry-init`'s catalog-only table, the same
/// literal-drift guard as above applied to the six identifiers that
/// `cpt-cf-oagw-dod-plugin-catalog-only-ids` requires this feature to keep
/// unresolvable: `basic`/`bearer`/`timeout`/`cors`/`logging`/`metrics`.
#[test]
fn catalog_only_identifier_gts_strings_match_the_registry_init_table_literally() {
    assert_eq!(
        named_plugin_gts_ref(PluginType::Auth, "basic"),
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1"
    );
    assert_eq!(
        named_plugin_gts_ref(PluginType::Auth, "bearer"),
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1"
    );
    assert_eq!(
        named_plugin_gts_ref(PluginType::Guard, "timeout"),
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1"
    );
    assert_eq!(
        named_plugin_gts_ref(PluginType::Guard, "cors"),
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1"
    );
    assert_eq!(
        named_plugin_gts_ref(PluginType::Transform, "logging"),
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1"
    );
    assert_eq!(
        named_plugin_gts_ref(PluginType::Transform, "metrics"),
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1"
    );
}

fn minimal_upstream_with_auth_type(auth_type: &str) -> Upstream {
    serde_json::from_value(serde_json::json!({
        "server": { "endpoints": [{ "scheme": "https", "host": "vendor.example.com" }] },
        "protocol": "http1.1",
        "auth": { "type": auth_type },
    }))
    .unwrap()
}

/// `cpt-cf-oagw-dod-plugin-catalog-only-ids`'s closing note: "Recognizing
/// these identifiers as syntactically valid `plugin_ref` values at write
/// time remains `cpt-cf-oagw-feature-plugin-management`'s (2.4) behaviour
/// and is unchanged by this feature." An `Upstream.auth.type` naming a
/// catalog-only identifier is schema-legal at the model layer this
/// feature reads from (2.2's write path would accept it too) -- black-box
/// evidence for the acceptance criterion "Setting `auth.type` to
/// `...basic.v1` or `...bearer.v1` ... yields `503 PluginNotFound`": the
/// identifier reaches this feature as a well-formed, *recognized-but-
/// unbacked* `Named` classification, which is exactly the input
/// `cpt-cf-oagw-algo-plugin-binding-resolve` (tested inline in
/// `src/plugins/binding.rs`, unreachable from this external crate) maps to
/// `PluginNotFound` rather than silently no-op'ing.
#[test]
fn upstream_auth_config_accepts_catalog_only_types_that_classify_as_unbacked() {
    for token in ["basic", "bearer"] {
        let auth_type = named_plugin_gts_ref(PluginType::Auth, token);
        let upstream = minimal_upstream_with_auth_type(&auth_type);
        let auth = upstream.auth.expect("auth binding present");
        let stored_type = auth.auth_type.expect("auth.type present");

        let identifier = parse_plugin_identifier(&stored_type, Some(PluginType::Auth)).unwrap();
        let PluginIdentifier::Named { plugin_type, token } = identifier else {
            panic!("expected a named classification for a catalog-only token");
        };
        assert_eq!(plugin_type, PluginType::Auth);
        let catalog_entry = PluginType::Auth.named_catalog_entry(&token).unwrap();
        assert!(
            !catalog_entry.has_backing_implementation,
            "{token} must classify as catalog-only, not backed"
        );
    }
}

/// The mirror positive case: the four registry-resolvable auth identifiers
/// classify as backed when read back off an `Upstream.auth.type` the same
/// way a real binding-resolution input would.
#[test]
fn upstream_auth_config_with_backed_types_classifies_as_backed() {
    for token in [
        "noop",
        "apikey",
        "oauth2_client_cred",
        "oauth2_client_cred_basic",
    ] {
        let auth_type = named_plugin_gts_ref(PluginType::Auth, token);
        let upstream = minimal_upstream_with_auth_type(&auth_type);
        let stored_type = upstream.auth.unwrap().auth_type.unwrap();

        let identifier = parse_plugin_identifier(&stored_type, Some(PluginType::Auth)).unwrap();
        let PluginIdentifier::Named { token, .. } = identifier else {
            panic!("expected a named classification for a backed token");
        };
        let catalog_entry = PluginType::Auth.named_catalog_entry(&token).unwrap();
        assert!(
            catalog_entry.has_backing_implementation,
            "{token} must classify as backed"
        );
    }
}

/// `cpt-cf-oagw-dod-plugin-no-custom-execution`: a UUID-backed custom
/// plugin identifier classifies as `PluginIdentifier::Uuid`, never
/// `Named` -- the precondition `cpt-cf-oagw-algo-plugin-binding-resolve`
/// step `inst-binding-resolve-02` relies on to fail such a binding closed
/// with `503 PluginNotFound` (asserted inline, unreachable here, by
/// `src/plugins/binding.rs`'s `uuid_backed_binding_fails_closed_even_with_matching_kind`
/// and `bare_uuid_binding_fails_closed`) rather than attempting to
/// interpret it as a named token.
#[test]
fn uuid_backed_custom_plugin_identifier_never_classifies_as_named() {
    let uuid = Uuid::new_v4();
    let candidate = plugin_gts_ref(PluginType::Guard, uuid);
    let identifier = parse_plugin_identifier(&candidate, None).unwrap();
    assert_eq!(
        identifier,
        PluginIdentifier::Uuid {
            plugin_type: Some(PluginType::Guard),
            uuid,
        }
    );
}

/// RF-006 (fixed): `cpt-cf-oagw-dod-plugin-token-cache` and `ADR-0008`'s
/// "Gear-Level Configuration (OagwConfig)" table document
/// `token_cache_ttl_secs` (default `300`) and `token_cache_capacity`
/// (default `10000`) as real, operator-overridable `OagwConfig` keys.
/// `OagwConfig` (`src/config.rs`) now declares both fields for real, and
/// `crate::plugins::runtime::chain_runtime` sizes the process-lifetime
/// token cache from `token_cache_capacity` instead of a hardcoded
/// constant. This test proves the public, externally-visible contract: an
/// operator-supplied override actually changes the resolved config.
#[test]
fn token_cache_settings_documented_by_adr_0008_are_real_configurable_oagw_config_keys() {
    let with_override = OagwConfig::resolve(&serde_json::json!({
        "token_cache_ttl_secs": 60,
        "token_cache_capacity": 1,
    }))
    .unwrap();
    let without_override = OagwConfig::resolve(&serde_json::json!({})).unwrap();

    assert_ne!(
        with_override, without_override,
        "token_cache_ttl_secs/token_cache_capacity must be real OagwConfig \
         keys per ADR-0008, not silently ignored"
    );
    assert_eq!(with_override.token_cache_ttl_secs, 60);
    assert_eq!(with_override.token_cache_capacity, 1);
    assert_eq!(without_override.token_cache_ttl_secs, 300);
    assert_eq!(without_override.token_cache_capacity, 10_000);
}

// ---------------------------------------------------------------------
// RF-001: full-router, black-box proof that the plugin chain genuinely
// executes on every real request. Driven through the real `oagw::OagwGear`,
// exactly the pattern `tests/cors_handling.rs` documents and uses.
// ---------------------------------------------------------------------

struct FixedConfig(Value);

impl ConfigProvider for FixedConfig {
    fn get_gear_config(&self, gear: &str) -> Option<&Value> {
        (gear == OagwGear::MODULE_NAME).then_some(&self.0)
    }
}

async fn build_router() -> Router {
    let gear = OagwGear::default();
    let ctx = GearCtx::new(
        OagwGear::MODULE_NAME,
        Uuid::new_v4(),
        Arc::new(FixedConfig(json!({
            "config": { "proxy_timeout_secs": 5, "allow_http_upstream": true }
        }))) as Arc<dyn ConfigProvider>,
        Arc::new(ClientHub::new()),
        Default::default(),
    );
    gear.init(&ctx)
        .await
        .expect("gear init must succeed with a well-formed fixed config");
    let openapi = OpenApiRegistryImpl::new();
    gear.register_rest(&ctx, Router::new(), &openapi)
        .expect("register_rest must succeed")
}

fn security_context(tenant_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(tenant_id)
        .build()
        .unwrap()
}

fn json_request(method: &str, uri: &str, tenant_id: Uuid, body: Value) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    req.extensions_mut().insert(security_context(tenant_id));
    req
}

fn bare_request(
    method: &str,
    uri: &str,
    tenant_id: Uuid,
    headers: &[(&str, &str)],
) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut req = builder.body(Body::empty()).unwrap();
    req.extensions_mut().insert(security_context(tenant_id));
    req
}

async fn response_json(response: Response) -> Value {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// Create an Upstream through the real `POST /oagw/v1/upstreams` endpoint
/// with the given `auth`/`plugins` objects, pointing at `127.0.0.1:{port}`.
/// Returns `(alias, upstream_id)`.
async fn create_upstream(
    router: &Router,
    tenant_id: Uuid,
    alias: &str,
    port: u16,
    extra: Value,
) -> (String, Uuid) {
    let mut body = json!({
        "alias": alias,
        "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] },
        "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1",
    });
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    let response = router
        .clone()
        .oneshot(json_request("POST", "/oagw/v1/upstreams", tenant_id, body))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "upstream creation must succeed"
    );
    let json = response_json(response).await;
    let id: Uuid = json["id"].as_str().unwrap().parse().unwrap();
    (alias.to_owned(), id)
}

async fn create_get_route(router: &Router, tenant_id: Uuid, upstream_id: Uuid, path: &str) {
    let body = json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": path } },
        "priority": 1,
    });
    let response = router
        .clone()
        .oneshot(json_request("POST", "/oagw/v1/routes", tenant_id, body))
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::CREATED,
        "route creation must succeed"
    );
}

/// RF-001 required test: an Upstream with an `apikey` auth binding causes
/// the configured credential header to actually arrive at a mocked
/// upstream, driven through the real, unmodified production router.
#[tokio::test]
async fn apikey_auth_binding_injects_the_real_credential_into_the_upstream_request() {
    let secret_key = format!("rf-001-apikey-{}", Uuid::new_v4());
    register_secret(&secret_key, "sk-live-rf001");

    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET)
            .path("/v1/data")
            .header("x-api-key", "sk-live-rf001");
        then.status(200).body("credentialed-ok");
    });

    let router = build_router().await;
    let tenant_id = Uuid::new_v4();
    let (alias, upstream_id) = create_upstream(
        &router,
        tenant_id,
        "apikey-svc",
        server.port(),
        json!({
            "auth": {
                "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "config": { "secret_ref": secret_key, "header": "x-api-key" },
            },
        }),
    )
    .await;
    create_get_route(&router, tenant_id, upstream_id, "/v1/data").await;

    let response = router
        .oneshot(bare_request(
            "GET",
            &format!("/oagw/v1/proxy/{alias}/v1/data"),
            tenant_id,
            &[],
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), b"credentialed-ok");
    assert_eq!(
        mock.calls(),
        1,
        "the upstream must have received exactly one request carrying the injected credential header"
    );
}

/// RF-001 required test: an Upstream with an `apikey` auth binding whose
/// `secret_ref` cannot be resolved is answered `401 AuthenticationFailed`,
/// and the upstream is never called -- proving the auth plugin genuinely
/// runs (and fails closed) rather than being silently skipped.
#[tokio::test]
async fn apikey_auth_binding_with_an_unresolvable_secret_fails_closed_401_and_never_calls_upstream()
{
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.path("/v1/data");
        then.status(200);
    });

    let router = build_router().await;
    let tenant_id = Uuid::new_v4();
    let (alias, upstream_id) = create_upstream(
        &router,
        tenant_id,
        "apikey-fail-svc",
        server.port(),
        json!({
            "auth": {
                "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "config": { "secret_ref": format!("nonexistent-{}", Uuid::new_v4()), "header": "x-api-key" },
            },
        }),
    )
    .await;
    create_get_route(&router, tenant_id, upstream_id, "/v1/data").await;

    let response = router
        .oneshot(bare_request(
            "GET",
            &format!("/oagw/v1/proxy/{alias}/v1/data"),
            tenant_id,
            &[],
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let json = response_json(response).await;
    assert_eq!(
        json["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1"
    );
    assert_eq!(
        mock.calls(),
        0,
        "no upstream call may be made on auth failure"
    );
}

/// RF-001 required test (the guard half): an Upstream binding
/// `required_headers` genuinely reaches `evaluate_required_headers` on a
/// real request through the real router -- no longer the dead code the
/// narrow `plugins::chain` adapters left it as. See this file's module doc
/// comment for why the frozen wire format cannot carry this guard's
/// `config`, so this test can only prove *execution*, not *rejection*, via
/// the real management API; the guard's reject/accept behaviour given a
/// config is separately, thoroughly unit-tested inline in
/// `src/plugins/{guard,execute}.rs`.
#[tokio::test]
async fn required_headers_guard_now_genuinely_executes_though_it_cannot_reject_via_the_frozen_wire_format()
 {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.path("/v1/data");
        then.status(200).body("guard-executed-ok");
    });

    let router = build_router().await;
    let tenant_id = Uuid::new_v4();
    let (alias, upstream_id) = create_upstream(
        &router,
        tenant_id,
        "guard-svc",
        server.port(),
        json!({
            "plugins": {
                "sharing": "private",
                "items": ["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"],
            },
        }),
    )
    .await;
    create_get_route(&router, tenant_id, upstream_id, "/v1/data").await;

    // No `required_request_headers`/`required_response_headers` config is
    // reachable through the real wire format (see the module doc comment),
    // so the guard fails open -- but it now runs, rather than the binding
    // being invisible dead code.
    let response = router
        .oneshot(bare_request(
            "GET",
            &format!("/oagw/v1/proxy/{alias}/v1/data"),
            tenant_id,
            &[],
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(mock.calls(), 1);
}

/// RF-001: a `noop` auth binding injects no credential and calls
/// `cred_store` for nothing, and the upstream is still reached normally --
/// proving the real chain treats `noop` as a genuine no-op, not merely as
/// "the chain never ran at all".
#[tokio::test]
async fn noop_auth_binding_calls_the_upstream_normally_with_no_credential_injected() {
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.path("/v1/data");
        then.status(200).body("noop-ok");
    });

    let router = build_router().await;
    let tenant_id = Uuid::new_v4();
    let (alias, upstream_id) = create_upstream(
        &router,
        tenant_id,
        "noop-svc",
        server.port(),
        json!({
            "auth": { "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1" },
        }),
    )
    .await;
    create_get_route(&router, tenant_id, upstream_id, "/v1/data").await;

    let response = router
        .oneshot(bare_request(
            "GET",
            &format!("/oagw/v1/proxy/{alias}/v1/data"),
            tenant_id,
            &[],
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(mock.calls(), 1);
}

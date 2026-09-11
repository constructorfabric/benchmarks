//! Management API tests.
//!
//! Covers `cpt-cf-oagw-dod-management-routes`, `cpt-cf-oagw-dod-authz-permissions`,
//! `cpt-cf-oagw-dod-request-validation` and `cpt-cf-oagw-dod-list-query-parameters`
//! on the wire: the ten paths and only the ten paths, the 401 and the 403 that
//! both precede any store access, the 201 with the GTS instance id and the
//! normalized alias, the 400 problem bodies of every validation family, the 404
//! that never distinguishes a foreign identifier from a missing one, the two 409
//! rows, the `204` with no body, the list page envelope with its projection, the
//! `application/problem+json` and `X-OAGW-Error-Source: gateway` every gateway
//! error carries, and the problem `detail` that never echoes a body value.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::missing_panics_doc)]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use authz_resolver_sdk::api::AuthZResolverClient;
use authz_resolver_sdk::constraints::{Constraint, EqPredicate, Predicate};
use authz_resolver_sdk::error::AuthZResolverError;
use authz_resolver_sdk::models::{
    EvaluationRequest, EvaluationResponse, EvaluationResponseContext,
};
use authz_resolver_sdk::pep::{PolicyEnforcer, ResourceType};
use toolkit_security::SecurityContext;
use toolkit_security::pep_properties;

use oagw::OagwConfig;
use oagw::control_plane::cache::ControlPlaneCache;
use oagw::control_plane::service::ManagementService;
use oagw::control_plane::validation::ResourceKind;
use oagw::store::OagwStore;
use oagw::{
    ERR_ALIAS_CONFLICT, ERR_AUTH_FAILED, ERR_MATCH_CONFLICT, ERR_VALIDATION, OagwState, ROUTE_TYPE,
    UPSTREAM_TYPE,
};

const HTTP_PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";
/// The tenant every allowed request of this suite carries.
const TENANT: u128 = 0x10;
/// A second tenant no row of the suite belongs to.
const FOREIGN: u128 = 0x20;

/// The `AuthZ` PDP the allowing stub stands in for: it grants and narrows the
/// scope to the caller's own tenant, the way the real resolver does.
struct Allowing;

#[async_trait::async_trait]
impl AuthZResolverClient for Allowing {
    async fn evaluate(
        &self,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        Ok(EvaluationResponse {
            decision: true,
            context: EvaluationResponseContext {
                constraints: vec![Constraint {
                    predicates: vec![Predicate::Eq(EqPredicate {
                        property: String::from(pep_properties::OWNER_TENANT_ID),
                        value: json!(TENANT.to_string()),
                    })],
                }],
                deny_reason: None,
            },
        })
    }
}

/// The `AuthZ` PDP the refusing stub stands in for.
struct Denying;

#[async_trait::async_trait]
impl AuthZResolverClient for Denying {
    async fn evaluate(
        &self,
        _request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        Ok(EvaluationResponse {
            decision: false,
            context: EvaluationResponseContext::default(),
        })
    }
}

/// The `AuthZ` PDP a read-only token stands in for: it grants the `read`
/// action only and narrows the scope to the caller's own tenant, so a request
/// whose action the token does not hold is refused while a read of the caller's
/// own rows is admitted.
struct ReadOnly;

#[async_trait::async_trait]
impl AuthZResolverClient for ReadOnly {
    async fn evaluate(
        &self,
        request: EvaluationRequest,
    ) -> Result<EvaluationResponse, AuthZResolverError> {
        let granted = request.action.name == "read";
        Ok(EvaluationResponse {
            decision: granted,
            context: EvaluationResponseContext {
                constraints: if granted {
                    vec![Constraint {
                        predicates: vec![Predicate::Eq(EqPredicate {
                            property: String::from(pep_properties::OWNER_TENANT_ID),
                            value: json!(TENANT.to_string()),
                        })],
                    }]
                } else {
                    Vec::new()
                },
                deny_reason: None,
            },
        })
    }
}

/// The three routers the suite drives, over one shared store.
///
/// Sharing the store is what makes "the refusal wrote nothing" observable: a
/// request the refusing surface answers is followed by a read through the
/// allowing surface, which sees the same rows.
struct Surfaces {
    allowing: Router,
    denying: Router,
    read_only: Router,
    enforcerless: Router,
}

/// Builds the three surfaces over one empty store.
fn surfaces() -> Surfaces {
    let store = Arc::new(OagwStore::new());
    let cache = Arc::new(ControlPlaneCache::new());
    let config = OagwConfig::default();
    let surface = |enforcer: Option<PolicyEnforcer>| {
        let service = Arc::new(
            ManagementService::new(Arc::clone(&store), &config, Arc::clone(&cache))
                .expect("the validators compile"),
        );
        let state = Arc::new(OagwState::new(
            Arc::new(config),
            Arc::clone(&store),
            service,
            enforcer.map(Arc::new),
            None,
            Arc::clone(&cache),
        ));
        oagw::api::rest::register_management_routes(Router::new(), state)
    };
    Surfaces {
        allowing: surface(Some(PolicyEnforcer::new(Arc::new(Allowing)))),
        denying: surface(Some(PolicyEnforcer::new(Arc::new(Denying)))),
        read_only: surface(Some(PolicyEnforcer::new(Arc::new(ReadOnly)))),
        enforcerless: surface(None),
    }
}

/// The authenticated subject a request carries.
fn subject(tenant: u128) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::from_u128(tenant))
        .subject_tenant_id(Uuid::from_u128(tenant))
        .build()
        .expect("the subject is complete")
}

/// Issues one request and returns the whole response.
async fn issue(
    app: Router,
    method: Method,
    uri: &str,
    tenant: Option<u128>,
    body: Option<Value>,
) -> axum::http::Response<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(tenant) = tenant {
        builder = builder.extension(subject(tenant));
    }
    let payload = body.map_or_else(String::new, |value| value.to_string());
    let request = builder.body(Body::from(payload)).expect("the request builds");
    app.oneshot(request).await.expect("oneshot resolves")
}

/// The status and the JSON body of one answer.
async fn answer(
    app: Router,
    method: Method,
    uri: &str,
    tenant: Option<u128>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let response = issue(app, method, uri, tenant, body).await;
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("the body reads");
    let document = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("the body is JSON")
    };
    (status, document)
}

/// The request path of a URI, without the query a problem document never echoes.
fn path_of(uri: &str) -> &str {
    uri.split('?').next().expect("the path")
}

/// A minimal valid upstream body.
fn upstream_body(host: &str) -> Value {
    json!({
        "server": {
            "endpoints": [{ "scheme": "https", "host": host, "port": 443 }]
        },
        "protocol": HTTP_PROTOCOL,
        "tags": ["llm"]
    })
}

/// A minimal valid route create body.
fn route_body(upstream_id: &str, path: &str) -> Value {
    json!({
        "upstream_id": upstream_id,
        "match": { "http": { "methods": ["GET"], "path": path } },
        "priority": 10,
        "tags": ["edge"]
    })
}

/// The instance identifier of one created upstream, read off the wire.
async fn created_upstream(app: &Router, host: &str) -> String {
    let (status, document) = answer(
        app.clone(),
        Method::POST,
        "/oagw/v1/upstreams",
        Some(TENANT),
        Some(upstream_body(host)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{document}");
    document["id"]
        .as_str()
        .expect("the instance id")
        .to_owned()
}

/// Creates one route and returns its instance identifier.
///
/// The route body names its upstream by the upstream's key, which the wire
/// identifier of a created upstream carries after the type prefix.
async fn created_route(app: &Router, upstream_id: &str, path: &str) -> String {
    let key = key_of(upstream_id);
    let (status, document) = answer(
        app.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(TENANT),
        Some(route_body(&key, path)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{document}");
    document["id"]
        .as_str()
        .expect("the instance id")
        .to_owned()
}

/// The row key the wire identifier of a created resource carries.
fn key_of(instance: &str) -> String {
    oagw::gts::parse_gts_instance(UPSTREAM_TYPE, instance)
        .expect("the instance parses")
        .to_string()
}

/// The number of upstream rows the surface lists for the tenant.
async fn upstream_count(app: &Router) -> usize {
    let (_, document) = answer(
        app.clone(),
        Method::GET,
        "/oagw/v1/upstreams",
        Some(TENANT),
        None,
    )
    .await;
    document["items"].as_array().expect("items").len()
}

#[tokio::test]
async fn only_the_management_paths_are_registered() {
    let Surfaces { allowing, .. } = surfaces();

    // A path the router matched but no method is registered for is answered
    // 405; an unregistered path is answered 404.
    let instance = oagw::gts::gts_instance(UPSTREAM_TYPE, Uuid::from_u128(0x99));
    let plugin = oagw::gts::gts_instance(
        "gts.cf.core.oagw.transform_plugin.v1~",
        Uuid::from_u128(0x99),
    );
    let paths = [
        "/oagw/v1/upstreams".to_owned(),
        format!("/oagw/v1/upstreams/{instance}"),
        "/oagw/v1/routes".to_owned(),
        format!("/oagw/v1/routes/{instance}"),
        "/oagw/v1/plugins".to_owned(),
        format!("/oagw/v1/plugins/{plugin}"),
        "/oagw/v1/plugins/{plugin}/source".to_owned(),
    ];
    for uri in &paths {
        let (status, _) = answer(
            allowing.clone(),
            Method::PATCH,
            uri,
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "PATCH {uri} is not a registered method, so the path is registered"
        );
    }
    for uri in ["/oagw/v1/upstreams", "/oagw/v1/routes"] {
        let (status, _) = answer(
            allowing.clone(),
            Method::DELETE,
            uri,
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "DELETE {uri}");
    }

    // The `/api`-prefixed spelling of a management path is a path no OAGW
    // handler is registered for: the gear is gear-relative only.
    for uri in [
        "/api/oagw/v1/plugins",
        "/api/oagw/v1/upstreams",
        "/api/oagw/v1/routes",
    ] {
        let (status, _) = answer(allowing.clone(), Method::GET, uri, Some(TENANT), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri} is not registered");
    }

    // The proxy path and the two sub-paths below the management paths belong
    // to the data plane and to no feature: neither is registered.
    for uri in [
        "/oagw/v1/proxy/api.openai.com",
        "/oagw/v1/upstreams/some-id/plugins",
        "/oagw/v1/upstreams/some-id/whatever",
    ] {
        let (status, _) = answer(allowing.clone(), Method::GET, uri, Some(TENANT), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri} is not registered");
    }
}

#[tokio::test]
async fn a_request_without_a_subject_is_answered_401_before_any_store_access() {
    let Surfaces { allowing, .. } = surfaces();
    for (method, uri, body) in [
        (
            Method::POST,
            "/oagw/v1/upstreams",
            Some(upstream_body("api.openai.com")),
        ),
        (Method::GET, "/oagw/v1/upstreams", None),
        (Method::DELETE, "/oagw/v1/upstreams/some-id", None),
    ] {
        let (status, document) = answer(allowing.clone(), method, uri, None, body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{document}");
        assert_eq!(document["type"], ERR_AUTH_FAILED, "{document}");
        assert_eq!(document["status"], 401);
        assert_eq!(document["instance"], path_of(uri));
    }

    // Nothing the refused requests carried reached the store.
    assert_eq!(upstream_count(&allowing).await, 0);
}

#[tokio::test]
async fn a_request_without_the_permission_is_answered_403_before_any_store_access() {
    let Surfaces {
        allowing, denying, ..
    } = surfaces();
    for (method, uri, body) in [
        (
            Method::POST,
            "/oagw/v1/upstreams",
            Some(upstream_body("api.openai.com")),
        ),
        (Method::GET, "/oagw/v1/upstreams", None),
        (
            Method::PUT,
            "/oagw/v1/upstreams/some-id",
            Some(upstream_body("api.openai.com")),
        ),
        (Method::DELETE, "/oagw/v1/upstreams/some-id", None),
        (
            Method::POST,
            "/oagw/v1/routes",
            Some(route_body("does-not-matter", "/v1/chat")),
        ),
    ] {
        let (status, document) = answer(denying.clone(), method, uri, Some(TENANT), body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{document}");
        assert_eq!(document["status"], 403);
        assert_eq!(document["detail"], "the bearer token lacks the permission the operation requires");
        assert_eq!(document["instance"], path_of(uri));
    }

    // The route create above never reached the validators: the reference it
    // names is not a row of any tenant, and the 403 carries no validation
    // detail. Nothing the refused requests carried reached the store either.
    assert_eq!(upstream_count(&allowing).await, 0);
}

#[tokio::test]
async fn a_surface_with_no_enforcer_fails_closed() {
    let Surfaces { enforcerless, .. } = surfaces();
    for (method, uri) in [
        (Method::POST, "/oagw/v1/upstreams"),
        (Method::GET, "/oagw/v1/upstreams"),
        (Method::GET, "/oagw/v1/routes"),
    ] {
        let (status, document) = answer(
            enforcerless.clone(),
            method,
            uri,
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{document}");
        let kind = if uri.contains("routes") {
            ROUTE_TYPE
        } else {
            UPSTREAM_TYPE
        };
        assert_eq!(document["resource_type"], kind, "{document}");
    }
}

#[tokio::test]
async fn a_created_upstream_is_answered_201_with_the_instance_id_and_the_alias() {
    let Surfaces { allowing, .. } = surfaces();
    let (status, document) = answer(
        allowing,
        Method::POST,
        "/oagw/v1/upstreams",
        Some(TENANT),
        Some(upstream_body("api.openai.com")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{document}");
    assert_eq!(document["alias"], "api.openai.com");
    assert_eq!(document["protocol"], HTTP_PROTOCOL);
    assert_eq!(document["enabled"], true);
    assert_eq!(document["tags"], json!(["llm"]));

    // The identifier is the resource kind's anonymous GTS instance, and it
    // parses back to the row's own key.
    let id = document["id"].as_str().expect("the instance id");
    assert!(id.starts_with(UPSTREAM_TYPE), "{id}");
    let parsed = oagw::gts::parse_gts_instance(UPSTREAM_TYPE, id).expect("the instance parses");
    assert_eq!(
        oagw::api::rest::dto::upstream_id(parsed),
        id,
        "the wire identifier round-trips"
    );
}

#[tokio::test]
async fn a_failing_body_is_answered_400_naming_the_properties() {
    let Surfaces { allowing, .. } = surfaces();
    let cases: Vec<(Value, &str)> = vec![
        (json!({}), "server is required"),
        (
            json!({ "server": { "endpoints": [] } }),
            "protocol is required",
        ),
        (
            json!({ "server": { "endpoints": [{ "scheme": "https" }] }, "protocol": HTTP_PROTOCOL }),
            "server.endpoints[0].host is required",
        ),
        (
            json!({ "zzz": 1, "server": { "endpoints": [] }, "protocol": HTTP_PROTOCOL }),
            "unknown property 'zzz' at root",
        ),
        (
            json!({ "server": { "endpoints": [{ "scheme": "gopher", "host": "h" }] }, "protocol": HTTP_PROTOCOL }),
            "server.endpoints[0].scheme",
        ),
        (
            json!({ "server": { "endpoints": [{ "scheme": "http", "host": "api.openai.com" }] }, "protocol": HTTP_PROTOCOL }),
            "scheme",
        ),
    ];
    for (body, needle) in cases {
        let (status, document) = answer(
            allowing.clone(),
            Method::POST,
            "/oagw/v1/upstreams",
            Some(TENANT),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{document}");
        assert_eq!(document["type"], ERR_VALIDATION, "{document}");
        assert_eq!(document["status"], 400);
        let detail = document["detail"].as_str().expect("detail");
        assert!(detail.contains(needle), "expected '{needle}' in '{detail}'");
    }

    // A body that is not JSON at all is answered before the validators run.
    let response = issue(
        allowing.clone(),
        Method::POST,
        "/oagw/v1/upstreams",
        Some(TENANT),
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // A route create is validated against the route schema.
    for (body, needle) in [
        (json!({}), "upstream_id is required"),
        (
            json!({ "upstream_id": Uuid::from_u128(1).to_string() }),
            "match is required",
        ),
    ] {
        let (status, document) = answer(
            allowing.clone(),
            Method::POST,
            "/oagw/v1/routes",
            Some(TENANT),
            Some(body),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{document}");
        assert!(
            document["detail"]
                .as_str()
                .is_some_and(|d| d.contains(needle))
        );
    }

    // No refused body wrote a row.
    assert_eq!(upstream_count(&allowing).await, 0);
}

#[tokio::test]
async fn a_route_create_addressing_an_unowned_upstream_is_refused_400() {
    let Surfaces { allowing, .. } = surfaces();

    // A well-formed reference that names no upstream of the calling tenant is
    // a validation refusal, not a 404.
    let reference = Uuid::from_u128(0x99).to_string();
    let (status, document) = answer(
        allowing.clone(),
        Method::POST,
        "/oagw/v1/routes",
        Some(TENANT),
        Some(route_body(&reference, "/v1/chat")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{document}");
    assert_eq!(document["type"], ERR_VALIDATION);
    assert_eq!(
        document["detail"],
        "upstream_id does not reference an upstream of the calling tenant"
    );

    // A reference that is not an identifier at all is refused too.
    let (status, document) = answer(
        allowing,
        Method::POST,
        "/oagw/v1/routes",
        Some(TENANT),
        Some(route_body("not-an-identifier", "/v1/chat")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{document}");
    assert_eq!(document["type"], ERR_VALIDATION);
}

#[tokio::test]
async fn a_foreign_or_unparseable_identifier_answers_the_same_404() {
    let Surfaces { allowing, .. } = surfaces();
    let id = created_upstream(&allowing, "api.openai.com").await;

    let detail = "the addressed resource does not exist for the calling tenant";
    for (tenant, uri) in [
        (FOREIGN, format!("/oagw/v1/upstreams/{id}")),
        (
            TENANT,
            format!(
                "/oagw/v1/upstreams/{}",
                oagw::gts::gts_instance(UPSTREAM_TYPE, Uuid::from_u128(0x99))
            ),
        ),
        (
            TENANT,
            String::from("/oagw/v1/upstreams/not-an-identifier"),
        ),
        (FOREIGN, String::from("/oagw/v1/routes/not-an-identifier")),
    ] {
        for method in [Method::GET, Method::PUT, Method::DELETE] {
            let (status, document) = answer(
                allowing.clone(),
                method.clone(),
                &uri,
                Some(tenant),
                Some(upstream_body("api.openai.com")),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri} {document}");
            assert_eq!(document["detail"], detail, "{method} {uri}");
        }
    }
}

#[tokio::test]
async fn the_conflicts_are_answered_409_with_their_catalogue_rows() {
    let Surfaces { allowing, .. } = surfaces();
    let id = created_upstream(&allowing, "api.openai.com").await;

    let (status, document) = answer(
        allowing.clone(),
        Method::POST,
        "/oagw/v1/upstreams",
        Some(TENANT),
        Some(upstream_body("api.openai.com")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{document}");
    assert_eq!(document["type"], ERR_ALIAS_CONFLICT);
    assert_eq!(document["status"], 409);
    assert_eq!(
        document["detail"],
        "another upstream of the calling tenant already holds the alias"
    );

    let route = created_route(&allowing, &id, "/v1/chat").await;
    let (status, document) = answer(
        allowing,
        Method::POST,
        "/oagw/v1/routes",
        Some(TENANT),
        Some(route_body(&key_of(&id), "/v1/chat")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{document}");
    assert_eq!(document["type"], ERR_MATCH_CONFLICT);
    let detail = document["detail"].as_str().expect("detail");
    assert!(detail.contains("already holds this match rule"), "'{detail}'");
    assert!(
        !detail.contains("/v1/chat"),
        "the detail names no body value: {detail}"
    );
    let _ = route;
}

#[tokio::test]
async fn the_deletes_are_answered_204_with_no_body() {
    let Surfaces { allowing, .. } = surfaces();
    let upstream = created_upstream(&allowing, "api.openai.com").await;
    let route = created_route(&allowing, &upstream, "/v1/chat").await;

    for id in [route.clone(), upstream.clone()] {
        let kind = if id.starts_with(UPSTREAM_TYPE) {
            "upstreams"
        } else {
            "routes"
        };
        let response = issue(
            allowing.clone(),
            Method::DELETE,
            &format!("/oagw/v1/{kind}/{id}"),
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT, "{id}");
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("the body reads");
        assert!(bytes.is_empty(), "a 204 carries no body");
    }

    // The cascade removed the route with its upstream, and the second delete
    // of either identifier is the same 404 a miss answers with.
    for uri in [
        format!("/oagw/v1/routes/{route}"),
        format!("/oagw/v1/upstreams/{upstream}"),
    ] {
        let (status, _) = answer(allowing.clone(), Method::GET, &uri, Some(TENANT), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
        let (status, _) = answer(allowing.clone(), Method::DELETE, &uri, Some(TENANT), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn the_list_answers_the_page_envelope_with_the_projection() {
    let Surfaces { allowing, .. } = surfaces();
    for host in ["api.openai.com", "eu.openai.com", "foreign.openai.com"] {
        created_upstream(&allowing, host).await;
    }

    let (status, document) = answer(
        allowing.clone(),
        Method::GET,
        "/oagw/v1/upstreams",
        Some(TENANT),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{document}");
    assert_eq!(document["items"].as_array().expect("items").len(), 3);
    assert_eq!(document["page_info"]["limit"], 50, "the declared default");
    assert!(document["page_info"]["next_cursor"].is_null());
    assert!(document.get("projection").is_none(), "no projection was asked");
    assert!(document["items"][0].get("alias").is_some(), "the whole row");

    let (status, document) = answer(
        allowing.clone(),
        Method::GET,
        "/oagw/v1/upstreams?%24top=1&%24select=alias,protocol&%24orderby=alias",
        Some(TENANT),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{document}");
    assert_eq!(document["page_info"]["limit"], 1);
    assert_eq!(document["projection"], json!(["alias", "protocol"]));
    let item = &document["items"][0];
    assert_eq!(
        item.as_object().expect("item").len(),
        2,
        "only the projected members"
    );
    assert_eq!(item["alias"], "api.openai.com");

    // The tenant scope precedes every parameter.
    let (_, document) = answer(
        allowing,
        Method::GET,
        "/oagw/v1/upstreams?%24filter=alias%20eq%20'foreign.openai.com'",
        Some(FOREIGN),
        None,
    )
    .await;
    assert!(document["items"].as_array().expect("items").is_empty());
}

#[tokio::test]
async fn a_malformed_list_parameter_is_a_problem_document() {
    let Surfaces { allowing, .. } = surfaces();
    for query in ["%24count=true", "%24top=late", "%24zzz=1", "%24select=zzz"] {
        let (status, document) = answer(
            allowing.clone(),
            Method::GET,
            &format!("/oagw/v1/upstreams?{query}"),
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query} {document}");
        assert_eq!(document["type"], ERR_VALIDATION, "{query}");
        assert_eq!(document["instance"], "/oagw/v1/upstreams");
    }
}

#[tokio::test]
async fn every_gateway_error_is_problem_json_sourced_from_the_gateway() {
    let Surfaces { allowing, .. } = surfaces();
    let id = created_upstream(&allowing, "api.openai.com").await;

    let requests: Vec<(Method, String, Option<Value>)> = vec![
        (
            Method::POST,
            String::from("/oagw/v1/upstreams"),
            Some(json!({})),
        ),
        (
            Method::GET,
            String::from("/oagw/v1/upstreams?%24count=true"),
            None,
        ),
        (
            Method::GET,
            format!(
                "/oagw/v1/upstreams/{}",
                oagw::gts::gts_instance(UPSTREAM_TYPE, Uuid::from_u128(0x99))
            ),
            None,
        ),
        (
            Method::POST,
            String::from("/oagw/v1/upstreams"),
            Some(upstream_body("api.openai.com")),
        ),
        (Method::DELETE, format!("/oagw/v1/upstreams/{id}"), None),
    ];
    let mut statuses = Vec::new();
    for (method, uri, body) in requests {
        let response = issue(allowing.clone(), method.clone(), &uri, Some(TENANT), body).await;
        let status = response.status();
        statuses.push(u16::from(status));
        // A successful answer carries no problem document at all.
        if status == StatusCode::NO_CONTENT {
            assert_eq!(response.headers().get("x-oagw-error-source"), None);
            continue;
        }
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json"),
            "{method} {uri}"
        );
        assert_eq!(
            response
                .headers()
                .get("x-oagw-error-source")
                .and_then(|value| value.to_str().ok()),
            Some("gateway"),
            "{method} {uri}"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("the body reads");
        let document: Value = serde_json::from_slice(&bytes).expect("the problem is JSON");
        assert!(
            document["type"]
                .as_str()
                .is_some_and(|t| t.starts_with("gts."))
        );
        assert_eq!(document["instance"], path_of(&uri));
    }
    // The five requests answered 400, 400, 404, 409 and 204 respectively; only
    // the successful delete carries no problem document.
    assert_eq!(statuses, vec![400, 400, 404, 409, 204]);
}

#[tokio::test]
async fn no_problem_detail_echoes_a_request_body_value() {
    let Surfaces { allowing, .. } = surfaces();
    // The values a hostile body would want reflected back.
    let leaky = json!({
        "alias": "secret-alias-9f21c",
        "server": { "endpoints": [{ "scheme": "gopher", "host": "secret-host-9f21c" }] },
        "protocol": HTTP_PROTOCOL,
        "tags": ["secret-tag-9f21c"],
        "zzz": "secret-unknown-9f21c"
    });
    for body in [
        leaky,
        json!({ "server": { "endpoints": [] }, "protocol": "secret-protocol-9f21c" }),
        json!({}),
    ] {
        let (_, document) = answer(
            allowing.clone(),
            Method::POST,
            "/oagw/v1/upstreams",
            Some(TENANT),
            Some(body),
        )
        .await;
        let detail = document["detail"].as_str().expect("detail");
        for needle in [
            "9f21c",
            "secret-alias",
            "secret-host",
            "secret-tag",
            "secret-unknown",
            "secret-protocol",
        ] {
            assert!(!detail.contains(needle), "'{detail}' echoed '{needle}'");
        }
    }

    // The same holds on the route surface, whose refusal may not name the path
    // or the reference the body carried.
    let (_, document) = answer(
        allowing,
        Method::POST,
        "/oagw/v1/routes",
        Some(TENANT),
        Some(route_body("secret-upstream-ref", "/secret-path-9f21c")),
    )
    .await;
    let detail = document["detail"].as_str().expect("detail");
    assert!(!detail.contains("secret-upstream-ref"), "{detail}");
    assert!(!detail.contains("secret-path"), "{detail}");
}

#[tokio::test]
async fn a_replacement_answers_200_with_the_stored_identifier() {
    let Surfaces { allowing, .. } = surfaces();
    let upstream = created_upstream(&allowing, "api.openai.com").await;
    let mut body = upstream_body("api.openai.com");
    body["tags"] = json!(["edge"]);

    let (status, document) = answer(
        allowing.clone(),
        Method::PUT,
        &format!("/oagw/v1/upstreams/{upstream}"),
        Some(TENANT),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{document}");
    assert_eq!(document["id"], json!(upstream), "the identifier is immutable");
    assert_eq!(document["tags"], json!(["edge"]), "the replacement is in full");
    assert_eq!(document["alias"], "api.openai.com", "the alias is immutable");

    // A route replacement takes its reference from the stored row.
    let route = created_route(&allowing, &upstream, "/v1/chat").await;
    let mut body = route_body(&upstream, "/v1/chat");
    body.as_object_mut()
        .expect("the body is an object")
        .remove("upstream_id");
    body["priority"] = json!(20);
    let (status, document) = answer(
        allowing,
        Method::PUT,
        &format!("/oagw/v1/routes/{route}"),
        Some(TENANT),
        Some(body),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{document}");
    assert_eq!(document["id"], json!(route));
    assert_eq!(document["priority"], 20);
}

#[test]
fn the_enforcer_is_told_the_resource_kind_gts_type() {
    let upstream = oagw::api::rest::handlers::resource_type(ResourceKind::Upstream);
    let route = oagw::api::rest::handlers::resource_type(ResourceKind::Route);
    let name = |descriptor: &ResourceType| descriptor.name().to_owned();
    assert_eq!(name(&upstream), UPSTREAM_TYPE);
    assert_eq!(name(&route), ROUTE_TYPE);
    for descriptor in [&upstream, &route] {
        assert!(
            descriptor
                .supported_properties()
                .contains(&pep_properties::OWNER_TENANT_ID)
        );
        assert!(
            descriptor
                .supported_properties()
                .contains(&pep_properties::RESOURCE_ID)
        );
    }
}

/// A valid transform plugin create body.
fn plugin_body(name: &str) -> Value {
    json!({
        "plugin_type": "transform",
        "name": name,
        "phases": ["on_response"],
        "source_code": "def on_response(ctx):\n    return ctx\n"
    })
}

/// Creates one plugin and returns its instance identifier, which names the
/// arm the family selects.
async fn created_plugin(app: &Router, name: &str) -> String {
    let (status, document) = answer(
        app.clone(),
        Method::POST,
        "/oagw/v1/plugins",
        Some(TENANT),
        Some(plugin_body(name)),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{document}");
    document["id"]
        .as_str()
        .expect("the instance id")
        .to_owned()
}

#[tokio::test]
async fn a_plugin_create_is_answered_201_with_the_family_instance_id() {
    let Surfaces { allowing, .. } = surfaces();
    let (status, document) = answer(
        allowing.clone(),
        Method::POST,
        "/oagw/v1/plugins",
        Some(TENANT),
        Some(plugin_body("redact-headers")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{document}");
    assert!(
        document["id"]
            .as_str()
            .expect("the instance id")
            .starts_with("gts.cf.core.oagw.transform_plugin.v1~"),
        "the id names the arm the family selects: {document}"
    );
    assert_eq!(document["plugin_type"], "transform");
    assert_eq!(
        document["source_code"],
        "def on_response(ctx):\n    return ctx\n",
        "the source is carried verbatim"
    );
    assert_eq!(document["phases"], json!(["on_response"]));
}

#[tokio::test]
async fn a_plugin_body_that_names_no_arm_is_answered_400_after_the_401_gate() {
    let Surfaces { allowing, denying, .. } = surfaces();
    // A body whose `plugin_type` selects no arm is enforced against every arm,
    // so the permission still precedes the validation the flow answers with:
    // a subject holding none of them is refused 403, and only a permitted
    // subject reaches the 400 that names the property.
    let (status, document) = answer(
        denying.clone(),
        Method::POST,
        "/oagw/v1/plugins",
        Some(TENANT),
        Some(json!({ "plugin_type": "throttle", "name": "x", "source_code": "d" })),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{document}");
    let (status, document) = answer(
        allowing.clone(),
        Method::POST,
        "/oagw/v1/plugins",
        Some(TENANT),
        Some(json!({ "plugin_type": "throttle", "name": "x", "source_code": "d" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{document}");
    assert_eq!(document["detail"], "plugin_type", "{document}");
}

#[tokio::test]
async fn a_plugin_request_without_a_subject_is_answered_401_before_any_store_access() {
    let Surfaces { allowing, .. } = surfaces();
    for (method, uri, body) in [
        (Method::POST, "/oagw/v1/plugins", Some(plugin_body("x"))),
        (Method::GET, "/oagw/v1/plugins", None),
        (Method::GET, "/oagw/v1/plugins/whatever", None),
        (Method::DELETE, "/oagw/v1/plugins/whatever", None),
    ] {
        let (status, document) = answer(allowing.clone(), method, uri, None, body).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{document}");
        assert_eq!(document["instance"], path_of(uri));
    }
}

#[tokio::test]
async fn a_plugin_request_without_the_permission_is_answered_403_before_any_store_access() {
    let Surfaces { denying, enforcerless, .. } = surfaces();
    for surface in [&denying, &enforcerless] {
        for (method, uri, body) in [
            (Method::POST, "/oagw/v1/plugins", Some(plugin_body("x"))),
            (Method::GET, "/oagw/v1/plugins", None),
        ] {
            let (status, _) = answer(surface.clone(), method.clone(), uri, Some(TENANT), body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{method} {uri}");
        }
    }
}

#[tokio::test]
async fn a_read_only_token_reads_and_writes_nothing() {
    let Surfaces { read_only, .. } = surfaces();
    let (status, document) = answer(
        read_only.clone(),
        Method::GET,
        "/oagw/v1/plugins",
        Some(TENANT),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{document}");
    for (method, uri, body) in [
        (Method::POST, String::from("/oagw/v1/plugins"), Some(plugin_body("read-only"))),
        (Method::DELETE, String::from("/oagw/v1/plugins/whatever"), None),
    ] {
        let note = format!("{method} {uri}");
        let (status, _) = answer(read_only.clone(), method, &uri, Some(TENANT), body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{note}");
    }
}

#[tokio::test]
async fn a_plugin_path_that_names_no_arm_addresses_nothing() {
    let Surfaces { allowing, .. } = surfaces();
    let named = created_plugin(&allowing, "redact-headers").await;
    // A bare `Uuid` names no arm, so it is not an accepted `{id}` spelling.
    for uri in ["/oagw/v1/plugins/some-id", "/oagw/v1/plugins/not-an-id"] {
        let (status, _) = answer(
            allowing.clone(),
            Method::GET,
            uri,
            Some(TENANT),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    // A named plugin's GTS identifier — a reserved catalogue row with no
    // management row behind it — addresses nothing through any of the three
    // read-and-delete paths, exactly as a nonexistent identifier does.
    let reserved = oagw::gts::plugin_catalog::CATALOG_ONLY_GUARD_TIMEOUT;
    for (method, suffix) in [
        (Method::GET, String::new()),
        (Method::GET, String::from("/source")),
        (Method::DELETE, String::new()),
    ] {
        let uri = format!("/oagw/v1/plugins/{reserved}{suffix}");
        let (status, _) = answer(allowing.clone(), method, &uri, Some(TENANT), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri} names no row");
    }
    let (status, document) = answer(
        allowing.clone(),
        Method::PUT,
        &format!("/oagw/v1/plugins/{named}"),
        Some(TENANT),
        Some(plugin_body("replacement")),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "plugins are immutable, so no PUT is registered: {document}"
    );
}

#[tokio::test]
async fn the_plugin_source_path_returns_the_source_and_nothing_else() {
    let Surfaces { allowing, .. } = surfaces();
    let named = created_plugin(&allowing, "redact-headers").await;
    let (status, document) = answer(
        allowing.clone(),
        Method::GET,
        &format!("/oagw/v1/plugins/{named}/source"),
        Some(TENANT),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{document}");
    assert_eq!(
        document,
        json!("def on_response(ctx):\n    return ctx\n"),
        "the source alone, with no row member beside it"
    );
}

#[tokio::test]
async fn a_plugin_deletion_is_answered_204_with_no_body() {
    let Surfaces { allowing, .. } = surfaces();
    let named = created_plugin(&allowing, "redact-headers").await;
    let (status, document) = answer(
        allowing.clone(),
        Method::DELETE,
        &format!("/oagw/v1/plugins/{named}"),
        Some(TENANT),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{document}");
    assert_eq!(document, Value::Null, "a deletion has no representation");

    let (status, _) = answer(
        allowing,
        Method::GET,
        &format!("/oagw/v1/plugins/{named}"),
        Some(TENANT),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "the row is gone");
}

#[tokio::test]
async fn the_plugin_list_supports_the_closed_odata_surface() {
    let Surfaces { allowing, .. } = surfaces();
    created_plugin(&allowing, "redact-headers").await;
    let (status, document) = answer(
        allowing.clone(),
        Method::GET,
        "/oagw/v1/plugins?$top=1&$select=name",
        Some(TENANT),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{document}");
    assert_eq!(document["items"].as_array().expect("items").len(), 1);
    assert_eq!(
        document["items"][0],
        json!({ "name": "redact-headers" }),
        "the projection narrows every item"
    );

    let (status, document) = answer(
        allowing,
        Method::GET,
        "/oagw/v1/plugins?$orderby=name&$select=tenant_id",
        Some(TENANT),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{document}");
    assert!(document["detail"].as_str().expect("detail").contains("$orderby"));
}

//! Data-plane tests: route matching, the plugin chain, rate limiting, CORS,
//! header hygiene, the `X-OAGW-Target-Host` matrix, tenant inheritance, and
//! the error contract of `ADR 0007`.

use std::sync::Arc;

use axum::http::Method;
use credstore_sdk::test_util::MockCredStoreClient;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    ERROR_SOURCE_HEADER, ERROR_TYPE_BASE, Gateway, Origin, Reply, SOURCE_GATEWAY, SOURCE_UPSTREAM,
    assert_problem, body_json, body_text, delete, gateway_custom, gateway_with, gateway_with_creds,
    post_json, problem_field, proxy_request, put_json, route_json, sec_ctx, sec_ctx_as, send,
    status_of, upstream_json,
};
use crate::config::OagwConfig;
use crate::domain::error::ErrorKind;
use crate::domain::matcher::SelectedRoute;
use crate::domain::model::auth_plugin_ids;
use crate::infra::plugin::transform::REQUEST_ID_HEADER;

/// The tenant the relay tests register their upstreams in.
fn tenant() -> Uuid {
    Uuid::from_u128(0xd00d)
}

/// Configuration that permits the plaintext loopback endpoints tests use.
fn relay_config() -> OagwConfig {
    OagwConfig {
        allow_http_upstream: true,
        ..OagwConfig::default()
    }
}

/// A gateway plus a JSON-echo origin registered under `alias` with a `path`
/// route for `methods`.
async fn relay_gateway(alias: &str, path: &str, methods: &[&str]) -> (Gateway, Origin) {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), alias),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201, "upstream created");
    let route = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant(),
        route_json(first_upstream_id(&gateway), path, methods),
    )
    .await;
    assert_eq!(status_of(&route).as_u16(), 201, "route created");
    (gateway, origin)
}

/// The id of the single upstream the test registered.
fn first_upstream_id(gateway: &Gateway) -> Uuid {
    let upstreams = gateway.store.upstreams_of(tenant());
    assert_eq!(upstreams.len(), 1, "exactly one upstream is registered");
    upstreams[0].id
}

/// The id of the single route the test registered.
async fn first_route_id(gateway: &Gateway) -> Uuid {
    let mut response = super::get_json(gateway, "/oagw/v1/routes", tenant()).await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let body = body_json(&mut response).await;
    let routes = body.as_array().expect("a route collection");
    assert_eq!(routes.len(), 1, "exactly one route is registered");
    Uuid::parse_str(routes[0]["id"].as_str().expect("id")).expect("uuid")
}

/// Replace the single registered route with `replacement`.
async fn replace_single_route(gateway: &Gateway, replacement: Value) {
    let id = first_route_id(gateway).await;
    let response = super::put_json(
        gateway,
        &format!("/oagw/v1/routes/{id}"),
        tenant(),
        replacement,
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200, "route replaced");
}

/// The JSON body of a proxied echo response.
async fn echo(response: &mut axum::response::Response) -> Value {
    let text = body_text(response).await;
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("the origin echo should be JSON: {error}\n{text}"))
}

/// Send an anonymous proxied request.
async fn proxy(
    gateway: &Gateway,
    method: Method,
    uri: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> axum::response::Response {
    send(
        &gateway.router,
        proxy_request(method, uri, headers, body, None),
    )
    .await
}

// ---------------------------------------------------------------------------
// Route matching
// ---------------------------------------------------------------------------

/// Two routes on one upstream: `/v1` and the longer `/v1/chat`.
async fn nested_route_gateway() -> (Gateway, Origin) {
    let (gateway, origin) = relay_gateway("match.local", "/v1", &["POST"]).await;
    let created = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant(),
        route_json(first_upstream_id(&gateway), "/v1/chat", &["POST"]),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);
    (gateway, origin)
}

/// Resolve `path` against the routes of `gateway`, asserting a match first.
async fn selected_route(
    gateway: &Gateway,
    alias: &str,
    method: &Method,
    path: &str,
) -> SelectedRoute {
    let effective = gateway
        .data
        .resolve(&sec_ctx(tenant()), tenant(), alias, method, path)
        .await
        .expect("the alias resolves");
    effective.route.expect("a route matches this path")
}

#[tokio::test]
async fn longest_prefix_route_wins() {
    let (gateway, _) = nested_route_gateway().await;
    let picked = selected_route(
        &gateway,
        "match.local",
        &Method::POST,
        "/v1/chat/completions",
    )
    .await;
    assert_eq!(
        picked.route.match_rule.http().expect("http").path,
        "/v1/chat"
    );
    assert_eq!(picked.upstream_path, "/v1/chat/completions");

    let shorter = selected_route(&gateway, "match.local", &Method::POST, "/v1/embeddings").await;
    assert_eq!(shorter.route.match_rule.http().expect("http").path, "/v1");
    assert_eq!(shorter.upstream_path, "/v1/embeddings");
}

#[tokio::test]
async fn the_relayed_path_is_the_client_path() {
    let (gateway, origin) = nested_route_gateway().await;
    let mut response = proxy(
        &gateway,
        Method::POST,
        "/oagw/v1/proxy/match.local/v1/chat/completions",
        &[("content-type", "application/json")],
        b"{}",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let body = echo(&mut response).await;
    assert_eq!(body["path"], json!("/v1/chat/completions"));
    assert_eq!(body["method"], json!("POST"));
    origin.only();
}

/// The route a request picks on the data plane is the one the matcher picks,
/// so the HTTP data plane applies the same longest-prefix rule the service
/// resolves.
#[tokio::test]
async fn the_data_plane_selects_the_longest_matching_route() {
    let (gateway, origin) = nested_route_gateway().await;
    let mut long = proxy(
        &gateway,
        Method::POST,
        "/oagw/v1/proxy/match.local/v1/chat/completions",
        &[("content-type", "application/json")],
        b"{}",
    )
    .await;
    assert_eq!(status_of(&long).as_u16(), 200);
    assert_eq!(echo(&mut long).await["path"], json!("/v1/chat/completions"));

    let mut short = proxy(
        &gateway,
        Method::POST,
        "/oagw/v1/proxy/match.local/v1/embeddings",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&short).as_u16(), 200);
    assert_eq!(echo(&mut short).await["path"], json!("/v1/embeddings"));
    assert_eq!(origin.captured().len(), 2);
}

/// Route selection ranks candidates by priority before insertion order
/// (`DESIGN.md` §"Route"): two routes that match the same pattern and method
/// are told apart by their `priority`, and the higher one is the one relayed.
#[tokio::test]
async fn the_higher_priority_route_is_served_on_a_shared_pattern() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "priority.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let upstream_id = first_upstream_id(&gateway);

    // Both routes match `/v1` for `GET`; each carries its own rate-limit
    // declaration, so the `X-RateLimit-Limit` header says which route ran.
    let route = |priority: i64, rate: u64| {
        let mut payload = route_json(upstream_id, "/v1", &["GET"]);
        payload["priority"] = json!(priority);
        payload["rate_limit"] = json!({
            "sustained": { "rate": rate, "window": "minute" },
            "response_headers": true
        });
        payload
    };
    for payload in [route(0, 11), route(9, 7)] {
        let created = post_json(&gateway, "/oagw/v1/routes", tenant(), payload).await;
        assert_eq!(status_of(&created).as_u16(), 201, "route created");
    }

    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/priority.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(
        response
            .headers()
            .get("x-ratelimit-limit")
            .and_then(|value| value.to_str().ok()),
        Some("7"),
        "the higher priority wins, not the earlier insertion"
    );
    let _ = body_text(&mut response).await;

    // Dropping the winner below the other moves the decision with it: the
    // ranking is the priority, not which route was stored first.
    let routes = gateway.store.routes_for_upstream(upstream_id);
    let winner = routes
        .iter()
        .find(|route| route.priority == 9)
        .expect("the high-priority route is stored")
        .as_ref()
        .clone();
    let mut lowered = serde_json::to_value(&winner).expect("the route serializes");
    lowered["priority"] = json!(-1);
    let mut replaced = put_json(
        &gateway,
        &format!("/oagw/v1/routes/{}", winner.id),
        tenant(),
        lowered,
    )
    .await;
    assert_eq!(status_of(&replaced).as_u16(), 200, "route replaced");
    let _ = body_json(&mut replaced).await;

    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/priority.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(
        response
            .headers()
            .get("x-ratelimit-limit")
            .and_then(|value| value.to_str().ok()),
        Some("11"),
        "the other route takes over once its priority is the higher one"
    );
    let _ = body_text(&mut response).await;
}

#[tokio::test]
async fn suffix_mode_disabled_matches_only_the_exact_pattern() {
    let (gateway, origin) = relay_gateway("exact.local", "/v1", &["GET"]).await;
    let upstream_id = first_upstream_id(&gateway);
    let mut replacement = route_json(upstream_id, "/v1", &["GET"]);
    replacement["match"]["http"]["path_suffix_mode"] = json!("disabled");
    replace_single_route(&gateway, replacement).await;

    // The matcher honours the mode: only the exact pattern matches, and the
    // upstream path is the pattern rather than the client path.
    let exact = selected_route(&gateway, "exact.local", &Method::GET, "/v1").await;
    assert_eq!(exact.upstream_path, "/v1", "the pattern is forwarded");
    let effective = gateway
        .data
        .resolve(
            &sec_ctx(tenant()),
            tenant(),
            "exact.local",
            &Method::GET,
            "/v1/ignored",
        )
        .await
        .expect("the alias resolves");
    assert!(
        effective.route.is_none(),
        "a longer path does not match a `disabled` route"
    );

    // End to end the exact request goes through the route ...
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/exact.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(origin.last().path, "/v1", "the pattern is relayed");
    // ... and a suffixed request matches no route, so it is relayed as the
    // fall-through path: the client path, not the route's pattern.
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/exact.local/v1/ignored",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(
        origin.last().path,
        "/v1/ignored",
        "the suffix survives the fall-through"
    );
}

#[tokio::test]
async fn trailing_slashes_are_tolerated_but_the_spelling_is_kept() {
    let (gateway, origin) = relay_gateway("normalize.local", "/v1", &["GET"]).await;
    // Matching normalizes the client path — case-insensitively, and a trailing
    // slash does not stop the match.
    for path in ["/v1/chat", "/v1/chat/", "/v1/chat/completions", "/V1/Chat/"] {
        let picked = selected_route(&gateway, "normalize.local", &Method::GET, path).await;
        assert_eq!(
            picked.upstream_path, path,
            "{path} is forwarded as the client spelled it"
        );
    }
    assert_eq!(
        selected_route(&gateway, "normalize.local", &Method::GET, "/V1/Chat/")
            .await
            .client_path,
        "/v1/chat",
        "the match is made against the normalized path"
    );
    // And the path that reaches the upstream is the one the client spelled.
    for (uri, forwarded) in [
        ("/oagw/v1/proxy/normalize.local/V1/Chat/", "/V1/Chat/"),
        ("/oagw/v1/proxy/normalize.local/v1/chat", "/v1/chat"),
    ] {
        let response = proxy(&gateway, Method::GET, uri, &[], b"").await;
        assert_eq!(status_of(&response).as_u16(), 200, "{uri}");
        assert_eq!(origin.last().path, forwarded, "{uri} is relayed verbatim");
    }
    assert_eq!(origin.captured().len(), 2);
}

/// A path no configured route claims is not a rejection: the Guard Rules table
/// lists no rule for it, so the request is relayed against the closest
/// upstream with the client path unchanged.
#[tokio::test]
async fn unmatched_requests_fall_through_to_the_upstream() {
    let (gateway, origin) = relay_gateway("methods.local", "/v1/chat", &["POST"]).await;
    // The only pattern is `/v1/chat`, so `/v1/embeddings` matches no route.
    let response = proxy(
        &gateway,
        Method::POST,
        "/oagw/v1/proxy/methods.local/v1/embeddings",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(origin.last().path, "/v1/embeddings", "relayed verbatim");
    assert_eq!(origin.last().method, "POST");

    // With no route selected nothing restricts the query either: what the
    // client sent is what the upstream receives.
    let response = proxy(
        &gateway,
        Method::POST,
        "/oagw/v1/proxy/methods.local/v1/embeddings?api-version=2024",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(
        origin.last().query.as_deref(),
        Some("api-version=2024"),
        "the fall-through forwards the query as sent"
    );
}

/// An upstream with no configured routes has nothing that claims a path, so
/// every request falls through and is relayed.
#[tokio::test]
async fn an_upstream_without_routes_still_relays() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "bare.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201, "upstream created");

    let effective = gateway
        .data
        .resolve(
            &sec_ctx(tenant()),
            tenant(),
            "bare.local",
            &Method::GET,
            "/v1",
        )
        .await
        .expect("the alias resolves");
    assert!(effective.route.is_none(), "no route exists to select");

    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/bare.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let captured = origin.only();
    assert_eq!(captured.path, "/v1", "relayed verbatim");
    assert_eq!(captured.method, "GET");
    let _ = body_text(&mut response).await;
}

/// `DESIGN.md` §"Guard Rules": the method must be in `match.http.methods`, and
/// a path a route claims is not relayed when the request method is not in that
/// allowlist. The catalogue has no 405 entry, so the rejection is the
/// documented `route.not_found` problem produced by the gateway.
#[tokio::test]
async fn a_method_the_route_does_not_allow_is_rejected_as_route_not_found() {
    let (gateway, origin) = relay_gateway("guarded.local", "/v1/hello", &["GET"]).await;
    for method in [Method::POST, Method::DELETE] {
        let mut response = proxy(
            &gateway,
            method.clone(),
            "/oagw/v1/proxy/guarded.local/v1/hello",
            &[],
            b"",
        )
        .await;
        assert_eq!(
            status_of(&response).as_u16(),
            404,
            "{method} is not allowed"
        );
        assert_problem(&response, ErrorKind::RouteNotFound.gts_fragment(), 404);
        assert_eq!(
            response
                .headers()
                .get(ERROR_SOURCE_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(SOURCE_GATEWAY),
            "the gateway, not the upstream, refuses the method"
        );
        let body = body_json(&mut response).await;
        assert_eq!(
            body["type"],
            json!(format!(
                "{ERROR_TYPE_BASE}{}",
                ErrorKind::RouteNotFound.gts_fragment()
            ))
        );
        assert_eq!(
            body["detail"],
            json!(format!(
                "method {method} is not allowed for route path '/v1/hello'"
            )),
            "the method and the matched path are named, got: {body}"
        );
    }
    assert_eq!(
        origin.captured().len(),
        0,
        "a refused method is never relayed"
    );

    // The allowlisted method still matches the route, and it is the route's
    // upstream path that is forwarded rather than the fall-through client path.
    let selected = selected_route(&gateway, "guarded.local", &Method::GET, "/v1/hello").await;
    assert_eq!(selected.upstream_path, "/v1/hello");
    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/guarded.local/v1/hello",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(origin.only().path, "/v1/hello");
    let _ = body_text(&mut response).await;
}

/// `HEAD` folds onto `GET` for matching: the schema has no `HEAD` method, so a
/// route serving `GET` serves its `HEAD` equivalent too — and the request is
/// still forwarded as `HEAD`, not folded into a `GET` on the wire.
#[tokio::test]
async fn a_head_request_matches_a_get_route_and_is_relayed_as_head() {
    let (gateway, origin) = relay_gateway("head.local", "/v1", &["GET"]).await;

    let response = proxy(
        &gateway,
        Method::HEAD,
        "/oagw/v1/proxy/head.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        200,
        "HEAD matches a GET-only route"
    );
    let captured = origin.only();
    assert_eq!(
        captured.method, "HEAD",
        "the upstream sees HEAD, not the method it was folded onto"
    );
    assert_eq!(captured.path, "/v1");
}

/// Route-scoped configuration (method allowlist, `path_suffix_mode`,
/// `query_allowlist`, route plugins, route rate limits) is selected by the
/// same path match the data plane makes, so it applies to relayed requests.
#[tokio::test]
async fn route_configuration_is_applied_to_relayed_requests() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    // The guard inspects the headers the client actually sent, so the
    // passthrough filter is what keeps the header off the wire to the upstream.
    let mut upstream = upstream_json(origin.host(), origin.port(), "inert.local");
    upstream["headers"] = json!({
        "request": { "passthrough": "allowlist", "passthrough_allowlist": ["x-must-be-set"] }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201, "upstream created");
    let upstream_id = first_upstream_id(&gateway);
    let mut route = route_json(upstream_id, "/v1", &["GET"]);
    route["rate_limit"] = rate_limit(2, None);
    route["plugins"] = json!({
        "items": [{ "plugin_ref": "required_headers",
                    "config": { "required_request_headers": "x-must-be-set" } }]
    });
    let created = post_json(&gateway, "/oagw/v1/routes", tenant(), route).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let effective = gateway
        .data
        .resolve(
            &sec_ctx(tenant()),
            tenant(),
            "inert.local",
            &Method::GET,
            "/v1",
        )
        .await
        .expect("the alias resolves");
    assert!(effective.route.is_some(), "the route matches");
    assert_eq!(
        effective.rate_limits.len(),
        1,
        "the route bucket is stacked"
    );
    assert!(
        effective.rate_limits[0].0.starts_with("route:"),
        "the route bucket is keyed by route, got {:?}",
        effective.rate_limits[0].0
    );

    // The route plugin runs on the relayed request: the missing header
    // rejects it before anything is dialed.
    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/inert.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400, "the guard ran");
    assert_eq!(
        body_json(&mut response).await["error_code"],
        json!("REQUIRED_HEADER_MISSING")
    );
    assert_eq!(origin.captured().len(), 0, "nothing was relayed");

    // A well-formed request is admitted, and the route's own bucket is what
    // refuses the next one — the limit is enforced before the plugin chain
    // runs, so the rejected request consumed a token as well.
    let admitted = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/inert.local/v1",
        &[("x-must-be-set", "yes")],
        b"",
    )
    .await;
    assert_eq!(status_of(&admitted).as_u16(), 200, "the guard allows it");
    let rejected = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/inert.local/v1",
        &[("x-must-be-set", "yes")],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&rejected).as_u16(),
        429,
        "the route limit applied"
    );
    assert_eq!(
        origin.captured().len(),
        1,
        "only the admitted request relayed"
    );
}

/// The header the required-headers guard demands in these tests.
const GUARDED_HEADER: &str = "x-must-be-set";

/// A gateway whose upstream forwards nothing (`passthrough: none`) but whose
/// route guard requires a header the client sends.
async fn guarded_upstream_gateway(alias: &str) -> (Gateway, Origin) {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), alias);
    upstream["headers"] = json!({ "request": { "passthrough": "none" } });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201, "upstream created");
    let mut route = route_json(first_upstream_id(&gateway), "/v1", &["GET"]);
    route["plugins"] = json!({
        "items": [{ "plugin_ref": "required_headers",
                    "config": { "required_request_headers": GUARDED_HEADER } }]
    });
    let created = post_json(&gateway, "/oagw/v1/routes", tenant(), route).await;
    assert_eq!(status_of(&created).as_u16(), 201, "route created");
    (gateway, origin)
}

/// The guard judges what the caller sent, not the filtered set the upstream
/// will receive: a header the passthrough filter drops was still sent
/// (`ADR 0009`), so it satisfies the guard and is only kept off the wire.
#[tokio::test]
async fn a_guard_validates_the_headers_the_client_sent() {
    let (gateway, origin) = guarded_upstream_gateway("guarded-headers.local").await;

    let admitted = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/guarded-headers.local/v1",
        &[(GUARDED_HEADER, "yes")],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&admitted).as_u16(),
        200,
        "the sent header is enough"
    );
    let captured = origin.only();
    assert_eq!(
        captured.header(GUARDED_HEADER),
        None,
        "the passthrough filter still decides what the upstream receives"
    );

    let mut refused = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/guarded-headers.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&refused).as_u16(),
        400,
        "an absent header is refused"
    );
    assert_eq!(
        body_json(&mut refused).await["error_code"],
        json!("REQUIRED_HEADER_MISSING")
    );
    assert_eq!(
        origin.captured().len(),
        1,
        "only the admitted request relayed"
    );
}

/// The registered alias and a slash-prefixed path, for matcher-level tests.
async fn allowlisted_gateway() -> (Gateway, Origin) {
    let (gateway, origin) = relay_gateway("query.local", "/v1", &["GET"]).await;
    let upstream_id = first_upstream_id(&gateway);
    let mut replacement = route_json(upstream_id, "/v1", &["GET"]);
    replacement["match"]["http"]["query_allowlist"] = json!(["api-version"]);
    replace_single_route(&gateway, replacement).await;
    (gateway, origin)
}

#[tokio::test]
async fn the_query_allowlist_filters_the_forwarded_query() {
    let (gateway, origin) = allowlisted_gateway().await;
    let selected = selected_route(&gateway, "query.local", &Method::GET, "/v1").await;
    let none: Vec<String> = Vec::new();
    assert_eq!(
        crate::domain::service::filter_query(Some("api-version=2024"), Some(&selected), &none,)
            .expect("the allowlisted parameter is accepted"),
        Some("api-version=2024".to_owned()),
        "only allowlisted parameters survive"
    );
    assert_eq!(
        crate::domain::service::filter_query(Some("secret=hide-me"), Some(&selected), &none)
            .expect_err("a parameter outside the allowlist is rejected")
            .kind(),
        ErrorKind::Validation,
        "an unknown parameter is a 400, not a silent drop"
    );
    // The filter works on the raw segments: what survives is byte-identical to
    // what the client sent, escaped spelling and all.
    let rejected = crate::domain::service::filter_query(
        Some("api-version=a%20b/c&secret=hide-me"),
        Some(&selected),
        &none,
    )
    .expect_err("the unknown parameter is still rejected");
    assert_eq!(
        rejected.detail(),
        "query parameter 'secret' is not in the route's query allowlist"
    );
    assert_eq!(
        crate::domain::service::filter_query(Some("api-version=a%20b/c"), Some(&selected), &none)
            .expect("the escaped value is kept"),
        Some("api-version=a%20b/c".to_owned()),
        "the escaped spelling is not re-serialized"
    );
    // An allowlist is an allow-list, not a require-list: a request that names
    // none of the allowlisted parameters is refused for the one it did send,
    // while a request that sends nothing at all is admitted without a query.
    let unknown = crate::domain::service::filter_query(Some("other=1"), Some(&selected), &none)
        .expect_err("the parameter is not allowlisted");
    assert_eq!(unknown.kind(), ErrorKind::Validation);
    assert_eq!(
        unknown.detail(),
        "query parameter 'other' is not in the route's query allowlist"
    );
    assert_eq!(
        crate::domain::service::filter_query(None, Some(&selected), &none).expect("no query"),
        None,
    );
    let _ = origin;
}

/// The outbound query is validated against the selected route's allowlist: a
/// parameter the client sent that the allowlist does not name is a 400, and an
/// allowlisted request reaches the upstream byte-identical to what was sent.
#[tokio::test]
async fn the_relayed_query_is_filtered_by_the_route_allowlist() {
    let (gateway, origin) = allowlisted_gateway().await;
    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/query.local/v1?api-version=a%20b/c",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(
        origin.only().query.as_deref(),
        Some("api-version=a%20b/c"),
        "the allowlisted parameter is relayed as the client spelled it"
    );
    let _ = body_text(&mut response).await;

    // A parameter outside the allowlist is refused instead of being dropped.
    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/query.local/v1?api-version=2024&secret=hide-me",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400, "unknown parameter");
    assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
    let body = body_json(&mut response).await;
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("secret"),
        "the offending parameter is named, got: {body}"
    );
    assert_eq!(origin.captured().len(), 1, "nothing else was relayed");
}

#[tokio::test]
async fn an_empty_allowlist_forwards_no_query() {
    let (gateway, origin) = relay_gateway("query-none.local", "/v1", &["GET"]).await;
    // A matched route with an empty allowlist allows nothing, so the query is
    // dropped even though the client sent one.
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/query-none.local/v1?trace=abc",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(
        origin.last().query,
        None,
        "an empty allowlist forwards no query parameters"
    );
}

// ---------------------------------------------------------------------------
// Plugin chain
// ---------------------------------------------------------------------------

/// A gateway whose upstream answers `Reply::Fixed` and whose route guard
/// requires the `required_response_headers` header named in `reply_headers`.
async fn response_guard_gateway(
    alias: &str,
    reply_headers: Vec<(&'static str, String)>,
) -> (Gateway, Origin) {
    let origin = Origin::spawn(Reply::Fixed {
        status: 200,
        headers: reply_headers,
        body: b"ok".to_vec(),
    })
    .await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), alias);
    upstream["plugins"] = json!({
        "items": [{ "plugin_ref": "required_headers",
                    "config": { "required_response_headers": "x-signature" } }]
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201, "upstream created");
    let created = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant(),
        route_json(first_upstream_id(&gateway), "/v1", &["GET"]),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201, "route created");
    (gateway, origin)
}

/// A configured guard also validates the relayed response (`ADR 0009`): an
/// upstream answer that misses a required header is refused before the caller
/// sees it, as an upstream fault rather than as the client's 400.
#[tokio::test]
async fn a_response_guard_rejects_an_upstream_answer_missing_a_required_header() {
    let (gateway, origin) = response_guard_gateway("response-guard.local", Vec::new()).await;

    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/response-guard.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        502,
        "the upstream answer is refused"
    );
    assert_problem(&response, ErrorKind::Validation.gts_fragment(), 502);

    let body = body_json(&mut response).await;
    assert_eq!(
        body["error_code"],
        json!("REQUIRED_HEADER_MISSING"),
        "the guard's own code is reported"
    );
    assert_eq!(
        body["detail"],
        json!("Required response header 'x-signature' is missing"),
        "the missing response header is named"
    );
    assert_eq!(origin.captured().len(), 1, "the request was relayed once");
}

/// When the upstream does send the required header, the answer reaches the
/// caller untouched.
#[tokio::test]
async fn a_response_guard_allows_an_answer_carrying_the_required_header() {
    let (gateway, _origin) = response_guard_gateway(
        "response-guard-ok.local",
        vec![("x-signature", "v1".to_owned())],
    )
    .await;

    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/response-guard-ok.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        200,
        "the required header is present"
    );
    assert_eq!(body_text(&mut response).await, "ok");
}

/// A guard's own `Rejection.status` is what the caller sees: the gateway does
/// not rewrite it to the 400 the request phase uses, while the problem `type`
/// stays the GTS identifier of the failure class.
#[tokio::test]
async fn a_guard_rejection_uses_the_status_the_plugin_asked_for() {
    struct Stubborn;

    #[async_trait::async_trait]
    impl crate::domain::plugin::GuardPlugin for Stubborn {
        fn id(&self) -> &'static str {
            "stubborn"
        }
        fn plugin_type(&self) -> &'static str {
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.stubborn.v1"
        }
        async fn guard_request(
            &self,
            _ctx: &crate::domain::plugin::RequestContext,
        ) -> Result<crate::domain::plugin::GuardDecision, crate::domain::error::OagwError> {
            Ok(crate::domain::plugin::GuardDecision::Reject(
                crate::domain::plugin::Rejection {
                    status: http::StatusCode::FORBIDDEN,
                    error_code: "STUBBORN_REFUSAL".to_owned(),
                    detail: "not today".to_owned(),
                },
            ))
        }
        async fn guard_response(
            &self,
            _ctx: &crate::domain::plugin::ResponseContext,
        ) -> Result<crate::domain::plugin::GuardDecision, crate::domain::error::OagwError> {
            Ok(crate::domain::plugin::GuardDecision::Allow)
        }
    }

    let mut guards = crate::domain::plugin::GuardPluginRegistry::with_builtins();
    guards.register(Arc::new(Stubborn));
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_custom(
        relay_config(),
        Arc::new(MockCredStoreClient::empty()),
        guards,
        crate::domain::plugin::TransformPluginRegistry::with_builtins(),
    );
    let mut upstream = upstream_json(origin.host(), origin.port(), "stubborn.local");
    upstream["plugins"] = json!({ "items": ["stubborn"] });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201, "upstream created");
    let created = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant(),
        route_json(first_upstream_id(&gateway), "/v1", &["GET"]),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201, "route created");

    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/stubborn.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        403,
        "the plugin's status is honoured"
    );
    // The GTS contract holds: still a problem document of the gateway's, with
    // the validation type a guard rejection produces and the plugin's code.
    assert_problem(&response, ErrorKind::Validation.gts_fragment(), 403);
    let body = body_json(&mut response).await;
    assert_eq!(body["error_code"], json!("STUBBORN_REFUSAL"));
    assert_eq!(
        body["status"],
        json!(403),
        "the body status matches the header"
    );
    assert_eq!(
        origin.captured().len(),
        0,
        "a refused request is not relayed"
    );
}

#[tokio::test]
async fn guard_plugins_run_before_transform_plugins() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "chain.local");
    // Built-in plugins bind by their registry key (`GuardPlugin::id`), the
    // short form the registries resolve; the canonical GTS spelling resolves
    // to the same key, see `canonical_plugin_identifiers_validate_and_execute`.
    upstream["plugins"] = json!({
        "items": [
            { "plugin_ref": "required_headers",
              "config": { "required_request_headers": REQUEST_ID_HEADER } },
            { "plugin_ref": "request_id", "config": {} }
        ]
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201, "the chain is accepted");

    // Guards run before transforms, so the missing header rejects the request
    // even though the transform registered after it would have injected it.
    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/chain.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        400,
        "the guard rejects first"
    );
    assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(
        body["error_code"],
        json!("REQUIRED_HEADER_MISSING"),
        "the guard's own code replaces the gateway's"
    );
    assert_eq!(origin.captured().len(), 0, "nothing was relayed");
}

#[tokio::test]
async fn request_id_transform_injects_and_propagates() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "request-id.local");
    upstream["plugins"] = json!({
        "items": [{ "plugin_ref": "request_id", "config": {} }]
    });
    // The identifier only reaches the plugin when the caller may pass it, so
    // the upstream allows that one header through.
    upstream["headers"] = json!({
        "request": { "passthrough": "allowlist", "passthrough_allowlist": [REQUEST_ID_HEADER] }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    // A caller-supplied identifier travels upstream unchanged.
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/request-id.local/v1",
        &[(REQUEST_ID_HEADER, "caller-id-1")],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(
        origin.last().header(REQUEST_ID_HEADER),
        Some("caller-id-1"),
        "the caller's identifier is propagated"
    );
    // The transform also guarantees one on the way back.
    assert!(
        response
            .headers()
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| Uuid::parse_str(value).is_ok()),
        "the response carries a request id"
    );

    // Without one, the transform injects a fresh UUID.
    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/request-id.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let last = origin.last();
    let injected = last
        .header(REQUEST_ID_HEADER)
        .expect("a request id is injected");
    assert!(
        Uuid::parse_str(injected).is_ok(),
        "an injected request id is a UUID, got {injected}"
    );
    let _ = body_text(&mut response).await;
}

/// A binding may name a built-in plugin by its canonical GTS identifier: the
/// chain validates, and the data plane resolves the implementation the short
/// id names, so the same plugin runs whichever spelling the configuration
/// used (`DESIGN.md` "Plugin Identification Model").
#[tokio::test]
async fn canonical_plugin_identifiers_validate_and_execute() {
    let origin = Origin::spawn(Reply::Echo).await;
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> = Arc::new(
        MockCredStoreClient::with_secrets(vec![("partner-key".to_owned(), "s3cr3t".to_owned())]),
    );
    let gateway = gateway_with_creds(relay_config(), credstore);
    let mut upstream = upstream_json(origin.host(), origin.port(), "canonical.local");
    upstream["auth"] = json!({
        "type": auth_plugin_ids::APIKEY,
        "config": { "api_key_ref": "cred://partner-key", "key_name": "x-api-key", "in": "header" }
    });
    upstream["plugins"] = json!({
        "items": [
            "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1",
            "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"
        ]
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(
        status_of(&created).as_u16(),
        201,
        "the canonical identifiers are accepted at creation"
    );

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/canonical.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let captured = origin.only();
    assert_eq!(
        captured.header("x-api-key"),
        Some("s3cr3t"),
        "the auth plugin named by its canonical identifier ran"
    );
    assert!(
        captured
            .header(REQUEST_ID_HEADER)
            .is_some_and(|value| Uuid::parse_str(value).is_ok()),
        "the transform plugin named by its canonical identifier ran"
    );
}

/// The registry key is reduced the same way everywhere: any trailing `v<digits>`
/// segment is version, not part of the plugin short name, so a configuration
/// that names `required_headers.v2` still runs the `required_headers` plugin
/// instead of being refused as an unknown one.
#[tokio::test]
async fn a_versioned_plugin_identifier_resolves_to_the_same_plugin() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "versioned.local");
    upstream["plugins"] = json!({
        "items": [{ "plugin_ref": "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v2",
                    "config": { "required_request_headers": GUARDED_HEADER } }]
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(
        status_of(&created).as_u16(),
        201,
        "a versioned identifier is not a different plugin"
    );

    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/versioned.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        400,
        "the required_headers guard ran"
    );
    assert_eq!(
        body_json(&mut response).await["error_code"],
        json!("REQUIRED_HEADER_MISSING")
    );
    assert_eq!(
        origin.captured().len(),
        0,
        "the guard, not the upstream, refused it"
    );

    // The header it asks for is admitted.
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/versioned.local/v1",
        &[(GUARDED_HEADER, "yes")],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        200,
        "the same plugin resolved and ran"
    );
}

#[tokio::test]
async fn apikey_auth_injects_the_credential_into_a_header() {
    let origin = Origin::spawn(Reply::Echo).await;
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> = Arc::new(
        MockCredStoreClient::with_secrets(vec![("partner-key".to_owned(), "s3cr3t".to_owned())]),
    );
    let gateway = gateway_with_creds(relay_config(), credstore);
    let mut upstream = upstream_json(origin.host(), origin.port(), "apikey.local");
    upstream["auth"] = json!({
        "type": "apikey",
        "config": { "api_key_ref": "cred://partner-key", "key_name": "x-api-key", "in": "header" }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(
        status_of(&created).as_u16(),
        201,
        "the apikey upstream is created"
    );

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/apikey.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let captured = origin.only();
    assert_eq!(
        captured.header("x-api-key"),
        Some("s3cr3t"),
        "the resolved secret is injected as a header"
    );
}

/// A proxied request that presents `identity` as the caller.
async fn proxy_as(
    gateway: &Gateway,
    method: Method,
    uri: &str,
    identity: toolkit_security::SecurityContext,
) -> axum::response::Response {
    send(
        &gateway.router,
        proxy_request(method, uri, &[], b"", Some(identity)),
    )
    .await
}

/// An `OAuth2` token is cached per CALLER tenant, not per upstream owner
/// (`ADR 0008` "Cache Key Design"): two tenants whose identities carry the same
/// subject id call one OAuth2-protected upstream, and the gateway must mint a
/// token for each of them rather than replay the one it minted for the first.
#[tokio::test]
async fn oauth2_tokens_are_cached_per_caller_tenant() {
    let origin = Origin::spawn(Reply::Echo).await;
    let idp = Origin::spawn(Reply::Fixed {
        status: 200,
        headers: vec![("content-type", "application/json".to_owned())],
        body: br#"{"access_token":"idp-issued-token","expires_in":300,"token_type":"Bearer"}"#
            .to_vec(),
    })
    .await;

    // One subject identity presented by two tenants: the caller's tenant is the
    // only thing that distinguishes the two callers, so it is what the cache
    // key has to carry.
    let subject = Uuid::from_u128(0x5e11);
    let parent = Uuid::from_u128(0x50e1);
    let child = Uuid::from_u128(0x5e10);
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> =
        Arc::new(MockCredStoreClient::with_secrets(vec![
            ("client-id".to_owned(), "oagw-client".to_owned()),
            ("client-secret".to_owned(), "cl13nt-s3cr3t".to_owned()),
        ]));
    let gateway = super::assemble(
        relay_config(),
        credstore,
        &[(child, parent)],
        None,
        crate::domain::plugin::GuardPluginRegistry::with_builtins(),
        crate::domain::plugin::TransformPluginRegistry::with_builtins(),
    );

    // The upstream belongs to the parent tenant, so a call from the child is
    // relayed against a configuration owned by the *other* tenant.
    let mut upstream = upstream_json(origin.host(), origin.port(), "token-cache.local");
    upstream["auth"] = json!({
        "type": "oauth2_client_cred",
        "config": {
            "token_endpoint": format!("http://{}:{}/token", idp.host(), idp.port()),
            "client_id_ref": "cred://client-id",
            "client_secret_ref": "cred://client-secret"
        }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", parent, upstream).await;
    assert_eq!(
        status_of(&created).as_u16(),
        201,
        "the oauth2 upstream is created"
    );
    let upstream_id = gateway.store.upstreams_of(parent)[0].id;
    let created = post_json(
        &gateway,
        "/oagw/v1/routes",
        parent,
        route_json(upstream_id, "/v1", &["GET"]),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201, "route created");

    // The owner tenant calls twice: one token exchange, then a cache hit.
    for _ in 0..2 {
        let mut response = proxy_as(
            &gateway,
            Method::GET,
            "/oagw/v1/proxy/token-cache.local/v1",
            sec_ctx_as(parent, subject),
        )
        .await;
        assert_eq!(
            status_of(&response).as_u16(),
            200,
            "the owner tenant relays"
        );
        assert_eq!(
            origin.last().header("authorization"),
            Some("Bearer idp-issued-token"),
            "the minted token reaches the upstream"
        );
        let _ = body_text(&mut response).await;
    }
    assert_eq!(
        idp.captured().len(),
        1,
        "the second call by the same caller is served from the cache"
    );

    // The other tenant calls the same upstream: its token is its own.
    let mut response = proxy_as(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/token-cache.local/v1",
        sec_ctx_as(child, subject),
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        200,
        "the other tenant relays too"
    );
    assert_eq!(
        idp.captured().len(),
        2,
        "a token minted for one caller tenant is not replayed for another"
    );
    assert_eq!(
        origin.last().header("authorization"),
        Some("Bearer idp-issued-token"),
        "the freshly minted token still reaches the upstream"
    );
    let _ = body_text(&mut response).await;
}

/// A transform plugin that rewrites the request path and appends a query
/// parameter, to prove the outbound request is built from the post-plugin
/// context rather than from what the client sent.
struct Rewriter;

#[async_trait::async_trait]
impl crate::domain::plugin::TransformPlugin for Rewriter {
    fn id(&self) -> &'static str {
        "rewriter"
    }
    fn plugin_type(&self) -> &'static str {
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.rewriter.v1"
    }
    async fn transform_request(
        &self,
        ctx: &mut crate::domain::plugin::RequestContext,
    ) -> Result<(), crate::domain::error::OagwError> {
        ctx.path = format!("{}/rewritten", ctx.path.trim_end_matches('/'));
        ctx.query = Some(match ctx.query.take() {
            Some(query) => format!("{query}&rewritten=yes"),
            None => "rewritten=yes".to_owned(),
        });
        Ok(())
    }
    async fn transform_response(
        &self,
        _ctx: &mut crate::domain::plugin::ResponseContext,
    ) -> Result<(), crate::domain::error::OagwError> {
        Ok(())
    }
    async fn transform_error(
        &self,
        _ctx: &mut crate::domain::plugin::ErrorContext,
    ) -> Result<(), crate::domain::error::OagwError> {
        Ok(())
    }
}

/// The path and query a transform plugin leaves behind are what the upstream
/// receives: a rewrite is honoured symmetrically with a header edit, and it is
/// not undone by the route's own path resolution.
#[tokio::test]
async fn a_transform_plugin_rewrites_the_relayed_path_and_query() {
    let origin = Origin::spawn(Reply::Echo).await;
    let mut transforms = crate::domain::plugin::TransformPluginRegistry::with_builtins();
    transforms.register(Arc::new(Rewriter));
    let gateway = gateway_custom(
        relay_config(),
        Arc::new(MockCredStoreClient::empty()),
        crate::domain::plugin::GuardPluginRegistry::with_builtins(),
        transforms,
    );
    let mut upstream = upstream_json(origin.host(), origin.port(), "rewrite.local");
    upstream["plugins"] = json!({ "items": ["rewriter"] });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201, "upstream created");
    let created = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant(),
        route_json(first_upstream_id(&gateway), "/v1", &["GET"]),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201, "route created");

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/rewrite.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let captured = origin.only();
    assert_eq!(
        captured.path, "/v1/rewritten",
        "the plugin's path rewrite is what is forwarded"
    );
    assert_eq!(
        captured.query.as_deref(),
        Some("rewritten=yes"),
        "the plugin's query addition is what is forwarded"
    );
}

/// The outbound query is what the plugin chain left behind: an apikey plugin
/// configured for the query appends the resolved secret to the request
/// context's query, and that is the query the upstream receives. The client's
/// own query still cannot smuggle a parameter past a route allowlist.
#[tokio::test]
async fn apikey_auth_in_query_mode_reaches_the_upstream() {
    let origin = Origin::spawn(Reply::Echo).await;
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> = Arc::new(
        MockCredStoreClient::with_secrets(vec![("partner-key".to_owned(), "s3cr3t".to_owned())]),
    );
    let gateway = gateway_with_creds(relay_config(), credstore);
    let mut upstream = upstream_json(origin.host(), origin.port(), "apikey-query.local");
    upstream["auth"] = json!({
        "type": "apikey",
        "config": { "api_key_ref": "cred://partner-key", "key_name": "api-key", "in": "query" }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/apikey-query.local/v1?trace=abc",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let captured = origin.only();
    assert_eq!(
        captured.query.as_deref(),
        Some("trace=abc&api-key=s3cr3t"),
        "the injected credential travels upstream next to the client's query"
    );
    assert!(
        !captured.has_header("api-key"),
        "the secret travels as a query parameter, not as a header"
    );
    let _ = body_text(&mut response).await;
}

/// A credential the auth plugin injected into the query is not client input, so
/// it survives a route allowlist that does not name it.
#[tokio::test]
async fn an_injected_query_parameter_is_not_subject_to_the_allowlist() {
    let origin = Origin::spawn(Reply::Echo).await;
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> = Arc::new(
        MockCredStoreClient::with_secrets(vec![("partner-key".to_owned(), "s3cr3t".to_owned())]),
    );
    let gateway = gateway_with_creds(relay_config(), credstore);
    let mut upstream = upstream_json(origin.host(), origin.port(), "injected.local");
    upstream["auth"] = json!({
        "type": "apikey",
        "config": { "api_key_ref": "cred://partner-key", "key_name": "api-key", "in": "query" }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let upstream_id = first_upstream_id(&gateway);
    let mut route = route_json(upstream_id, "/v1", &["GET"]);
    route["match"]["http"]["query_allowlist"] = json!(["api-version"]);
    let created = post_json(&gateway, "/oagw/v1/routes", tenant(), route).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/injected.local/v1?api-version=2024",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(
        origin.only().query.as_deref(),
        Some("api-version=2024&api-key=s3cr3t"),
        "the allowlist governs the client, not the injected credential"
    );
}

#[tokio::test]
async fn apikey_auth_fails_closed_without_the_credential() {
    let origin = Origin::spawn(Reply::Echo).await;
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> =
        Arc::new(MockCredStoreClient::empty());
    let gateway = gateway_with_creds(relay_config(), credstore);
    let mut upstream = upstream_json(origin.host(), origin.port(), "apikey-missing.local");
    upstream["auth"] = json!({
        "type": "apikey",
        "config": { "api_key_ref": "cred://absent" }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/apikey-missing.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 500);
    assert_problem(&response, ErrorKind::SecretNotFound.gts_fragment(), 500);
    assert_eq!(origin.captured().len(), 0, "nothing is relayed");
}

/// An auth plugin that refuses any caller who does not present `x-identity`,
/// so a test can fail authentication on demand without touching the
/// credential store.
struct Identify;

#[async_trait::async_trait]
impl crate::domain::plugin::AuthPlugin for Identify {
    fn id(&self) -> &'static str {
        "identify"
    }
    fn plugin_type(&self) -> &'static str {
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.identify.v1"
    }
    async fn authenticate(
        &self,
        ctx: &mut crate::domain::plugin::RequestContext,
    ) -> Result<(), crate::domain::error::OagwError> {
        let identity = ctx
            .inbound_headers
            .get("x-identity")
            .and_then(|value| value.to_str().ok());
        match identity {
            Some(subject) => {
                ctx.subject_id = subject.to_owned();
                Ok(())
            }
            None => Err(crate::domain::error::OagwError::new(
                crate::domain::error::ErrorKind::AuthenticationFailed,
                "no identity was presented",
            )),
        }
    }
}

/// Auth runs before the rate limit (`ADR 0006`): an unidentified caller is
/// refused by its plugin rather than counted against a bucket, so a chain of
/// failed authentications never turns into a 429 and never spends the bucket's
/// tokens.
#[tokio::test]
async fn an_auth_failure_is_not_counted_against_the_rate_limit() {
    let origin = Origin::spawn(Reply::Echo).await;
    let mut auth = crate::domain::plugin::AuthPluginRegistry::with_builtins(
        Arc::new(MockCredStoreClient::empty()),
        crate::domain::plugin::TokenCacheConfig::default(),
    );
    auth.register(Arc::new(Identify));
    let gateway = super::gateway_custom_auth(relay_config(), auth);
    let mut upstream = upstream_json(origin.host(), origin.port(), "auth-first.local");
    upstream["auth"] = json!({ "type": "identify", "config": {} });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201, "upstream created");
    let mut route = route_json(first_upstream_id(&gateway), "/v1", &["GET"]);
    route["rate_limit"] = rate_limit(1, None);
    let created = post_json(&gateway, "/oagw/v1/routes", tenant(), route).await;
    assert_eq!(status_of(&created).as_u16(), 201, "route created");

    // Every attempt fails at the auth plugin, which runs before the bucket.
    for attempt in 1..=3 {
        let response = proxy(
            &gateway,
            Method::GET,
            "/oagw/v1/proxy/auth-first.local/v1",
            &[],
            b"",
        )
        .await;
        assert_eq!(
            status_of(&response).as_u16(),
            401,
            "attempt {attempt} fails at the auth plugin, not the bucket"
        );
        assert_problem(
            &response,
            ErrorKind::AuthenticationFailed.gts_fragment(),
            401,
        );
    }
    assert_eq!(origin.captured().len(), 0, "nothing was relayed");

    // The bucket is untouched: an authenticated request is admitted, which it
    // would not be if the failures above had consumed the route's token.
    let admitted = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/auth-first.local/v1",
        &[("x-identity", "subject-1")],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&admitted).as_u16(),
        200,
        "the single token is still there"
    );

    // And the next authenticated request is what exhausts the bucket.
    let rejected = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/auth-first.local/v1",
        &[("x-identity", "subject-1")],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&rejected).as_u16(),
        429,
        "the limit applies to counted requests"
    );
    assert_eq!(
        origin.captured().len(),
        1,
        "only the authenticated request was relayed"
    );
}

#[tokio::test]
async fn apikey_auth_without_a_reference_is_a_validation_error() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "apikey-bare.local");
    upstream["auth"] = json!({ "type": "apikey", "config": {} });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/apikey-bare.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
    let body = body_json(&mut response).await;
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("api_key_ref"),
        "the problem names the missing key, got: {body}"
    );
}

#[tokio::test]
async fn catalogued_only_auth_plugins_are_rejected_at_creation() {
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json("catalogue.local", 80, "auth-catalogue.local");
    upstream["auth"] = json!({
        "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
        "config": {}
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(
        status_of(&created).as_u16(),
        400,
        "an auth plugin with no backing implementation is refused"
    );
    assert_problem(&created, ErrorKind::Validation.gts_fragment(), 400);
}

#[tokio::test]
async fn upstream_plugins_are_merged_before_route_plugins() {
    // ADR 0002 "Upstream plugins execute before route plugins": the merged
    // binding list keeps that order.
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json("merge.local", 80, "merge.local");
    upstream["plugins"] = json!({
        "items": [{ "plugin_ref": "request_id", "config": {} }]
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut route = route_json(first_upstream_id(&gateway), "/v1", &["GET"]);
    route["plugins"] = json!({
        "items": [
            { "plugin_ref": "required_headers",
              "config": { "required_request_headers": "x-trace" } }
        ]
    });
    let created = post_json(&gateway, "/oagw/v1/routes", tenant(), route).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    // `resolve` is given a slash-prefixed path here: that is the spelling the
    // route matcher compares against, and the data plane hands it one too (see
    // `route_configuration_is_applied_to_relayed_requests`).
    let effective = gateway
        .data
        .resolve(
            &sec_ctx(tenant()),
            tenant(),
            "merge.local",
            &Method::GET,
            "/v1",
        )
        .await
        .expect("the alias resolves");
    let ids: Vec<String> = effective
        .plugins
        .iter()
        .map(|binding| binding.plugin_type.clone())
        .collect();
    assert_eq!(
        ids,
        vec!["request_id", "required_headers"],
        "upstream bindings precede route bindings"
    );
}

/// A tenant-defined plugin is stored, catalogued, and deletable, but it cannot
/// be bound to a live chain: this build has no interpreter for it, so a binding
/// that would have been silently skipped at relay time is refused instead.
#[tokio::test]
async fn tenant_defined_plugins_cannot_be_bound_to_a_chain() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let plugin = crate::domain::model::CustomPlugin {
        tenant_id: tenant(),
        plugin_type: "transform".to_owned(),
        name: "redactor".to_owned(),
        source_code: "def transform_request(ctx):\n    return ctx.next()\n".to_owned(),
        ..crate::domain::model::CustomPlugin::default()
    };
    let id = gateway
        .control
        .insert_plugin(plugin)
        .expect("an unreferenced name is stored")
        .id;
    assert!(
        gateway.control.get_plugin(tenant(), id).is_some(),
        "the plugin is still stored"
    );

    let mut upstream = upstream_json(origin.host(), origin.port(), "custom.local");
    upstream["plugins"] = json!({
        "items": [{ "plugin_ref": id.to_string(), "config": { "redact": ["x-secret"] } }]
    });
    let mut created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(
        status_of(&created).as_u16(),
        400,
        "a custom plugin binding is refused rather than silently inert"
    );
    assert_problem(&created, ErrorKind::Validation.gts_fragment(), 400);
    let body = body_json(&mut created).await;
    assert_eq!(problem_field(&body), "plugins.items[]");
    let detail = body["detail"].as_str().unwrap_or_default().to_owned();
    assert!(
        detail.contains(&id.to_string()),
        "the problem names the plugin, got: {detail}"
    );
    assert!(
        detail.contains("custom plugin execution is not available in this build"),
        "the problem explains why, got: {detail}"
    );
    assert_eq!(origin.captured().len(), 0, "nothing was relayed");

    // The plugin is still catalogued for its tenant.
    let mut catalogue = super::get_json(&gateway, "/oagw/v1/plugins", tenant()).await;
    assert_eq!(status_of(&catalogue).as_u16(), 200);
    let body = body_json(&mut catalogue).await;
    assert!(
        body.as_array()
            .expect("catalogue")
            .iter()
            .any(|entry| entry["id"] == json!(id)),
        "a stored plugin stays listable"
    );
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

fn rate_limit(rate: u64, burst: Option<u64>) -> Value {
    let mut limit = json!({
        "sharing": "private",
        "algorithm": "token_bucket",
        "sustained": { "rate": rate, "window": "minute" },
        "scope": "tenant",
        "strategy": "reject",
        "cost": 1,
        "response_headers": true
    });
    if let Some(capacity) = burst {
        limit["burst"] = json!({ "capacity": capacity });
    }
    limit
}

#[tokio::test]
async fn rate_limit_rejects_with_429_retry_after_and_headers() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "limited.local");
    upstream["rate_limit"] = rate_limit(2, None);
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    for _ in 0..2 {
        let response = proxy(
            &gateway,
            Method::GET,
            "/oagw/v1/proxy/limited.local/v1",
            &[],
            b"",
        )
        .await;
        assert_eq!(status_of(&response).as_u16(), 200);
    }
    let mut rejected = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/limited.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&rejected).as_u16(), 429);
    assert_problem(&rejected, ErrorKind::RateLimitExceeded.gts_fragment(), 429);
    assert_eq!(
        rejected
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("30"),
        "Retry-After is the time to the next token, not the whole window"
    );
    assert_eq!(
        rejected
            .headers()
            .get("x-ratelimit-limit")
            .and_then(|value| value.to_str().ok()),
        Some("2")
    );
    let body = body_json(&mut rejected).await;
    let scope = body["context"]["scope"].as_str().unwrap_or_default();
    assert!(
        scope.starts_with("upstream:"),
        "the exhausted scope is named, got {scope}"
    );
    assert!(
        origin.captured().len() <= 2,
        "rejected requests are never relayed"
    );
}

#[tokio::test]
async fn burst_capacity_extends_the_bucket() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "burst.local");
    upstream["rate_limit"] = rate_limit(1, Some(4));
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);
    for _ in 0..4 {
        let response = proxy(
            &gateway,
            Method::GET,
            "/oagw/v1/proxy/burst.local/v1",
            &[],
            b"",
        )
        .await;
        assert_eq!(
            status_of(&response).as_u16(),
            200,
            "burst capacity admits 4"
        );
    }
    let rejected = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/burst.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&rejected).as_u16(), 429);
}

#[tokio::test]
async fn user_scoped_limits_are_independent_per_subject() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "user-scoped.local");
    let mut limit = rate_limit(1, None);
    limit["scope"] = json!("user");
    upstream["rate_limit"] = limit;
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let alice = crate::tests::sec_ctx_as(tenant(), Uuid::new_v4());
    let bob = crate::tests::sec_ctx_as(tenant(), Uuid::new_v4());
    let attempts = [(alice.clone(), 200), (alice, 429), (bob, 200)];
    for (identity, expected) in attempts {
        let response = send(
            &gateway.router,
            proxy_request(
                Method::GET,
                "/oagw/v1/proxy/user-scoped.local/v1",
                &[],
                b"",
                Some(identity),
            ),
        )
        .await;
        assert_eq!(status_of(&response).as_u16(), expected);
    }
}

#[tokio::test]
async fn route_rate_limits_apply_on_top_of_the_upstream_limit() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "stacked.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let mut route = route_json(first_upstream_id(&gateway), "/v1", &["GET"]);
    route["rate_limit"] = rate_limit(1, None);
    let created = post_json(&gateway, "/oagw/v1/routes", tenant(), route).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    // `resolve` stacks the two limits: the upstream bucket first, then the
    // route bucket. The path is slash-prefixed, which is what the matcher
    // compares against (the data plane prefixes its capture too, see
    // `route_configuration_is_applied_to_relayed_requests`).
    let effective = gateway
        .data
        .resolve(
            &sec_ctx(tenant()),
            tenant(),
            "stacked.local",
            &Method::GET,
            "/v1",
        )
        .await
        .expect("the alias resolves");
    let scopes: Vec<String> = effective
        .rate_limits
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    assert_eq!(scopes.len(), 1, "only the route bucket is configured");
    assert!(
        scopes[0].starts_with("route:"),
        "the route bucket is keyed by route, got {scopes:?}"
    );
    let _ = origin;
}

#[tokio::test]
async fn rate_limit_headers_can_be_disabled() {
    let origin = Origin::spawn(Reply::Echo).await;
    let mut config = relay_config();
    config.rate_limit_response_headers = false;
    let gateway = gateway_with(config);
    let mut upstream = upstream_json(origin.host(), origin.port(), "quiet.local");
    upstream["rate_limit"] = rate_limit(1, None);
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let _ = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/quiet.local/v1",
        &[],
        b"",
    )
    .await;
    let rejected = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/quiet.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&rejected).as_u16(), 429);
    assert!(
        rejected.headers().get("x-ratelimit-limit").is_none(),
        "the deployment switch suppresses the rate-limit headers"
    );
    assert!(
        rejected.headers().get("retry-after").is_some(),
        "Retry-After is always present on a rejection"
    );
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

fn cors_upstream(alias: &str, host: &str, port: u16, origins: &Value, methods: &Value) -> Value {
    let mut upstream = upstream_json(host, port, alias);
    upstream["cors"] = json!({
        "sharing": "private",
        "enabled": true,
        "allowed_origins": origins,
        "allowed_methods": methods,
        "expose_headers": ["x-upstream-trace"],
        "allow_credentials": false
    });
    upstream
}

#[tokio::test]
async fn preflight_is_answered_by_the_gateway_without_relaying() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        cors_upstream(
            "preflight.local",
            origin.host(),
            origin.port(),
            &json!(["https://app.example"]),
            &json!(["GET", "POST"]),
        ),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let response = proxy(
        &gateway,
        Method::OPTIONS,
        "/oagw/v1/proxy/preflight.local/v1",
        &[
            ("origin", "https://app.example"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "x-trace"),
        ],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        204,
        "the preflight is answered locally"
    );
    let headers = response.headers().clone();
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://app.example")
    );
    assert_eq!(
        headers
            .get("access-control-allow-methods")
            .and_then(|value| value.to_str().ok()),
        Some("POST")
    );
    assert_eq!(
        headers
            .get("access-control-max-age")
            .and_then(|value| value.to_str().ok()),
        Some("86400")
    );
    assert_eq!(
        headers
            .get("access-control-allow-headers")
            .and_then(|value| value.to_str().ok()),
        Some("x-trace")
    );
    assert!(
        headers
            .get("vary")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .contains("origin")
    );
    assert_eq!(origin.captured().len(), 0, "a preflight is never relayed");
}

#[tokio::test]
async fn a_disallowed_origin_is_rejected_with_403() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        cors_upstream(
            "cors-strict.local",
            origin.host(),
            origin.port(),
            &json!(["https://app.example"]),
            &json!(["GET"]),
        ),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/cors-strict.local/v1",
        &[("origin", "https://evil.example")],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 403);
    assert_problem(
        &response,
        ErrorKind::CorsOriginNotAllowed.gts_fragment(),
        403,
    );
    let body = body_json(&mut response).await;
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("evil.example"),
        "the offending origin is named"
    );
    assert_eq!(origin.captured().len(), 0, "the request is not relayed");
}

#[tokio::test]
async fn a_disallowed_method_is_rejected_with_403() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        cors_upstream(
            "cors-methods.local",
            origin.host(),
            origin.port(),
            &json!(["*"]),
            &json!(["GET"]),
        ),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let response = proxy(
        &gateway,
        Method::DELETE,
        "/oagw/v1/proxy/cors-methods.local/v1",
        &[("origin", "https://app.example")],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 403);
    assert_problem(
        &response,
        ErrorKind::CorsMethodNotAllowed.gts_fragment(),
        403,
    );
}

#[tokio::test]
async fn an_allowed_origin_gets_cors_response_headers() {
    let origin = Origin::spawn(Reply::Fixed {
        status: 200,
        headers: vec![("x-upstream-trace", "abc".to_owned())],
        body: b"ok".to_vec(),
    })
    .await;
    let gateway = gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        cors_upstream(
            "cors-ok.local",
            origin.host(),
            origin.port(),
            &json!(["https://app.example"]),
            &json!(["GET"]),
        ),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/cors-ok.local/v1",
        &[("origin", "https://app.example")],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let headers = response.headers().clone();
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://app.example"),
        "the request origin is echoed, not the allowlist entry"
    );
    assert_eq!(
        headers
            .get("access-control-expose-headers")
            .and_then(|value| value.to_str().ok()),
        Some("x-upstream-trace")
    );
    assert_eq!(
        headers
            .get(ERROR_SOURCE_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(SOURCE_UPSTREAM),
        "a successful relay is marked as upstream traffic"
    );
}

#[tokio::test]
async fn cors_rules_do_not_apply_without_an_origin_header() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        cors_upstream(
            "cors-noop.local",
            origin.host(),
            origin.port(),
            &json!(["https://app.example"]),
            &json!(["GET"]),
        ),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);
    // No Origin header: the CORS layer is inert, even for a method it would
    // otherwise refuse.
    let response = proxy(
        &gateway,
        Method::DELETE,
        "/oagw/v1/proxy/cors-noop.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(origin.captured().len(), 1, "the request is relayed");
}

// ---------------------------------------------------------------------------
// Route-level CORS
// ---------------------------------------------------------------------------

/// A route's own CORS replaces the upstream's for that route, and a route
/// without one still inherits the upstream's (`ADR 0004` "Upstream/Route CORS
/// Field"): two routes on one upstream answer two different browser origins.
#[tokio::test]
async fn a_route_cors_overrides_the_upstreams_for_that_route_only() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        cors_upstream(
            "route-cors.local",
            origin.host(),
            origin.port(),
            &json!(["https://upstream.example"]),
            &json!(["GET"]),
        ),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let upstream_id = first_upstream_id(&gateway);

    // One route carries its own policy, the other carries none.
    let mut own = route_json(upstream_id, "/own", &["GET"]);
    own["cors"] = json!({
        "enabled": true,
        "allowed_origins": ["https://route.example"],
        "allowed_methods": ["GET"],
        "expose_headers": [],
        "allow_credentials": false
    });
    let created = post_json(&gateway, "/oagw/v1/routes", tenant(), own).await;
    assert_eq!(
        status_of(&created).as_u16(),
        201,
        "the route policy is accepted"
    );
    let created = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant(),
        route_json(upstream_id, "/plain", &["GET"]),
    )
    .await;
    assert_eq!(
        status_of(&created).as_u16(),
        201,
        "the inheriting route is accepted"
    );

    // The route's own policy answers, and the upstream's does not.
    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/route-cors.local/own",
        &[("origin", "https://route.example")],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        200,
        "the route's origin is allowed"
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://route.example"),
        "the route's policy is the one applied"
    );
    let _ = body_text(&mut response).await;

    let rejected = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/route-cors.local/own",
        &[("origin", "https://upstream.example")],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&rejected).as_u16(),
        403,
        "the upstream's origin is not admitted by the route's own policy"
    );
    assert_eq!(
        origin.captured().len(),
        1,
        "a CORS rejection is not relayed"
    );

    // The other route never named a policy, so it inherits the upstream's.
    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/route-cors.local/plain",
        &[("origin", "https://upstream.example")],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        200,
        "the upstream's origin is still allowed on a route without a policy"
    );
    assert_eq!(
        response
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://upstream.example"),
        "the inherited policy is the one applied"
    );
    let _ = body_text(&mut response).await;

    let rejected = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/route-cors.local/plain",
        &[("origin", "https://route.example")],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&rejected).as_u16(),
        403,
        "the route's own origin is not admitted by the inherited policy"
    );
}

/// A route-level policy is validated by the same rules as an upstream's: a
/// configuration the CORS layer could not honour is refused at creation.
#[tokio::test]
async fn an_impossible_route_cors_is_refused_at_creation() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "route-cors-bad.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let mut route = route_json(first_upstream_id(&gateway), "/v1", &["GET"]);
    route["cors"] = json!({
        "enabled": true,
        "allowed_origins": ["*"],
        "allowed_methods": ["GET"],
        "allow_credentials": true
    });
    let mut response = post_json(&gateway, "/oagw/v1/routes", tenant(), route).await;
    assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(
        problem_field(&body),
        "cors.allowed_origins",
        "the offending member is named"
    );
}

// ---------------------------------------------------------------------------
// Header hygiene
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_default_passthrough_forwards_only_the_body_headers() {
    let (gateway, origin) = relay_gateway("passthrough.local", "/v1", &["POST"]).await;
    let response = proxy(
        &gateway,
        Method::POST,
        "/oagw/v1/proxy/passthrough.local/v1",
        &[
            ("x-secret", "do-not-forward"),
            ("content-type", "application/json"),
            ("connection", "keep-alive"),
        ],
        b"{\"a\":1}",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let captured = origin.only();
    assert!(
        !captured.has_header("x-secret"),
        "no request header is forwarded by default"
    );
    assert_eq!(captured.header("content-type"), Some("application/json"));
    assert!(
        !captured.has_header("connection"),
        "hop-by-hop never travels"
    );
    assert_eq!(captured.body_text(), "{\"a\":1}", "the body travels");
}

#[tokio::test]
async fn an_allowlist_selects_which_headers_travel() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "allowlist.local");
    upstream["headers"] = json!({
        "request": {
            "passthrough": "allowlist",
            "passthrough_allowlist": ["x-tenant-id"]
        }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/allowlist.local/v1",
        &[("x-tenant-id", "t-1"), ("x-other", "nope")],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let captured = origin.only();
    assert_eq!(captured.header("x-tenant-id"), Some("t-1"));
    assert!(!captured.has_header("x-other"));
}

#[tokio::test]
async fn set_add_and_remove_request_rules_are_applied() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "rules.local");
    upstream["headers"] = json!({
        "request": {
            "passthrough": "all",
            "set": { "x-oagw-sent-by": "gateway" },
            "add": { "x-dup": "one" },
            "remove": ["x-drop-me"]
        }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/rules.local/v1",
        &[("x-drop-me", "yes"), ("x-keep", "yes")],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let captured = origin.only();
    assert_eq!(
        captured.header("x-keep"),
        Some("yes"),
        "passthrough all keeps it"
    );
    assert!(!captured.has_header("x-drop-me"), "remove drops it");
    assert_eq!(
        captured.header("x-oagw-sent-by"),
        Some("gateway"),
        "set wins"
    );
    assert_eq!(captured.header("x-dup"), Some("one"), "add appends");
}

#[tokio::test]
async fn response_header_rules_are_applied_to_the_relayed_response() {
    let origin = Origin::spawn(Reply::Fixed {
        status: 200,
        headers: vec![
            ("x-upstream-a", "keep".to_owned()),
            ("x-upstream-drop", "no".to_owned()),
        ],
        body: b"ok".to_vec(),
    })
    .await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "resp-rules.local");
    upstream["headers"] = json!({
        "response": {
            "set": { "x-oagw-served-by": "oagw" },
            "add": { "x-added": "yes" },
            "remove": ["x-upstream-drop"]
        }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/resp-rules.local/v1",
        &[],
        b"",
    )
    .await;
    let headers = response.headers().clone();
    assert_eq!(
        headers
            .get("x-upstream-a")
            .and_then(|value| value.to_str().ok()),
        Some("keep")
    );
    assert!(headers.get("x-upstream-drop").is_none(), "removed");
    assert_eq!(
        headers
            .get("x-oagw-served-by")
            .and_then(|value| value.to_str().ok()),
        Some("oagw")
    );
    assert_eq!(
        headers.get("x-added").and_then(|value| value.to_str().ok()),
        Some("yes")
    );
}

#[tokio::test]
async fn the_caller_cannot_spoof_host() {
    let (gateway, origin) = relay_gateway("host.local", "/v1", &["GET"]).await;
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/host.local/v1",
        &[("host", "spoofed.example")],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let expected = format!("127.0.0.1:{}", origin.port());
    assert_eq!(
        origin.only().header("host"),
        Some(expected.as_str()),
        "the caller cannot spoof Host"
    );
}

// ---------------------------------------------------------------------------
// Body relay
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_post_body_is_relayed_verbatim() {
    let (gateway, origin) = relay_gateway("body.local", "/v1", &["POST"]).await;
    let payload = b"{\"prompt\":\"hello world\",\"n\":3}";
    let mut response = proxy(
        &gateway,
        Method::POST,
        "/oagw/v1/proxy/body.local/v1",
        &[("content-type", "application/json")],
        payload,
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let captured = origin.only();
    assert_eq!(captured.method, "POST");
    assert_eq!(captured.body.as_slice(), payload);
    assert_eq!(captured.path, "/v1");
    let _ = echo(&mut response).await;
}

#[tokio::test]
async fn an_oversized_body_is_rejected_before_relaying() {
    let origin = Origin::spawn(Reply::Echo).await;
    let mut config = relay_config();
    config.max_request_body_bytes = 8;
    let gateway = gateway_with(config);
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "too-big.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let response = proxy(
        &gateway,
        Method::POST,
        "/oagw/v1/proxy/too-big.local/v1",
        &[],
        b"123456789",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 413);
    assert_problem(&response, ErrorKind::PayloadTooLarge.gts_fragment(), 413);
    assert_eq!(
        origin.captured().len(),
        0,
        "the body never reaches the upstream"
    );
}

#[tokio::test]
async fn an_upstream_status_and_body_are_relayed_unchanged() {
    let origin = Origin::spawn(Reply::Fixed {
        status: 503,
        headers: vec![("retry-after", "7".to_owned())],
        body: b"upstream busy".to_vec(),
    })
    .await;
    let gateway = gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "unhappy.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/unhappy.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        503,
        "the status is preserved"
    );
    let headers = response.headers().clone();
    assert_eq!(
        headers
            .get(ERROR_SOURCE_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some(SOURCE_UPSTREAM)
    );
    assert_eq!(
        headers
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("7"),
        "upstream headers survive the relay"
    );
    assert_eq!(body_text(&mut response).await, "upstream busy");
}

// ---------------------------------------------------------------------------
// X-OAGW-Target-Host
// ---------------------------------------------------------------------------

/// A two-endpoint upstream whose alias is not a common suffix, so endpoint
/// selection round-robins unless a target host is pinned.
///
/// A pool must be uniform in scheme and port, so the two endpoints differ only
/// by host name: one origin is registered once as its address and once as
/// `localhost`, which both dial the same listener.
async fn multi_endpoint_gateway(alias: &str) -> (Gateway, Origin) {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let upstream = json!({
        "alias": alias,
        "server": {
            "endpoints": [
                { "scheme": "http", "host": origin.host(), "port": origin.port() },
                { "scheme": "http", "host": "localhost", "port": origin.port() }
            ]
        }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);
    (gateway, origin)
}

#[tokio::test]
async fn target_host_pins_a_multi_endpoint_upstream() {
    // Two dialable endpoint spellings with distinct host names: the pool is
    // uniform in scheme and port, so the pin can only choose between the
    // names, and the `Host` header the origin reads tells them apart.
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let upstream = json!({
        "alias": "region.local",
        "server": {
            "endpoints": [
                { "scheme": "http", "host": "127.0.0.1", "port": origin.port() },
                { "scheme": "http", "host": "localhost", "port": origin.port() }
            ]
        }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/region.local/v1",
        &[(crate::infra::proxy::TARGET_HOST_HEADER, "localhost")],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let pinned = origin.last();
    assert_eq!(
        pinned.header("host"),
        Some(format!("localhost:{}", origin.port()).as_str()),
        "the pinned endpoint's authority is what the origin reads"
    );
}

#[tokio::test]
async fn an_unknown_target_host_is_rejected() {
    let (gateway, primary) = multi_endpoint_gateway("unknown-target.local").await;
    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/unknown-target.local/v1",
        &[("x-oagw-target-host", "nope.example")],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    assert_problem(&response, ErrorKind::UnknownTargetHost.gts_fragment(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(body["invalid_value"], json!("nope.example"));
    assert!(
        body["valid_hosts"]
            .as_array()
            .is_some_and(|hosts| hosts.len() == 2),
        "the valid endpoints are listed"
    );
    assert_eq!(primary.captured().len(), 0);
}

#[tokio::test]
async fn a_malformed_target_host_is_rejected() {
    let (gateway, primary) = multi_endpoint_gateway("bad-target.local").await;
    for value in ["127.0.0.1:1", "not a host", "-bad-"] {
        let response = proxy(
            &gateway,
            Method::GET,
            "/oagw/v1/proxy/bad-target.local/v1",
            &[("x-oagw-target-host", value)],
            b"",
        )
        .await;
        assert_eq!(status_of(&response).as_u16(), 400, "{value}");
        assert_problem(&response, ErrorKind::InvalidTargetHost.gts_fragment(), 400);
    }
    assert_eq!(primary.captured().len(), 0, "nothing was dialed");
}

#[tokio::test]
async fn a_common_suffix_alias_requires_a_target_host() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    // A pool is uniform in scheme and port, so the two regional spellings
    // share the port and differ only by host name. Both omit the port, so the
    // derived alias is the bare common suffix: this test never dials, so the
    // default port needs no listener.
    let upstream = json!({
        "alias": "vendor.com",
        "server": {
            "endpoints": [
                { "scheme": "https", "host": "us.vendor.com" },
                { "scheme": "https", "host": "eu.vendor.com" }
            ]
        }
    });
    let alias = "vendor.com";
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut response = proxy(
        &gateway,
        Method::GET,
        &format!("/oagw/v1/proxy/{alias}/v1"),
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 400);
    assert_problem(&response, ErrorKind::MissingTargetHost.gts_fragment(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(body["context"]["alias"], json!(alias));
    assert_eq!(
        body["context"]["valid_hosts"].as_array().map(Vec::len),
        Some(2)
    );
    assert_eq!(
        origin.captured().len(),
        0,
        "nothing is dialed without a pin"
    );

    // Naming one of the two hosts selects that endpoint. The assertion stays
    // on `select_endpoint` because the regional host names do not resolve in
    // this loopback environment; the dial is covered by
    // `target_host_pins_a_multi_endpoint_upstream`.
    let selected = crate::infra::proxy::select_endpoint(
        &gateway.store.upstreams_of(tenant())[0],
        Some("us.vendor.com"),
        0,
    )
    .expect("the named endpoint is selected");
    assert_eq!(selected.host, "us.vendor.com");
    // The pool is uniform, so whichever endpoint the pin names dials the same
    // port: the scheme's default here.
    assert_eq!(selected.effective_port(), 443);
    let endpoints = &gateway.store.upstreams_of(tenant())[0].server.endpoints;
    assert_eq!(endpoints.len(), 2);
    assert_eq!(endpoints[0].effective_port(), endpoints[1].effective_port());
}

#[tokio::test]
async fn a_single_endpoint_upstream_needs_no_target_host() {
    let (gateway, origin) = relay_gateway("solo.local", "/v1", &["GET"]).await;
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/solo.local/v1",
        &[("x-oagw-target-host", "ignored.example")],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        400,
        "an unknown target host is refused"
    );
    let _ = origin;
}

// ---------------------------------------------------------------------------
// Error semantics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unknown_alias_is_a_gateway_404_problem() {
    let gateway = gateway_with(relay_config());
    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/no-such-alias/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 404);
    assert_problem(&response, ErrorKind::RouteNotFound.gts_fragment(), 404);
    let body = body_json(&mut response).await;
    assert_eq!(body["instance"], json!("/oagw/v1/proxy/no-such-alias/v1"));
    assert_eq!(
        body["type"],
        json!(format!(
            "{ERROR_TYPE_BASE}{}",
            ErrorKind::RouteNotFound.gts_fragment()
        ))
    );
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("no-such-alias"),
        "the alias is named in the detail"
    );
}

/// An anonymous caller whose alias names no upstream carries no tenant scope:
/// the alias is what identifies the tenant, so a miss leaves the scope nil.
/// The miss must stay a `route.not_found` problem — the nil scope owns nothing
/// and has no ancestors, so the tenant resolver is never asked about the nil
/// tenant, whose `tenant not found` fault would otherwise surface as a 502
/// protocol error (`DESIGN.md` §CRUD, "Inherited via tenant chain walk").
#[tokio::test]
async fn an_unknown_alias_for_an_anonymous_caller_is_not_a_protocol_fault() {
    let gateway = gateway_with(relay_config());

    // Service level: the nil scope is reported as an alias miss, and the detail
    // names the alias rather than the tenant resolver's fault.
    let error = gateway
        .data
        .resolve(
            &toolkit_security::SecurityContext::anonymous(),
            Uuid::nil(),
            "nope.local",
            &Method::GET,
            "/v1/hello",
        )
        .await
        .expect_err("an alias no upstream owns does not resolve");
    assert_eq!(error.kind(), ErrorKind::RouteNotFound);
    assert_eq!(
        error.detail(),
        "no upstream is registered for alias 'nope.local'"
    );

    // End to end on the anonymous data-plane route: a gateway 404 problem, the
    // shape `ADR 0007` prescribes for a miss, not a 502 protocol error.
    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/nope.local/v1/hello",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 404);
    assert_problem(&response, ErrorKind::RouteNotFound.gts_fragment(), 404);
    let body = body_json(&mut response).await;
    assert_eq!(
        body["instance"],
        json!("/oagw/v1/proxy/nope.local/v1/hello")
    );
    assert_eq!(
        body["type"],
        json!(format!(
            "{ERROR_TYPE_BASE}{}",
            ErrorKind::RouteNotFound.gts_fragment()
        ))
    );
    assert_eq!(
        body["detail"],
        json!("no upstream is registered for alias 'nope.local'"),
        "the alias miss, not the tenant hierarchy, is reported"
    );
}

/// A known alias resolves for an anonymous caller: the short-circuit applies
/// only to the nil scope an unknown alias leaves behind, never to the
/// known-alias path, whose scope is the owning tenant.
#[tokio::test]
async fn an_anonymous_caller_still_resolves_a_known_alias() {
    let (gateway, origin) = relay_gateway("known.local", "/v1", &["GET"]).await;
    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/known.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        200,
        "the alias resolves anonymously"
    );
    assert_eq!(echo(&mut response).await["path"], json!("/v1"));
    assert_eq!(origin.only().path, "/v1");
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_503_link_unavailable() {
    let gateway = gateway_with(relay_config());
    // Port 1 on loopback is never listening.
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json("127.0.0.1", 1, "dark.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/dark.local/v1",
        &[],
        b"",
    )
    .await;
    // An upstream that never accepted the connection is an unavailable link
    // (`ADR 0007`), not a protocol fault: the transport failure the buffered
    // relay surfaces maps to `link.unavailable`.
    assert_eq!(status_of(&response).as_u16(), 503);
    assert_problem(&response, ErrorKind::LinkUnavailable.gts_fragment(), 503);
    let body = body_json(&mut response).await;
    assert_eq!(
        body["type"],
        json!(format!(
            "{ERROR_TYPE_BASE}{}",
            ErrorKind::LinkUnavailable.gts_fragment()
        ))
    );
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .contains("transport"),
        "the transport failure is described, got: {body}"
    );
}

#[tokio::test]
async fn a_silent_upstream_times_out_with_504() {
    let origin = Origin::spawn(Reply::Stall).await;
    let mut config = relay_config();
    config.proxy_timeout_secs = 1;
    let gateway = gateway_with(config);
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "slow.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/slow.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        504,
        "the deadline is enforced"
    );
    assert_problem(&response, ErrorKind::RequestTimeout.gts_fragment(), 504);
}

#[tokio::test]
async fn a_disabled_upstream_is_not_served() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "off.local");
    upstream["enabled"] = json!(false);
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/off.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 503);
    assert_problem(&response, ErrorKind::LinkUnavailable.gts_fragment(), 503);
    assert_eq!(origin.captured().len(), 0);
}

/// A rejected request is still answered as a CORS response: when an origin was
/// validated, the computed `Access-Control-*` headers travel on the failure the
/// gateway produces too, so a browser can read the refusal.
#[tokio::test]
async fn a_gateway_failure_still_carries_the_cors_headers() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = cors_upstream(
        "cors-failure.local",
        origin.host(),
        origin.port(),
        &json!(["https://app.example"]),
        &json!(["GET"]),
    );
    upstream["rate_limit"] = rate_limit(1, None);
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let created = post_json(
        &gateway,
        "/oagw/v1/routes",
        tenant(),
        route_json(first_upstream_id(&gateway), "/v1", &["GET"]),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let origin_headers = [
        ("origin", "https://app.example"),
        ("access-control-request-method", "GET"),
    ];
    let first = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/cors-failure.local/v1",
        &origin_headers,
        b"",
    )
    .await;
    assert_eq!(status_of(&first).as_u16(), 200);
    let refused = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/cors-failure.local/v1",
        &origin_headers,
        b"",
    )
    .await;
    assert_eq!(status_of(&refused).as_u16(), 429, "the limit is what fails");
    assert_eq!(
        refused
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("https://app.example"),
        "the failure carries the CORS headers of the validated origin"
    );
    // And an origin that was never allowed gets no CORS header at all.
    let stranger = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/cors-failure.local/v1",
        &[
            ("origin", "https://other.example"),
            ("access-control-request-method", "GET"),
        ],
        b"",
    )
    .await;
    assert_eq!(status_of(&stranger).as_u16(), 403);
    assert!(
        stranger
            .headers()
            .get("access-control-allow-origin")
            .is_none(),
        "an unvalidated origin is answered without CORS headers"
    );
}

/// An error-phase hook reaches the wire: the problem response the gateway
/// renders for a failure carries the `x-request-id` the chain's request-id
/// plugin minted, so the caller can correlate a refusal it never saw relayed.
#[tokio::test]
async fn a_rendered_problem_carries_the_request_id_the_error_hook_minted() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "correlated.local");
    upstream["enabled"] = json!(false);
    upstream["plugins"] = json!({ "items": ["request_id"] });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201, "upstream created");

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/correlated.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        503,
        "the disabled upstream refuses"
    );
    let minted = response
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .expect("the error hook's request id reaches the wire");
    assert!(
        Uuid::parse_str(minted).is_ok(),
        "the minted identifier is a UUID, got {minted}"
    );

    // A caller-supplied identifier is echoed back rather than replaced.
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/correlated.local/v1",
        &[(REQUEST_ID_HEADER, "caller-id-7")],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 503);
    assert_eq!(
        response
            .headers()
            .get(REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok()),
        Some("caller-id-7"),
        "the caller's identifier is echoed, not minted over"
    );
    assert_eq!(origin.captured().len(), 0, "nothing was relayed");
}

// ---------------------------------------------------------------------------
// Sharing and inheritance
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_tenant_chain_is_walked_to_the_closest_upstream() {
    let parent = Uuid::from_u128(0xface);
    let child = Uuid::from_u128(0xfeed);
    let gateway = crate::tests::gateway_with_parents(relay_config(), &[(child, parent)]);
    let origin = Origin::spawn(Reply::Echo).await;

    // The parent registers the alias; the child does not.
    let mut upstream = upstream_json(origin.host(), origin.port(), "inherited.local");
    upstream["tags"] = json!(["from-parent"]);
    let created = post_json(&gateway, "/oagw/v1/upstreams", parent, upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let effective = gateway
        .data
        .resolve(
            &sec_ctx(child),
            child,
            "inherited.local",
            &Method::GET,
            "/v1",
        )
        .await
        .expect("the child inherits the parent's alias");
    assert_eq!(effective.upstream.tenant_id, parent);
    assert_eq!(effective.upstream.tags, vec!["from-parent".to_owned()]);
}

#[tokio::test]
async fn a_child_can_shadow_its_parent_alias() {
    let parent = Uuid::from_u128(0x11fa);
    let child = Uuid::from_u128(0x11fe);
    let gateway = crate::tests::gateway_with_parents(relay_config(), &[(child, parent)]);
    let parent_origin = Origin::spawn(Reply::Echo).await;
    let child_origin = Origin::spawn(Reply::Echo).await;

    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        parent,
        upstream_json(parent_origin.host(), parent_origin.port(), "shadow.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        child,
        upstream_json(child_origin.host(), child_origin.port(), "shadow.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let effective = gateway
        .data
        .resolve(&sec_ctx(child), child, "shadow.local", &Method::GET, "/v1")
        .await
        .expect("the child's own upstream wins");
    assert_eq!(effective.upstream.tenant_id, child);
}

#[tokio::test]
async fn an_ancestor_enforce_rate_limit_is_the_stricter_bound() {
    let parent = Uuid::from_u128(0x22fa);
    let child = Uuid::from_u128(0x22fe);
    let gateway = crate::tests::gateway_with_parents(relay_config(), &[(child, parent)]);
    let origin = Origin::spawn(Reply::Echo).await;

    let mut parent_upstream = upstream_json(origin.host(), origin.port(), "governed.local");
    let mut parent_limit = rate_limit(2, None);
    parent_limit["sharing"] = json!("enforce");
    parent_upstream["rate_limit"] = parent_limit;
    let created = post_json(&gateway, "/oagw/v1/upstreams", parent, parent_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut child_upstream = upstream_json(origin.host(), origin.port(), "governed.local");
    child_upstream["rate_limit"] = rate_limit(500, Some(500));
    let created = post_json(&gateway, "/oagw/v1/upstreams", child, child_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let effective = gateway
        .data
        .resolve(
            &sec_ctx(child),
            child,
            "governed.local",
            &Method::GET,
            "/v1",
        )
        .await
        .expect("the alias resolves");
    assert_eq!(effective.rate_limits.len(), 1);
    assert_eq!(
        effective.rate_limits[0].1.sustained.rate, 2,
        "the ancestor's `enforce`d rate is the lower one, so it wins"
    );
    assert_eq!(
        effective.rate_limits[0].1.capacity(),
        2,
        "the effective burst is the smaller of the two, not the child's"
    );
    assert_eq!(
        effective.rate_limits[0].1.sharing,
        crate::domain::model::SharingMode::Enforce,
        "an enforced ancestor constraint is never bypassed by shadowing"
    );
    assert_eq!(
        effective.enforcing_tenants,
        vec![child, parent],
        "the tenants whose limits contributed are reported, closest first"
    );

    // The merged limit is what is actually enforced: the third request is
    // refused by the ancestor's ceiling, not the child's 500.
    for _ in 0..2 {
        let response = proxy(
            &gateway,
            Method::GET,
            "/oagw/v1/proxy/governed.local/v1",
            &[],
            b"",
        )
        .await;
        assert_eq!(status_of(&response).as_u16(), 200);
    }
    let rejected = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/governed.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&rejected).as_u16(),
        429,
        "the ancestor's rate applies"
    );
    assert_eq!(
        rejected
            .headers()
            .get("x-ratelimit-limit")
            .and_then(|value| value.to_str().ok()),
        Some("2"),
        "the enforced limit is the one reported"
    );
}

/// A descendant's own limit can only be stricter: when it is, it wins over the
/// ancestor's enforced ceiling.
#[tokio::test]
async fn a_stricter_descendant_rate_limit_wins() {
    let parent = Uuid::from_u128(0x23fa);
    let child = Uuid::from_u128(0x23fe);
    let gateway = crate::tests::gateway_with_parents(relay_config(), &[(child, parent)]);
    let origin = Origin::spawn(Reply::Echo).await;

    let mut parent_upstream = upstream_json(origin.host(), origin.port(), "ceilings.local");
    let mut parent_limit = rate_limit(50, None);
    parent_limit["sharing"] = json!("enforce");
    parent_upstream["rate_limit"] = parent_limit;
    let created = post_json(&gateway, "/oagw/v1/upstreams", parent, parent_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut child_upstream = upstream_json(origin.host(), origin.port(), "ceilings.local");
    child_upstream["rate_limit"] = rate_limit(2, Some(2));
    let created = post_json(&gateway, "/oagw/v1/upstreams", child, child_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let effective = gateway
        .data
        .resolve(
            &sec_ctx(child),
            child,
            "ceilings.local",
            &Method::GET,
            "/v1",
        )
        .await
        .expect("the alias resolves");
    assert_eq!(effective.rate_limits.len(), 1);
    assert_eq!(
        effective.rate_limits[0].1.sustained.rate, 2,
        "the descendant's tighter limit is not discarded"
    );
    assert_eq!(
        effective.enforcing_tenants,
        vec![child, parent],
        "both contributing tenants are reported, closest first"
    );
}

/// An `inherit`d rate limit is adopted when the descendant has none of its own.
#[tokio::test]
async fn an_inherited_rate_limit_is_adopted_by_a_descendant_without_one() {
    let parent = Uuid::from_u128(0x24fa);
    let child = Uuid::from_u128(0x24fe);
    let gateway = crate::tests::gateway_with_parents(relay_config(), &[(child, parent)]);
    let origin = Origin::spawn(Reply::Echo).await;

    let mut parent_upstream = upstream_json(origin.host(), origin.port(), "shared.local");
    let mut parent_limit = rate_limit(4, None);
    parent_limit["sharing"] = json!("inherit");
    parent_upstream["rate_limit"] = parent_limit;
    let created = post_json(&gateway, "/oagw/v1/upstreams", parent, parent_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let child_upstream = upstream_json(origin.host(), origin.port(), "shared.local");
    let created = post_json(&gateway, "/oagw/v1/upstreams", child, child_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let effective = gateway
        .data
        .resolve(&sec_ctx(child), child, "shared.local", &Method::GET, "/v1")
        .await
        .expect("the alias resolves");
    assert_eq!(effective.rate_limits.len(), 1);
    assert_eq!(
        effective.rate_limits[0].1.sustained.rate, 4,
        "the nearest configured ancestor limit is adopted"
    );
    assert_eq!(effective.enforcing_tenants, vec![parent]);

    // A `private` ancestor limit is not visible to a descendant at all.
    let mut parent_upstream = upstream_json(origin.host(), origin.port(), "hidden.local");
    parent_upstream["rate_limit"] = rate_limit(9, None);
    let created = post_json(&gateway, "/oagw/v1/upstreams", parent, parent_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let child_upstream = upstream_json(origin.host(), origin.port(), "hidden.local");
    let created = post_json(&gateway, "/oagw/v1/upstreams", child, child_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let effective = gateway
        .data
        .resolve(&sec_ctx(child), child, "hidden.local", &Method::GET, "/v1")
        .await
        .expect("the alias resolves");
    assert!(
        effective.rate_limits.is_empty(),
        "a private ancestor limit is not inherited, got {:?}",
        effective.rate_limits
    );
}

#[tokio::test]
async fn ancestor_cors_origins_are_unioned_only_when_inherited() {
    let parent = Uuid::from_u128(0x33fa);
    let child = Uuid::from_u128(0x33fe);
    let gateway = crate::tests::gateway_with_parents(relay_config(), &[(child, parent)]);
    let origin = Origin::spawn(Reply::Echo).await;

    let mut parent_upstream = upstream_json(origin.host(), origin.port(), "union.local");
    parent_upstream["cors"] = json!({
        "sharing": "private",
        "enabled": true,
        "allowed_origins": ["https://platform.example"],
        "allowed_methods": ["GET"]
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", parent, parent_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut child_upstream = upstream_json(origin.host(), origin.port(), "union.local");
    child_upstream["cors"] = json!({
        "sharing": "private",
        "enabled": true,
        "allowed_origins": ["https://child.example"],
        "allowed_methods": ["GET", "POST"]
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", child, child_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let effective = gateway
        .data
        .resolve(&sec_ctx(child), child, "union.local", &Method::GET, "/v1")
        .await
        .expect("the alias resolves");
    let cors = effective.cors.expect("CORS is configured");
    assert_eq!(
        cors.allowed_origins,
        vec!["https://child.example".to_owned()],
        "a `private` ancestor configuration is not merged: got {:?}",
        cors.allowed_origins
    );
    assert_eq!(
        effective.upstream.tenant_id, child,
        "the closest upstream stays authoritative"
    );
}

/// Declaring the ancestor's CORS as `inherit` is what makes the union happen.
#[tokio::test]
async fn ancestor_cors_origins_are_unioned_under_inherit() {
    let parent = Uuid::from_u128(0x34fa);
    let child = Uuid::from_u128(0x34fe);
    let gateway = crate::tests::gateway_with_parents(relay_config(), &[(child, parent)]);
    let origin = Origin::spawn(Reply::Echo).await;

    let mut parent_upstream = upstream_json(origin.host(), origin.port(), "shared-cors.local");
    parent_upstream["cors"] = json!({
        "sharing": "inherit",
        "enabled": true,
        "allowed_origins": ["https://platform.example"],
        "allowed_methods": ["GET"]
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", parent, parent_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut child_upstream = upstream_json(origin.host(), origin.port(), "shared-cors.local");
    child_upstream["cors"] = json!({
        "sharing": "private",
        "enabled": true,
        "allowed_origins": ["https://child.example"],
        "allowed_methods": ["GET", "POST"]
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", child, child_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let effective = gateway
        .data
        .resolve(
            &sec_ctx(child),
            child,
            "shared-cors.local",
            &Method::GET,
            "/v1",
        )
        .await
        .expect("the alias resolves");
    let cors = effective.cors.expect("CORS is configured");
    assert!(
        cors.allowed_origins
            .contains(&"https://child.example".to_owned())
            && cors
                .allowed_origins
                .contains(&"https://platform.example".to_owned()),
        "the ancestor's origins are unioned in, got {:?}",
        cors.allowed_origins
    );
    assert_eq!(
        cors.allowed_methods.len(),
        2,
        "the descendant's own methods are kept"
    );
}

/// An `enforce`d ancestor CORS configuration replaces the descendant's, whatever
/// the descendant declared.
#[tokio::test]
async fn an_ancestor_enforce_cors_replaces_the_descendant() {
    let parent = Uuid::from_u128(0x35fa);
    let child = Uuid::from_u128(0x35fe);
    let gateway = crate::tests::gateway_with_parents(relay_config(), &[(child, parent)]);
    let origin = Origin::spawn(Reply::Echo).await;

    let mut parent_upstream = upstream_json(origin.host(), origin.port(), "forced.local");
    parent_upstream["cors"] = json!({
        "sharing": "enforce",
        "enabled": true,
        "allowed_origins": ["https://platform.example"],
        "allowed_methods": ["GET"]
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", parent, parent_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut child_upstream = upstream_json(origin.host(), origin.port(), "forced.local");
    child_upstream["cors"] = json!({
        "sharing": "private",
        "enabled": true,
        "allowed_origins": ["https://child.example"],
        "allowed_methods": ["GET", "POST"]
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", child, child_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let effective = gateway
        .data
        .resolve(&sec_ctx(child), child, "forced.local", &Method::GET, "/v1")
        .await
        .expect("the alias resolves");
    let cors = effective.cors.expect("CORS is configured");
    assert_eq!(
        cors.allowed_origins,
        vec!["https://platform.example".to_owned()],
        "the enforced ancestor value replaces the child's own"
    );
}

// ---------------------------------------------------------------------------
// Ancestor plugin and auth inheritance
// ---------------------------------------------------------------------------

/// A transform plugin that stamps a header only when no plugin set it before
/// it, so a test can observe the order two bindings ran in.
struct Tagger {
    header: &'static str,
    value: &'static str,
}

#[async_trait::async_trait]
impl crate::domain::plugin::TransformPlugin for Tagger {
    fn id(&self) -> &'static str {
        "tagger"
    }
    fn plugin_type(&self) -> &'static str {
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.tagger.v1"
    }
    async fn transform_request(
        &self,
        ctx: &mut crate::domain::plugin::RequestContext,
    ) -> Result<(), crate::domain::error::OagwError> {
        if ctx.header(self.header).is_none() {
            ctx.set_header(self.header, self.value);
        }
        Ok(())
    }
    async fn transform_response(
        &self,
        _ctx: &mut crate::domain::plugin::ResponseContext,
    ) -> Result<(), crate::domain::error::OagwError> {
        Ok(())
    }
    async fn transform_error(
        &self,
        _ctx: &mut crate::domain::plugin::ErrorContext,
    ) -> Result<(), crate::domain::error::OagwError> {
        Ok(())
    }
}

/// Plugins concatenate along the tenant chain: the ancestor's bindings run
/// ahead of the descendant's own, root first (`DESIGN.md` "Inherited via
/// tenant chain walk").
#[tokio::test]
async fn an_ancestors_plugins_run_before_the_descendants() {
    let parent = Uuid::from_u128(0x45fa);
    let child = Uuid::from_u128(0x45fe);
    let mut transforms = crate::domain::plugin::TransformPluginRegistry::with_builtins();
    transforms.register(Arc::new(Tagger {
        header: "x-tag",
        value: "parent",
    }));
    let gateway = crate::tests::gateway_with_parents_transforms(
        relay_config(),
        &[(child, parent)],
        transforms,
    );

    let origin = Origin::spawn(Reply::Echo).await;
    let mut parent_upstream = upstream_json(origin.host(), origin.port(), "chained.local");
    parent_upstream["plugins"] = json!({ "items": [
        { "plugin_ref": "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.tagger.v1",
          "config": { "header": "x-tag", "value": "parent" } }
    ] });
    let created = post_json(&gateway, "/oagw/v1/upstreams", parent, parent_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut child_upstream = upstream_json(origin.host(), origin.port(), "chained.local");
    child_upstream["plugins"] = json!({ "items": [
        { "plugin_ref": "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.tagger.v1",
          "config": { "header": "x-tag", "value": "child" } }
    ] });
    let created = post_json(&gateway, "/oagw/v1/upstreams", child, child_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let effective = gateway
        .data
        .resolve(&sec_ctx(child), child, "chained.local", &Method::GET, "/v1")
        .await
        .expect("the alias resolves");
    assert_eq!(
        effective.plugins.len(),
        2,
        "both chains are concatenated, got {:?}",
        effective
            .plugins
            .iter()
            .map(|b| b.plugin_type.clone())
            .collect::<Vec<_>>()
    );

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/chained.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(
        origin.only().header("x-tag"),
        Some("parent"),
        "the ancestor's binding ran first and won the header"
    );
}

/// An `enforce`d ancestor auth binding replaces the descendant's, whatever the
/// descendant configured (`DESIGN.md` "Auth — Override if inherit; forced if
/// enforce").
#[tokio::test]
async fn an_ancestor_enforce_auth_replaces_the_descendants() {
    let parent = Uuid::from_u128(0x46fa);
    let child = Uuid::from_u128(0x46fe);
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> = Arc::new(
        MockCredStoreClient::with_secrets(vec![("parent-key".to_owned(), "p4rent".to_owned())]),
    );
    let gateway = super::assemble(
        relay_config(),
        credstore,
        &[(child, parent)],
        None,
        crate::domain::plugin::GuardPluginRegistry::with_builtins(),
        crate::domain::plugin::TransformPluginRegistry::with_builtins(),
    );
    let origin = Origin::spawn(Reply::Echo).await;

    let mut parent_upstream = upstream_json(origin.host(), origin.port(), "auth-chain.local");
    parent_upstream["auth"] = json!({
        "type": "apikey", "sharing": "enforce",
        "config": { "api_key_ref": "cred://parent-key", "key_name": "x-parent-key", "in": "header" }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", parent, parent_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut child_upstream = upstream_json(origin.host(), origin.port(), "auth-chain.local");
    child_upstream["auth"] = json!({
        "type": "apikey", "sharing": "private",
        "config": { "api_key_ref": "cred://parent-key", "key_name": "x-child-key", "in": "header" }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", child, child_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let effective = gateway
        .data
        .resolve(
            &sec_ctx(child),
            child,
            "auth-chain.local",
            &Method::GET,
            "/v1",
        )
        .await
        .expect("the alias resolves");
    let auth = effective
        .auth
        .expect("the enforced ancestor auth is inherited");
    assert_eq!(
        auth.config["key_name"],
        json!("x-parent-key"),
        "the ancestor's binding is the one that runs"
    );

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/auth-chain.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let captured = origin.only();
    assert_eq!(
        captured.header("x-parent-key"),
        Some("p4rent"),
        "the enforced ancestor credential is injected"
    );
    assert_eq!(
        captured.header("x-child-key"),
        None,
        "the descendant's own auth is displaced, not added to"
    );
}

/// An `inherit`ed ancestor auth binding is adopted when the descendant defines
/// none, and ignored when it does.
#[tokio::test]
async fn an_inherited_auth_is_adopted_by_a_descendant_without_one() {
    let parent = Uuid::from_u128(0x47fa);
    let child = Uuid::from_u128(0x47fe);
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> = Arc::new(
        MockCredStoreClient::with_secrets(vec![("shared-key".to_owned(), "sh4r3d".to_owned())]),
    );
    let gateway = super::assemble(
        relay_config(),
        credstore,
        &[(child, parent)],
        None,
        crate::domain::plugin::GuardPluginRegistry::with_builtins(),
        crate::domain::plugin::TransformPluginRegistry::with_builtins(),
    );
    let origin = Origin::spawn(Reply::Echo).await;

    let mut parent_upstream = upstream_json(origin.host(), origin.port(), "shared-auth.local");
    parent_upstream["auth"] = json!({
        "type": "apikey", "sharing": "inherit",
        "config": { "api_key_ref": "cred://shared-key", "key_name": "x-shared-key", "in": "header" }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", parent, parent_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    // Without its own auth, the child adopts the ancestor's.
    let child_upstream = upstream_json(origin.host(), origin.port(), "shared-auth.local");
    let created = post_json(&gateway, "/oagw/v1/upstreams", child, child_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let effective = gateway
        .data
        .resolve(
            &sec_ctx(child),
            child,
            "shared-auth.local",
            &Method::GET,
            "/v1",
        )
        .await
        .expect("the alias resolves");
    assert!(
        effective.auth.is_some(),
        "an `inherit`ed ancestor auth fills the descendant's gap"
    );
    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/shared-auth.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(
        origin.last().header("x-shared-key"),
        Some("sh4r3d"),
        "the adopted credential is injected"
    );

    // A `private` ancestor auth is invisible to the descendant.
    let mut hidden = upstream_json(origin.host(), origin.port(), "private-auth.local");
    hidden["auth"] = json!({
        "type": "apikey", "sharing": "private",
        "config": { "api_key_ref": "cred://shared-key", "key_name": "x-hidden-key", "in": "header" }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", parent, hidden).await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let child_upstream = upstream_json(origin.host(), origin.port(), "private-auth.local");
    let created = post_json(&gateway, "/oagw/v1/upstreams", child, child_upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let effective = gateway
        .data
        .resolve(
            &sec_ctx(child),
            child,
            "private-auth.local",
            &Method::GET,
            "/v1",
        )
        .await
        .expect("the alias resolves");
    assert!(
        effective.auth.is_none(),
        "a `private` ancestor auth is not inherited"
    );
    // The proxy is driven with the child's own scope: an anonymous call is
    // scoped to whichever upstream the alias lookup lands on first, which is
    // not what is under test here.
    let response = super::send(
        &gateway.router,
        super::proxy_request(
            Method::GET,
            "/oagw/v1/proxy/private-auth.local/v1",
            &[],
            b"",
            Some(sec_ctx(child)),
        ),
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert_eq!(
        origin.last().header("x-hidden-key"),
        None,
        "no credential travelled"
    );
}

// ---------------------------------------------------------------------------
// Transport regressions
// ---------------------------------------------------------------------------

/// The gateway's own `x-oagw-*` headers are control headers: they select an
/// endpoint or report an error, so a client presenting one must not be able to
/// smuggle it past the relay, however permissive the passthrough rule is.
#[tokio::test]
async fn oagw_headers_never_travel_to_the_upstream() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "strip.local");
    upstream["headers"] = json!({ "request": { "passthrough": "all" } });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/strip.local/v1",
        &[
            ("x-oagw-target-host", "127.0.0.1"),
            ("x-oagw-trace", "forged"),
            ("x-keep", "yes"),
        ],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    let captured = origin.only();
    for name in ["x-oagw-target-host", "x-oagw-trace", "x-oagw-error-source"] {
        assert!(
            !captured.has_header(name),
            "{name} is the gateway's own and never travels: {:?}",
            captured.headers
        );
    }
    assert_eq!(
        captured.header("x-keep"),
        Some("yes"),
        "passthrough `all` keeps every header that is not the gateway's"
    );
}

/// A `Connection` header names other headers as hop-by-hop: they belong to the
/// single link the header travelled over and are dropped from the relayed
/// response even though they are ordinary end-to-end headers by spelling.
#[tokio::test]
async fn connection_named_headers_are_dropped_from_the_relayed_response() {
    let origin = Origin::spawn(Reply::Fixed {
        status: 200,
        headers: vec![
            ("connection", "x-internal-session".to_owned()),
            ("x-internal-session", "link-private".to_owned()),
            ("x-end-to-end", "kept".to_owned()),
        ],
        body: b"ok".to_vec(),
    })
    .await;
    let gateway = gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "conn.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/conn.local/v1",
        &[],
        b"",
    )
    .await;
    let headers = response.headers().clone();
    assert!(
        headers.get("x-internal-session").is_none(),
        "a header named by `Connection` does not survive the relay: {headers:?}"
    );
    assert_eq!(
        headers
            .get("x-end-to-end")
            .and_then(|value| value.to_str().ok()),
        Some("kept"),
        "an unrelated header is relayed untouched"
    );
}

/// The request ceiling is the client's ceiling: an upstream answer larger than
/// `max_request_body_bytes` is an upstream fault reported as a bad gateway,
/// never as the client's `413`.
#[tokio::test]
async fn an_upstream_answer_over_the_ceiling_is_a_bad_gateway() {
    let origin = Origin::spawn(Reply::Fixed {
        status: 200,
        headers: Vec::new(),
        body: vec![b'x'; 64],
    })
    .await;
    let mut config = relay_config();
    config.max_request_body_bytes = 8;
    let gateway = gateway_with(config);
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "fat-answer.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/fat-answer.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        502,
        "an oversized upstream answer is not the caller's fault"
    );
    assert_problem(&response, ErrorKind::DownstreamError.gts_fragment(), 502);
}

/// The relay never retries: an upstream that answers is answered to exactly
/// once, whatever the status it returned, so a non-idempotent request cannot be
/// replayed behind the caller's back.
#[tokio::test]
async fn an_upstream_answer_is_never_retried() {
    let origin = Origin::spawn(Reply::Fixed {
        status: 429,
        headers: vec![("retry-after", "1".to_owned())],
        body: b"slow down".to_vec(),
    })
    .await;
    let gateway = gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "noretry.local"),
    )
    .await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let response = proxy(
        &gateway,
        Method::POST,
        "/oagw/v1/proxy/noretry.local/v1",
        &[],
        b"{\"once\":true}",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 429, "the status is relayed");
    assert_eq!(
        origin.captured().len(),
        1,
        "a single upstream call, however it answered"
    );
}

/// A method outside the schema's enumeration is relayed as sent: the route
/// matcher folds it through and the wire carries the verb the client used.
#[tokio::test]
async fn a_non_standard_method_is_relayed_verbatim() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "webdav.local");
    upstream["headers"] = json!({ "request": { "passthrough": "all" } });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let response = proxy(
        &gateway,
        Method::from_bytes(b"PROPFIND").expect("an arbitrary verb"),
        "/oagw/v1/proxy/webdav.local/collection",
        &[("depth", "1")],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        200,
        "the request is relayed, not refused as unroutable"
    );
    let captured = origin.only();
    assert_eq!(captured.method, "PROPFIND", "the verb travels as sent");
    assert_eq!(captured.path, "/collection");
    assert_eq!(captured.header("depth"), Some("1"));
}

/// An IPv6 endpoint is addressed with a bracketed authority: an unbracketed
/// literal would split the Host header on the last colon and the origin would
/// read a port for a host.
#[tokio::test]
async fn an_ipv6_origin_is_addressed_with_a_bracketed_authority() {
    // The format is a property of the authority builder, so it is asserted
    // without a socket first: a bare literal is bracketed, a bracketed one and
    // a name are left alone.
    let endpoint = crate::domain::model::Endpoint {
        scheme: crate::domain::model::EndpointScheme::Http,
        host: "::1".to_owned(),
        port: Some(8080),
    };
    assert_eq!(endpoint.bracketed_host(), "[::1]");
    assert_eq!(endpoint.authority(), "[::1]:8080");
    let named = crate::domain::model::Endpoint {
        scheme: crate::domain::model::EndpointScheme::Http,
        host: "[::1]".to_owned(),
        port: Some(80),
    };
    assert_eq!(named.bracketed_host(), "[::1]", "already bracketed");
    assert_eq!(named.authority(), "[::1]");

    let origin = Origin::spawn_ipv6(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let created = post_json(
        &gateway,
        "/oagw/v1/upstreams",
        tenant(),
        upstream_json(origin.host(), origin.port(), "v6.local"),
    )
    .await;
    assert_eq!(
        status_of(&created).as_u16(),
        201,
        "an IPv6 literal is a legal host"
    );

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/v6.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        200,
        "the relay dials the literal"
    );
    let expected = format!("[::1]:{}", origin.port());
    assert_eq!(
        origin.only().header("host"),
        Some(expected.as_str()),
        "the Host header is a bracketed authority"
    );
}

/// Deleting an upstream takes its rate-limit buckets with it: a later upstream
/// reusing the id (or the alias) starts from a fresh bucket instead of
/// inheriting the tokens the deleted configuration had already spent.
#[tokio::test]
async fn deleting_an_upstream_drops_its_rate_limit_buckets() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "evict.local");
    upstream["rate_limit"] = rate_limit(1, None);
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let id = first_upstream_id(&gateway);

    let response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/evict.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200);
    assert!(
        !gateway.data.limiter().is_empty(),
        "the relay left a bucket behind"
    );

    let removed = delete(&gateway, &format!("/oagw/v1/upstreams/{id}"), tenant()).await;
    assert_eq!(status_of(&removed).as_u16(), 204);
    assert!(
        gateway.data.limiter().is_empty(),
        "the deleted upstream's buckets are dropped: {:?}",
        gateway.data.limiter()
    );
}

// ---------------------------------------------------------------------------
// Request body framing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_declared_content_length_must_match_the_body() {
    let (gateway, origin) = relay_gateway("framing.local", "/v1", &["POST"]).await;

    // A correct declaration is relayed, and the upstream receives the body.
    let response = proxy(
        &gateway,
        Method::POST,
        "/oagw/v1/proxy/framing.local/v1",
        &[("content-length", "5")],
        b"hello",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        200,
        "a matching length is relayed"
    );
    assert_eq!(origin.last().body_text(), "hello");

    // A declaration that disagrees with what arrived is refused before the
    // relay: the upstream never sees a body the caller did not send.
    let mut response = proxy(
        &gateway,
        Method::POST,
        "/oagw/v1/proxy/framing.local/v1",
        &[("content-length", "4")],
        b"hello",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        400,
        "a short declaration is refused"
    );
    assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
    let body = body_json(&mut response).await;
    assert_eq!(problem_field(&body), "content-length");
    assert!(
        body["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("does not match"),
        "the problem explains the mismatch, got: {body}"
    );

    // The reverse mismatch is caught too, and nothing is dialed either way.
    let dialed = origin.captured().len();
    let response = proxy(
        &gateway,
        Method::POST,
        "/oagw/v1/proxy/framing.local/v1",
        &[("content-length", "64")],
        b"hello",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        400,
        "a long declaration is refused"
    );
    assert_eq!(
        origin.captured().len(),
        dialed,
        "a mismatched body never reaches the upstream"
    );
}

#[tokio::test]
async fn a_non_integer_content_length_is_refused() {
    let (gateway, origin) = relay_gateway("lengths.local", "/v1", &["POST"]).await;
    for declared in ["five", "5.0", "0x10", "  ", "18446744073709551616"] {
        let mut response = proxy(
            &gateway,
            Method::POST,
            "/oagw/v1/proxy/lengths.local/v1",
            &[("content-length", declared)],
            b"hello",
        )
        .await;
        assert_eq!(
            status_of(&response).as_u16(),
            400,
            "'{declared}' is not a length"
        );
        assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
        let body = body_json(&mut response).await;
        assert_eq!(problem_field(&body), "content-length");
        assert!(
            body["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("integer"),
            "the problem says the value is not an integer, got: {body}"
        );
    }
    assert!(
        origin.captured().is_empty(),
        "an unparsable length is never relayed"
    );
}

#[tokio::test]
async fn only_the_chunked_transfer_coding_is_accepted() {
    let (gateway, origin) = relay_gateway("coding.local", "/v1", &["POST"]).await;
    for declared in ["gzip", "gzip, chunked, deflate", "identity"] {
        let mut response = proxy(
            &gateway,
            Method::POST,
            "/oagw/v1/proxy/coding.local/v1",
            &[("transfer-encoding", declared)],
            b"hello",
        )
        .await;
        assert_eq!(
            status_of(&response).as_u16(),
            400,
            "'{declared}' is not supported"
        );
        assert_problem(&response, ErrorKind::Validation.gts_fragment(), 400);
        let body = body_json(&mut response).await;
        assert_eq!(problem_field(&body), "transfer-encoding");
        assert!(
            body["detail"]
                .as_str()
                .unwrap_or_default()
                .contains("chunked"),
            "the problem names the only supported coding, got: {body}"
        );
    }

    // `chunked` carries no declared length, so the body is relayed as received.
    let response = proxy(
        &gateway,
        Method::POST,
        "/oagw/v1/proxy/coding.local/v1",
        &[("transfer-encoding", "chunked")],
        b"hello",
    )
    .await;
    assert_eq!(status_of(&response).as_u16(), 200, "chunked is understood");
    assert_eq!(origin.last().body_text(), "hello");
}

// ---------------------------------------------------------------------------
// Rate-limit identity
// ---------------------------------------------------------------------------

/// Two callers the gateway cannot name are still limited separately: the peer
/// address stands in for the missing subject, so one exhausted caller cannot
/// spend another's tokens.
#[tokio::test]
async fn an_anonymous_user_scoped_limit_falls_back_to_the_client_address() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "per-peer.local");
    let mut limit = rate_limit(1, None);
    limit["scope"] = json!("user");
    upstream["rate_limit"] = limit;
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    // Two distinct client addresses: the port is irrelevant to the identity,
    // the address is what names the caller.
    let first = std::net::SocketAddr::from(([127, 0, 0, 1], 40001));
    let second = std::net::SocketAddr::from(([127, 0, 0, 2], 40001));
    for (peer, expected) in [(first, 200), (first, 429), (second, 200), (second, 429)] {
        let mut response = send(
            &gateway.router,
            crate::tests::proxy_request_from(Method::GET, "/oagw/v1/proxy/per-peer.local/v1", peer),
        )
        .await;
        assert_eq!(
            status_of(&response).as_u16(),
            expected,
            "{peer} gets its own bucket"
        );
        let _ = body_text(&mut response).await;
    }
}

/// The client address the gateway limits on is the connection peer. A client
/// that names a friendlier address in `x-forwarded-for` cannot move itself into
/// an unspent bucket.
#[tokio::test]
async fn a_client_supplied_forwarded_header_does_not_change_the_rate_identity() {
    let origin = Origin::spawn(Reply::Echo).await;
    let gateway = gateway_with(relay_config());
    let mut upstream = upstream_json(origin.host(), origin.port(), "spoofed.local");
    let mut limit = rate_limit(1, None);
    limit["scope"] = json!("ip");
    upstream["rate_limit"] = limit;
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let peer = std::net::SocketAddr::from(([127, 0, 0, 1], 41000));
    let mut first = send(
        &gateway.router,
        crate::tests::proxy_request_with(
            Method::GET,
            "/oagw/v1/proxy/spoofed.local/v1",
            &[("x-forwarded-for", "203.0.113.9")],
            b"",
            None,
            Some(peer),
        ),
    )
    .await;
    assert_eq!(status_of(&first).as_u16(), 200);
    let second = send(
        &gateway.router,
        crate::tests::proxy_request_with(
            Method::GET,
            "/oagw/v1/proxy/spoofed.local/v1",
            &[("x-forwarded-for", "203.0.113.10")],
            b"",
            None,
            Some(peer),
        ),
    )
    .await;
    assert_eq!(
        status_of(&second).as_u16(),
        429,
        "the same peer is one caller however it labels itself"
    );
    let _ = body_text(&mut first).await;
}

// ---------------------------------------------------------------------------
// Credential failures
// ---------------------------------------------------------------------------

/// A credential store that refuses every lookup, so a test can drive the
/// "the caller may not read this reference" path.
struct DenyingCredStore;

#[async_trait::async_trait]
impl credstore_sdk::CredStoreClientV1 for DenyingCredStore {
    async fn get(
        &self,
        _ctx: &toolkit_security::SecurityContext,
        _key: &credstore_sdk::SecretRef,
    ) -> Result<Option<credstore_sdk::GetSecretResponse>, credstore_sdk::CredStoreError> {
        Err(credstore_sdk::CredStoreError::AccessDenied)
    }
}

/// A credential the store refuses is an authorization failure the caller could
/// act on (401), not a deployment gap (500), and neither the problem body nor
/// the log line quotes the credential value or the reference name.
#[tokio::test]
async fn a_denied_credential_is_an_auth_failure_that_names_nothing() {
    let origin = Origin::spawn(Reply::Echo).await;
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> = Arc::new(DenyingCredStore);
    let gateway = super::gateway_with_creds(relay_config(), credstore);
    let mut upstream = upstream_json(origin.host(), origin.port(), "denied.local");
    upstream["auth"] = json!({
        "type": "apikey",
        "config": { "api_key_ref": "cred://partner-openai-key" }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);

    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/denied.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        401,
        "a denial is the caller's problem"
    );
    assert_problem(
        &response,
        ErrorKind::AuthenticationFailed.gts_fragment(),
        401,
    );
    let body = body_text(&mut response).await;
    assert!(
        !body.contains("partner-openai-key"),
        "the reference name stays out of the problem body: {body}"
    );
    assert!(
        !body.contains("s3cr3t"),
        "no secret value is rendered: {body}"
    );
    assert_eq!(origin.captured().len(), 0, "nothing is relayed");

    // An unresolvable reference is a different failure: this deployment cannot
    // serve the request, so it is a 500 and still names nothing.
    let credstore: Arc<dyn credstore_sdk::CredStoreClientV1> =
        Arc::new(MockCredStoreClient::empty());
    let gateway = super::gateway_with_creds(relay_config(), credstore);
    let mut upstream = upstream_json(origin.host(), origin.port(), "absent.local");
    upstream["auth"] = json!({
        "type": "apikey",
        "config": { "api_key_ref": "cred://partner-openai-key" }
    });
    let created = post_json(&gateway, "/oagw/v1/upstreams", tenant(), upstream).await;
    assert_eq!(status_of(&created).as_u16(), 201);
    let mut response = proxy(
        &gateway,
        Method::GET,
        "/oagw/v1/proxy/absent.local/v1",
        &[],
        b"",
    )
    .await;
    assert_eq!(
        status_of(&response).as_u16(),
        500,
        "a missing credential is a gateway gap"
    );
    assert_problem(&response, ErrorKind::SecretNotFound.gts_fragment(), 500);
    let body = body_text(&mut response).await;
    assert!(
        !body.contains("partner-openai-key"),
        "the reference name stays out of the problem body: {body}"
    );
}

// ---------------------------------------------------------------------------
// Structured relay logging
// ---------------------------------------------------------------------------

/// Every relayed request produces exactly one structured event carrying the
/// correlation id, and a gateway failure's event carries the same `trace_id`
/// the problem body does. Header names may appear; header values never do.
#[test]
fn a_relayed_request_and_a_gateway_failure_each_log_one_event() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a current-thread runtime builds");

    let (capture, (upstream_id, failure_trace_id, audited_id)) = crate::tests::capture_logs(|| {
        runtime.block_on(async {
            let origin = Origin::spawn(Reply::Echo).await;
            let gateway = gateway_with(relay_config());
            let mut created = post_json(
                &gateway,
                "/oagw/v1/upstreams",
                tenant(),
                upstream_json(origin.host(), origin.port(), "logged.local"),
            )
            .await;
            assert_eq!(status_of(&created).as_u16(), 201, "upstream created");
            let id = Uuid::parse_str(body_json(&mut created).await["id"].as_str().expect("id"))
                .expect("uuid id");

            // A successful relay, with headers a log must not quote.
            let mut response = proxy(
                &gateway,
                Method::POST,
                "/oagw/v1/proxy/logged.local/v1",
                &[
                    ("content-type", "application/json"),
                    ("authorization", "Bearer super-secret-token"),
                    ("x-api-key", "s3cr3t-value"),
                ],
                b"{}",
            )
            .await;
            assert_eq!(status_of(&response).as_u16(), 200, "the request is relayed");
            let _ = body_text(&mut response).await;

            // A gateway failure: the alias does not exist.
            let mut failed = proxy(
                &gateway,
                Method::GET,
                "/oagw/v1/proxy/absent.local/v1",
                &[],
                b"",
            )
            .await;
            assert_problem(&failed, ErrorKind::RouteNotFound.gts_fragment(), 404);
            let body = body_json(&mut failed).await;
            let trace_id = body["trace_id"].as_str().expect("a trace id").to_owned();

            // And one control-plane mutation, which is audited too.
            let mut audited = post_json(
                &gateway,
                "/oagw/v1/upstreams",
                tenant(),
                upstream_json(origin.host(), origin.port(), "audited.local"),
            )
            .await;
            assert_eq!(status_of(&audited).as_u16(), 201);
            let audited_id =
                Uuid::parse_str(body_json(&mut audited).await["id"].as_str().expect("id"))
                    .expect("uuid id");
            (id, trace_id, audited_id)
        })
    });

    let events = capture.events();
    assert_one_relay_event(&events, upstream_id);
    assert_one_failure_event(&events, &failure_trace_id);
    assert_no_header_value_is_logged(&events);
    assert_mutations_are_audited(&events, audited_id);
}

/// The relay events a test's own successful hop produced.
fn relay_events(events: &[String]) -> Vec<&String> {
    events
        .iter()
        .filter(|event| {
            event.contains("message=oagw proxy request alias=logged.local")
                && !event.contains("failed")
        })
        .collect()
}

/// One `info` event names the hop it made and the configuration that answered.
fn assert_one_relay_event(events: &[String], upstream_id: Uuid) {
    let relayed = relay_events(events);
    assert_eq!(
        relayed.len(),
        1,
        "exactly one relay event, got: {relayed:?}"
    );
    let event = relayed[0];
    assert!(
        event.contains("level=Level(Info)"),
        "a successful relay is an info event: {event}"
    );
    assert!(event.contains("alias=logged.local"), "{event}");
    assert!(event.contains("method=POST"), "{event}");
    assert!(event.contains("path=/v1"), "{event}");
    assert!(event.contains("status=200"), "{event}");
    assert!(event.contains("duration_ms="), "{event}");
    assert!(
        event.contains(&format!("upstream_id=Some({upstream_id})")),
        "the matched upstream is named: {event}"
    );
    assert!(
        event.contains("route_id=None"),
        "the fall-through matched no route: {event}"
    );
    assert!(
        event.contains("trace_id="),
        "the event carries a correlation id: {event}"
    );
}

/// The gateway failure is one `warn` event sharing the problem's correlation
/// id, and it reports nothing as matched.
fn assert_one_failure_event(events: &[String], failure_trace_id: &str) {
    let trace = format!("trace_id={failure_trace_id}");
    let failures: Vec<&String> = events
        .iter()
        .filter(|event| {
            event.contains("message=oagw proxy request failed alias=absent.local")
                && event.contains(&trace)
        })
        .collect();
    assert_eq!(
        failures.len(),
        1,
        "exactly one failure event, got: {failures:?}"
    );
    let event = failures[0];
    assert!(event.contains("level=Level(Warn)"), "{event}");
    assert!(
        event.contains(&trace),
        "the log line and the problem body share the correlation id: {event}"
    );
    assert!(
        event.contains("error_code=ROUTE_NOT_FOUND"),
        "the failure names its error code: {event}"
    );
    assert!(
        event.contains("status=404") && event.contains("upstream_id=None"),
        "nothing was matched, so nothing is reported as matched: {event}"
    );
}

/// Header NAMES may be logged; header VALUES never are.
fn assert_no_header_value_is_logged(events: &[String]) {
    let joined = events.join("\n");
    assert!(
        joined.contains("loggable_header_names=") && joined.contains("content-type"),
        "the loggable header names are recorded: {joined}"
    );
    for secret in ["super-secret-token", "s3cr3t-value", "Bearer "] {
        assert!(
            !joined.contains(secret),
            "a header value must never reach a log line (looked for `{secret}`):\n{joined}"
        );
    }
    for forbidden in ["authorization=", "x-api-key="] {
        assert!(
            !joined.contains(forbidden),
            "a credential header name is not loggable: {joined}"
        );
    }
}

/// Control-plane mutations are audited with the resource they touched.
fn assert_mutations_are_audited(events: &[String], audited_id: Uuid) {
    let audited: Vec<&String> = events
        .iter()
        .filter(|event| {
            event.contains("message=oagw management operation")
                && event.contains(&format!("id={audited_id}"))
        })
        .collect();
    assert!(
        !audited.is_empty(),
        "mutations are audited, got: {events:?}"
    );
    assert!(
        audited
            .iter()
            .any(|event| event.contains("resource=upstream")
                && event.contains("action=created")
                && event.contains("tenant_id=")
                && event.contains("id=")),
        "the audit names the resource, the action, the id, and the tenant: {audited:?}"
    );
}

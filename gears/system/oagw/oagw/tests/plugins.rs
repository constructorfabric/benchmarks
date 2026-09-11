//! AT-2: the custom-plugin collection — immutability, the Starlark source
//! endpoint, the in-use guard and the built-in catalog's invisibility
//! (`contracts/management-api.md` § Plugins).

mod common;

use axum::http::StatusCode;
use common::net::{read_request, write_response};
use common::{Caller, app, create_route, create_upstream, route_body, send, upstream_body};

const SOURCE: &str = "def apply(ctx):\n    ctx.request.headers['x-signed'] = '1'\n";

fn plugin_body(kind: &str, name: &str) -> String {
    serde_json::json!({
        "type": kind,
        "name": name,
        "source": SOURCE,
        "config_schema": { "type": "object" },
    })
    .to_string()
}

#[tokio::test]
async fn plugins_are_created_read_and_never_replaced() {
    let app = app();
    let caller = Caller::default();

    let created = send(
        &app,
        &caller,
        "POST",
        "/oagw/v1/plugins",
        Some(&plugin_body("transform", "signer")),
    )
    .await;
    assert_eq!(created.0, StatusCode::CREATED, "{}", created.2);
    let id = created.2["id"].as_str().expect("id").to_owned();
    assert_eq!(created.2["name"], "signer");
    assert_eq!(created.2["type"], "transform");
    assert_eq!(created.2["source"], SOURCE);

    let listed = send(&app, &caller, "GET", "/oagw/v1/plugins", None).await;
    assert_eq!(listed.0, StatusCode::OK);
    assert_eq!(listed.2.as_array().map(Vec::len), Some(1));

    let read = send(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/plugins/{id}"),
        None,
    )
    .await;
    assert_eq!(read.0, StatusCode::OK);
    assert_eq!(read.2["name"], "signer");

    // The stored Starlark source, as text.
    let source = common::request_with(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/plugins/{id}/source"),
        None,
    )
    .await;
    assert_eq!(source.0.status, StatusCode::OK);
    assert_eq!(
        source
            .0
            .headers
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(String::from_utf8_lossy(&source.1), SOURCE);

    // There is no PUT: plugins are immutable.
    let (status, _, _) = send(
        &app,
        &caller,
        "PUT",
        &format!("/oagw/v1/plugins/{id}"),
        Some(&plugin_body("transform", "signer")),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);

    // Deleting an unreferenced plugin succeeds.
    let (status, _, _) = send(
        &app,
        &caller,
        "DELETE",
        &format!("/oagw/v1/plugins/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let gone = send(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/plugins/{id}"),
        None,
    )
    .await;
    assert_eq!(gone.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn duplicate_names_and_in_use_deletes_are_rejected() {
    let app = app();
    let caller = Caller::default();

    let first = send(
        &app,
        &caller,
        "POST",
        "/oagw/v1/plugins",
        Some(&plugin_body("guard", "policy")),
    )
    .await;
    assert_eq!(first.0, StatusCode::CREATED, "{}", first.2);
    let id = first.2["id"].as_str().expect("id").to_owned();

    // Same (kind, name): 409.
    let (status, _, body) = send(
        &app,
        &caller,
        "POST",
        "/oagw/v1/plugins",
        Some(&plugin_body("guard", "policy")),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // The same name under a different kind is a distinct row.
    let (status, _, body) = send(
        &app,
        &caller,
        "POST",
        "/oagw/v1/plugins",
        Some(&plugin_body("transform", "policy")),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Binding the plugin makes it undeletable.
    let upstream = create_upstream(&app, &caller, &upstream_body("local-8099", 8099)).await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();
    let bound = serde_json::json!({
        "enabled": true,
        "upstream_id": upstream_id,
        "match": { "http": {
            "methods": ["GET"], "path": "/v1",
            "query_allowlist": [], "path_suffix_mode": "append"
        }},
        "plugins": { "items": [ id ] },
    })
    .to_string();
    let route = create_route(&app, &caller, &bound).await;
    assert_eq!(route["plugins"]["items"][0], id.as_str());

    let (status, _, body) = send(
        &app,
        &caller,
        "DELETE",
        &format!("/oagw/v1/plugins/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.plugin.in_use.v1"
    );

    // Unbind it, and the delete goes through.
    let route_id = route["id"].as_str().expect("route id").to_owned();
    let (status, _, _) = send(
        &app,
        &caller,
        "DELETE",
        &format!("/oagw/v1/routes/{route_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = send(
        &app,
        &caller,
        "DELETE",
        &format!("/oagw/v1/plugins/{id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn builtins_are_bindable_but_never_listed() {
    let app = app();
    let caller = Caller::default();

    // A built-in id is accepted in a plugin set...
    let upstream = create_upstream(&app, &caller, &upstream_body("local-8098", 8098)).await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();
    let body = serde_json::json!({
        "enabled": true,
        "upstream_id": upstream_id,
        "match": { "http": {
            "methods": ["GET"], "path": "/v1",
            "query_allowlist": [], "path_suffix_mode": "append"
        }},
        "plugins": { "items": [
            common::noop_auth(),
            { "plugin_ref": common::required_headers_guard(), "config": { "required_request_headers": "x-tenant" } }
        ] },
    })
    .to_string();
    create_route(&app, &caller, &body).await;

    // ...but never appears in the collection, and is not readable by id.
    let listed = send(&app, &caller, "GET", "/oagw/v1/plugins", None).await;
    assert_eq!(listed.2.as_array().map(Vec::len), Some(0));

    let gts_id = common::noop_auth();
    let (status, _, _) = send(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/plugins/{gts_id}"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn catalog_only_and_unknown_identifiers_are_rejected() {
    let app = app();
    let caller = Caller::default();
    let upstream = create_upstream(&app, &caller, &upstream_body("local-8097", 8097)).await;
    let upstream_id = upstream["id"].as_str().expect("id").to_owned();

    let with_plugin = |id: &str| {
        serde_json::json!({
            "enabled": true,
            "upstream_id": upstream_id,
            "match": { "http": {
                "methods": ["GET"], "path": "/v1",
                "query_allowlist": [], "path_suffix_mode": "append"
            }},
            "plugins": { "items": [ id ] },
        })
        .to_string()
    };

    // Catalog-only: known, but with no backing implementation.
    let bearer = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1";
    let (status, _, body) = send(
        &app,
        &caller,
        "POST",
        "/oagw/v1/routes",
        Some(&with_plugin(bearer)),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["type"]
            .as_str()
            .unwrap_or_default()
            .ends_with("validation.error.v1"),
        "{body}"
    );

    // Outside the catalog entirely.
    let (status, _, body) = send(
        &app,
        &caller,
        "POST",
        "/oagw/v1/routes",
        Some(&with_plugin(
            "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.nope.v1",
        )),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Neither a GTS id nor a uuid.
    let (status, _, _) = send(
        &app,
        &caller,
        "POST",
        "/oagw/v1/routes",
        Some(&with_plugin("not-a-plugin")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A well-formed uuid that is not stored either.
    let (status, _, _) = send(
        &app,
        &caller,
        "POST",
        "/oagw/v1/routes",
        Some(&with_plugin("00000000-0000-0000-0000-000000000001")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Sanity: the same upstream still accepts a bindable built-in.
    let (status, _, body) = send(
        &app,
        &caller,
        "POST",
        "/oagw/v1/routes",
        Some(
            &serde_json::json!({
                "enabled": true, "upstream_id": upstream_id,
                "match": { "http": { "methods": ["GET"], "path": "/other",
                    "query_allowlist": [], "path_suffix_mode": "append" }}
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

/// A route binding the built-ins whose phases only matter on the live path:
/// the no-op auth, the required-headers guard and the request-id transform.
fn chained_route(upstream_id: &str, path: &str) -> String {
    serde_json::json!({
        "enabled": true,
        "upstream_id": upstream_id,
        "match": { "http": {
            "methods": ["GET", "POST"], "path": path,
            "query_allowlist": [], "path_suffix_mode": "append"
        }},
        "plugins": { "items": [
            common::noop_auth(),
            { "plugin_ref": common::required_headers_guard(),
              "config": { "required_request_headers": "x-tenant" } },
            common::request_id_transform(),
        ] },
    })
    .to_string()
}

/// An upstream that forwards the caller's `x-tenant` header and nothing else,
/// so the guard sees exactly what a caller supplied.
fn forwarding_upstream(alias: &str, port: u16) -> String {
    serde_json::json!({
        "enabled": true,
        "alias": alias,
        "server": { "endpoints": [
            { "scheme": "http", "host": "127.0.0.1", "port": port }
        ]},
        "protocol": common::PROTOCOL_HTTP,
        "headers": {
            "request": {
                "set": {},
                "add": {},
                "remove": [],
                "passthrough": "allowlist",
                "passthrough_allowlist": ["x-tenant"]
            },
            "response": {
                "set": { "x-gateway-stage": "oagw" },
                "add": { "x-added-by-gateway": "yes" },
                "remove": ["x-upstream-internal"]
            }
        }
    })
    .to_string()
}

#[tokio::test]
async fn a_proxied_request_runs_the_bound_plugin_chain() {
    let (addr, listener) = common::net::bind_upstream().await;
    let port = addr.port();
    let alias = format!("chain-{port}");
    let app = app();
    let caller = Caller::default();

    let upstream = create_upstream(&app, &caller, &forwarding_upstream(&alias, port)).await;
    let upstream_id = upstream["id"].as_str().expect("upstream id").to_owned();
    let _ = create_route(&app, &caller, &chained_route(&upstream_id, "/v1")).await;

    // The upstream answers the one request it receives and reports the head.
    let captured = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
    let writer = std::sync::Arc::clone(&captured);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let (head, _body) = read_request(&mut stream).await;
        *writer.lock().await = head;
        write_response(
            &mut stream,
            "HTTP/1.1 200 OK",
            &[
                ("content-type", "application/json"),
                ("x-upstream-internal", "secret-1"),
            ],
            b"{\"ok\":true}",
        )
        .await;
    });

    // The guard rejects a request that arrives without the header it demands.
    let (status, _, problem) = send(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/chat"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("REQUIRED_HEADER_MISSING"),
        "{problem}"
    );

    // With the header present the request is forwarded: the guard is satisfied
    // and the request-id transform has stamped the outbound request.
    let (status, response_headers, body) = common::send_with_headers(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/chat"),
        &[("x-tenant", "acme")],
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], true);

    let head = captured.lock().await.clone();
    assert!(
        head.to_ascii_lowercase().contains("x-tenant: acme"),
        "{head}"
    );
    let sent_id = head
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("x-request-id:"))
        .and_then(|line| line.split_once(':'))
        .map(|(_, value)| value.trim().to_owned());
    let sent_id = sent_id.expect("the transform stamps the outbound request");
    assert!(
        uuid::Uuid::parse_str(&sent_id).is_ok(),
        "minted correlation id: {sent_id}"
    );

    // The response carries the same correlation identifier back to the caller.
    assert_eq!(
        response_headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok()),
        Some(sent_id.as_str())
    );

    // The upstream's response header rules ran after the hop-by-hop stripping:
    // `set` replaces, `add` appends, `remove` drops.
    assert_eq!(
        response_headers
            .get("x-gateway-stage")
            .and_then(|value| value.to_str().ok()),
        Some("oagw")
    );
    assert_eq!(
        response_headers
            .get("x-added-by-gateway")
            .and_then(|value| value.to_str().ok()),
        Some("yes")
    );
    assert!(response_headers.get("x-upstream-internal").is_none());

    server.await.expect("upstream task");
}

/// AT-4 / FR-020: a failed token exchange is the caller's problem, not a hang:
/// the only documented status this suite had not yet produced.
#[tokio::test]
async fn a_failed_token_exchange_is_an_authentication_failure() {
    let alias = "authfail.local";
    let harness = common::harness_with_credstore(oagw::config::OagwConfig::default(), {
        let store = common::FakeCredStore::default();
        store.put("client-id", "service:gateway");
        store.put("client-secret", "shh");
        store
    });
    let app = harness.app.clone();
    let caller = common::Caller::default();

    let (token_addr, token_listener) = common::net::bind_upstream().await;
    // The authorization server refuses the exchange outright.
    let token_server = tokio::spawn(async move {
        let (mut stream, _) = token_listener.accept().await.expect("token endpoint");
        let (_head, _body) = read_request(&mut stream).await;
        write_response(
            &mut stream,
            "HTTP/1.1 500 Internal Server Error",
            &[("content-type", "application/json")],
            b"{\"error\":\"server_error\"}",
        )
        .await;
    });

    let upstream = create_upstream(
        &app,
        &caller,
        &serde_json::json!({
            "enabled": true, "alias": alias,
            "server": { "endpoints": [
                { "scheme": "http", "host": "127.0.0.1", "port": 9 }
            ]},
            "protocol": common::PROTOCOL_HTTP,
            "auth": {
                "type": common::oauth2_auth(),
                "config": {
                    "token_endpoint": format!("http://127.0.0.1:{}", token_addr.port()),
                    "client_id_ref": "cred://client-id",
                    "client_secret_ref": "cred://client-secret"
                }
            }
        })
        .to_string(),
    )
    .await;
    let _ = create_route(
        &app,
        &caller,
        &route_body(upstream["id"].as_str().expect("id"), "/v1"),
    )
    .await;

    let (status, headers, problem) = send(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{problem}");
    assert_eq!(
        problem["type"], "gts.cf.core.errors.err.v1~cf.oagw.auth.failed.v1",
        "{problem}"
    );
    assert_eq!(problem["title"], "Authentication Failed");
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("gateway")
    );
    assert_eq!(
        headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/problem+json")
    );

    token_server.await.expect("token endpoint task");
}

// ─── Authentication on the live path (AT-5, FR-012) ─────────────────

#[tokio::test]
async fn an_api_key_plugin_injects_the_credential_into_the_forwarded_request() {
    let (addr, listener) = common::net::bind_upstream().await;
    let port = addr.port();
    let alias = format!("keyed-{port}");
    let app = app();
    let caller = Caller::default();
    let bound = create_upstream(
        &app,
        &caller,
        &serde_json::json!({
            "enabled": true, "alias": alias,
            "server": { "endpoints": [
                { "scheme": "http", "host": "127.0.0.1", "port": port }
            ]},
            "protocol": common::PROTOCOL_HTTP,
            "auth": {
                "type": common::apikey_auth(),
                "config": { "api_key": "sk-inline-123", "in": "header", "header": "x-api-key" }
            }
        })
        .to_string(),
    )
    .await;

    let id = bound["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route_body(&id, "/v1")).await;

    let captured = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
    let writer = std::sync::Arc::clone(&captured);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let (head, _body) = read_request(&mut stream).await;
        *writer.lock().await = head;
        write_response(
            &mut stream,
            "HTTP/1.1 200 OK",
            &[("content-type", "application/json")],
            b"{\"ok\":true}",
        )
        .await;
    });

    let (status, _, body) = send(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1/chat"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let head = captured.lock().await.clone();
    assert!(
        head.to_ascii_lowercase()
            .contains("x-api-key: sk-inline-123"),
        "{head}"
    );
    // The caller's own credentials are not forwarded alongside the injected one.
    assert!(
        !head.to_ascii_lowercase().contains("authorization:"),
        "{head}"
    );
    server.await.expect("upstream task");
}

#[tokio::test]
async fn an_unresolvable_credential_is_a_secret_not_found_problem() {
    let alias = "unresolved.local";
    let harness = common::harness_with_credstore(
        oagw::config::OagwConfig::default(),
        common::FakeCredStore::default(),
    );
    let app = harness.app.clone();
    let caller = common::Caller::default();
    let addr = common::net::free_port().await;

    let upstream = create_upstream(
        &app,
        &caller,
        &serde_json::json!({
            "enabled": true, "alias": alias,
            "server": { "endpoints": [
                { "scheme": "http", "host": "127.0.0.1", "port": addr }
            ]},
            "protocol": common::PROTOCOL_HTTP,
            "auth": {
                "type": common::apikey_auth(),
                "config": { "key_ref": "cred://absent-key" }
            }
        })
        .to_string(),
    )
    .await;
    let _ = create_route(
        &app,
        &caller,
        &route_body(upstream["id"].as_str().expect("id"), "/v1"),
    )
    .await;

    let (status, _, problem) = send(
        &app,
        &caller,
        "GET",
        &format!("/oagw/v1/proxy/{alias}/v1"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.secret.not_found.v1"
    );
    // No credential material in the error surface.
    assert!(
        !problem.to_string().contains("absent"),
        "reference echoed back: {problem}"
    );
}

#[tokio::test]
async fn an_oauth2_plugin_fetches_a_token_once_and_reuses_it() {
    let alias = "oauthed.local";
    let harness = common::harness_with_credstore(oagw::config::OagwConfig::default(), {
        let store = common::FakeCredStore::default();
        store.put("client-id", "service:gateway");
        store.put("client-secret", "shh");
        store
    });
    let app = harness.app.clone();
    let caller = common::Caller::default();

    let (upstream_addr, upstream_listener) = common::net::bind_upstream().await;
    let (token_addr, token_listener) = common::net::bind_upstream().await;

    // The token endpoint: answers every exchange with a fresh bearer token
    // and counts how many times it was asked.
    let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = std::sync::Arc::clone(&requests);
    let token_server = tokio::spawn(async move {
        let (mut stream, _) = token_listener.accept().await.expect("token endpoint");
        let (_head, _body) = read_request(&mut stream).await;
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        write_response(
            &mut stream,
            "HTTP/1.1 200 OK",
            &[("content-type", "application/json")],
            b"{\"access_token\":\"tok-1\",\"token_type\":\"bearer\",\"expires_in\":3600}",
        )
        .await;
    });

    let heads = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let heads_writer = std::sync::Arc::clone(&heads);
    let upstream_server = tokio::spawn(async move {
        for _ in 0..2 {
            let (mut stream, _) = upstream_listener.accept().await.expect("accept");
            let (head, _body) = read_request(&mut stream).await;
            heads_writer.lock().await.push(head);
            write_response(
                &mut stream,
                "HTTP/1.1 200 OK",
                &[("content-type", "application/json")],
                b"{\"ok\":true}",
            )
            .await;
        }
    });

    let bound = create_upstream(
        &app,
        &caller,
        &serde_json::json!({
            "enabled": true, "alias": alias,
            "server": { "endpoints": [
                { "scheme": "http", "host": "127.0.0.1", "port": upstream_addr.port() }
            ]},
            "protocol": common::PROTOCOL_HTTP,
            "auth": {
                "type": common::oauth2_auth(),
                "config": {
                    "token_endpoint": format!("http://127.0.0.1:{}", token_addr.port()),
                    "client_id_ref": "cred://client-id",
                    "client_secret_ref": "cred://client-secret"
                }
            }
        })
        .to_string(),
    )
    .await;
    let id = bound["id"].as_str().expect("id").to_owned();
    let _ = create_route(&app, &caller, &route_body(&id, "/v1")).await;

    for _ in 0..2 {
        let (status, _, json) = send(
            &app,
            &caller,
            "GET",
            &format!("/oagw/v1/proxy/{alias}/v1"),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{json}");
    }

    token_server.await.expect("token endpoint task");
    upstream_server.await.expect("upstream task");

    // One exchange, two requests carrying the same token.
    assert_eq!(
        requests.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the token endpoint was asked more than once"
    );
    let seen = heads.lock().await;
    assert_eq!(seen.len(), 2);
    for head in seen.iter() {
        assert!(
            head.to_ascii_lowercase()
                .contains("authorization: bearer tok-1"),
            "{head}"
        );
    }
}

#[tokio::test]
async fn an_authentication_type_without_an_implementation_is_refused() {
    let app = app();
    let caller = Caller::default();
    let addr = common::net::free_port().await;

    let (status, _, problem) = send(
        &app,
        &caller,
        "POST",
        "/oagw/v1/upstreams",
        Some(
            &serde_json::json!({
                "enabled": true, "alias": "bearer.local",
                "server": { "endpoints": [
                    { "scheme": "http", "host": "127.0.0.1", "port": addr }
                ]},
                "protocol": common::PROTOCOL_HTTP,
                "auth": {
                    "type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
                    "config": {}
                }
            })
            .to_string(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert!(
        problem["detail"]
            .as_str()
            .unwrap_or_default()
            .contains("no backing implementation"),
        "{problem}"
    );
}

//! AT-3: the data plane's passthrough and matching semantics
//! (`contracts/proxy-api.md` § 1, § 2).
//!
//! Most match-stage rejections are observed without a listening upstream: once
//! the pipeline gets past matching it dials the loopback endpoint, and a closed
//! port answers `503 link.unavailable`. A `503` therefore proves the request
//! matched; a `400` or `404` proves it did not.

mod common;

use axum::body::Body;
use axum::http::StatusCode;
use common::net::{bind_upstream, header_of, read_request, serve, write_raw, write_response};
use common::{
    Caller, app, create_route, create_upstream, route_body, send, upstream_body, with_context,
};
use tower::ServiceExt;

/// Proxies `path` with the query attached, through the router.
async fn proxy(
    app: &axum::Router,
    caller: &Caller,
    alias: &str,
    path: &str,
    query: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let mut target = format!("/oagw/v1/proxy/{alias}{path}");
    if let Some(query) = query {
        target.push('?');
        target.push_str(query);
    }
    send(app, caller, "GET", &target, None).await
}

/// A GET route on `path` that forwards the `q` query parameter.
fn allowlisted_route(upstream_id: &str, path: &str) -> String {
    serde_json::json!({
        "enabled": true,
        "upstream_id": upstream_id,
        "match": { "http": {
            "methods": ["GET"], "path": path,
            "query_allowlist": ["q"], "path_suffix_mode": "append"
        }}
    })
    .to_string()
}

/// The bound upstream id for `alias`.
async fn bound_upstream(app: &axum::Router, caller: &Caller, alias: &str, port: u16) -> String {
    let upstream = create_upstream(app, caller, &upstream_body(alias, port)).await;
    upstream["id"].as_str().expect("upstream id").to_owned()
}

#[tokio::test]
async fn a_request_is_forwarded_and_the_response_passed_through() {
    let (addr, listener) = bind_upstream().await;
    let port = addr.port();
    let alias = format!("stub-{port}");
    let app = app();
    let caller = Caller::default();

    let upstream_id = bound_upstream(&app, &caller, &alias, addr.port()).await;
    let _ = create_route(&app, &caller, &allowlisted_route(&upstream_id, "/v1")).await;

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let (head, body) = read_request(&mut stream).await;
        // Framed by hand so the upstream genuinely speaks `chunked`: the data
        // plane decodes the chunks and strips the hop-by-hop headers, so what
        // the caller sees is the payload with an explicit length.
        write_raw(
            &mut stream,
            concat!(
                "HTTP/1.1 200 OK\r\n",
                "x-upstream: yes\r\n",
                "transfer-encoding: chunked\r\n",
                "connection: keep-alive\r\n",
                "\r\n",
                "b\r\n{\"ok\":true}\r\n",
                "0\r\n\r\n",
            )
            .as_bytes(),
        )
        .await;
        (head, body)
    });

    let (status, headers, body) = proxy(&app, &caller, &alias, "/v1/chat", Some("q=1")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, serde_json::json!({"ok": true}));
    // The upstream's own headers survive; the hop-by-hop ones do not.
    assert_eq!(
        headers.get("x-upstream").and_then(|v| v.to_str().ok()),
        Some("yes")
    );
    assert!(headers.get("transfer-encoding").is_none());
    assert!(headers.get("connection").is_none());
    // Every proxied response is tagged as coming from the upstream.
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|v| v.to_str().ok()),
        Some("upstream")
    );

    let (head, forwarded) = server.await.expect("upstream task");
    assert_eq!(forwarded, b"");
    assert!(head.starts_with("GET /v1/chat?q=1 HTTP/1.1"), "{head}");
    // Host is the target endpoint's authority, not the caller's.
    assert_eq!(
        header_of(&head, "host").as_deref(),
        Some(format!("127.0.0.1:{port}").as_str())
    );
    // Gateway-consumed headers never reach the upstream.
    assert!(header_of(&head, "x-oagw-target-host").is_none());
    assert!(header_of(&head, "x-oagw-error-source").is_none());
    // The correlation identifier the gateway generated.
    assert!(header_of(&head, "x-request-id").is_some());
}

#[tokio::test]
async fn the_forwarded_request_carries_the_body_and_the_target_host_choice() {
    let (addr, listener) = bind_upstream().await;
    let alias = format!("hdr-{}", addr.port());
    let app = app();
    let caller = Caller::default();

    let upstream_id = bound_upstream(&app, &caller, &alias, addr.port()).await;
    let _ = create_route(&app, &caller, &route_body(&upstream_id, "/v1")).await;

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let (head, body) = read_request(&mut stream).await;
        write_response(&mut stream, "HTTP/1.1 204 No Content", &[], b"").await;
        (head, body)
    });

    let target = format!("/oagw/v1/proxy/{alias}/v1/chat");
    let mut request = axum::http::Request::builder()
        .method("GET")
        .uri(target)
        .header("authorization", "Bearer caller-token")
        .header("x-custom", "keep")
        .header("x-oagw-target-host", "127.0.0.1")
        .body(Body::from("payload"))
        .expect("request");
    request.extensions_mut().insert(caller.context());
    let response: axum::response::Response = app.clone().oneshot(request).await.expect("response");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let (head, body) = server.await.expect("upstream task");
    assert_eq!(body, b"payload");
    // The default header policy forwards nothing: credentials are never copied
    // through, and the gateway-consumed header is dropped with the rest.
    assert!(header_of(&head, "authorization").is_none());
    assert!(header_of(&head, "x-custom").is_none());
    assert!(header_of(&head, "x-oagw-target-host").is_none());
    assert_eq!(header_of(&head, "content-length").as_deref(), Some("7"));
}

#[tokio::test]
async fn path_suffix_and_query_allowlist_rules_are_enforced() {
    let addr = common::free_port().await;
    let alias = format!("suffix-{}", addr);
    let app = app();
    let caller = Caller::default();

    let upstream_id = bound_upstream(&app, &caller, &alias, addr).await;

    // `append` forwards the suffix; the allowlist constrains the query.
    let append = serde_json::json!({
        "enabled": true,
        "upstream_id": upstream_id,
        "match": { "http": {
            "methods": ["GET"], "path": "/v1",
            "query_allowlist": ["q"], "path_suffix_mode": "append"
        }}
    })
    .to_string();
    let _ = create_route(&app, &caller, &append).await;

    // A suffix-less route rejects one.
    let disabled = serde_json::json!({
        "enabled": true,
        "upstream_id": upstream_id,
        "match": { "http": {
            "methods": ["GET"], "path": "/exact",
            "query_allowlist": [], "path_suffix_mode": "disabled"
        }}
    })
    .to_string();
    let _ = create_route(&app, &caller, &disabled).await;

    // An allowed query parameter passes the match stage and reaches the dial.
    let (status, _, body) = proxy(&app, &caller, &alias, "/v1/chat", Some("q=1")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1"
    );

    // A parameter outside the allowlist is rejected before dialling.
    let (status, _, body) = proxy(&app, &caller, &alias, "/v1/chat", Some("q=1&extra=2")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );

    // An empty allowlist with no query still matches.
    let (status, _, _) = proxy(&app, &caller, &alias, "/exact", None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    // `disabled` rejects a suffix.
    let (status, _, _) = proxy(&app, &caller, &alias, "/exact/extra", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_method_outside_the_allowlist_is_rejected() {
    let addr = common::free_port().await;
    let alias = format!("method-{}", addr);
    let app = app();
    let caller = Caller::default();

    let upstream_id = bound_upstream(&app, &caller, &alias, addr).await;
    let _ = create_route(&app, &caller, &route_body(&upstream_id, "/v1")).await;

    let target = format!("/oagw/v1/proxy/{alias}/v1/chat");
    let (status, _, body) = send(&app, &caller, "POST", &target, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.validation.error.v1"
    );
}

#[tokio::test]
async fn unknown_aliases_and_unmatched_paths_are_not_found() {
    let app = app();
    let caller = Caller::default();

    // No upstream with that alias anywhere in the chain.
    let (status, _, body) = proxy(&app, &caller, "no-such-alias", "/v1", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );

    // A known upstream whose routes do not cover the path.
    let upstream_id = bound_upstream(&app, &caller, "orphan", 9).await;
    let _ = create_route(&app, &caller, &route_body(&upstream_id, "/v1")).await;

    let (status, _, body) = proxy(&app, &caller, "orphan", "/other", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(
        body["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1"
    );
}

#[tokio::test]
async fn the_target_host_header_selects_between_pool_members() {
    let app = app();
    let caller = Caller::default();

    // Two endpoints, no header: 400 with the pool's hosts. The alias is the
    // one the derivation produces for a shared registrable suffix on a
    // non-standard port.
    let pool = serde_json::json!({
        "enabled": true,
        "server": { "endpoints": [
            { "scheme": "http", "host": "a.vendor.test", "port": 8443 },
            { "scheme": "http", "host": "b.vendor.test", "port": 8443 }
        ]},
        "protocol": common::PROTOCOL_HTTP,
    })
    .to_string();
    let upstream = create_upstream(&app, &caller, &pool).await;
    let upstream_id = upstream["id"].as_str().expect("upstream id").to_owned();
    assert_eq!(upstream["alias"], "vendor.test:8443");
    let _ = create_route(&app, &caller, &route_body(&upstream_id, "/v1")).await;

    let target = "/oagw/v1/proxy/vendor.test:8443/v1/chat";
    let (status, _, problem) = send(&app, &caller, "GET", target, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.missing_target_host.v1"
    );
    assert_eq!(problem["valid_hosts"].as_array().map(Vec::len), Some(2));

    // A value that is not a bare host or address.
    let (status, _, problem) =
        send_with_target(&app, &caller, target, "a.vendor.test:8443/path").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.invalid_target_host.v1"
    );
    assert_eq!(problem["invalid_value"], "a.vendor.test:8443/path");

    // A host outside the pool.
    let (status, _, problem) = send_with_target(&app, &caller, target, "10.0.0.1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{problem}");
    assert_eq!(
        problem["type"],
        "gts.cf.core.errors.err.v1~cf.oagw.routing.unknown_target_host.v1"
    );
    assert_eq!(problem["valid_hosts"].as_array().map(Vec::len), Some(2));
}

async fn send_with_target(
    app: &axum::Router,
    caller: &Caller,
    path: &str,
    target: &str,
) -> (StatusCode, axum::http::HeaderMap, serde_json::Value) {
    let mut request = axum::http::Request::builder()
        .method("GET")
        .uri(path)
        .header("authorization", "Bearer test")
        .header("x-oagw-target-host", target)
        .body(Body::empty())
        .expect("request");
    request.extensions_mut().insert(caller.context());
    let response = app.clone().oneshot(request).await.expect("response");
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20).await.expect("body");
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (parts.status, parts.headers, json)
}

/// Writes a raw request and reads the status line the gateway answers with.
async fn raw_status(router: std::net::SocketAddr, request: &str) -> String {
    let mut client = tokio::net::TcpStream::connect(router)
        .await
        .expect("connect");
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    client.write_all(request.as_bytes()).await.expect("write");
    let mut buffer = vec![0_u8; 1024];
    let read = tokio::time::timeout(std::time::Duration::from_secs(5), client.read(&mut buffer))
        .await
        .expect("answer before the deadline")
        .expect("read");
    buffer.truncate(read);
    String::from_utf8_lossy(&buffer)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn a_malformed_framing_header_is_answered_with_400() {
    let (addr, listener) = bind_upstream().await;
    let port = addr.port();
    let alias = format!("framing-{port}");
    let app = app();
    let caller = Caller::default();

    let upstream_id = bound_upstream(&app, &caller, &alias, port).await;
    let _ = create_route(&app, &caller, &route_body(&upstream_id, "/v1")).await;
    let router = serve(with_context(app, &caller)).await;

    // A malformed `Content-Length` is refused by the transport, before any
    // pipeline stage: the upstream never sees a connection.
    let malformed = format!(
        "POST /oagw/v1/proxy/{alias}/v1/chat HTTP/1.1\r\nhost: {router}\r\n\
         content-length: twelve\r\n\r\n"
    );
    let status = raw_status(router, &malformed).await;
    assert!(status.starts_with("HTTP/1.1 400"), "{status}");

    // An unsupported transfer encoding (both a length and a framing header)
    // is refused the same way.
    let conflicting = format!(
        "POST /oagw/v1/proxy/{alias}/v1/chat HTTP/1.1\r\nhost: {router}\r\n\
         content-length: 2\r\ntransfer-encoding: deflate\r\n\r\n"
    );
    let status = raw_status(router, &conflicting).await;
    assert!(status.starts_with("HTTP/1.1 400"), "{status}");

    // The upstream listener is never dialled: an accept would block forever.
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), listener.accept())
            .await
            .is_err(),
        "the upstream was dialled"
    );
}

/// FR-004 / AT-2: the caller's spelling of the alias is not the stored one.
/// Aliases are normalized on submission, so a lookup is too: ASCII lowercase
/// and no trailing dot.
#[tokio::test]
async fn an_alias_is_resolved_regardless_of_case_or_trailing_dot() {
    let (addr, listener) = bind_upstream().await;
    let port = addr.port();
    let app = app();
    let caller = Caller::default();

    let upstream_id = bound_upstream(&app, &caller, "vendor.example.com", port).await;
    let _ = create_route(&app, &caller, &route_body(&upstream_id, "/v1")).await;

    // Two connections: one per spelling of the alias.
    let server = tokio::spawn(async move {
        let mut heads = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let (head, _body) = read_request(&mut stream).await;
            write_response(
                &mut stream,
                "HTTP/1.1 200 OK",
                &[("content-type", "application/json")],
                b"{\"ok\":true}",
            )
            .await;
            heads.push(head);
        }
        heads
    });

    // Uppercase spelling.
    let (status, _, body) = proxy(&app, &caller, "VENDOR.Example.COM", "/v1/chat", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // A trailing dot, as a resolver-suffixed client would send.
    let (status, _, body) = proxy(&app, &caller, "vendor.example.com.", "/v1/chat", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(server.await.expect("upstream task").len(), 2);
}

/// AT-4 / FR-018: an upstream's own error response is not reshaped. It is
/// forwarded byte-for-byte and tagged as coming from the upstream.
#[tokio::test]
async fn an_upstream_error_is_passed_through_unchanged() {
    let (addr, listener) = bind_upstream().await;
    let port = addr.port();
    let alias = format!("failing-{port}");
    let app = app();
    let caller = Caller::default();

    let upstream_id = bound_upstream(&app, &caller, &alias, port).await;
    let _ = create_route(&app, &caller, &route_body(&upstream_id, "/v1")).await;

    let payload = b"{\"error\":\"quota exhausted\",\"reasons\":[\"rate\",\"budget\"]}";
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let (head, _body) = read_request(&mut stream).await;
        write_raw(
            &mut stream,
            concat!(
                "HTTP/1.1 500 Internal Server Error\r\n",
                "content-type: application/json\r\n",
                "x-upstream-trace: trace-1\r\n",
                "\r\n",
            )
            .as_bytes(),
        )
        .await;
        write_raw(&mut stream, payload).await;
        head
    });

    let (status, headers, body) = proxy(&app, &caller, &alias, "/v1/chat", None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    // Byte-for-byte: the upstream's document, not a problem-details rewrite.
    assert_eq!(
        body,
        serde_json::from_slice::<serde_json::Value>(payload).expect("json")
    );
    assert_eq!(
        headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        headers
            .get("x-oagw-error-source")
            .and_then(|value| value.to_str().ok()),
        Some("upstream")
    );
    assert_eq!(
        headers
            .get("x-upstream-trace")
            .and_then(|value| value.to_str().ok()),
        Some("trace-1")
    );

    let head = server.await.expect("upstream task");
    assert!(head.starts_with("GET /v1/chat HTTP/1.1"), "{head}");
}

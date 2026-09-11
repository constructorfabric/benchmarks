//! Integration tests of the streaming relay legs
//! (`cpt-cf-oagw-flow-request-proxy-sse-streaming`,
//! `cpt-cf-oagw-flow-request-proxy-stream-lifecycle`).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use oagw::domain::dto::{EndpointScheme, HttpMethod};
use oagw::test_support::{
    permissive_surface, route_for, seed_route, seed_upstream, stub_upstream, upstream_at,
};
use uuid::Uuid;

/// The `oagw` block the proxy tests need: `http` upstreams admitted, a proxy
/// timeout the timeout test can hit, and the default body limit.
fn proxy_config(timeout_secs: u64) -> Option<serde_json::Value> {
    Some(serde_json::json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": timeout_secs,
        "max_body_size_bytes": 1_048_576
    }))
}

/// The surface with one upstream and one route seeded over a stub upstream.
async fn seeded(
    timeout_secs: u64,
    script: Vec<String>,
) -> (oagw::test_support::ManagementSurface, oagw::test_support::StubUpstream, Uuid) {
    let surface = permissive_surface(proxy_config(timeout_secs)).await;
    let stub = stub_upstream(script).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let upstream = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port);
    let upstream_id = seed_upstream(&surface, upstream);
    seed_route(&surface, route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get]));
    (surface, stub, tenant)
}

/// A chunked response body carrying `frames` as one `data:` event each.
fn sse_response(frames: &[&str]) -> String {
    let mut body = String::new();
    for frame in frames {
        let chunk = format!("data: {frame}\n\n");
        body.push_str(&format!("{:x}\r\n{}\r\n", chunk.len(), chunk));
    }
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{body}0\r\n\r\n"
    )
}

#[tokio::test]
async fn an_event_stream_is_relayed_as_it_arrives() {
    let (surface, _stub, tenant) = seeded(5, vec![sse_response(&["one", "two"])]).await;
    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1/feed", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    assert_eq!(exchange.header("content-type"), Some("text/event-stream"));
    assert_eq!(exchange.header("x-oagw-error-source"), Some("upstream"));
    assert_eq!(exchange.text(), "data: one\n\ndata: two\n\n");
}

#[tokio::test]
async fn a_buffered_response_is_not_rewritten() {
    let (surface, _stub, tenant) = seeded(5, Vec::new()).await;
    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1/feed", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::OK);
    assert_eq!(exchange.header("content-type"), Some("text/plain"));
    assert_eq!(exchange.text(), "payload");
}

#[tokio::test]
async fn a_response_larger_than_the_body_limit_is_a_downstream_error() {
    // The ceiling the gear enforces on the buffered leg is its own
    // `max_body_size_bytes`, not the shared client's unlimited one: the
    // response body never accumulates past it.
    let body = "x".repeat(4 * 1_048_576);
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    );
    let (surface, _stub, tenant) = seeded(5, vec![response]).await;

    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1/feed", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::BAD_GATEWAY);
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
    assert!(exchange.text().contains("cf.oagw.downstream.error.v1"));
}

#[tokio::test]
async fn a_stalled_response_body_is_bounded_by_the_proxy_timeout() {
    // Headers arrive inside the budget and the body then stalls: the buffered
    // read is bounded by the remaining budget, so the exchange ends at the
    // timeout instead of holding a pool connection open until the client gives
    // up.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("the listener binds");
    let address = listener.local_addr().expect("the address");
    tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = vec![0u8; 4096];
        let _ = socket.read(&mut request).await;
        // The head is answered and the body is then never completed and the
        // socket is never closed, so only the budget can end the read.
        let _ = socket
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: 100\r\n\r\nabc")
            .await;
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    });

    let surface = permissive_surface(proxy_config(1)).await;
    let tenant = Uuid::new_v4();
    let upstream = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, "127.0.0.1", address.port());
    let upstream_id = seed_upstream(&surface, upstream);
    seed_route(&surface, route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get]));

    let started = std::time::Instant::now();
    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1/feed", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "the body read returned inside the configured budget"
    );
}

#[tokio::test]
async fn an_upstream_that_never_answers_times_out_without_a_retry() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("the listener binds");
    let address = listener.local_addr().expect("the address");
    let accepts = Arc::clone(&attempts);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            accepts.fetch_add(1, Ordering::SeqCst);
            // Accepted and then never answered: the proxy timeout decides.
            drop(tokio::spawn(async move {
                let socket = socket;
                let _ = socket;
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }));
        }
    });

    let surface = permissive_surface(proxy_config(1)).await;
    let tenant = Uuid::new_v4();
    let upstream = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, "127.0.0.1", address.port());
    let upstream_id = seed_upstream(&surface, upstream);
    seed_route(&surface, route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get]));

    let exchange = surface
        .proxy_for(tenant, Uuid::new_v4(), "GET", "/oagw/v1/proxy/api.vendor.com/v1/feed", &[], b"")
        .await;
    assert_eq!(exchange.status, http::StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
    // A timeout is a client-side deadline, never a failover trigger: the
    // original request is not re-sent to the same endpoint.
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

/// Serve the router over a real TCP listener, so an upgrade handshake reaches
/// the handler the way the runtime drives it.
///
/// The security context the api-gateway resolves ahead of the gear is injected
/// by a test-only middleware, the same seam the request harness uses.
async fn serve(
    surface: &oagw::test_support::ManagementSurface,
    tenant: Uuid,
    principal: Uuid,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    use hyper_util::rt::TokioIo;
    use hyper_util::service::TowerToHyperService;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("the listener binds");
    let address = listener.local_addr().expect("the address");
    let authenticated = surface
        .router
        .clone()
        .layer(axum::middleware::from_fn(
            move |mut request: axum::extract::Request, next: axum::middleware::Next| async move {
            let security = oagw::test_support::security_context(tenant, principal);
            request.extensions_mut().insert(security);
            next.run(request).await
            },
        ));
    let handle = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let service = TowerToHyperService::new(authenticated.clone());
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(socket), service)
                .with_upgrades()
                .await;
        }
    });
    (address, handle)
}

#[tokio::test]
async fn a_websocket_session_is_relayed_over_the_handler() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // The upstream accepts the session; the client leg is closed as soon as the
    // head is read, so the relay records the abort rather than an open session.
    let stub = stub_upstream(vec![
        "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: upgrade\r\n\r\n"
            .to_owned(),
    ])
    .await;
    let (host, port) = stub.endpoint();
    let surface = permissive_surface(proxy_config(5)).await;
    let tenant = Uuid::new_v4();
    let upstream = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port);
    let upstream_id = seed_upstream(&surface, upstream);
    seed_route(&surface, route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get]));

    let (address, _server) = serve(&surface, tenant, Uuid::new_v4()).await;

    let mut client = tokio::net::TcpStream::connect(address).await.expect("the client connects");
    let handshake = format!(
        "GET /oagw/v1/proxy/api.vendor.com/v1/ws HTTP/1.1\r\n\
         host: {address}\r\n\
         upgrade: websocket\r\n\
         connection: Upgrade\r\n\
         sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         sec-websocket-version: 13\r\n\r\n"
    );
    client.write_all(handshake.as_bytes()).await.expect("the handshake is written");

    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let read = client.read(&mut byte).await.expect("the head is readable");
        assert!(read > 0, "the connection closed before the response head completed");
        head.extend_from_slice(&byte[..read]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    assert!(head.starts_with("HTTP/1.1 101"), "the session is accepted: {head}");
    assert!(head.to_ascii_lowercase().contains("upgrade: websocket"), "{head}");
    // The relay is open: the upstream handshake reached the upstream unstripped.
    assert_eq!(stub.received()[0].header("sec-websocket-key"), Some("dGhlIHNhbXBsZSBub25jZQ=="));
}

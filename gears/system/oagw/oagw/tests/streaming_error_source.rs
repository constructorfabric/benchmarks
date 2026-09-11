//! Integration tests of the error-source distinction on a streaming session
//! (FEATURE `error-handling`, flow `error-handling-streaming-source`).
//!
//! A gateway failure raised before the response head is produced is
//! problem+json exactly as a buffered one is; a session that aborts after the
//! head was sent keeps the source header it was stamped with and is never
//! rewritten.

use oagw::domain::dto::{EndpointScheme, HttpMethod};
use oagw::test_support::{
    permissive_surface, route_for, seed_route, seed_upstream, stub_upstream, upstream_at,
};
use uuid::Uuid;

/// The `oagw` block the streaming tests need.
fn proxy_config() -> Option<serde_json::Value> {
    Some(serde_json::json!({
        "allow_http_upstream": true,
        "proxy_timeout_secs": 5,
        "max_body_size_bytes": 1_048_576
    }))
}

/// The surface with one upstream and one route seeded over a stub upstream.
async fn seeded(script: Vec<String>) -> (oagw::test_support::ManagementSurface, oagw::test_support::StubUpstream, Uuid) {
    let surface = permissive_surface(proxy_config()).await;
    let stub = stub_upstream(script).await;
    let (host, port) = stub.endpoint();
    let tenant = Uuid::new_v4();
    let upstream = upstream_at(tenant, "api.vendor.com", EndpointScheme::Http, &host, port);
    let upstream_id = seed_upstream(&surface, upstream);
    seed_route(&surface, route_for(tenant, upstream_id, "/v1", &[HttpMethod::Get]));
    (surface, stub, tenant)
}

/// A chunked SSE body that is never terminated, so the relay sees an abort.
fn unterminated_sse() -> String {
    let chunk = "data: one\n\n";
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{}\r\n",
        chunk.len(),
        chunk
    )
}

/// Send one authenticated request and return the head and the body frames the
/// client received, in order, tolerating a mid-body error — which is exactly
/// what a session that aborts after the head was sent looks like.
async fn raw_request(
    surface: &oagw::test_support::ManagementSurface,
    tenant: Uuid,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> (
    http::StatusCode,
    Vec<(String, String)>,
    Vec<Result<bytes::Bytes, axum::Error>>,
) {
    use tower::ServiceExt;

    let mut builder = http::Request::builder().method(method).uri(path);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut request = builder
        .body(axum::body::Body::empty())
        .expect("the request is well formed");
    request
        .extensions_mut()
        .insert(oagw::test_support::security_context(tenant, Uuid::new_v4()));
    let response = surface.router.clone().oneshot(request).await.expect("the router answers");
    let status = response.status();
    let response_headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), value.to_str().unwrap_or_default().to_owned()))
        .collect();
    let mut body = response.into_body();
    let mut frames = Vec::new();
    loop {
        match http_body_util::BodyExt::frame(&mut body).await {
            Some(Ok(frame)) => match frame.into_data() {
                Ok(data) => frames.push(Ok(data)),
                Err(_) => continue,
            },
            Some(Err(error)) => {
                frames.push(Err(error));
                break;
            }
            None => break,
        }
    }
    (status, response_headers, frames)
}

#[tokio::test]
async fn a_gateway_failure_before_the_head_is_problem_json_and_gateway_sourced() {
    // The route only admits `GET /v1`; an event-stream request to a path no
    // route matches fails before any head is produced.
    let (surface, _stub, tenant) = seeded(Vec::new()).await;
    let exchange = surface
        .proxy_for(
            tenant,
            Uuid::new_v4(),
            "GET",
            "/oagw/v1/proxy/api.vendor.com/v9/feed",
            &[("accept", "text/event-stream")],
            b"",
        )
        .await;
    assert_eq!(exchange.status, http::StatusCode::NOT_FOUND);
    assert_eq!(exchange.header("content-type"), Some("application/problem+json"));
    assert_eq!(exchange.header("x-oagw-error-source"), Some("gateway"));
    let document: serde_json::Value = serde_json::from_slice(&exchange.body).expect("problem+json");
    assert_eq!(document["type"], "gts://gts.cf.core.errors.err.v1~cf.oagw.route.not_found.v1");
}

#[tokio::test]
async fn a_session_that_opens_is_upstream_sourced_and_relayed() {
    let (surface, _stub, tenant) = seeded(vec![
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\nb\r\ndata: one\n\n\r\n0\r\n\r\n"
            .to_owned(),
    ])
    .await;
    let (status, headers, frames) = raw_request(
        &surface,
        tenant,
        "GET",
        "/oagw/v1/proxy/api.vendor.com/v1/feed",
        &[("accept", "text/event-stream")],
    )
    .await;
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(header(&headers, "content-type"), Some("text/event-stream"));
    assert_eq!(header(&headers, "x-oagw-error-source"), Some("upstream"));
    let body: String = frames
        .into_iter()
        .map(|frame| String::from_utf8_lossy(&frame.expect("a relayed frame")).to_string())
        .collect();
    assert_eq!(body, "data: one\n\n", "the events are relayed unmodified");
}

#[tokio::test]
async fn an_aborted_session_keeps_the_source_header_it_was_stamped_with() {
    let (surface, _stub, tenant) = seeded(vec![unterminated_sse()]).await;
    let (status, headers, frames) = raw_request(
        &surface,
        tenant,
        "GET",
        "/oagw/v1/proxy/api.vendor.com/v1/feed",
        &[("accept", "text/event-stream")],
    )
    .await;
    // The head was already sent, so the classification cannot change the
    // status or rewrite the body: the stamp the head carried is preserved and
    // the frames the upstream did produce are the ones the client receives.
    assert_eq!(status, http::StatusCode::OK);
    assert_eq!(header(&headers, "x-oagw-error-source"), Some("upstream"));
    assert_eq!(header(&headers, "content-type"), Some("text/event-stream"));
    let relayed: Vec<String> = frames
        .iter()
        .filter_map(|frame| frame.as_ref().ok())
        .map(|data| String::from_utf8_lossy(data).to_string())
        .collect();
    assert_eq!(relayed.join(""), "data: one\n\n", "no problem+json body is spliced in");
    assert!(frames.last().is_some_and(|frame| frame.is_err()), "the abort ends the stream");
}

/// The header value of `name`, compared as the framework lowercased it.
fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.as_str())
}

//! WebSocket proxying.
//!
//! The inbound upgrade is completed with axum's `WebSocketUpgrade`; the
//! outbound leg uses `tokio-tungstenite`'s async client over the same
//! [`UpstreamIo`] transport the HTTP path uses. The upstream handshake is
//! completed *before* the client upgrade is accepted, so a failed dial still
//! yields a normal problem response.

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocketUpgrade};
use axum::http::header;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite as ts;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::domain::error::OagwError;
use crate::domain::model::Endpoint;
use crate::proxy::transport::UpstreamIo;

/// Connects to the upstream WebSocket endpoint.
///
/// # Errors
///
/// Connection, TLS and handshake failures map to the OAGW problems.
pub async fn connect_upstream(
    tls: &crate::proxy::transport::TlsContext,
    url: &str,
    tls_required: bool,
    timeout: std::time::Duration,
    subprotocol: Option<&str>,
    extra_headers: &[(String, String)],
) -> Result<(WebSocketStream<UpstreamIo>, ts::handshake::client::Response), OagwError> {
    let (host, port) = split_authority(url, tls_required);
    let io = crate::proxy::transport::connect(tls, &host, port, tls_required, timeout).await?;

    let uri: http::Uri = url
        .parse()
        .map_err(|e| OagwError::protocol_error(format!("invalid upstream websocket url: {e}")))?;
    let mut request = uri
        .into_client_request()
        .map_err(|e| OagwError::protocol_error(format!("invalid websocket request: {e}")))?;
    {
        let headers = request.headers_mut();
        if let Some(proto) = subprotocol
            && let Ok(value) = http::HeaderValue::from_str(proto)
        {
            headers.insert(http::header::SEC_WEBSOCKET_PROTOCOL, value);
        }
        for (name, value) in extra_headers {
            if let (Ok(name), Ok(value)) = (
                http::HeaderName::from_bytes(name.as_bytes()),
                http::HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        }
    }

    let (stream, response) = tokio_tungstenite::client_async(request, io)
        .await
        .map_err(|e| OagwError::protocol_error(format!("websocket handshake failed: {e}")))?;
    Ok((stream, response))
}

/// Splits a `ws(s)://` URL into host and port.
fn split_authority(url: &str, tls_required: bool) -> (String, u16) {
    let rest = url
        .strip_prefix("wss://")
        .or_else(|| url.strip_prefix("ws://"))
        .unwrap_or(url);
    let authority = rest.split('/').next().unwrap_or(rest);
    let default_port = if tls_required { 443 } else { 80 };
    match authority.rsplit_once(':') {
        Some((host, port)) => (
            host.trim_matches(['[', ']']).to_owned(),
            port.parse().unwrap_or(default_port),
        ),
        None => (authority.trim_matches(['[', ']']).to_owned(), default_port),
    }
}

/// Completes the proxy pipeline for a WebSocket upgrade request.
///
/// The upstream handshake runs first, so a failed dial answers with a normal
/// problem response rather than a half-open socket.
///
/// # Errors
///
/// Propagates every resolution, validation and connection failure.
pub(crate) async fn upgrade(
    state: &std::sync::Arc<crate::proxy::ProxyState>,
    security: &toolkit_security::SecurityContext,
    parts: &axum::http::request::Parts,
    upgrade: WebSocketUpgrade,
    alias: &str,
    path_suffix: &str,
) -> Result<Response, OagwError> {
    let request_path = crate::proxy::normalize_suffix(path_suffix);
    let target = crate::proxy::resolve(state, security, parts, alias, &request_path).await?;
    crate::proxy::enforce_rate_limit(state, security, &target, &request_path).await?;

    let outbound_path = crate::proxy::build_outbound_path(&target.route, &request_path)?;
    let query = crate::proxy::filter_query(&target.route, parts.uri.query().unwrap_or_default())?;

    let outbound_headers = crate::proxy::headers::build_outbound_headers(
        &parts.headers,
        &target.upstream.headers.request,
        &target.endpoint.host_header(),
    );
    let request_context =
        crate::proxy::run_request_plugins(state, security, &target, outbound_headers).await?;
    let extra = websocket_extra_headers(&request_context.headers);

    let subprotocol = parts
        .headers
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let url = websocket_url(&target.endpoint, &outbound_path, &query);
    let timeout = state.config.proxy_timeout();
    let tls_required = !target.endpoint.scheme.is_plaintext();

    let (upstream, upstream_response) = connect_upstream(
        &state.tls,
        &url,
        tls_required,
        timeout,
        subprotocol.as_deref(),
        &extra,
    )
    .await?;
    if upstream_response.status() != http::StatusCode::SWITCHING_PROTOCOLS {
        return Err(OagwError::protocol_error(format!(
            "upstream refused the websocket upgrade with status {}",
            upstream_response.status()
        ))
        .with_extension("upstream_id", target.upstream_uuid.to_string()));
    }

    // ADR-0007: every response — including a successful upgrade — names the
    // error source so a client can tell a gateway answer from an upstream one.
    let mut response = upgrade.on_upgrade(move |socket| pump(socket, upstream));
    response.headers_mut().insert(
        http::HeaderName::from_static(crate::proxy::headers::ERROR_SOURCE_HEADER),
        http::HeaderValue::from_static("upstream"),
    );
    Ok(response)
}

/// The outbound WebSocket URL for a resolved endpoint.
#[must_use]
pub fn websocket_url(endpoint: &Endpoint, path: &str, query: &str) -> String {
    let scheme = if endpoint.scheme.is_plaintext() {
        "ws"
    } else {
        "wss"
    };
    let path = crate::proxy::normalize_suffix(path);
    if query.is_empty() {
        format!("{scheme}://{}{path}", endpoint.host_header())
    } else {
        format!("{scheme}://{}{path}?{query}", endpoint.host_header())
    }
}

/// Extra headers to send upstream: everything the plugin chain injected except
/// the connection and WebSocket negotiation headers tungstenite owns.
#[must_use]
pub fn websocket_extra_headers(headers: &http::HeaderMap) -> Vec<(String, String)> {
    let reserved = [
        header::HOST.as_str(),
        header::CONNECTION.as_str(),
        header::UPGRADE.as_str(),
        "sec-websocket-key",
        "sec-websocket-version",
        "sec-websocket-protocol",
        "sec-websocket-extensions",
        "sec-websocket-accept",
    ];
    headers
        .iter()
        .filter(|(name, _)| {
            let name = name.as_str();
            !crate::proxy::headers::HOP_BY_HOP_HEADERS.contains(&name) && !reserved.contains(&name)
        })
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_ascii_lowercase(), value.to_owned()))
        })
        .collect()
}

/// Bidirectionally pumps two WebSocket streams until either side closes.
pub async fn pump(client: axum::extract::ws::WebSocket, upstream: WebSocketStream<UpstreamIo>) {
    let (mut client_sink, mut client_stream) = client.split();
    let (mut upstream_sink, mut upstream_stream) = upstream.split();

    let to_client = tokio::spawn(async move {
        while let Some(Ok(msg)) = upstream_stream.next().await {
            if let Some(msg) = convert_from(msg)
                && client_sink.send(msg).await.is_err()
            {
                break;
            }
        }
        let _ = client_sink.close().await;
    });

    let to_upstream = tokio::spawn(async move {
        while let Some(Ok(msg)) = client_stream.next().await {
            if let Some(msg) = convert_to(msg)
                && upstream_sink.send(msg).await.is_err()
            {
                break;
            }
        }
        let _ = upstream_sink.close().await;
    });

    let _ = to_client.await;
    let _ = to_upstream.await;
}

/// Converts a tungstenite message into an axum one.
#[must_use]
pub fn convert_from(message: ts::Message) -> Option<Message> {
    use ts::Message as Ts;
    Some(match message {
        Ts::Text(text) => Message::Text(Utf8Bytes::from(text.as_str())),
        Ts::Binary(bytes) => Message::Binary(bytes),
        Ts::Ping(ping) => Message::Ping(ping),
        Ts::Pong(pong) => Message::Pong(pong),
        Ts::Close(Some(close)) => Message::Close(Some(CloseFrame {
            code: close.code.into(),
            reason: Utf8Bytes::from(close.reason.as_str()),
        })),
        Ts::Close(None) => Message::Close(None),
        Ts::Frame(_) => return None,
    })
}

/// Converts an axum message into a tungstenite one.
#[must_use]
pub fn convert_to(message: Message) -> Option<ts::Message> {
    Some(match message {
        Message::Text(text) => ts::Message::Text(ts::Utf8Bytes::from(text.as_str())),
        Message::Binary(bytes) => ts::Message::Binary(bytes),
        Message::Ping(ping) => ts::Message::Ping(ping),
        Message::Pong(pong) => ts::Message::Pong(pong),
        Message::Close(Some(frame)) => ts::Message::Close(Some(ts::protocol::CloseFrame {
            code: frame.code.into(),
            reason: ts::Utf8Bytes::from(frame.reason.as_str()),
        })),
        Message::Close(None) => ts::Message::Close(None),
    })
}

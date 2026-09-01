//! WebSocket bridging (server leg = axum, upstream leg = tokio-tungstenite).
//!
//! The proxy handler detects an upgrade request, connects to the upstream
//! `ws://`/`wss://` endpoint with the hop-by-hop-stripped client headers and
//! transparently bridges messages in both directions until either side
//! closes.

use axum::extract::ws::{Message as AxumMessage, WebSocket};
use futures_util::{SinkExt, StreamExt};
use http::HeaderMap;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message as TungMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

/// Errors while establishing the upstream WebSocket connection.
#[derive(Debug, thiserror::Error)]
pub enum WsConnectError {
    /// The upstream URL / handshake failed.
    #[error("websocket upstream error: {0}")]
    Handshake(String),
}

/// Headers that belong to the WebSocket handshake / are hop-by-hop and must
/// not be replayed verbatim.
const HOP_BY_HOP_UPGRADE: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "sec-websocket-version",
    "sec-websocket-key",
    "sec-websocket-extensions",
];

/// Whether the request carries a WebSocket upgrade.
#[must_use]
pub fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let connection = headers
        .get(http::header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    connection.split(',').any(|t| t.trim() == "upgrade")
        && headers
            .get(http::header::UPGRADE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// Connect to an upstream WebSocket endpoint, forwarding the given headers.
///
/// The handshake request is generated from the URL (which supplies the
/// `Sec-WebSocket-Key`, `Connection: Upgrade`, `Upgrade: websocket` and
/// protocol-version headers) and then the caller's headers are layered over
/// it after stripping hop-by-hop/upgrade headers.
///
/// # Errors
///
/// Returns [`WsConnectError::Handshake`] when the URL cannot be turned into a
/// valid handshake request or the upstream handshake fails.
///
/// [`WsConnectError::Handshake`]: WsConnectError::Handshake
pub async fn connect_upstream(
    url: &str,
    headers: &HeaderMap,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, WsConnectError> {
    let mut request = url
        .to_owned()
        .into_client_request()
        .map_err(|e| WsConnectError::Handshake(e.to_string()))?;
    for (name, value) in headers {
        if HOP_BY_HOP_UPGRADE.contains(&name.as_str()) {
            continue;
        }
        request.headers_mut().insert(name, value.clone());
    }
    let (stream, _response) = connect_async(request)
        .await
        .map_err(|e| WsConnectError::Handshake(e.to_string()))?;
    Ok(stream)
}

/// Bridge messages between the axum (server) socket and the upstream
/// tungstenite socket until either direction terminates.
pub async fn bridge(client: WebSocket, upstream: WebSocketStream<MaybeTlsStream<TcpStream>>) {
    let (mut client_tx, mut client_rx) = client.split();
    let (mut up_tx, mut up_rx) = upstream.split();

    tokio::select! {
        () = async {
            while let Some(Ok(msg)) = client_rx.next().await {
                let tung = to_tungstenite(msg);
                let closing = matches!(tung, TungMessage::Close(_));
                if up_tx.send(tung).await.is_err() { break; }
                if closing { break; }
            }
        } => {}
        () = async {
            while let Some(Ok(msg)) = up_rx.next().await {
                let Some(axum) = to_axum(msg) else { continue; };
                let closing = matches!(axum, AxumMessage::Close(_));
                if client_tx.send(axum).await.is_err() { break; }
                if closing { break; }
            }
        } => {}
    }

    // Best-effort close on both legs.
    let _client_close = client_tx.send(AxumMessage::Close(None)).await;
    let _upstream_close = up_tx.send(TungMessage::Close(None)).await;
}

fn to_tungstenite(msg: AxumMessage) -> TungMessage {
    match msg {
        AxumMessage::Text(t) => TungMessage::Text(
            tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes::from(t.to_string()),
        ),
        AxumMessage::Binary(b) => TungMessage::Binary(b),
        AxumMessage::Ping(b) => TungMessage::Ping(b),
        AxumMessage::Pong(b) => TungMessage::Pong(b),
        AxumMessage::Close(c) => TungMessage::Close(c.map(|f| {
            tokio_tungstenite::tungstenite::protocol::frame::CloseFrame {
                code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::from(
                    f.code,
                ),
                reason: tokio_tungstenite::tungstenite::protocol::frame::Utf8Bytes::from(
                    f.reason.to_string(),
                ),
            }
        })),
    }
}

fn to_axum(msg: TungMessage) -> Option<AxumMessage> {
    match msg {
        TungMessage::Text(t) => {
            let bytes = bytes::Bytes::from(t.to_string());
            axum::extract::ws::Utf8Bytes::try_from(bytes)
                .map(AxumMessage::Text)
                .ok()
        }
        TungMessage::Binary(b) => Some(AxumMessage::Binary(b)),
        TungMessage::Ping(b) => Some(AxumMessage::Ping(b)),
        TungMessage::Pong(b) => Some(AxumMessage::Pong(b)),
        TungMessage::Close(c) => Some(AxumMessage::Close(c.map(|f| {
            axum::extract::ws::CloseFrame {
                code: u16::from(f.code),
                reason: axum::extract::ws::Utf8Bytes::try_from(bytes::Bytes::from(
                    f.reason.to_string(),
                ))
                .unwrap_or_else(|_| axum::extract::ws::Utf8Bytes::from_static("")),
            }
        }))),
        TungMessage::Frame(_) => None,
    }
}

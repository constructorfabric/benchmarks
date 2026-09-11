//! The WebSocket bridge.
//!
//! An upgraded request is never relayed as a plain HTTP exchange: the gateway dials the
//! upstream with the same `Upgrade` framing, then splices the two byte streams together so
//! frames flow in both directions without a buffering round trip in between.

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, Message as UpstreamMessage};

use crate::error::{ErrorKind, OagwError};
use crate::proxy::target::Target;

/// Builds the upstream URL of a bridged upgrade.
#[must_use]
pub fn upstream_url(destination: &Target, path: &str, query: &str) -> String {
    if query.is_empty() {
        format!("{}{}", ws_origin(destination), path)
    } else {
        format!("{}{}?{}", ws_origin(destination), path, query)
    }
}

/// Bridges an inbound WebSocket upgrade to the upstream endpoint.
///
/// The caller's handshake is completed immediately — a slow upstream must not hold the
/// browser's connection open half-done — and the two sockets are then spliced in a spawned
/// task that ends when either side closes.
///
/// # Errors
///
/// Returns a link error when the upstream cannot be dialled and a protocol error when the
/// upstream declines the upgrade.
pub async fn bridge(
    upgrade: WebSocketUpgrade,
    destination: &Target,
    path: &str,
    query: &str,
) -> Result<Response, OagwError> {
    let url = upstream_url(destination, path, query);

    let (upstream, response) = tokio_tungstenite::connect_async(url.as_str()).await.map_err(|err| {
        OagwError::new(
            ErrorKind::LinkUnavailable,
            format!("upstream websocket dial failed: {err}"),
        )
    })?;
    if response.status() != http::StatusCode::SWITCHING_PROTOCOLS {
        return Err(OagwError::new(
            ErrorKind::ProtocolError,
            format!("upstream answered a websocket upgrade with {}", response.status()),
        ));
    }

    Ok(upgrade.on_upgrade(move |client| async move {
        splice(client, upstream).await;
    }))
}

/// Relays frames between the caller's socket and the upstream's until either closes.
async fn splice(
    client: WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    let (mut client_sink, mut client_stream) = client.split();
    let (mut upstream_sink, mut upstream_stream) = upstream.split();

    let caller_to_upstream = async {
        while let Some(Ok(frame)) = client_stream.next().await {
            if let Err(error) = upstream_sink.send(convert_outbound(frame)).await {
                tracing::debug!(target: "oagw", "the upstream sink rejected a frame: {error}");
                break;
            }
        }
        let _closed = upstream_sink.close().await;
    };

    let upstream_to_caller = async {
        while let Some(Ok(frame)) = upstream_stream.next().await {
            if let Some(frame) = convert_inbound(frame) {
                if let Err(error) = client_sink.send(frame).await {
                    tracing::debug!(target: "oagw", "the client sink rejected a frame: {error}");
                    break;
                }
            }
        }
        let _closed = client_sink.close().await;
    };

    tokio::join!(caller_to_upstream, upstream_to_caller);
}

/// Converts a caller frame into an upstream frame.
///
/// The caller's frame set is total over `Message`, so this never fails: a raw frame is a
/// library detail the caller cannot produce, and only the inbound direction can see one.
#[must_use]
pub fn convert_outbound(frame: Message) -> UpstreamMessage {
    match frame {
        Message::Text(text) => UpstreamMessage::Text(text.as_str().into()),
        Message::Binary(bytes) => UpstreamMessage::Binary(bytes),
        Message::Ping(bytes) => UpstreamMessage::Ping(bytes),
        Message::Pong(bytes) => UpstreamMessage::Pong(bytes),
        Message::Close(reason) => UpstreamMessage::Close(reason.map(|frame| CloseFrame {
            code: frame.code.into(),
            reason: frame.reason.as_str().into(),
        })),
    }
}

/// Converts an upstream frame into a caller frame.
#[must_use]
pub fn convert_inbound(frame: UpstreamMessage) -> Option<Message> {
    match frame {
        UpstreamMessage::Text(text) => Some(Message::Text(text.as_str().into())),
        UpstreamMessage::Binary(bytes) => Some(Message::Binary(bytes)),
        UpstreamMessage::Ping(bytes) => Some(Message::Ping(bytes)),
        UpstreamMessage::Pong(bytes) => Some(Message::Pong(bytes)),
        UpstreamMessage::Close(reason) => Some(Message::Close(reason.map(|reason| {
            axum::extract::ws::CloseFrame {
                code: reason.code.into(),
                reason: reason.reason.as_str().into(),
            }
        }))),
        UpstreamMessage::Frame(_) => None,
    }
}

/// The `ws://` or `wss://` origin of a target.
#[must_use]
pub fn ws_origin(destination: &Target) -> String {
    let scheme = match destination.endpoint.scheme.as_str() {
        "wss" | "https" => "wss",
        _ => "ws",
    };
    format!("{scheme}://{}", destination.authority)
}

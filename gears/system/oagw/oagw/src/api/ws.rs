//! WebSocket proxying (DESIGN §4.6).
//!
//! The inbound upgrade is completed first and the upstream connection is then opened with
//! `tokio-tungstenite`; frames are pumped in both directions until either side closes.

use axum::extract::ws::{Message as AxumMessage, WebSocket, WebSocketUpgrade};
use axum::http::request::Parts;
use axum::extract::FromRequestParts;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::protocol::CloseFrame as WsCloseFrame;

use super::ApiState;
use crate::domain::error::DomainError;
use crate::infra::api::problem;
use crate::infra::proxy::service::Resolved;

/// Bridge a WebSocket upgrade to the resolved upstream.
pub(super) async fn proxy_ws(
    _state: &ApiState,
    mut parts: Parts,
    resolved: Resolved,
) -> Response {
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };

    let url = upstream_url(&resolved, parts.uri.query().unwrap_or_default());
    let instance = parts.uri.path().to_string();
    let (upstream, _response) = match tokio_tungstenite::connect_async(url).await {
        Ok(pair) => pair,
        Err(error) => {
            return problem::problem_response(
                &DomainError::Downstream(format!("upstream refused the WebSocket upgrade: {error}")),
                &resolved.meta(),
                &instance,
            );
        }
    };

    let mut upgraded = upgrade.on_upgrade(move |socket| pump(socket, upstream)).into_response();
    // The 101 came from the upstream, so a client that checks the stamp sees where it came from
    // (ADR-0007).
    problem::stamp_upstream_source(&mut upgraded);
    upgraded
}

/// The URL the upstream connection is opened against.
#[must_use]
pub fn upstream_url(resolved: &Resolved, query: &str) -> String {
    let scheme = match resolved.endpoint.scheme.as_str() {
        "http" => "ws",
        "https" => "wss",
        other => other,
    };
    let suffix = if query.is_empty() {
        String::new()
    } else {
        format!("?{query}")
    };
    format!("{scheme}://{}{}{}", resolved.endpoint.authority(), resolved.path, suffix)
}

/// Pump frames between the caller and the upstream until either side closes.
async fn pump(
    socket: WebSocket,
    upstream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) {
    let (mut caller_out, mut caller_in) = socket.split();
    let (mut upstream_out, mut upstream_in) = upstream.split();

    let to_caller = async {
        while let Some(frame) = upstream_in.next().await {
            match frame {
                Ok(message) => {
                    if let Some(converted) = to_axum(message)
                        && caller_out.send(converted).await.is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = caller_out.close().await;
    };

    let to_upstream = async {
        while let Some(Ok(message)) = caller_in.next().await {
            let Some(converted) = to_tungstenite(message) else {
                continue;
            };
            if upstream_out.send(converted).await.is_err() {
                break;
            }
        }
        let _ = upstream_out.close().await;
    };

    let _ = futures_util::future::select(Box::pin(to_caller), Box::pin(to_upstream)).await;
}

/// Convert an upstream frame into an axum frame, dropping the raw-frame kind.
fn to_axum(message: WsMessage) -> Option<AxumMessage> {
    match message {
        WsMessage::Text(text) => Some(AxumMessage::Text(text.as_str().into())),
        WsMessage::Binary(bytes) => Some(AxumMessage::Binary(bytes)),
        WsMessage::Ping(bytes) => Some(AxumMessage::Ping(bytes)),
        WsMessage::Pong(bytes) => Some(AxumMessage::Pong(bytes)),
        WsMessage::Close(frame) => Some(AxumMessage::Close(frame.map(|frame| {
            axum::extract::ws::CloseFrame {
                code: frame.code.into(),
                reason: frame.reason.as_str().into(),
            }
        }))),
        WsMessage::Frame(_) => None,
    }
}

/// Convert an axum frame into an upstream frame, dropping the raw-frame kind.
fn to_tungstenite(message: AxumMessage) -> Option<WsMessage> {
    match message {
        AxumMessage::Text(text) => Some(WsMessage::Text(text.as_str().into())),
        AxumMessage::Binary(bytes) => Some(WsMessage::Binary(bytes)),
        AxumMessage::Ping(bytes) => Some(WsMessage::Ping(bytes)),
        AxumMessage::Pong(bytes) => Some(WsMessage::Pong(bytes)),
        AxumMessage::Close(frame) => Some(WsMessage::Close(frame.map(|frame| WsCloseFrame {
            code: frame.code.into(),
            reason: frame.reason.as_str().into(),
        }))),
    }
}

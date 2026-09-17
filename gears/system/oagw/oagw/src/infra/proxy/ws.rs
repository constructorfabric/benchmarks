//! WebSocket upgrade bridging (DESIGN.md "Proxying WebSocket upgrades").
//!
//! The client leg is taken with `hyper::upgrade::on` as soon as the inbound
//! request is parsed, so the hand-off survives the request body being read.
//! The upstream leg is taken from the response the gateway dialled. Both legs
//! are awaited inside a detached task — hyper only completes them once the
//! 101 has been written — and the two duplex streams are then pumped into each
//! other, so an upgrade is end-to-end and never buffered.

use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use hyper::upgrade::OnUpgrade;
use hyper_util::rt::TokioIo;

use crate::domain::error::DomainError;
use crate::domain::model::ResponseHeaderRules;
use crate::infra::proxy::http::{OutboundClient, response_headers};

/// Whether the inbound request asks for an HTTP upgrade.
#[must_use]
pub fn is_upgrade(method: &Method, headers: &HeaderMap) -> bool {
    let upgrade = header_contains(headers, axum::http::header::UPGRADE, "websocket");
    let connection = header_contains(headers, axum::http::header::CONNECTION, "upgrade");
    let key = headers.contains_key(axum::http::header::SEC_WEBSOCKET_KEY);
    *method == Method::GET && upgrade && connection && key
}

/// Whether `name` carries `token`, case-insensitively.
fn header_contains(headers: &HeaderMap, name: axum::http::HeaderName, token: &str) -> bool {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case(token))
        })
}

/// Take the client half of an upgrade out of the inbound request.
///
/// Called before the request is decomposed: `hyper` hands the on-upgrade
/// future out of the request extensions and never puts it back. A request
/// that carries no hand-off — every non-upgrade — leaves `None`.
#[must_use]
pub fn take_upgrade(request: &mut Request<Body>) -> Option<OnUpgrade> {
    request.extensions().get::<OnUpgrade>()?;
    Some(hyper::upgrade::on(request))
}

/// Bridge an upgrade end-to-end.
///
/// The upstream 101 is returned to the caller verbatim; the two byte streams
/// are pumped until either side closes.
///
/// # Errors
///
/// Returns `cf.oagw.protocol.error.v1` when the upstream refuses the upgrade.
/// Bridging itself runs detached: once the 101 is written the gateway no
/// longer mediates the conversation.
pub async fn bridge(
    client: &OutboundClient,
    upstream: Request<Body>,
    client_upgrade: Option<OnUpgrade>,
    timeout: Duration,
) -> Result<axum::response::Response, DomainError> {
    let mut response = client.send(upstream, timeout).await?;
    if response.status() != StatusCode::SWITCHING_PROTOCOLS {
        return Err(DomainError::protocol_error(format!(
            "upstream refused the WebSocket upgrade with status {}",
            response.status().as_u16()
        )));
    }
    let headers = response_headers(response.headers(), &ResponseHeaderRules::default());
    let upstream_upgrade = hyper::upgrade::on(&mut response);
    let mut builder = axum::response::Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    for (name, value) in &headers {
        builder = builder.header(name, value);
    }
    let rendered = builder
        .body(Body::empty())
        .map_err(|error| DomainError::protocol_error(format!("upgrade response: {error}")))?;
    tokio::spawn(relay(upstream_upgrade, client_upgrade));
    Ok(rendered)
}

/// Wait for both upgrade legs and pump them into each other.
///
/// A leg that never arrives simply ends the bridge: the 101 has already been
/// written, so there is no status left to report.
async fn relay(upstream: OnUpgrade, client: Option<OnUpgrade>) {
    let Some(client) = client else {
        return;
    };
    let upstream = match upstream.await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::debug!(error = %error, "upstream upgrade leg failed");
            return;
        }
    };
    let client = match client.await {
        Ok(stream) => stream,
        Err(error) => {
            tracing::debug!(error = %error, "client upgrade leg failed");
            return;
        }
    };
    drop(
        tokio::io::copy_bidirectional(&mut TokioIo::new(upstream), &mut TokioIo::new(client)).await,
    );
}

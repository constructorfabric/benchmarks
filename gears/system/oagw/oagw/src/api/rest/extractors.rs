//! Extractors shared by the OAGW handlers.
//!
//! [`JsonBody`] replaces axum's `Json`: axum maps *deserialisation* failures to
//! `422`, while DESIGN section 3.3 requires `400` for a malformed body and for
//! an unknown field, so every failure is re-rendered as an
//! [`OagwError::Validation`] problem document.
//!
//! [`UpstreamList`], [`RouteList`] and [`PluginList`] parse the OData query
//! parameters of the matching list endpoint (see [`crate::domain::odata`]); an
//! unknown field or a malformed expression is a `400` before the handler runs.

use axum::body::HttpBody;
use axum::extract::FromRequest;
use axum::extract::FromRequestParts;
use axum::http::Request;
use axum::http::header::CONTENT_LENGTH;
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::domain::error::OagwError;
use crate::domain::odata::{FieldCatalog, ListQuery, PLUGIN_FIELDS, ROUTE_FIELDS, UPSTREAM_FIELDS};

/// Largest accepted management-API request body (1 MiB).
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Strict JSON body: `400` for invalid syntax, unknown fields and wrong types.
#[derive(Debug, Clone)]
pub struct JsonBody<T>(pub T);

impl<S, T> FromRequest<S> for JsonBody<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = OagwError;

    async fn from_request(
        request: Request<axum::body::Body>,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let bytes = read_body(request).await?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|error| OagwError::validation(format!("malformed JSON body: {error}")))?;
        let parsed = T::deserialize(value)
            .map_err(|error| OagwError::validation(format!("invalid request body: {error}")))?;
        Ok(Self(parsed))
    }
}

/// Reads a request body, rejecting an oversized one with `413`.
///
/// The body is collected frame by frame instead of through
/// `axum::body::to_bytes`: a `chunked` body carries no `content-length`, so the
/// declared-length check below never fires for it, and axum's own limit error
/// is an opaque `axum_core::Error` that cannot be told apart from a transport
/// failure without the `http-body-util` type it wraps. Counting the bytes here
/// keeps both spellings of an oversized body on the same `413` path — DESIGN
/// section 3.3 — and leaves a genuine read failure as a `400`.
///
/// # Errors
///
/// Returns [`OagwError::PayloadTooLarge`] when the body (declared or streamed)
/// exceeds the cap and [`OagwError::Validation`] when the body cannot be read.
async fn read_body(request: Request<axum::body::Body>) -> Result<bytes::Bytes, OagwError> {
    let declared = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok());
    if let Some(length) = declared.filter(|length| *length > MAX_BODY_BYTES) {
        return Err(OagwError::payload_too_large(format!(
            "request body of {length} bytes exceeds the {MAX_BODY_BYTES} byte limit"
        )));
    }

    let mut body = request.into_body();
    let mut buffer = bytes::BytesMut::new();
    loop {
        let frame =
            match futures_util::future::poll_fn(|cx| std::pin::Pin::new(&mut body).poll_frame(cx))
                .await
            {
                Some(Ok(frame)) => frame,
                Some(Err(error)) => {
                    return Err(OagwError::validation(format!(
                        "request body could not be read: {error}"
                    )));
                }
                None => break,
            };
        let Ok(data) = frame.into_data() else {
            continue; // a trailer, which a JSON body never carries
        };
        if buffer.len() + data.len() > MAX_BODY_BYTES {
            return Err(OagwError::payload_too_large(format!(
                "request body exceeds the {MAX_BODY_BYTES} byte limit"
            )));
        }
        buffer.extend_from_slice(&data);
    }
    Ok(buffer.freeze())
}

/// Parsed OData query of a list request.
#[derive(Debug, Clone)]
pub struct UpstreamList(pub ListQuery);

/// Parsed OData query of the route list request.
#[derive(Debug, Clone)]
pub struct RouteList(pub ListQuery);

/// Parsed OData query of the plugin list request.
#[derive(Debug, Clone)]
pub struct PluginList(pub ListQuery);

impl<S> FromRequestParts<S> for UpstreamList
where
    S: Send + Sync,
{
    type Rejection = OagwError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parse_list(parts, &UPSTREAM_FIELDS).map(Self)
    }
}

impl<S> FromRequestParts<S> for RouteList
where
    S: Send + Sync,
{
    type Rejection = OagwError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parse_list(parts, &ROUTE_FIELDS).map(Self)
    }
}

impl<S> FromRequestParts<S> for PluginList
where
    S: Send + Sync,
{
    type Rejection = OagwError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parse_list(parts, &PLUGIN_FIELDS).map(Self)
    }
}

/// Parses the query string of `parts` against `catalog`.
///
/// # Errors
///
/// Returns the [`OagwError::Validation`] reported by
/// [`ListQuery::parse`].
fn parse_list(parts: &Parts, catalog: &'static FieldCatalog) -> Result<ListQuery, OagwError> {
    ListQuery::parse(parts.uri.query(), catalog)
}

#[cfg(test)]
#[path = "extractors_tests.rs"]
mod tests;

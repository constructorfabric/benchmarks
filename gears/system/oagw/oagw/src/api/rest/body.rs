//! Body extraction of the data plane.
//!
//! The proxy accepts any content type, so the body is read as raw bytes. The
//! stock [`axum::extract::Bytes`] extractor answers an oversized request with
//! a bare `413` and no body; [`ProxyBody`] turns that rejection into the
//! gear's own problem body (ADR-0007) so a too-large payload is reported like
//! every other gateway failure.

use axum::extract::FromRequest;
use axum::extract::FromRequestParts;
use axum::extract::Request;
use axum::extract::rejection::BytesRejection;
use axum::http::request::Parts;
use bytes::Bytes;
use serde_json::json;

use crate::api::rest::error::OagwError;
use crate::domain::error::{DomainError, ErrorKind};

/// The body limit the data plane routes are composed with, reported in a 413
/// problem body.
#[derive(Debug, Clone, Copy)]
pub struct MaxBodyBytes(pub u64);

/// The pending protocol upgrade of the inbound connection, if the caller
/// asked for one.
///
/// hyper plants the `OnUpgrade` handle in the request extensions of a
/// `Connection: Upgrade` request; taking it out here is what lets the data
/// plane splice the caller's socket onto the upstream's later on. Without a
/// real HTTP server in front of the handler (unit tests, `oneshot`) there is
/// no handle, and the request degrades to a plain proxy call.
#[derive(Debug, Default)]
pub struct ProxyUpgrade(pub Option<hyper::upgrade::OnUpgrade>);

impl<S> FromRequestParts<S> for ProxyUpgrade
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.extensions.remove::<hyper::upgrade::OnUpgrade>()))
    }
}

/// The raw request body, limited and rendered as a problem on overflow.
#[derive(Debug, Clone)]
pub struct ProxyBody(pub Bytes);

impl<S> FromRequest<S> for ProxyBody
where
    S: Send + Sync,
{
    type Rejection = OagwError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let mut req = req;
        let limit = req.extensions().get::<MaxBodyBytes>().map(|limit| limit.0);
        if let Some(limit) = limit {
            // `DefaultBodyLimit` only carries its kind as an extension, so it
            // is applied here rather than layered on the route.
            axum::extract::DefaultBodyLimit::max(usize::try_from(limit).unwrap_or(usize::MAX))
                .apply(&mut req);
        }
        Bytes::from_request(req, state)
            .await
            .map(Self)
            .map_err(|rejection| OagwError::new(too_large(&rejection, limit)))
    }
}

/// The problem an oversized body produces.
fn too_large(rejection: &BytesRejection, limit: Option<u64>) -> DomainError {
    let limit = limit.unwrap_or(crate::config::DEFAULT_MAX_BODY_BYTES);
    DomainError::new(
        ErrorKind::PayloadTooLarge,
        format!(
            "request body exceeds the {limit} byte limit of the gateway ({})",
            rejection.body_text()
        ),
    )
    .with_field("limit", json!(limit))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
    use super::*;
    use axum::http::Request as HttpRequest;

    #[tokio::test]
    async fn an_upgrade_is_only_taken_when_the_server_planted_one() {
        let request = HttpRequest::builder()
            .body(axum::body::Body::empty())
            .expect("request");
        let (mut parts, _) = request.into_parts();
        assert!(
            ProxyUpgrade::from_request_parts(&mut parts, &())
                .await
                .unwrap()
                .0
                .is_none()
        );
    }

    fn request_with(bytes: &'static [u8]) -> HttpRequest<axum::body::Body> {
        let mut request = HttpRequest::builder()
            .extension(MaxBodyBytes(16))
            .body(axum::body::Body::from(bytes))
            .expect("request");
        axum::extract::DefaultBodyLimit::max(16).apply(&mut request);
        request
    }

    #[tokio::test]
    async fn an_oversized_body_is_a_problem_not_a_bare_rejection() {
        let Err(error) = ProxyBody::from_request(request_with(b"0123456789abcdefghij"), &()).await
        else {
            panic!("a body over the limit must be rejected");
        };
        assert_eq!(error.0.kind, ErrorKind::PayloadTooLarge);
        assert_eq!(error.0.kind.status(), 413);
        assert_eq!(error.0.field("limit"), Some(&json!(16)));
    }

    #[tokio::test]
    async fn a_body_within_the_limit_is_accepted() {
        let body = ProxyBody::from_request(request_with(b"payload"), &())
            .await
            .expect("body");
        assert_eq!(body.0, Bytes::from_static(b"payload"));
    }
}

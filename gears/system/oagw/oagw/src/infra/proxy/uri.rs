//! Outbound URI construction for the proxy data plane.

use crate::domain::error::DomainError;
use crate::domain::model::{Endpoint, EndpointScheme};

/// Build the absolute URI of an upstream call.
///
/// # Errors
/// Returns [`DomainError::ProtocolError`] when the endpoint cannot be turned
/// into a URI.
pub fn build_uri(
    endpoint: &Endpoint,
    path: &str,
    query: Option<&str>,
) -> Result<http::Uri, DomainError> {
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    let authority = format!("{}:{}", endpoint.host, endpoint.port);
    let mut raw = format!("{}://{authority}{path}", scheme_prefix(endpoint.scheme));
    if let Some(query) = query
        && !query.is_empty()
    {
        raw.push('?');
        raw.push_str(query);
    }
    raw.parse()
        .map_err(|_| DomainError::ProtocolError("failed to build the upstream URI".to_owned()))
}

/// The URI scheme of an endpoint.
#[must_use]
pub const fn scheme_prefix(scheme: EndpointScheme) -> &'static str {
    match scheme {
        EndpointScheme::Http => "http",
        EndpointScheme::Https => "https",
        EndpointScheme::Wss => "wss",
        EndpointScheme::Wt => "wt",
        EndpointScheme::Grpc => "grpc",
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn endpoint(scheme: EndpointScheme, host: &str, port: u16) -> Endpoint {
        Endpoint {
            scheme,
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn https_endpoints_use_the_https_scheme() {
        let uri = build_uri(
            &endpoint(EndpointScheme::Https, "api.openai.com", 443),
            "/v1/models",
            None,
        )
        .unwrap();
        assert_eq!(uri.to_string(), "https://api.openai.com:443/v1/models");
    }

    #[test]
    fn plaintext_endpoints_use_http() {
        let uri = build_uri(
            &endpoint(EndpointScheme::Http, "127.0.0.1", 8080),
            "/v1/x",
            Some("a=1"),
        )
        .unwrap();
        assert_eq!(uri.to_string(), "http://127.0.0.1:8080/v1/x?a=1");
    }

    #[test]
    fn empty_queries_are_dropped() {
        let uri = build_uri(
            &endpoint(EndpointScheme::Http, "127.0.0.1", 80),
            "/v1/x",
            Some(""),
        )
        .unwrap();
        assert_eq!(uri.to_string(), "http://127.0.0.1:80/v1/x");
    }
}

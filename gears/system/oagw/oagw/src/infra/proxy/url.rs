//! Upstream URL construction.

use crate::domain::dto::{Endpoint, Upstream};
use crate::domain::error::DomainError;

/// Builds the absolute upstream URL for a request.
///
/// # Errors
/// Returns [`DomainError::Validation`] when the endpoint host cannot form a
/// URL.
pub fn build(
    upstream: &Upstream,
    endpoint: &Endpoint,
    path: &str,
    query: &[(String, String)],
) -> Result<url::Url, DomainError> {
    let scheme = match endpoint.scheme {
        crate::domain::dto::Scheme::Http | crate::domain::dto::Scheme::Ws => "http",
        crate::domain::dto::Scheme::Https
        | crate::domain::dto::Scheme::Wss
        | crate::domain::dto::Scheme::Wt
        | crate::domain::dto::Scheme::Grpc => "https",
    };
    let mut url = url::Url::parse(&format!(
        "{scheme}://{}:{}{path}",
        endpoint.host, endpoint.port
    ))
    .map_err(|error| {
        DomainError::Validation(format!(
            "endpoint {}:{} cannot form a URL: {error}",
            endpoint.host, endpoint.port
        ))
    })?;
    let _ = upstream;
    if !query.is_empty() {
        let pairs: Vec<(String, String)> = query
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect();
        url.query_pairs_mut().extend_pairs(pairs);
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::dto::{Protocol, ServerConfig};

    fn upstream() -> Upstream {
        Upstream {
            id: uuid::Uuid::nil(),
            tenant_id: uuid::Uuid::nil(),
            enabled: true,
            alias: "api.openai.com".into(),
            tags: vec![],
            server: ServerConfig {
                endpoints: vec![crate::domain::dto::Endpoint {
                    scheme: crate::domain::dto::Scheme::Http,
                    host: "127.0.0.1".into(),
                    port: 8080,
                }],
            },
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn builds_a_url_with_query() {
        let endpoint = crate::domain::dto::Endpoint {
            scheme: crate::domain::dto::Scheme::Http,
            host: "127.0.0.1".into(),
            port: 8080,
        };
        let url = build(
            &upstream(),
            &endpoint,
            "/v1/models",
            &[("a".to_owned(), "1".to_owned())],
        )
        .expect("builds");
        assert_eq!(url.as_str(), "http://127.0.0.1:8080/v1/models?a=1");
    }

    #[test]
    fn plaintext_schemes_dial_http() {
        for scheme in [crate::domain::dto::Scheme::Http, crate::domain::dto::Scheme::Ws] {
            let endpoint = crate::domain::dto::Endpoint {
                scheme,
                host: "localhost".into(),
                port: 9090,
            };
            let url =
                build(&upstream(), &endpoint, "/", &[]).expect("builds");
            assert!(url.as_str().starts_with("http://"));
        }
    }
}

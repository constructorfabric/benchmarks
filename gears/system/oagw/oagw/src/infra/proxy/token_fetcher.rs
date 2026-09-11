//! The production token fetcher: the `OAuth2` client-credentials round trip.
//!
//! It rides on the same outbound client the data plane proxies with, so the
//! `IdP` is reached through the same connector, timeout and plaintext policy as
//! any upstream. Only the token's *shape* is logged — never its value.

use crate::domain::error::OagwError;
use crate::domain::plugin::Credential;
use crate::domain::plugin::oauth2_client_cred::{ClientAuthMethod, FetchedToken, TokenFetcher};
use crate::infra::proxy::outbound::OutboundClient;
use async_trait::async_trait;
use base64::Engine as _;
use form_urlencoded::Serializer;
use futures_util::StreamExt;

/// Largest token response body accepted.
const MAX_TOKEN_BODY: usize = 1_000_000;

/// Fetches access tokens over HTTP.
#[derive(Clone)]
pub struct HttpTokenFetcher {
    outbound: OutboundClient,
}

impl HttpTokenFetcher {
    /// Builds a fetcher over the shared outbound client.
    #[must_use]
    pub fn new(outbound: OutboundClient) -> Self {
        Self { outbound }
    }
}

#[async_trait]
impl TokenFetcher for HttpTokenFetcher {
    async fn fetch(
        &self,
        endpoint: &str,
        method: ClientAuthMethod,
        client_id: &Credential,
        client_secret: &Credential,
        scopes: Option<&str>,
    ) -> Result<FetchedToken, OagwError> {
        let (scheme, host, port, path) = split_endpoint(endpoint)?;
        self.outbound.check_scheme(&scheme)?;
        let body = token_request_body(method, client_id, client_secret, scopes);
        let mut headers = vec![
            (
                "content-type".to_owned(),
                "application/x-www-form-urlencoded".to_owned(),
            ),
            ("accept".to_owned(), "application/json".to_owned()),
            ("content-length".to_owned(), body.len().to_string()),
        ];
        if method == ClientAuthMethod::Basic {
            headers.push((
                "authorization".to_owned(),
                basic_authorization(client_id, client_secret),
            ));
        }

        let request = crate::infra::proxy::outbound::build_request(
            "POST", &scheme, &host, port, &path, None, &headers, body,
        )?;
        let response = self.outbound.send(request).await?;
        if !(200..300).contains(&response.status) {
            return Err(OagwError::AuthenticationFailed(format!(
                "token endpoint answered {}",
                response.status
            )));
        }
        let payload = read_body(response.body).await?;
        let json: serde_json::Value = serde_json::from_slice(&payload).map_err(|error| {
            OagwError::ProtocolError(format!("token endpoint returned unusable JSON: {error}"))
        })?;
        let bearer = json
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                OagwError::ProtocolError("token endpoint returned no access_token".to_owned())
            })?
            .to_owned();
        let expires_in = json
            .get("expires_in")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(300);
        tracing::debug!(endpoint = %endpoint, expires_in, "fetched an OAuth2 access token");
        Ok(FetchedToken { bearer, expires_in })
    }
}

/// Reads a token response to the end; token bodies are small.
async fn read_body(
    mut body: crate::domain::services::proxy::BodyStream,
) -> Result<Vec<u8>, OagwError> {
    let mut payload = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk = chunk.map_err(|error| {
            OagwError::DownstreamError(format!("token response body failed: {error}"))
        })?;
        if payload.len() + chunk.len() > MAX_TOKEN_BODY {
            return Err(OagwError::ProtocolError(
                "token response body is unreasonably large".to_owned(),
            ));
        }
        payload.extend_from_slice(&chunk);
    }
    Ok(payload)
}

/// Splits a token endpoint URL into the parts the outbound client needs.
fn split_endpoint(endpoint: &str) -> Result<(String, String, u16, String), OagwError> {
    let parsed = url::Url::parse(endpoint).map_err(|_| {
        OagwError::ValidationError(format!("token endpoint '{endpoint}' is not a URL"))
    })?;
    let scheme = parsed.scheme().to_owned();
    let host = parsed
        .host_str()
        .ok_or_else(|| OagwError::ValidationError("token endpoint has no host".to_owned()))?
        .to_owned();
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| OagwError::ValidationError("token endpoint has no port".to_owned()))?;
    let path = if parsed.path().is_empty() {
        "/".to_owned()
    } else {
        parsed.path().to_owned()
    };
    Ok((scheme, host, port, path))
}

/// Encodes the token request body for the client authentication method.
fn token_request_body(
    method: ClientAuthMethod,
    client_id: &Credential,
    client_secret: &Credential,
    scopes: Option<&str>,
) -> bytes::Bytes {
    let mut serializer = Serializer::new(String::new());
    serializer.append_pair("grant_type", "client_credentials");
    if method == ClientAuthMethod::Form {
        serializer.append_pair("client_id", &String::from_utf8_lossy(client_id.expose()));
        serializer.append_pair(
            "client_secret",
            &String::from_utf8_lossy(client_secret.expose()),
        );
    }
    if let Some(scopes) = scopes.filter(|value| !value.is_empty()) {
        serializer.append_pair("scope", scopes);
    }
    bytes::Bytes::from(serializer.finish())
}

/// Builds the `Basic` authorization header value.
fn basic_authorization(client_id: &Credential, client_secret: &Credential) -> String {
    let raw = format!(
        "{}:{}",
        String::from_utf8_lossy(client_id.expose()),
        String::from_utf8_lossy(client_secret.expose())
    );
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    )
}

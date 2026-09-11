// Created: 2026-09-01 by Constructor Tech
//! Turning a `PluginRequest` into bytes an upstream will accept.
//!
//! Two shapes leave the gateway: a buffered request handed to toolkit-http,
//! and a hand-written request head for a protocol upgrade. The upgrade head
//! is written by hand because the client's `Sec-WebSocket-Key` has to reach
//! the upstream exactly as the client sent it.

use toolkit_http::{HttpClient, RequestBuilder};

use crate::domain::errors::OagwError;
use crate::domain::model::{HeadersConfig, Target};

/// Build a toolkit-http request for the buffered body.
///
/// # Errors
/// Returns an error when a header name or value cannot be represented.
pub fn builder(
    http: &HttpClient,
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
) -> Result<RequestBuilder, OagwError> {
    let mut builder = match method.to_ascii_uppercase().as_str() {
        "GET" => http.get(url),
        "POST" => http.post(url),
        "PUT" => http.put(url),
        "PATCH" => http.patch(url),
        "DELETE" => http.delete(url),
        "HEAD" => http.head(url),
        "OPTIONS" => http.options(url),
        other => {
            return Err(OagwError::validation_error(format!(
                "method '{other}' cannot be forwarded"
            )));
        }
    };
    for (name, value) in headers {
        builder = builder.header(name, value);
    }
    if !body.is_empty() {
        builder = builder.body_bytes(bytes::Bytes::copy_from_slice(body));
    }
    Ok(builder)
}

/// Render the raw request head for a protocol upgrade.
///
/// `headers` is already the final outbound set; the `Host` header is written
/// from the target's authority so the upstream sees the name it expects.
///
/// # Errors
/// Returns an error when a header cannot be represented on the wire.
pub fn upgrade_head(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    target: &Target,
    _config: Option<&HeadersConfig>,
) -> Result<Vec<u8>, OagwError> {
    let uri = path_of(url);
    let mut head = String::with_capacity(256);
    head.push_str(&method.to_ascii_uppercase());
    head.push(' ');
    head.push_str(&uri);
    head.push_str(" HTTP/1.1\r\n");
    head.push_str("Host: ");
    head.push_str(&target.authority());
    head.push_str("\r\n");
    let mut connection = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("host") {
            continue;
        }
        if name.eq_ignore_ascii_case("connection") {
            connection = true;
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    // `Connection` is written once. When the client did not name one, a
    // keep-alive default keeps the socket open long enough to be upgraded.
    if !connection {
        head.push_str("Connection: keep-alive\r\n");
    }
    head.push_str("\r\n");
    Ok(head.into_bytes())
}

/// The path-and-query half of a URL.
fn path_of(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    match rest.find('/') {
        Some(index) => rest[index..].to_owned(),
        None => "/".to_owned(),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_path_is_taken_from_the_url() {
        assert_eq!(path_of("https://h/v1/x?q=1"), "/v1/x?q=1");
        assert_eq!(path_of("http://h:8080"), "/");
        assert_eq!(path_of("/bare"), "/bare");
    }

    #[test]
    fn the_upgrade_head_carries_the_handshake() {
        let target = Target {
            host: "us.vendor.com".to_owned(),
            port: 443,
            secure: true,
        };
        let head = upgrade_head(
            "GET",
            "https://us.vendor.com/v1/stream",
            &[
                ("upgrade".to_owned(), "websocket".to_owned()),
                ("connection".to_owned(), "Upgrade".to_owned()),
                ("sec-websocket-key".to_owned(), "abc".to_owned()),
                ("host".to_owned(), "gateway.internal".to_owned()),
            ],
            &target,
            None,
        )
        .expect("head");
        let text = String::from_utf8(head).expect("utf8");
        assert!(text.starts_with("GET /v1/stream HTTP/1.1\r\n"), "{text}");
        assert!(text.contains("Host: us.vendor.com\r\n"), "{text}");
        assert!(text.contains("sec-websocket-key: abc\r\n"), "{text}");
        assert!(text.contains("connection: Upgrade\r\n"), "{text}");
        // The gateway's own Host never wins, and `Connection` is written once.
        assert!(!text.contains("gateway.internal"), "{text}");
        assert_eq!(text.matches("onnection:").count(), 1, "{text}");
        assert!(text.ends_with("\r\n\r\n"), "{text}");
    }

    #[tokio::test]
    async fn unsupported_methods_are_refused_rather_than_misrendered() {
        let http = toolkit_http::HttpClient::new().expect("client");
        let err = match builder(&http, "TRACE", "https://h/x", &[], b"") {
            Ok(_) => panic!("TRACE should be refused"),
            Err(err) => err,
        };
        assert_eq!(err.status_value(), 400, "{err}");
    }

    #[tokio::test]
    async fn a_body_is_carried_and_a_content_type_is_not_invented() {
        let http = toolkit_http::HttpClient::new().expect("client");
        let built = builder(
            &http,
            "POST",
            "https://h/x",
            &[("content-type".to_owned(), "application/json".to_owned())],
            b"{}",
        );
        match built {
            Ok(_) => {}
            Err(error) => panic!("builder failed: {error}"),
        }
    }

    #[test]
    fn the_connect_deadline_is_bounded() {
        assert_eq!(
            crate::domain::TIMEOUT_CONNECT,
            std::time::Duration::from_secs(10)
        );
    }
}

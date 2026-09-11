//! Real-socket helpers.
//!
//! [`tower::ServiceExt::oneshot`] buffers bodies and never carries connection
//! state, so it cannot observe incremental delivery or a protocol upgrade. The
//! tests that cover streaming therefore bind real loopback sockets: the
//! upstream is a raw TCP listener the test frames by hand, and the gear's
//! router is served by the same hyper stack the platform uses.

#![allow(dead_code)]

use std::sync::Arc;

use axum::Router;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

/// Binds an ephemeral loopback port for a hand-rolled upstream.
#[must_use]
pub async fn bind_upstream() -> (std::net::SocketAddr, TcpListener) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind upstream");
    let addr = listener.local_addr().expect("local addr");
    (addr, listener)
}

/// An ephemeral loopback port with nothing listening on it.
///
/// Tests that only need to prove a request *reached* the dial stage use this:
/// the connection is refused, which the data plane answers as
/// `503 link.unavailable` rather than hanging until its deadline.
#[must_use]
pub async fn free_port() -> u16 {
    let (addr, listener) = bind_upstream().await;
    drop(listener);
    addr.port()
}

/// Serves the router over loopback HTTP/1.1 with upgrade support.
#[must_use]
pub async fn serve(router: Router) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind router");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(serve_connection(stream, Arc::new(router.clone())));
        }
    });
    addr
}

async fn serve_connection(
    stream: TcpStream,
    router: Arc<Router>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    hyper::server::conn::http1::Builder::new()
        .serve_connection(
            TokioIo::new(stream),
            hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                let router = Arc::clone(&router);
                async move {
                    let response = Router::clone(&router)
                        .oneshot(request.map(axum::body::Body::new))
                        .await?;
                    Ok::<_, std::convert::Infallible>(response)
                }
            }),
        )
        .with_upgrades()
        .await
        .map_err(std::convert::Into::into)
}

/// Reads one HTTP/1.1 request head and its body from an upstream connection.
///
/// Returns the head verbatim (for header assertions) and the body bytes.
pub async fn read_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
    let mut buffer = Vec::new();
    let head_end = loop {
        let mut chunk = [0_u8; 4096];
        let read = stream.read(&mut chunk).await.expect("read request");
        assert!(read > 0, "upstream closed before a request arrived");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(index) = find_head_end(&buffer) {
            break index;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let body = buffer[head_end + 4..].to_vec();
    (head, body)
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Writes a complete HTTP/1.1 response with an explicit `Content-Length`.
pub async fn write_response(
    stream: &mut TcpStream,
    status_line: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) {
    let mut head = format!("{status_line}\r\n");
    for (name, value) in headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str(&format!("content-length: {}\r\n", body.len()));
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).await.expect("write head");
    stream.write_all(body).await.expect("write body");
    stream.flush().await.expect("flush");
}

/// Writes a raw chunk of an already-framed response, without a final length.
pub async fn write_raw(stream: &mut TcpStream, bytes: &[u8]) {
    stream.write_all(bytes).await.expect("write raw");
    stream.flush().await.expect("flush");
}

/// Asserts the head contains the given header, case-insensitively, and returns
/// its value.
#[must_use]
pub fn header_of(head: &str, name: &str) -> Option<String> {
    for line in head.lines().skip(1) {
        let Some((field, value)) = line.split_once(':') else {
            continue;
        };
        if field.trim().eq_ignore_ascii_case(name) {
            return Some(value.trim().to_owned());
        }
    }
    None
}

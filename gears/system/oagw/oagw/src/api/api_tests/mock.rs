#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The upstream side of the proxy tests: an in-process `axum` app on a random port.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::Response;
use bytes::Bytes;

/// What the mock saw of one request.
#[derive(Debug, Clone)]
pub struct Recorded {
    /// Method of the inbound call.
    pub method: String,
    /// Path of the inbound call.
    pub path: String,
    /// Query string of the inbound call.
    pub query: String,
    /// Headers of the inbound call, lowercased names.
    pub headers: Vec<(String, String)>,
    /// Buffered body of the inbound call.
    #[allow(dead_code)]
    pub body: String,
}

impl Recorded {
    /// Number of values the named header carried, duplicates included.
    pub fn header_count(&self, name: &str) -> usize {
        self.headers.iter().filter(|(n, _)| n == name).count()
    }

    /// First value of the named header.
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    }
}

/// Shared log of the requests the mock received.
#[derive(Clone, Default)]
pub struct Recorder(Arc<std::sync::Mutex<Vec<Recorded>>>);

impl Recorder {
    /// Everything received so far, oldest first.
    pub fn snapshot(&self) -> Vec<Recorded> {
        self.0.lock().unwrap().clone()
    }

    /// Number of requests received so far.
    pub fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    /// True when nothing has been received.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A running mock upstream.
pub struct Mock {
    recorder: Recorder,
    address: SocketAddr,
}

impl Mock {
    /// Everything the mock has received so far.
    pub fn snapshot(&self) -> Vec<Recorded> {
        self.recorder.snapshot()
    }

    /// Number of requests the mock received.
    pub fn calls(&self) -> usize {
        self.recorder.len()
    }

    /// True when the mock received nothing.
    pub fn is_empty(&self) -> bool {
        self.recorder.is_empty()
    }

    /// `address` the mock answers on.
    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

async fn record(recorder: Recorder, request: Request, next: axum::middleware::Next) -> Response {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap_or_default();
    recorder.0.lock().unwrap().push(Recorded {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        query: parts.uri.query().unwrap_or_default().to_string(),
        headers: parts
            .headers
            .iter()
            .map(|(n, v)| {
                (
                    n.as_str().to_ascii_lowercase(),
                    v.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect(),
        body: String::from_utf8_lossy(&bytes).to_string(),
    });
    next.run(Request::from_parts(parts, Body::from(bytes))).await
}

/// The buffered upstream used by most tests.
pub fn app(recorder: Recorder) -> Router {
    use axum::routing::{get, post};
    Router::new()
        .route("/v1", get(|| async { "upstream-ok" }))
        .route("/v1/charges", get(|| async { "charges-ok" }))
        .route("/v1/echo", post(|body: String| async move { body }))
        .route("/sse", get(sse))
        .layer(axum::middleware::from_fn(move |request: Request, next| {
            let recorder = recorder.clone();
            async move { record(recorder, request, next).await }
        }))
}

/// The app with a WebSocket echo endpoint.
pub fn ws_app(recorder: Recorder) -> Router {
    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use axum::routing::get;
    Router::new()
        .route(
            "/ws",
            get(|upgrade: WebSocketUpgrade| async move {
                upgrade.on_upgrade(|mut socket: WebSocket| async move {
                    while let Some(Ok(message)) = socket.recv().await {
                        match message {
                            Message::Text(text) => {
                                let reply: axum::extract::ws::Utf8Bytes =
                                    format!("echo:{}", text.as_str()).into();
                                if socket.send(Message::Text(reply)).await.is_err() {
                                    break;
                                }
                            }
                            Message::Close(_) => break,
                            _ => continue,
                        }
                    }
                })
            }),
        )
        .layer(axum::middleware::from_fn(move |request: Request, next| {
            let recorder = recorder.clone();
            async move { record(recorder, request, next).await }
        }))
}

/// An event stream that emits its first event immediately and its second one later.
async fn sse() -> Response {
    let stream = async_stream::stream! {
        yield Ok::<_, std::convert::Infallible>(Bytes::from_static(b"event: start\ndata: one\n\n"));
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        yield Ok(Bytes::from_static(b"data: two\n\n"));
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// Spawn the app built by `build` on a random local port.
pub async fn spawn(build: fn(Recorder) -> Router) -> Mock {
    let recorder = Recorder::default();
    let app = build(recorder.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Mock { recorder, address }
}

/// Read a response body back as text, for assertion messages.
pub async fn body_of(response: Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).to_string()
}

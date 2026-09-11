//! A hand-rolled HTTP upstream the proxy tests dial.
//!
//! The gateway opens one connection per request, so a tiny
//! accept-respond-close loop is enough — and unlike a canned mock library it
//! can stream chunks with real flushes in between, which the SSE test needs.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A request the mock upstream received.
#[derive(Debug, Clone)]
pub struct Captured {
    /// Request method.
    pub method: String,
    /// Raw request target, exactly as the gateway sent it.
    pub target: String,
    /// Request headers, in order, lower-cased names.
    pub headers: Vec<(String, String)>,
    /// Request body.
    pub body: Vec<u8>,
}

impl Captured {
    /// The path portion of the request target.
    #[must_use]
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or(&self.target)
    }

    /// The query portion of the request target, without the `?`.
    #[must_use]
    pub fn query(&self) -> &str {
        self.target.split_once('?').map(|(_, q)| q).unwrap_or("")
    }

    /// The body as UTF-8, for assertions.
    #[must_use]
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The last value of a header, lower-cased name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

/// The body a mock upstream sends back.
pub enum MockBody {
    /// A fixed body sent with a `Content-Length`.
    Bytes(&'static str),
    /// Sequential chunks sent with chunked framing and flushed between them.
    Chunks(Vec<Vec<u8>>),
}

/// A canned response the mock upstream sends.
pub struct MockResponse {
    /// Status line code.
    pub status: u16,
    /// Response headers, lower-cased names.
    pub headers: Vec<(&'static str, &'static str)>,
    /// The body.
    pub body: MockBody,
}

impl MockResponse {
    /// A `200 OK` JSON response.
    #[must_use]
    pub fn json(body: &'static str) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type", "application/json")],
            body: MockBody::Bytes(body),
        }
    }

    /// A `200 OK` response with an explicit content type.
    #[must_use]
    pub fn ok(content_type: &'static str, body: &'static str) -> Self {
        Self {
            status: 200,
            headers: vec![("content-type", content_type_label(content_type))],
            body: MockBody::Bytes(body),
        }
    }
}

/// The content type for a canned response.
fn content_type_label(kind: &str) -> &'static str {
    match kind {
        "text" => "text/plain",
        "sse" => "text/event-stream",
        _ => "application/json",
    }
}

/// A running mock upstream bound to an ephemeral port.
pub struct MockUpstream {
    port: u16,
    captured: Arc<Mutex<Vec<Captured>>>,
}

impl MockUpstream {
    /// Binds an ephemeral port and serves `respond` to every request.
    ///
    /// # Panics
    ///
    /// Panics when the loopback listener cannot be bound.
    pub async fn start<F>(respond: F) -> Self
    where
        F: Fn(&Captured) -> MockResponse + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let port = listener.local_addr().expect("addr").port();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let responder = Arc::new(respond);

        let sink = Arc::clone(&captured);
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let responder = Arc::clone(&responder);
                let sink = Arc::clone(&sink);
                tokio::spawn(async move {
                    if let Some((socket, request)) = read_request(socket).await {
                        sink.lock().expect("captured lock").push(request.clone());
                        write_response(socket, &responder(&request)).await;
                    }
                });
            }
        });

        Self { port, captured }
    }

    /// The port to point an upstream endpoint at.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The requests received so far.
    #[must_use]
    pub fn captured(&self) -> Vec<Captured> {
        self.captured.lock().expect("captured lock").clone()
    }

    /// The single request received so far.
    ///
    /// # Panics
    ///
    /// Panics when no request arrived.
    #[must_use]
    pub fn last(&self) -> Captured {
        self.captured
            .lock()
            .expect("captured lock")
            .last()
            .cloned()
            .expect("the gateway sent at least one request")
    }
}

/// An upstream that always answers `200 OK` with a JSON echo of the request.
///
/// # Panics
///
/// Panics when the listener cannot be bound.
pub async fn echo_upstream() -> MockUpstream {
    MockUpstream::start(|captured: &Captured| MockResponse {
        status: 200,
        headers: vec![("content-type", "application/json")],
        body: MockBody::Bytes(Box::leak(
            serde_json::to_string(&serde_json::json!({
                "method": captured.method,
                "path": captured.path(),
                "query": captured.query(),
                "host": captured.header("host"),
                "body": captured.body_text(),
                "content_type": captured.header("content-type"),
                "authorization": captured.header("authorization"),
            }))
            .expect("echo serialises")
            .into_boxed_str(),
        )),
    })
    .await
}

/// Reads one HTTP request head plus its `Content-Length` body.
async fn read_request(socket: TcpStream) -> Option<(TcpStream, Captured)> {
    let mut socket = socket;
    let mut buffer = Vec::with_capacity(2048);
    let mut chunk = [0_u8; 1024];
    let head_end = loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = find_head_end(&buffer) {
            break end;
        }
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let (method, target, _version) = request_line
        .split_once(' ')
        .and_then(|(m, rest)| rest.split_once(' ').map(|(t, v)| (m, t, v)))?;

    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }

    let length = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);

    Some((
        socket,
        Captured {
            method: method.to_owned(),
            target: target.to_owned(),
            headers,
            body,
        },
    ))
}

/// The index of the `\r\n\r\n` separating head from body.
fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Writes a canned response and closes the connection.
async fn write_response(mut socket: TcpStream, response: &MockResponse) {
    let reason = reason_phrase(response.status);
    let mut head = format!("HTTP/1.1 {} {reason}\r\n", response.status);
    let mut chunked = false;
    for (name, value) in &response.headers {
        if *name == "transfer-encoding" {
            chunked = true;
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("connection: close\r\n");

    match &response.body {
        MockBody::Bytes(bytes) => {
            if !chunked {
                head.push_str(&format!("content-length: {}\r\n", bytes.len()));
            }
            head.push_str("\r\n");
            if socket.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            if chunked {
                let _ = write_chunk(&mut socket, bytes.as_bytes()).await;
                let _ = socket.write_all(b"0\r\n\r\n").await;
            } else {
                let _ = socket.write_all(bytes.as_bytes()).await;
            }
        }
        MockBody::Chunks(chunks) => {
            head.push_str("transfer-encoding: chunked\r\n\r\n");
            if socket.write_all(head.as_bytes()).await.is_err() {
                return;
            }
            for chunk in chunks {
                if write_chunk(&mut socket, chunk).await.is_err() {
                    return;
                }
                let _ = socket.flush().await;
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
        }
    }
    let _ = socket.shutdown().await;
}

/// Writes one chunked-encoding data chunk.
async fn write_chunk(socket: &mut TcpStream, bytes: &[u8]) -> Result<(), std::io::Error> {
    socket
        .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
        .await?;
    socket.write_all(bytes).await?;
    socket.write_all(b"\r\n").await
}

/// The reason phrase for the statuses the mock upstream answers with.
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

/// A WebSocket echo upstream; returns the port to point an endpoint at.
///
/// # Panics
///
/// Panics when the listener cannot be bound.
pub async fn ws_echo_upstream() -> u16 {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite as ts;

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut ws = match tokio_tungstenite::accept_async(socket).await {
                    Ok(ws) => ws,
                    Err(_) => return,
                };
                while let Some(Ok(message)) = ws.next().await {
                    match message {
                        ts::Message::Text(text) => {
                            if ws
                                .send(ts::Message::Text(ts::Utf8Bytes::from(text.as_str())))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                        ts::Message::Close(_) => return,
                        other => {
                            if ws.send(other).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    port
}

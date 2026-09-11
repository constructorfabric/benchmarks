//! Shared harness for the OAGW acceptance tests.
//!
//! Spins the gear's own router up in-process and, where a test needs a real
//! upstream, a purpose-built one on a loopback port.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use oagw::config::OagwConfig;
use oagw::gear::OagwState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use toolkit::api::OpenApiRegistryImpl;
use tower::ServiceExt;

/// A router plus the state behind it.
pub struct Harness {
    pub router: Router,
}

impl Harness {
    /// A harness whose configuration mirrors the graded one.
    pub fn graded() -> Self {
        Self::with_config(OagwConfig {
            proxy_timeout_secs: 2,
            allow_http_upstream: true,
            ssrf_policy: oagw::config::SsrfPolicy { enabled: false },
            ..OagwConfig::default()
        })
    }

    /// A harness with plaintext upstream connections refused.
    pub fn plaintext_refused() -> Self {
        Self::with_config(OagwConfig {
            proxy_timeout_secs: 2,
            allow_http_upstream: false,
            ..OagwConfig::default()
        })
    }

    pub fn with_config(config: OagwConfig) -> Self {
        let openapi = OpenApiRegistryImpl::new();
        let state = Arc::new(OagwState::new(config));
        Self {
            router: oagw::api::rest::register_routes(Router::new(), &openapi, state),
        }
    }

    /// Issue a JSON request and decode the response.
    pub async fn json(
        &self,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let (status, _headers, bytes) = self.raw(method, uri, body, &[]).await;
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        };
        (status, json)
    }

    /// Issue a request and return status, headers and raw bytes.
    pub async fn raw(
        &self,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let mut req = Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let req = match body {
            Some(b) => req
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&b).unwrap()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let resp = self.router.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let hdrs = resp.headers().clone();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, hdrs, bytes.to_vec())
    }

    /// Register an upstream pointing at `port` and a catch-all route, and
    /// return the alias.
    pub async fn wire_upstream(&self, alias: &str, scheme: &str, port: u16) -> String {
        let (s, up) = self
            .json(
                "POST",
                "/oagw/v1/upstreams",
                Some(serde_json::json!({
                    "alias": alias,
                    "server": {"endpoints": [{"scheme": scheme, "host": "127.0.0.1", "port": port}]},
                    "protocol": "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1"
                })),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "upstream create failed: {up}");
        let id = up["id"].as_str().unwrap().to_owned();
        let (s, r) = self
            .json(
                "POST",
                "/oagw/v1/routes",
                Some(serde_json::json!({
                    "upstream_id": id,
                    "match": {"http": {"methods": ["GET", "POST", "PUT", "DELETE", "PATCH"], "path": "/"}}
                })),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "route create failed: {r}");
        alias.to_owned()
    }
}

/// The value of a response header, if present.
pub fn header(h: &axum::http::HeaderMap, name: &str) -> Option<String> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// A minimal upstream that answers a fixed canned response per path.
pub struct FakeUpstream {
    pub port: u16,
}

impl FakeUpstream {
    /// Start an upstream that answers:
    /// * `/boom` with a 500 and a JSON body,
    /// * `/slow` after a delay longer than the proxy timeout,
    /// * `/sse` with an event stream,
    /// * `/noheader` with a 200 that omits `x-required`,
    /// * anything else with a 200 echoing the request line and headers.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(handle(sock));
            }
        });
        Self { port }
    }
}

async fn handle(mut sock: TcpStream) {
    let mut buf = vec![0_u8; 16 * 1024];
    let n = match sock.read(&mut buf).await {
        Ok(0) | Err(_) => return,
        Ok(n) => n,
    };
    let req = String::from_utf8_lossy(&buf[..n]).to_string();
    let first = req.lines().next().unwrap_or_default().to_owned();
    let path = first.split_whitespace().nth(1).unwrap_or("/").to_owned();

    if path.starts_with("/slow") {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }

    let out = if path.starts_with("/boom") {
        let body = br#"{"upstream":"error"}"#;
        format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect::<Vec<u8>>()
    } else if path.starts_with("/sse") {
        let head = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\n\r\n";
        let _ = sock.write_all(head).await;
        let _ = sock.flush().await;
        for i in 0..3 {
            // The third event lands after the 2 s proxy timeout, proving the
            // timeout bounds establishment and not the stream's lifetime.
            tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
            let ev = format!("data: ev{i}\n\n");
            let chunk = format!("{:x}\r\n{ev}\r\n", ev.len());
            if sock.write_all(chunk.as_bytes()).await.is_err() {
                return;
            }
            let _ = sock.flush().await;
        }
        let _ = sock.write_all(b"0\r\n\r\n").await;
        return;
    } else if path.starts_with("/noheader") {
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".to_vec()
    } else {
        // Echo what arrived so the test can assert on header transformation.
        let seen_hop = req.to_lowercase().contains("x-custom-hop");
        let seen_te = req.to_lowercase().contains("\r\nte:");
        let seen_target = req.to_lowercase().contains("x-oagw-target-host");
        let host = req
            .lines()
            .find(|l| l.to_lowercase().starts_with("host:"))
            .map(|l| l[5..].trim().to_owned())
            .unwrap_or_default();
        let body = serde_json::json!({
            "line": first,
            "path": path,
            "host": host,
            "saw_hop_by_hop_te": seen_te,
            "saw_custom_hop": seen_hop,
            "saw_target_host": seen_target,
        })
        .to_string();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Required: yes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    };
    let _ = sock.write_all(&out).await;
    let _ = sock.flush().await;
}

/// A minimal WebSocket echo upstream.
pub struct FakeWsUpstream {
    pub port: u16,
}

impl FakeWsUpstream {
    /// Start an upstream that completes the handshake, echoes text frames with
    /// an `ECHO:` prefix, and mirrors a close frame back.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(ws_handle(sock));
            }
        });
        Self { port }
    }
}

const WS_GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

fn ws_accept(key: &str) -> String {
    use sha1_of::sha1;
    let mut input = key.as_bytes().to_vec();
    input.extend_from_slice(WS_GUID);
    base64_encode(&sha1(&input))
}

async fn ws_handle(mut sock: TcpStream) {
    let mut buf = vec![0_u8; 8192];
    let n = match sock.read(&mut buf).await {
        Ok(0) | Err(_) => return,
        Ok(n) => n,
    };
    let req = String::from_utf8_lossy(&buf[..n]).to_string();
    let mut key = String::new();
    let mut proto = String::new();
    for line in req.lines() {
        let lower = line.to_lowercase();
        if let Some(v) = lower.strip_prefix("sec-websocket-key:") {
            key = line[line.len() - v.trim().len()..].trim().to_owned();
        }
        if let Some(v) = lower.strip_prefix("sec-websocket-protocol:") {
            proto = line[line.len() - v.trim().len()..]
                .split(',')
                .next()
                .unwrap_or_default()
                .trim()
                .to_owned();
        }
    }
    let mut resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n",
        ws_accept(&key)
    );
    if !proto.is_empty() {
        resp.push_str(&format!("Sec-WebSocket-Protocol: {proto}\r\n"));
    }
    resp.push_str("\r\n");
    if sock.write_all(resp.as_bytes()).await.is_err() {
        return;
    }

    loop {
        let mut h = [0_u8; 2];
        if sock.read_exact(&mut h).await.is_err() {
            return;
        }
        let opcode = h[0] & 0x0f;
        let masked = h[1] & 0x80 != 0;
        let mut len = usize::from(h[1] & 0x7f);
        if len == 126 {
            let mut e = [0_u8; 2];
            if sock.read_exact(&mut e).await.is_err() {
                return;
            }
            len = usize::from(u16::from_be_bytes(e));
        }
        let mut mask = [0_u8; 4];
        if masked && sock.read_exact(&mut mask).await.is_err() {
            return;
        }
        let mut payload = vec![0_u8; len];
        if sock.read_exact(&mut payload).await.is_err() {
            return;
        }
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
        }
        if opcode == 0x8 {
            // Mirror the close frame, close code and all.
            let mut out = vec![0x88, u8::try_from(payload.len()).unwrap_or(0)];
            out.extend_from_slice(&payload);
            let _ = sock.write_all(&out).await;
            return;
        }
        let mut echoed = b"ECHO:".to_vec();
        echoed.extend_from_slice(&payload);
        let mut out = vec![0x80 | opcode, u8::try_from(echoed.len()).unwrap_or(0)];
        out.extend_from_slice(&echoed);
        if sock.write_all(&out).await.is_err() {
            return;
        }
    }
}

fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if c.len() > 1 {
            T[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// A tiny SHA-1, so the harness needs no extra dependency.
mod sha1_of {
    pub fn sha1(msg: &[u8]) -> [u8; 20] {
        let mut h: [u32; 5] = [
            0x6745_2301,
            0xEFCD_AB89,
            0x98BA_DCFE,
            0x1032_5476,
            0xC3D2_E1F0,
        ];
        let ml = (msg.len() as u64) * 8;
        let mut data = msg.to_vec();
        data.push(0x80);
        while data.len() % 64 != 56 {
            data.push(0);
        }
        data.extend_from_slice(&ml.to_be_bytes());

        for block in data.chunks(64) {
            let mut w = [0_u32; 80];
            for (i, word) in block.chunks(4).enumerate() {
                w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
            }
            for i in 16..80 {
                w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
            }
            let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
            for (i, wi) in w.iter().enumerate() {
                let (f, k) = match i {
                    0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                    20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                    40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                    _ => (b ^ c ^ d, 0xCA62_C1D6),
                };
                let tmp = a
                    .rotate_left(5)
                    .wrapping_add(f)
                    .wrapping_add(e)
                    .wrapping_add(k)
                    .wrapping_add(*wi);
                e = d;
                d = c;
                c = b.rotate_left(30);
                b = a;
                a = tmp;
            }
            h[0] = h[0].wrapping_add(a);
            h[1] = h[1].wrapping_add(b);
            h[2] = h[2].wrapping_add(c);
            h[3] = h[3].wrapping_add(d);
            h[4] = h[4].wrapping_add(e);
        }
        let mut out = [0_u8; 20];
        for (i, v) in h.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
        }
        out
    }
}

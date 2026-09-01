// Created: 2026-08-31 by Constructor Tech
// @cpt-dod:cpt-cf-oagw-dod-testing-proxy-data-plane:p2
//! WebSocket upgrade proxying (PRD session flows; DESIGN §3.2 header table).
//!
//! A handshake is a `GET`: it resolves the alias, matches a route, runs the
//! plugin request phase, CORS and the rate limit, and is then dialled like any
//! other request. What differs is the answer — a 101 switches the *client*
//! connection over too, which an in-process `oneshot` can never complete. Every
//! test here therefore serves the router on a real listener and drives a raw
//! socket, and the mock upstream is a raw listener as well: the gateway is a
//! byte bridge, so the mock needs no RFC-strict framing to prove the session.
//!
//! Three decisions of the slice are pinned here. The upgrade headers the
//! upstream receives are the gateway's own canonical values, never the client's
//! token lists, because those lists are a smuggling channel; the number of live
//! sessions is capped by `oagw.config.max_websocket_sessions`, so a session
//! beyond the cap is refused before a dial; and an upstream that answers 101
//! without accepting the session (RFC 6455 §4.2.2) is a 502 for the client,
//! which is never told the protocol switched when it did not.

mod common;

use anyhow::{Context as _, Result};
use common::{
    LogCapture, ProxyHarness, domain_route, domain_upstream, loopback_endpoint, problem_type,
};
use oagw::config::OagwConfig;
use oagw::domain::model::{
    BurstConfig, CorsConfig, Endpoint, HttpMethod, RateLimitConfig, Scheme, SharingMode,
    SustainedRate,
};
use oagw::domain::proxy::chain::NoChain;
use std::fmt::Write as _;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// Alias every test routes through.
const ALIAS: &str = "api.vendor.com";
/// Path of the route every test matches.
const ROUTE_PATH: &str = "/v1/ws";
/// The handshake path on the gateway.
const HANDSHAKE_PATH: &str = "/oagw/v1/proxy/api.vendor.com/v1/ws";
/// Base64 key a client sends; the mock echoes it back as the accept value.
const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
/// How long an answer body may take to end before the test gives up.
const BODY_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
/// A close frame the mock echoes back untouched.
const CLOSE: [u8; 2] = [0x88, 0x00];
/// A body a handshake must not carry.
const BODY: &str = "hello";

// ── Policies ─────────────────────────────────────────────────────────────

/// A token-bucket policy of `rate` per second.
fn rate_limit(rate: u64, capacity: u64) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: "token_bucket".to_owned(),
        sustained: SustainedRate {
            rate,
            window: "second".to_owned(),
        },
        burst: Some(BurstConfig { capacity }),
        scope: "global".to_owned(),
        strategy: "reject".to_owned(),
        cost: 1,
        response_headers: true,
    }
}

/// An enabled CORS policy for `origins`.
fn cors(origins: &[&str]) -> CorsConfig {
    CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: origins.iter().map(ToString::to_string).collect(),
        allowed_methods: Vec::from(["GET".to_owned()]),
        expose_headers: Vec::new(),
        allow_credentials: false,
    }
}

// ── Mock upstream ────────────────────────────────────────────────────────

/// What the mock answers to the handshake it reads.
#[derive(Clone, Copy)]
enum Behaviour {
    /// Complete the handshake, then echo every frame byte for byte.
    Echo,
    /// Complete the handshake, then mirror the request head back over the
    /// session before echoing frames: what the upstream received is what the
    /// bridge sent, key redacted.
    Mirror,
    /// Refuse the handshake with a plain HTTP answer.
    Refuse {
        /// Status line without the version.
        status: &'static str,
        /// Body of the refusal.
        body: &'static str,
    },
    /// Answer 200 with the request head echoed in the body, key redacted.
    Inspect,
    /// Answer a 101 that carries neither an `Upgrade` nor an accept value, then
    /// close: the status switches, the session is never accepted.
    Deny,
}

/// A raw TCP listener standing in for the upstream.
struct Mock {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
}

impl Mock {
    /// How many connections the mock accepted.
    fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

/// Spawn the mock upstream of `behaviour` on an ephemeral port.
async fn spawn_mock(behaviour: Behaviour) -> Result<Mock> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;
    let connections = Arc::new(AtomicUsize::new(0));
    let accepted = Arc::clone(&connections);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            accepted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                // A mock whose connection the test gave up on is not a failure.
                answer(socket, behaviour).await.ok();
            });
        }
    });
    Ok(Mock { addr, connections })
}

/// Answer one upstream connection and, for a completed handshake, echo frames.
async fn answer(mut socket: TcpStream, behaviour: Behaviour) -> Result<()> {
    let (head, leftover) = read_head(&mut socket).await?;
    match behaviour {
        Behaviour::Echo => {
            let accept =
                header_of(&head, "sec-websocket-key").unwrap_or_else(|| "<missing>".to_owned());
            let upgrade = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: \
                 Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
            );
            socket.write_all(upgrade.as_bytes()).await?;
            socket.write_all(&leftover).await?;
            echo(&mut socket).await?;
        }
        Behaviour::Mirror => {
            let accept =
                header_of(&head, "sec-websocket-key").unwrap_or_else(|| "<missing>".to_owned());
            let upgrade = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: \
                 Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
            );
            socket.write_all(upgrade.as_bytes()).await?;
            // Over the session, so the bridge is the only thing that could have
            // altered it.
            let mirrored = frame(redact_key(&head).as_bytes());
            socket.write_all(&mirrored).await?;
            echo(&mut socket).await?;
        }
        Behaviour::Refuse { status, body } => {
            let reply = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: \
                 {}\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await?;
            socket.flush().await?;
        }
        Behaviour::Inspect => {
            let body = redact_key(&head);
            let reply = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: \
                 {}\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(reply.as_bytes()).await?;
            socket.flush().await?;
        }
        Behaviour::Deny => {
            // A 101 that accepts nothing: the dial client arms the upgrade on
            // the status alone, so only the head can tell this from a session.
            let answer = "HTTP/1.1 101 Switching Protocols\r\nConnection: upgrade\r\n\r\n";
            socket.write_all(answer.as_bytes()).await?;
            socket.flush().await?;
        }
    }
    Ok(())
}

/// Echo every byte that arrives until the peer stops sending.
async fn echo(socket: &mut TcpStream) -> Result<()> {
    let mut buffer = [0u8; 4096];
    loop {
        let read = socket.read(&mut buffer).await?;
        if read == 0 {
            return Ok(());
        }
        socket.write_all(&buffer[..read]).await?;
    }
}

/// Read the request head, returning the bytes that followed it.
async fn read_head(socket: &mut TcpStream) -> Result<(String, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    while find_head_end(&buffer).is_none() {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let split = find_head_end(&buffer).map_or(buffer.len(), |at| at + 4);
    let head = String::from_utf8_lossy(&buffer[..split]).into_owned();
    Ok((head, buffer[split..].to_vec()))
}

/// Offset of the blank line that ends a head, when it has arrived.
fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Value of one request header, case-insensitively.
fn header_of(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (candidate, value) = line.split_once(':')?;
        candidate
            .trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

/// Replace the value of the handshake key: the test asserts its presence, never
/// its content.
fn redact_key(head: &str) -> String {
    head.lines()
        .map(|line| {
            if line.to_ascii_lowercase().starts_with("sec-websocket-key") {
                "sec-websocket-key: <present>".to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

// ── Served data plane ────────────────────────────────────────────────────

/// A data plane served on a real listener, with a mock upstream behind it.
struct Served {
    /// Address of the gateway.
    gateway: SocketAddr,
    /// The mock upstream the data plane dials.
    mock: Mock,
}

/// Serve a data plane whose upstream is `endpoint` and whose route carries the
/// two policies.
async fn serve_with(
    behaviour: Behaviour,
    scheme: Scheme,
    rate_limit: Option<RateLimitConfig>,
    cors: Option<CorsConfig>,
    config: OagwConfig,
) -> Result<Served> {
    let mock = spawn_mock(behaviour).await?;
    let harness = ProxyHarness::with_config_and_chain(&config, Arc::new(NoChain));
    // The port is only known once the mock listens, so the endpoint is built
    // here rather than by the caller.
    let endpoint = Endpoint {
        scheme,
        ..http_endpoint(mock.addr.port())
    };
    let mut record = domain_upstream(harness.tenant(), ALIAS, vec![endpoint], true);
    record.rate_limit = rate_limit;
    record.cors = cors;
    let id = harness.seed_upstream(record);
    let route = domain_route(harness.tenant(), id, &[HttpMethod::Get], ROUTE_PATH, &[]);
    harness
        .store()
        .insert_route_checked(route)
        .context("the test route must seed")?;
    let gateway = serve(&harness).await?;
    Ok(Served { gateway, mock })
}

/// Serve the harness over a real listener, injecting the tenant context the
/// middleware would have produced.
///
/// A handshake needs a socket the gateway can hand over, so `oneshot` cannot
/// drive these tests.
async fn serve(harness: &ProxyHarness) -> Result<SocketAddr> {
    let ctx = common::security_context(harness.tenant())?;
    let app = harness
        .router()
        .clone()
        .layer(axum::middleware::map_request(
            move |mut request: http::Request<axum::body::Body>| {
                let ctx = ctx.clone();
                async move {
                    let (mut parts, body) = request.into_parts();
                    parts.extensions.insert(ctx);
                    request = http::Request::from_parts(parts, body);
                    request
                }
            },
        ));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::warn!(%error, "the test listener stopped");
        }
    });
    Ok(addr)
}

/// A plaintext endpoint of the mock upstream.
fn http_endpoint(port: u16) -> Endpoint {
    Endpoint {
        scheme: oagw::domain::model::Scheme::Http,
        host: loopback_endpoint(port).host,
        port,
    }
}

// ── Client ───────────────────────────────────────────────────────────────

/// The head of the answer the gateway gave the handshake.
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
}

impl Answer {
    /// Visible value of one answer header.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// A raw client socket driving one handshake and the session behind it.
struct Session {
    stream: TcpStream,
    /// Bytes already read but not part of the answer head.
    pending: Vec<u8>,
}

impl Session {
    /// Open a socket, send the handshake and read the answer head.
    async fn handshake(gateway: SocketAddr, extra: &[(&str, &str)]) -> Result<(Self, Answer)> {
        let extras = extra.iter().fold(String::new(), |mut head, (name, value)| {
            let _written = write!(head, "{name}: {value}\r\n");
            head
        });
        let mut request = handshake_head(gateway);
        request.push_str(&extras);
        request.push_str("\r\n");
        Self::open(gateway, &request).await
    }

    /// Open a socket, send `request` verbatim and read the answer head.
    async fn open(gateway: SocketAddr, request: &str) -> Result<(Self, Answer)> {
        let mut stream = TcpStream::connect(gateway).await?;
        stream.write_all(request.as_bytes()).await?;
        let (head, leftover) = read_head(&mut stream).await?;
        let status = head
            .lines()
            .next()
            .and_then(|line| line.split(' ').nth(1))
            .and_then(|status| status.parse::<u16>().ok())
            .context("the answer carries no status line")?;
        let headers = head
            .lines()
            .skip(1)
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_ascii_lowercase(), value.trim().to_owned()))
            })
            .collect();
        Ok((
            Self {
                stream,
                pending: leftover,
            },
            Answer { status, headers },
        ))
    }

    /// Fill `buffer` from what is already read, then from the socket.
    async fn fill(&mut self, buffer: &mut [u8]) -> Result<()> {
        // What the head read already pulled in satisfies the read first.
        let take = self.pending.len().min(buffer.len());
        buffer[..take].copy_from_slice(&self.pending[..take]);
        self.pending.drain(..take);
        let mut filled = take;
        while filled < buffer.len() {
            let read = self.stream.read(&mut buffer[filled..]).await?;
            if read == 0 {
                return Err(anyhow::anyhow!("the connection ended early"));
            }
            filled += read;
        }
        Ok(())
    }

    /// Send one raw frame.
    async fn send(&mut self, frame: &[u8]) -> Result<()> {
        self.stream.write_all(frame).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Read one frame the bridge forwarded.
    async fn frame(&mut self) -> Result<Vec<u8>> {
        let mut head = [0u8; 2];
        self.fill(&mut head).await?;
        let short = usize::from(head[1] & 0x7f);
        let length = if short == 126 {
            let mut extended = [0u8; 2];
            self.fill(&mut extended).await?;
            usize::from(u16::from_be_bytes(extended))
        } else {
            short
        };
        let mut payload = vec![0u8; length];
        self.fill(&mut payload).await?;
        Ok(payload)
    }

    /// Half-close the socket and drain what the bridge still sends.
    async fn drain(mut self) -> Result<usize> {
        self.stream.shutdown().await?;
        let mut rest = std::mem::take(&mut self.pending);
        self.stream.read_to_end(&mut rest).await?;
        Ok(rest.len())
    }

    /// Read the body of an answer that was not an upgrade.
    ///
    /// The answer may be chunked, so the remaining bytes are read to the end of
    /// the connection rather than to a declared length; a body that never ends
    /// fails the test on its budget instead of hanging it.
    async fn body(&mut self, answer: &Answer) -> Result<String> {
        let length = answer
            .header("content-length")
            .and_then(|v| v.parse::<usize>().ok());
        let mut body = if let Some(length) = length {
            let mut declared = vec![0u8; length];
            tokio::time::timeout(BODY_BUDGET, self.fill(&mut declared))
                .await
                .context("the answer body never arrived")??;
            declared
        } else {
            let mut rest = std::mem::take(&mut self.pending);
            tokio::time::timeout(BODY_BUDGET, self.stream.read_to_end(&mut rest))
                .await
                .context("the answer body never ended")??;
            rest
        };
        body.extend_from_slice(&std::mem::take(&mut self.pending));
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}

/// The handshake request head, without the extra headers a test adds.
fn handshake_head(gateway: SocketAddr) -> String {
    format!(
        "GET {HANDSHAKE_PATH} HTTP/1.1\r\nHost: {gateway}\r\nUpgrade: \
         websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: \
         {KEY}\r\nSec-WebSocket-Version: 13\r\n"
    )
}

/// A request head with exactly the headers a test names.
fn request_head(gateway: SocketAddr, headers: &[(&str, &str)]) -> String {
    let pairs = headers
        .iter()
        .fold(String::new(), |mut head, (name, value)| {
            let _written = write!(head, "{name}: {value}\r\n");
            head
        });
    format!("GET {HANDSHAKE_PATH} HTTP/1.1\r\nHost: {gateway}\r\n{pairs}\r\n")
}

/// A text frame the way the tests hand-roll it: no mask, short or 16-bit length.
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut sent = vec![0x81];
    if payload.len() < 126 {
        sent.push(u8::try_from(payload.len()).unwrap_or(126));
    } else {
        sent.push(126);
        let length = u16::try_from(payload.len()).unwrap_or(u16::MAX);
        sent.extend_from_slice(&length.to_be_bytes());
    }
    sent.extend_from_slice(payload);
    sent
}

// ── The bridge ───────────────────────────────────────────────────────────

/// A handshake is bridged: the client's socket carries the session from then
/// on, and the upstream's accept value reaches it verbatim.
#[tokio::test]
async fn a_websocket_handshake_is_bridged_to_the_upstream() -> Result<()> {
    let served = serve_with(
        Behaviour::Echo,
        Scheme::Http,
        None,
        None,
        common::proxy_config(),
    )
    .await?;
    let (mut session, answer) = Session::handshake(served.gateway, &[]).await?;
    assert_eq!(answer.status, 101);
    assert_eq!(
        answer.header("sec-websocket-accept"),
        Some(KEY),
        "the upstream's accept value, forwarded verbatim"
    );
    // The tokens are case-insensitive (RFC 9110 §7.6.1); the gateway emits the
    // canonical lower-case form.
    assert_eq!(answer.header("connection"), Some("upgrade"));
    assert_eq!(answer.header("upgrade"), Some("websocket"));
    // A switch of protocols is not an error, and ADR-0007 marks error
    // provenance only: the upgraded head carries no source marker.
    assert_eq!(answer.header("x-oagw-error-source"), None);

    let payload = b"hello over the bridge";
    session.send(&frame(payload)).await?;
    assert_eq!(session.frame().await?, payload, "the echo came back");
    session.send(&CLOSE).await?;
    // A close frame carries no payload of its own; arriving at all is the point.
    assert!(
        session.frame().await?.is_empty(),
        "the close frame came back"
    );
    assert_eq!(
        session.drain().await?,
        0,
        "the bridge closed both sides of the session"
    );
    Ok(())
}

/// The handshake is a request: the quota is spent on it, not on the session.
#[tokio::test]
async fn the_handshake_is_gated_on_the_rate_limit() -> Result<()> {
    let served = serve_with(
        Behaviour::Echo,
        Scheme::Http,
        Some(rate_limit(1, 1)),
        None,
        common::proxy_config(),
    )
    .await?;
    let (session, answer) = Session::handshake(served.gateway, &[]).await?;
    assert_eq!(answer.status, 101);
    assert_eq!(session.drain().await?, 0);

    let (mut second, refused) = Session::handshake(served.gateway, &[]).await?;
    assert_eq!(refused.status, 429);
    assert_eq!(refused.header("x-ratelimit-remaining"), Some("0"));
    assert!(
        refused.header("retry-after").is_some(),
        "the refusal carries the retry guidance of the bucket"
    );
    // The refusal is an ordinary answer, body and all.
    assert!(
        second.body(&refused).await?.contains("rate limit"),
        "the refusal says why"
    );
    assert_eq!(
        served.mock.connections(),
        1,
        "a refused handshake is never dialled"
    );
    Ok(())
}

/// A disallowed `Origin` is refused before the dial, with the `Vary` alone.
#[tokio::test]
async fn the_handshake_is_gated_on_cors() -> Result<()> {
    let served = serve_with(
        Behaviour::Echo,
        Scheme::Http,
        None,
        Some(cors(&["https://good.example"])),
        common::proxy_config(),
    )
    .await?;
    let (mut session, refused) =
        Session::handshake(served.gateway, &[("Origin", "https://evil.example")]).await?;
    assert_eq!(refused.status, 403);
    assert_eq!(refused.header("vary"), Some("Origin"));
    assert_eq!(
        refused.header("access-control-allow-origin"),
        None,
        "a refused origin is never named"
    );
    assert!(
        session.body(&refused).await?.contains("origin"),
        "the refusal says why"
    );
    assert_eq!(served.mock.connections(), 0, "no dial happened");
    Ok(())
}

/// A failed handshake is an ordinary HTTP answer, forwarded untouched.
#[tokio::test]
async fn a_failed_handshake_is_a_normal_response() -> Result<()> {
    let served = serve_with(
        Behaviour::Refuse {
            status: "401 Unauthorized",
            body: "the upstream refused the token",
        },
        Scheme::Http,
        None,
        None,
        common::proxy_config(),
    )
    .await?;
    let (session, answer) = Session::handshake(served.gateway, &[]).await?;
    assert_eq!(answer.status, 401);
    // Not an upgrade, so the answer keeps the provenance marker of the data
    // plane: the answer came from the upstream.
    assert_eq!(answer.header("x-oagw-error-source"), Some("upstream"));
    assert_eq!(
        session.drain().await?,
        "the upstream refused the token".len(),
        "the body of the failure reached the client"
    );
    Ok(())
}

/// `Upgrade` without the handshake's key is not an upgrade: it is proxied as a
/// request and loses the hop-by-hop headers on the way out.
#[tokio::test]
async fn a_handshake_that_is_not_one_is_untouched() -> Result<()> {
    let served = serve_with(
        Behaviour::Inspect,
        Scheme::Http,
        None,
        None,
        common::proxy_config(),
    )
    .await?;

    // A `websocket` token without the key: no handshake, normal proxy path.
    let request = request_head(
        served.gateway,
        &[
            ("Upgrade", "websocket"),
            ("Connection", "Upgrade"),
            ("Sec-WebSocket-Version", "13"),
        ],
    );
    let (mut session, answer) = Session::open(served.gateway, &request).await?;
    assert_eq!(answer.status, 200, "no key, no upgrade");
    let body = session.body(&answer).await?;
    assert!(body.contains("sec-websocket-version: 13"), "body: {body}");
    assert!(
        !body.to_ascii_lowercase().contains("connection:"),
        "the hop-by-hop headers are still stripped: {body}"
    );
    assert!(
        !body.to_ascii_lowercase().contains("upgrade:"),
        "the hop-by-hop headers are still stripped: {body}"
    );

    // An ordinary request that merely carries an `Upgrade` header: same thing.
    let request = request_head(
        served.gateway,
        &[
            ("Upgrade", "h2c"),
            ("Connection", "Upgrade, HTTP2-Settings"),
            ("Sec-WebSocket-Key", KEY),
        ],
    );
    let (mut session, answer) = Session::open(served.gateway, &request).await?;
    assert_eq!(answer.status, 200, "no websocket token, no upgrade");
    let body = session.body(&answer).await?;
    assert!(
        !body.to_ascii_lowercase().contains("upgrade:"),
        "an upgrade that is not a handshake is stripped: {body}"
    );
    Ok(())
}

/// A TLS endpoint cannot be dialled by the plaintext bridge: 503, no dial.
#[tokio::test]
async fn a_tls_upstream_refuses_the_handshake_without_dialling() -> Result<()> {
    let served = serve_with(
        Behaviour::Echo,
        Scheme::Https,
        None,
        None,
        common::proxy_config(),
    )
    .await?;
    let (mut session, answer) = Session::handshake(served.gateway, &[]).await?;
    assert_eq!(answer.status, 503);
    assert_eq!(
        answer.header("content-type"),
        Some("application/problem+json")
    );
    // The refusal is the gateway's own, not the upstream's.
    assert_eq!(answer.header("x-oagw-error-source"), Some("gateway"));
    let body = session.body(&answer).await?;
    assert!(
        body.contains(&problem_type("link.unavailable.v1")),
        "body: {body}"
    );
    assert!(
        body.contains("https"),
        "the refusal names the scheme it cannot dial: {body}"
    );
    assert_eq!(served.mock.connections(), 0, "no dial happened");
    Ok(())
}

/// A session outlives the body budget: the bridge has no deadline at all.
#[tokio::test]
async fn the_session_has_no_body_budget() -> Result<()> {
    let config = OagwConfig {
        proxy_timeout_secs: 1,
        proxy_idle_timeout_secs: Some(5),
        proxy_stream_timeout_secs: Some(1),
        allow_http_upstream: true,
        ..common::proxy_config()
    };
    let served = serve_with(Behaviour::Echo, Scheme::Http, None, None, config).await?;
    let (mut session, answer) = Session::handshake(served.gateway, &[]).await?;
    assert_eq!(answer.status, 101);
    // Longer than the one-second budget of a forwarded body.
    tokio::time::sleep(std::time::Duration::from_millis(1_400)).await;
    let payload = b"still bridged";
    session.send(&frame(payload)).await?;
    assert_eq!(
        session.frame().await?,
        payload,
        "a session is not a body: it has no overall budget"
    );
    assert_eq!(session.drain().await?, 0);
    Ok(())
}

/// The 429 problem of a refused handshake keeps its type and its guidance.
#[tokio::test]
async fn a_refused_handshake_is_a_problem_document() -> Result<()> {
    let served = serve_with(
        Behaviour::Echo,
        Scheme::Http,
        Some(rate_limit(1, 1)),
        None,
        common::proxy_config(),
    )
    .await?;
    let (_first, allowed) = Session::handshake(served.gateway, &[]).await?;
    assert_eq!(allowed.status, 101);
    let (mut session, refused) = Session::handshake(served.gateway, &[]).await?;
    assert_eq!(refused.status, 429);
    assert_eq!(
        refused.header("content-type"),
        Some("application/problem+json")
    );
    let body = session.body(&refused).await?;
    assert!(
        body.contains(&problem_type("rate_limit.exceeded.v1")),
        "body: {body}"
    );
    Ok(())
}

/// The cap on live sessions is real: a handshake that finds no free slot is
/// refused before a dial, and the refusal names the bound.
#[tokio::test]
async fn a_session_beyond_the_cap_is_refused() -> Result<()> {
    let config = OagwConfig {
        max_websocket_sessions: 1,
        ..common::proxy_config()
    };
    let served = serve_with(Behaviour::Echo, Scheme::Http, None, None, config).await?;
    let (open, answer) = Session::handshake(served.gateway, &[]).await?;
    assert_eq!(answer.status, 101);
    // `open` stays in scope on purpose: the session holds the one slot.
    let (mut refused, refused_head) = Session::handshake(served.gateway, &[]).await?;
    assert_eq!(refused_head.status, 503, "one slot, one session");
    assert_eq!(
        refused_head.header("content-type"),
        Some("application/problem+json")
    );
    let body = refused.body(&refused_head).await?;
    assert!(
        body.contains(&problem_type("link.unavailable.v1")),
        "body: {body}"
    );
    assert!(
        body.contains("no free websocket session slot") && body.contains("the limit is 1"),
        "the refusal names the bound: {body}"
    );
    assert_eq!(
        served.mock.connections(),
        1,
        "only the session that opened was dialled"
    );
    drop(open);
    Ok(())
}

/// A handshake that carries a body is rejected before a dial: bytes after its
/// head are not protocol content, and dialling them would hang the bridge.
#[tokio::test]
async fn a_handshake_that_carries_a_body_is_a_bad_request() -> Result<()> {
    let served = serve_with(
        Behaviour::Inspect,
        Scheme::Http,
        None,
        None,
        common::proxy_config(),
    )
    .await?;
    // The length has to be declared, or the gateway has no body to see.
    let request = format!(
        "{}Content-Length: {}\r\n\r\n{BODY}",
        handshake_head(served.gateway),
        BODY.len()
    );
    let (mut session, answer) = Session::open(served.gateway, &request).await?;
    assert_eq!(answer.status, 400);
    assert_eq!(
        answer.header("content-type"),
        Some("application/problem+json")
    );
    let body = session.body(&answer).await?;
    assert!(
        body.contains(&problem_type("validation.error.v1")),
        "body: {body}"
    );
    assert_eq!(served.mock.connections(), 0, "no dial happened");
    Ok(())
}

/// The head the upstream receives on a successful handshake: the two upgrade
/// headers carry the gateway's canonical values, and nothing else of the
/// client's token lists rides along.
#[tokio::test]
async fn the_upstream_sees_canonical_upgrade_headers() -> Result<()> {
    let served = serve_with(
        Behaviour::Mirror,
        Scheme::Http,
        None,
        None,
        common::proxy_config(),
    )
    .await?;
    // The client names h2c and its settings alongside the websocket upgrade;
    // neither may reach the upstream through a WebSocket bridge.
    let (mut session, answer) = Session::handshake(
        served.gateway,
        &[
            ("Upgrade", "websocket, h2c"),
            ("Connection", "upgrade, HTTP2-Settings"),
        ],
    )
    .await?;
    assert_eq!(answer.status, 101);
    let seen = String::from_utf8_lossy(&session.frame().await?).to_ascii_lowercase();
    assert!(
        seen.contains("upgrade: websocket"),
        "the upstream saw: {seen}"
    );
    assert!(
        seen.contains("connection: upgrade"),
        "the upstream saw: {seen}"
    );
    assert!(
        seen.contains("sec-websocket-version: 13"),
        "the upgrade content of the handshake is forwarded: {seen}"
    );
    assert!(
        seen.contains("sec-websocket-key: <present>"),
        "the handshake key is present, never spelled out: {seen}"
    );
    assert!(
        !seen.contains("h2c"),
        "a second protocol never ships through a websocket bridge: {seen}"
    );
    assert!(
        !seen.contains("http2-settings"),
        "the header of another upgrade is dropped: {seen}"
    );
    assert_eq!(session.drain().await?, 0);
    Ok(())
}

/// An upstream that answers 101 without accepting the session does not give the
/// client a session: the gateway refuses while it still owns the answer, so the
/// client is never told the protocol switched when it did not.
///
/// A bare 101 is the trap RFC 6455 §4.2.2 closes: hyper arms the response
/// upgrade from the status alone, so without a check on the head the client
/// would hold a socket whose first read is EOF — a "session" that carries
/// nothing. The acceptance is judged in `switch_protocols`, which runs inside
/// the request task, so the record of the refusal is captured by the per-test
/// subscriber of `common` (the bridge's own records are not, which is why the
/// tests of a live session assert behaviour only).
#[tokio::test]
async fn an_upstream_that_did_not_accept_the_upgrade_is_a_502() -> Result<()> {
    let served = serve_with(
        Behaviour::Deny,
        Scheme::Http,
        None,
        None,
        common::proxy_config(),
    )
    .await?;
    let capture = LogCapture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    let (mut session, answer) = Session::handshake(served.gateway, &[]).await?;
    assert_eq!(
        answer.status, 502,
        "the client is not handed a session that was never accepted"
    );
    assert_eq!(
        answer.header("content-type"),
        Some("application/problem+json")
    );
    assert_eq!(
        answer.header("x-oagw-error-source"),
        Some("gateway"),
        "the refusal is the gateway's own answer"
    );
    let body = session.body(&answer).await?;
    assert!(
        body.contains(&problem_type("protocol.error.v1")),
        "body: {body}"
    );
    assert_eq!(
        served.mock.connections(),
        1,
        "the gateway dialled once and did not retry"
    );
    let refused = capture
        .lines()
        .into_iter()
        .find(|line| line.contains("websocket session refused"))
        .context("the refusal of the upgrade was not logged")?;
    assert!(
        refused.contains(ALIAS) && refused.contains("the answer names no websocket upgrade"),
        "the record names the alias and the reason: {refused}"
    );
    Ok(())
}

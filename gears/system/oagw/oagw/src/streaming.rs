//! Streaming support of the data plane: PRD §5.4 "Streaming Support"
//! (`cpt-cf-oagw-fr-streaming`), DESIGN §3.3 "Proxy API", the DESIGN §5.1
//! traceability row for `cpt-cf-oagw-fr-streaming`, and ADR-0007.
//!
//! DESIGN.md has no streaming chapter of its own — §5 is the traceability
//! section, and it carries no §5.4 — so every citation in this module names an
//! anchor that does exist. Writing that design section is a design-owner item,
//! not something this crate authors (the citations in it are the crate's only
//! contribution to the record).
//!
//! The gateway does **not** terminate a WebSocket. It signs the handshake the
//! caller sees, forwards the caller's own `Sec-WebSocket-Key` to the upstream,
//! and — once the upstream answers `101 Switching Protocols` — splices the two
//! connections together at the *message* level. Everything past the handshake is
//! the caller's and the upstream's business: the gateway translates message
//! types and nothing else, and it never looks at a payload.
//!
//! The message level is the level the two ends actually speak. A byte-level
//! splice (piping the raw sockets into each other) would be cheaper, but the two
//! legs were produced by two different HTTP stacks and each of them may still be
//! holding bytes it decoded as part of the handshake — axum's client leg and
//! hyper's upstream leg both buffer. Message-level splicing hands each leg's
//! framing to the implementation that already owns it, so no buffered byte is
//! ever delivered to the wrong protocol machine.
//!
//! Two properties are load-bearing here, and both are security properties:
//!
//! * **No extension negotiation.** tungstenite negotiates no WebSocket
//!   extension, so the gateway offers none and never echoes one. A negotiated
//!   extension the splice could not honour (say `permessage-deflate`) would
//!   corrupt both legs; see
//!   [`crate::domain::headers::restore_websocket_handshake`].
//! * **No payload logging.** A proxied WebSocket is the caller's channel to the
//!   upstream: message contents are the same class of data as a proxied request
//!   or response body, which the audit record already refuses to carry (DESIGN
//!   §4.3). Nothing in this module logs a message — a spliced session is
//!   reported only by its correlation id, its leg and a transport error string.

use axum::extract::ws::{Message, WebSocket};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite as ts;

/// The `Sec-WebSocket-Accept` an upstream's `101` must carry before this gateway
/// signs any handshake of its own (RFC 6455 §4.2.2).
///
/// The caller's key travels to the upstream unchanged, so the value the upstream
/// answers with is predictable and checkable — and tungstenite checks it on
/// `connect_async`, which the gateway's own client leg does not do, because
/// [`WebSocketStream::from_raw_socket`] declares the handshake already complete.
/// This is the check that stands in for it: an upstream that cannot produce the
/// accept derived from the caller's own key has not agreed to *this* protocol,
/// and the gateway must not countersign the `101`.
#[must_use]
pub fn expected_accept(caller_key: &str) -> String {
    ts::handshake::derive_accept_key(caller_key.as_bytes())
}

/// Splice a client WebSocket onto the upgraded upstream connection.
///
/// Both legs are already handshakes-complete: `client` is the socket axum
/// upgraded after answering `101`, `upstream` is the socket hyper took from the
/// upstream's `101` — after [`expected_accept`] verified that the upstream
/// really did agree to the protocol the caller offered. The two are pumped into
/// each other until either direction ends; at that point the session is over for
/// both, and the leg that is still writable is told so with a Close frame.
///
/// `request_id` is the correlation id of the request whose handshake began this
/// session. A spliced session outlives the handshake that produced it, and the
/// records it writes are the only trace it leaves, so they carry the same id the
/// request's audit record and its `oagw.ws_upgrade_failed` records carry.
///
/// A WebSocket failure on one leg is not a gateway error document: there is no
/// problem+json inside a WebSocket, and the only thing a gateway can do about a
/// leg that died is end the session. This function therefore never returns an
/// error — it closes what it can and stops.
///
/// `S` is the raw upstream stream, already adapted to tokio's I/O traits by the
/// caller: hyper's `Upgraded` speaks hyper's own `Read`/`Write`, so the data
/// plane wraps it in [`hyper_util::rt::TokioIo`] before handing it here, exactly
/// as axum does on the client leg.
pub async fn splice<S>(client: WebSocket, upstream: WebSocketStream<S>, request_id: &str)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut client_sink, mut client_source) = client.split();
    let (mut upstream_sink, mut upstream_source) = upstream.split();

    // Either direction may end first; when one does, the whole session ends.
    // `select!` drops the losing future, which releases its half of the split,
    // so both sinks are reachable again for the closing below.
    tokio::select! {
        _ = relay_client(&mut client_source, &mut upstream_sink, request_id) => {
            tracing::debug!(target: "oagw.ws", event = "splice_closed", request_id = %request_id, leg = "client");
        }
        _ = relay_upstream(&mut upstream_source, &mut client_sink, request_id) => {
            tracing::debug!(target: "oagw.ws", event = "splice_closed", request_id = %request_id, leg = "upstream");
        }
    }

    // Tell the leg that is still writable the session is over, but do not wait
    // for its closing handshake: a peer that never replies would hold this task
    // open for as long as the socket stayed alive. Dropping both legs closes the
    // sockets, which is the outcome the surviving peer observes either way.
    let _ = client_sink.send(Message::Close(None)).await;
    let _ = upstream_sink.send(ts::Message::Close(None)).await;
}

/// Pump the client's leg into the upstream: translate and forward until either
/// side stops.
///
/// `Err` from the client means the leg is broken (a protocol violation, or the
/// socket went away) and the session ends; `None` means the client ended it.
/// Both are the same outcome here, and neither is ever a gateway error — but a
/// broken leg is *recorded* ([`log_splice_error`]), because an operator who could
/// only see `splice_closed` could not tell a peer's clean `Close(1000)` from a
/// frame the peer never should have sent.
async fn relay_client<Src, Dst>(source: &mut Src, sink: &mut Dst, request_id: &str)
where
    Src: Stream<Item = Result<Message, axum::Error>> + Unpin,
    Dst: Sink<ts::Message, Error = ts::Error> + Unpin,
{
    while let Some(item) = source.next().await {
        let message = match item {
            Ok(message) => message,
            Err(error) => {
                log_splice_error(request_id, "client", &error);
                return;
            }
        };
        let Some(message) = into_tungstenite(message) else {
            // axum's own recommendation for `Frame` (tungstenite issue #268).
            continue;
        };
        let closing = matches!(message, ts::Message::Close(_));
        if sink.send(message).await.is_err() {
            return;
        }
        if closing {
            // The close handshake is relayed, not doubled: the peer answers the
            // leg that originated it.
            return;
        }
    }

    // The client's leg ended without a Close frame — an abrupt TCP close, or a
    // dropped socket. The upstream is still told, so it does not wait for
    // something that will never arrive.
    let _ = sink.send(ts::Message::Close(None)).await;
}

/// Pump the upstream's leg into the client, mirroring [`relay_client`].
async fn relay_upstream<Src, Dst>(source: &mut Src, sink: &mut Dst, request_id: &str)
where
    Src: Stream<Item = Result<ts::Message, ts::Error>> + Unpin,
    Dst: Sink<Message, Error = axum::Error> + Unpin,
{
    while let Some(item) = source.next().await {
        let message = match item {
            Ok(message) => message,
            Err(error) => {
                log_splice_error(request_id, "upstream", &error);
                return;
            }
        };
        let Some(message) = from_tungstenite(message) else {
            continue;
        };
        let closing = matches!(message, Message::Close(_));
        if sink.send(message).await.is_err() {
            return;
        }
        if closing {
            return;
        }
    }

    let _ = sink.send(Message::Close(None)).await;
}

/// Record the failure of one leg of a spliced session.
///
/// The record is the whole difference between "the peer said goodbye" and "the
/// peer's socket broke": a `splice_closed` alone cannot tell them apart, and an
/// operator chasing a broken upstream would otherwise have nothing to go on.
/// Its field set is exactly the correlation id, the leg and the transport's own
/// error string — never a message, a frame, a header or any other payload, which
/// is the rule the module documentation states.
fn log_splice_error(request_id: &str, leg: &'static str, error: impl std::fmt::Display) {
    tracing::debug!(
        target: "oagw.ws",
        event = "splice_error",
        request_id = %request_id,
        leg = leg,
        error = %error,
    );
}

/// Translate an axum message into the form the upstream leg speaks.
///
/// axum's own conversions are private, and its [`Message`] re-implements
/// tungstenite's so that it can hide the raw `Frame` variant; the two line up on
/// the same `bytes` and UTF-8 string types, so this is a renames only. The
/// text payload is copied: [`Message::Text`] vouches for UTF-8, and an infallible
/// translation is worth more here than the zero copy of a conversion that can
/// fail.
#[must_use]
pub fn into_tungstenite(message: Message) -> Option<ts::Message> {
    match message {
        Message::Text(text) => Some(ts::Message::Text(ts::Utf8Bytes::from(text.as_str()))),
        Message::Binary(binary) => Some(ts::Message::Binary(binary)),
        Message::Ping(ping) => Some(ts::Message::Ping(ping)),
        Message::Pong(pong) => Some(ts::Message::Pong(pong)),
        Message::Close(Some(frame)) => Some(ts::Message::Close(Some(ts::protocol::CloseFrame {
            code: ts::protocol::frame::coding::CloseCode::from(frame.code),
            reason: ts::Utf8Bytes::from(frame.reason.as_str()),
        }))),
        Message::Close(None) => Some(ts::Message::Close(None)),
    }
}

/// Translate an upstream message into the form the client leg speaks.
///
/// [`ts::Message::Frame`] maps to `None`: it is only produced while reading
/// control frames the protocol machine already answered, and tungstenite's
/// maintainers recommend ignoring it.
#[must_use]
pub fn from_tungstenite(message: ts::Message) -> Option<Message> {
    match message {
        ts::Message::Text(text) => Some(Message::Text(axum::extract::ws::Utf8Bytes::from(
            text.as_str(),
        ))),
        ts::Message::Binary(binary) => Some(Message::Binary(binary)),
        ts::Message::Ping(ping) => Some(Message::Ping(ping)),
        ts::Message::Pong(pong) => Some(Message::Pong(pong)),
        ts::Message::Close(Some(frame)) => {
            Some(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: u16::from(frame.code),
                reason: axum::extract::ws::Utf8Bytes::from(frame.reason.as_str()),
            })))
        }
        ts::Message::Close(None) => Some(Message::Close(None)),
        ts::Message::Frame(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::marker::PhantomData;
    use std::sync::{Arc, Mutex as StdMutex};

    /// A sink that records everything it is handed, and never fails.
    ///
    /// The relays take a `Sink` with a stack-specific `Error` type, so the double
    /// is parameterised by it and `E` is never actually built: the error is what
    /// the bound demands, not something the test exercises.
    struct RecordingSink<M, E> {
        sent: Arc<StdMutex<Vec<M>>>,
        _error: PhantomData<fn() -> E>,
    }

    impl<M, E> RecordingSink<M, E> {
        /// The sink, and the slot its messages land in.
        fn new() -> (Self, Arc<StdMutex<Vec<M>>>) {
            let sent = Arc::new(StdMutex::new(Vec::new()));
            (
                Self {
                    sent: Arc::clone(&sent),
                    _error: PhantomData,
                },
                sent,
            )
        }
    }

    impl<M, E> Sink<M> for RecordingSink<M, E>
    where
        M: Unpin,
    {
        type Error = E;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn start_send(self: std::pin::Pin<&mut Self>, item: M) -> Result<(), Self::Error> {
            self.sent
                .lock()
                .expect("the recording sink is not poisoned")
                .push(item);
            Ok(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// The messages that reached a recording sink, without the sink itself.
    fn sent_of<M>(sent: &Arc<StdMutex<Vec<M>>>) -> Vec<M>
    where
        M: Clone,
    {
        sent.lock()
            .expect("the recording sink is not poisoned")
            .clone()
    }

    fn close_code(message: &ts::Message) -> Option<u16> {
        match message {
            ts::Message::Close(Some(frame)) => Some(u16::from(frame.code)),
            _ => None,
        }
    }

    /// Round-tripping an axum message through the upstream leg and back is the
    /// identity, for every message kind the gateway relays.
    #[test]
    fn messages_round_trip_through_the_two_legs_unchanged() {
        for message in [
            Message::text("hello"),
            Message::Binary(bytes::Bytes::from_static(b"\x00\x01\xff")),
            Message::Ping(bytes::Bytes::from_static(b"ping")),
            Message::Pong(bytes::Bytes::from_static(b"pong")),
            Message::Close(None),
            Message::Close(Some(axum::extract::ws::CloseFrame {
                code: 1008,
                reason: axum::extract::ws::Utf8Bytes::from_static("policy"),
            })),
        ] {
            let expected = message.clone();
            let upstream = into_tungstenite(message).expect("relayed");
            let back = from_tungstenite(upstream).expect("relayed back");
            assert_eq!(back, expected, "{expected:?} survives the splice");
        }
    }

    #[test]
    fn a_close_frame_keeps_its_code_and_reason_across_the_splice() {
        let frame = axum::extract::ws::CloseFrame {
            code: 4001,
            reason: axum::extract::ws::Utf8Bytes::from_static("route is gone"),
        };
        let ts_frame = match into_tungstenite(Message::Close(Some(frame.clone()))) {
            Some(ts::Message::Close(Some(frame))) => frame,
            other => panic!("a close frame is relayed as one: {other:?}"),
        };
        assert_eq!(u16::from(ts_frame.code), frame.code);
        assert_eq!(ts_frame.reason.as_str(), frame.reason.as_str());
    }

    #[test]
    fn a_raw_frame_is_not_a_message_to_relay() {
        // tungstenite only surfaces `Frame` for control frames it already
        // answered; turning it into a relayed message would duplicate them.
        let frame = ts::Message::Frame(ts::protocol::frame::Frame::ping(Bytes::new()));
        assert_eq!(from_tungstenite(frame), None);
    }

    #[test]
    fn a_text_message_reaches_the_upstream_as_text() {
        let message = into_tungstenite(Message::text("proxied"));
        assert_eq!(
            message,
            Some(ts::Message::Text(ts::Utf8Bytes::from_static("proxied")))
        );
    }

    #[test]
    fn a_binary_message_reaches_the_client_as_binary() {
        let payload = Bytes::from_static(&[0xff, 0x00, 0x7f]);
        let message = from_tungstenite(ts::Message::Binary(payload.clone()));
        assert_eq!(message, Some(Message::Binary(payload)));
    }

    // -- The relay half of the splice ---------------------------------------
    //
    // The lifecycle of a spliced session is proven end to end, over real
    // sockets, in `tests/websocket_sse.rs`; what a unit test can add here is the
    // *decision* each leg makes on its own, with a source the test controls.

    #[tokio::test]
    async fn a_client_leg_that_ends_without_a_close_tells_the_upstream_so() {
        let (mut sink, sent) = RecordingSink::<ts::Message, ts::Error>::new();
        let mut source = futures_util::stream::iter(vec![Ok(Message::text("last word"))]);

        relay_client(&mut source, &mut sink, "rid-abrupt").await;

        let sent = sent_of(&sent);
        assert_eq!(sent.len(), 2, "the message, then the close: {sent:?}");
        assert!(matches!(sent[0], ts::Message::Text(_)));
        assert_eq!(
            close_code(&sent[1]),
            None,
            "an unannounced end is a bare close"
        );
    }

    #[tokio::test]
    async fn a_client_close_is_relayed_and_not_doubled() {
        let (mut sink, sent) = RecordingSink::<ts::Message, ts::Error>::new();
        let mut source = futures_util::stream::iter(vec![
            Ok(Message::text("bye")),
            Ok(Message::Close(Some(axum::extract::ws::CloseFrame {
                code: 1000,
                reason: axum::extract::ws::Utf8Bytes::from_static("done"),
            }))),
        ]);

        relay_client(&mut source, &mut sink, "rid-clean").await;

        let sent = sent_of(&sent);
        assert_eq!(
            sent.len(),
            2,
            "the relayed close is the last word: {sent:?}"
        );
        assert_eq!(close_code(&sent[1]), Some(1000), "the code is the peer's");
    }

    #[tokio::test]
    async fn a_broken_client_leg_stops_the_relay_without_forwarding_more() {
        let (mut sink, sent) = RecordingSink::<ts::Message, ts::Error>::new();
        // A protocol violation on the leg: the relay stops rather than guessing.
        let broken = axum::Error::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "reserved opcode",
        ));
        let mut source =
            futures_util::stream::iter(vec![Err(broken), Ok(Message::text("never relayed"))]);

        relay_client(&mut source, &mut sink, "rid-broken").await;

        assert!(
            sent_of(&sent).is_empty(),
            "a broken leg forwards nothing: {:?}",
            sent_of(&sent)
        );
    }

    #[tokio::test]
    async fn an_upstream_close_reaches_the_client_with_its_code() {
        let (mut sink, sent) = RecordingSink::<Message, axum::Error>::new();
        let mut source = futures_util::stream::iter(vec![Ok(ts::Message::Close(Some(
            ts::protocol::CloseFrame {
                code: ts::protocol::frame::coding::CloseCode::Away,
                reason: ts::Utf8Bytes::from_static("going away"),
            },
        )))]);

        relay_upstream(&mut source, &mut sink, "rid-upstream").await;

        let sent = sent_of(&sent);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(
            sent[0],
            Message::Close(Some(axum::extract::ws::CloseFrame {
                code: 1001,
                reason: axum::extract::ws::Utf8Bytes::from_static("going away"),
            }))
        );
    }

    #[tokio::test]
    async fn an_upstream_leg_that_ends_without_a_close_tells_the_client_so() {
        let (mut sink, sent) = RecordingSink::<Message, axum::Error>::new();
        // No item at all: the upstream's socket was closed under the session.
        let mut source = futures_util::stream::iter(Vec::<Result<ts::Message, ts::Error>>::new());

        relay_upstream(&mut source, &mut sink, "rid-abrupt-upstream").await;

        let sent = sent_of(&sent);
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0], Message::Close(None));
    }

    #[tokio::test]
    async fn a_broken_upstream_leg_stops_the_relay_without_forwarding_more() {
        let (mut sink, sent) = RecordingSink::<Message, axum::Error>::new();
        let broken = ts::Error::Protocol(ts::error::ProtocolError::InvalidOpcode(0xF));

        let mut source =
            futures_util::stream::iter(vec![Err(broken), Ok(ts::Message::text("never relayed"))]);

        relay_upstream(&mut source, &mut sink, "rid-broken-upstream").await;

        assert!(
            sent_of(&sent).is_empty(),
            "a broken leg forwards nothing: {:?}",
            sent_of(&sent)
        );
    }

    #[test]
    fn the_expected_accept_is_the_one_rfc_6455_derives_from_the_key() {
        // The key of RFC 6455 §1.3's own example handshake, and the accept the
        // RFC prints for it. The test harness in `tests/websocket_sse.rs` uses
        // the same pair, so the two cannot drift.
        assert_eq!(
            expected_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }
}

//! The streaming entities and the three routines of the domain layer.
//!
//! Covers `cpt-cf-oagw-dod-stream-entities`, `cpt-cf-oagw-dod-stream-upgrade`,
//! `cpt-cf-oagw-dod-stream-timeouts`, and `cpt-cf-oagw-dod-stream-errors`: the
//! three-part upgrade detection and each of its negatives, the suspended
//! headers the handshake records, the 101 judgement, the two-value transfer
//! mode, every transition and every invalid transition of the lifecycle
//! machine, the 60-second constant with no configuration surface, and the two
//! error answers with their GTS types and their missing `Retry-After`.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::time::Duration;

use oagw::domain::proxy::ProxyContext;
use oagw::domain::stream::{
    self, StreamLifecycle, StreamOutcome, TransferMode, IDLE_TIMEOUT, IDLE_TIMEOUT_SECS,
    HANDSHAKE_HEADERS,
};
use oagw::domain::ErrorKind;

const TENANT: uuid::Uuid = uuid::Uuid::from_u128(0x61);
const UPSTREAM: uuid::Uuid = uuid::Uuid::from_u128(0x62);

/// The header pairs an inbound request held, in arrival order.
fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(name, value)| (String::from(*name), String::from(*value)))
        .collect()
}

/// The context of one inbound request, with the headers the caller states.
fn context(method: &str, pairs: &[(&str, &str)]) -> ProxyContext {
    ProxyContext {
        method: String::from(method),
        alias: String::from("events.example.test"),
        path_suffix: None,
        query: None,
        headers: headers(pairs),
        target_host: None,
        tenant_id: TENANT,
        subject_id: None,
        correlation: None,
    }
}

// @cpt-dod:cpt-cf-oagw-dod-stream-entities:p1

#[test]
fn the_two_entities_carry_the_members_the_feature_assigns() {
    let session = oagw::domain::stream::StreamSession::open_for_incremental(
        TENANT,
        UPSTREAM,
        Some(String::from("text/event-stream")),
    );
    assert_eq!(session.tenant_id, TENANT);
    assert_eq!(session.upstream_id, UPSTREAM);
    assert_eq!(session.mode, TransferMode::Incremental);
    assert_eq!(session.content_type.as_deref(), Some("text/event-stream"));
    assert_eq!(session.lifecycle, StreamLifecycle::Open);
    assert_eq!(session.idle_timeout, IDLE_TIMEOUT);
    assert!(session.moved == 0, "no byte has moved yet");
    assert!(session.outcome.is_none(), "the exchange has not ended");
    assert!(session.caller.open);
    assert!(session.upstream.open);

    let handshake = oagw::domain::stream::UpgradeHandshake::build(
        stream::upgrade_detection("GET", Some("websocket"), Some("upgrade"))
            .expect("the three parts hold"),
        &context("GET", &[("Upgrade", "websocket"), ("Connection", "upgrade")]),
    );
    assert!(!handshake.suspended.is_empty(), "the handshake carries its two");
    assert_eq!(handshake.answer, stream::UpgradeAnswer::NotJudged);
}

#[test]
fn the_lifecycle_state_is_a_state_of_the_machine_and_not_a_third_type() {
    // The session carries `StreamLifecycle`, which is the one state machine the
    // feature owns; the handshake carries no lifecycle of its own.
    let session = oagw::domain::stream::StreamSession::open_for_handshake(TENANT, UPSTREAM);
    assert_eq!(session.lifecycle, StreamLifecycle::Opening);
    assert_eq!(session.mode, TransferMode::Tunnel);
}

// @cpt-dod:cpt-cf-oagw-dod-stream-upgrade:p1

#[test]
fn the_three_parts_of_the_detection_hold_together() {
    let detection =
        stream::upgrade_detection("GET", Some("websocket"), Some("keep-alive, upgrade"));
    assert!(detection.is_some(), "the token list names the upgrade token");
    assert!(
        stream::upgrade_detection("GET", Some("WebSocket"), Some("Upgrade")).is_some(),
        "both headers are compared case-insensitively"
    );
}

#[test]
fn each_negative_of_the_detection_refuses_the_upgrade() {
    // A POST is not the method the handshake requires.
    assert!(
        stream::upgrade_detection("POST", Some("websocket"), Some("upgrade")).is_none(),
        "the method part fails"
    );
    // Another protocol is not the upgrade the feature delivers.
    assert!(
        stream::upgrade_detection("GET", Some("h2c"), Some("upgrade")).is_none(),
        "the protocol part fails"
    );
    // A value carrying whitespace or a protocol list is matched against the one
    // literal and against nothing else.
    assert!(
        stream::upgrade_detection("GET", Some("websocket, h2c"), Some("upgrade")).is_none(),
        "the protocol is matched as one literal"
    );
    // The connection part must name the upgrade token.
    assert!(
        stream::upgrade_detection("GET", Some("websocket"), Some("keep-alive")).is_none(),
        "the connection part fails"
    );
    // Either header absent is a negative.
    assert!(stream::upgrade_detection("GET", None, Some("upgrade")).is_none());
    assert!(stream::upgrade_detection("GET", Some("websocket"), None).is_none());
}

#[test]
fn the_suspension_records_the_two_and_the_handshake_headers_only() {
    let context = context(
        "GET",
        &[
            ("Upgrade", "websocket"),
            ("Connection", "keep-alive, Upgrade"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("Sec-WebSocket-Version", "13"),
            ("Sec-WebSocket-Extensions", "permessage-deflate"),
            ("Sec-WebSocket-Protocol", "chat"),
            ("Keep-Alive", "timeout=5"),
            ("TE", "trailers"),
            ("Trailer", "X-Checksum"),
            ("Transfer-Encoding", "chunked"),
            ("Proxy-Authorization", "Basic dXNlcjpwYXNz"),
            ("Proxy-Authenticate", "Basic"),
            ("X-Custom", "probe"),
            ("Authorization", "Bearer secret"),
        ],
    );
    let detection = stream::upgrade_detection("GET", Some("websocket"), Some("keep-alive, Upgrade"))
        .expect("the three parts hold");
    let handshake = oagw::domain::stream::UpgradeHandshake::build(detection, &context);
    let names: Vec<String> = handshake
        .suspended
        .iter()
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect();
    for suspended in ["upgrade", "connection"] {
        assert!(
            names.contains(&String::from(suspended)),
            "{suspended} is suspended for the handshake"
        );
    }
    for forwarded in HANDSHAKE_HEADERS {
        assert!(
            names.contains(&String::from(forwarded)),
            "{forwarded} is forwarded as the handshake's own header"
        );
    }
    for stripped in [
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
    ] {
        assert!(
            !names.contains(&String::from(stripped)),
            "{stripped} stays stripped on an upgrade request"
        );
    }
    assert!(
        !names.contains(&String::from("authorization")),
        "the credential is never a candidate"
    );
    assert!(
        !names.contains(&String::from("x-custom")),
        "no other inbound header is admitted by the suspension"
    );
}

#[test]
fn the_handshake_judgement_names_only_the_101_as_taken() {
    let mut handshake = oagw::domain::stream::UpgradeHandshake::build(
        stream::upgrade_detection("GET", Some("websocket"), Some("upgrade"))
            .expect("the three parts hold"),
        &context("GET", &[("Upgrade", "websocket"), ("Connection", "upgrade")]),
    );
    assert_eq!(handshake.answer, stream::UpgradeAnswer::NotJudged);
    handshake.judge(101);
    assert_eq!(handshake.answer, stream::UpgradeAnswer::Taken);
    handshake.judge(200);
    assert_eq!(handshake.answer, stream::UpgradeAnswer::NotTaken);
}

// @cpt-dod:cpt-cf-oagw-dod-stream-timeouts:p1

#[test]
fn the_idle_timeout_is_sixty_seconds_and_reached_by_no_configuration() {
    assert_eq!(IDLE_TIMEOUT_SECS, 60);
    assert_eq!(IDLE_TIMEOUT, Duration::from_secs(60));
    // No key of the configuration surface carries the value, and no upstream or
    // route configuration reaches it either: the session reads it from the
    // constant and from nothing else.
    let session = oagw::domain::stream::StreamSession::open_for_incremental(TENANT, UPSTREAM, None);
    assert_eq!(session.idle_timeout, Duration::from_secs(60));
    let handshake = oagw::domain::stream::StreamSession::open_for_handshake(TENANT, UPSTREAM);
    assert_eq!(handshake.idle_timeout, Duration::from_secs(60));
}

// @cpt-dod:cpt-cf-oagw-dod-stream-errors:p1

#[test]
fn the_transfer_mode_has_two_values_and_no_third() {
    // The mode is selected from the request and the response headers alone.
    let detection = stream::upgrade_detection("GET", Some("websocket"), Some("upgrade"));
    assert_eq!(
        stream::select_mode(detection, 101, None).0,
        TransferMode::Tunnel,
        "a 101 to a detected upgrade is a tunnel"
    );
    assert_eq!(
        stream::select_mode(None, 101, Some("text/event-stream")).0,
        TransferMode::Incremental,
        "a 101 without the detection is a body transfer"
    );
    for content_type in [
        Some("text/event-stream"),
        Some("application/json"),
        Some("application/grpc+proto"),
        None,
    ] {
        assert_eq!(
            stream::select_mode(detection, 200, content_type).0,
            TransferMode::Incremental,
            "every body that is not a tunnel is incremental"
        );
    }
}

#[test]
fn the_mode_carries_the_answer_or_the_content_type() {
    let (mode, carry) = stream::select_mode(
        stream::upgrade_detection("GET", Some("websocket"), Some("upgrade")),
        101,
        None,
    );
    assert_eq!(mode, TransferMode::Tunnel);
    assert_eq!(carry, Some(stream::SessionCarry::Answer(101)));

    let (mode, carry) = stream::select_mode(None, 200, Some("text/event-stream"));
    assert_eq!(mode, TransferMode::Incremental);
    assert_eq!(
        carry,
        Some(stream::SessionCarry::ContentType(String::from(
            "text/event-stream"
        )))
    );

    let (_, carry) = stream::select_mode(None, 200, None);
    assert!(carry.is_none(), "no content type is recorded when none is named");
}

#[test]
fn the_two_error_answers_carry_their_types_and_no_retry_after() {
    let stalled = stream::answer_of(StreamOutcome::Stalled).expect("the stall is answered");
    assert_eq!(stalled.kind, ErrorKind::IdleTimeout);
    assert_eq!(stalled.kind.http_status(), 504);
    assert_eq!(
        stalled.kind.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.timeout.idle.v1"
    );
    assert_eq!(stalled.source, oagw::domain::ErrorSource::Gateway);
    assert!(
        stalled.context.retry_after_seconds.is_none(),
        "the idle answer carries no Retry-After, although the row is retriable"
    );

    let aborted = stream::answer_of(StreamOutcome::Aborted).expect("the abort is answered");
    assert_eq!(aborted.kind, ErrorKind::StreamAborted);
    assert_eq!(aborted.kind.http_status(), 502);
    assert_eq!(
        aborted.kind.gts_type(),
        "gts.cf.core.errors.err.v1~cf.oagw.stream.aborted.v1"
    );
    assert!(aborted.context.retry_after_seconds.is_none());
    assert_ne!(
        aborted.kind, ErrorKind::DownstreamError,
        "the downstream variant is never used for a mid-flight termination"
    );

    for outcome in [
        StreamOutcome::ClientDisconnected,
        StreamOutcome::UpstreamClosed,
    ] {
        assert!(
            stream::answer_of(outcome).is_none(),
            "a clean teardown direction is not an error answer"
        );
    }
}

// @cpt-dod:cpt-cf-oagw-dod-stream-lifecycle:p1

#[test]
fn every_declared_transition_is_taken() {
    // Opening to Open, on a 101 for a tunnel.
    let lifecycle = StreamLifecycle::Opening
        .opened()
        .expect("the handshake was taken up");
    assert_eq!(lifecycle, StreamLifecycle::Open);
    // Opening to Closed, on a refusal or a failure before data.
    assert_eq!(
        StreamLifecycle::Opening.refused().expect("the handshake was refused"),
        StreamLifecycle::Closed
    );
    // Open to Closing, when one side signals the end.
    let closing = StreamLifecycle::Open.closing().expect("one side ended");
    assert_eq!(closing, StreamLifecycle::Closing);
    // Closing to Closed, when the other half is torn down.
    assert_eq!(
        closing.closed().expect("the other half is torn down"),
        StreamLifecycle::Closed
    );
    // Open to Closed, on a mid-flight abort.
    assert_eq!(
        StreamLifecycle::Open.aborted().expect("a half failed"),
        StreamLifecycle::Closed
    );
}

#[test]
fn every_invalid_transition_is_refused() {
    // Closed is terminal.
    let closed = StreamLifecycle::Closed;
    for attempt in [closed.opened(), closed.closing(), closed.closed(), closed.aborted()] {
        assert!(attempt.is_err(), "no transition leaves Closed");
    }
    assert!(
        StreamLifecycle::Closing.opened().is_err(),
        "a closing session cannot be reopened"
    );
    assert!(
        StreamLifecycle::Closing.closing().is_err(),
        "a session that is closing does not close twice over Closing"
    );
    assert!(
        StreamLifecycle::Closing.aborted().is_err(),
        "a closing session is not aborted, because it is already draining"
    );
    assert!(
        StreamLifecycle::Opening.closing().is_err(),
        "a session that never opened takes the refusal transition instead"
    );
}

#[test]
fn a_closed_session_is_never_left_with_an_open_half() {
    let mut session = oagw::domain::stream::StreamSession::open_for_handshake(TENANT, UPSTREAM);
    session.refuse();
    assert_eq!(session.lifecycle, StreamLifecycle::Closed);
    assert!(!session.caller.open);
    assert!(!session.upstream.open);
    assert_eq!(session.outcome, Some(StreamOutcome::Aborted));
}

#[test]
fn the_teardown_directions_move_through_closing_to_closed() {
    let mut client_first =
        oagw::domain::stream::StreamSession::open_for_incremental(TENANT, UPSTREAM, None);
    client_first.disconnect();
    assert_eq!(client_first.lifecycle, StreamLifecycle::Closed);
    assert_eq!(client_first.outcome, Some(StreamOutcome::ClientDisconnected));
    assert!(!client_first.upstream.open, "the upstream half is closed");
    assert!(!client_first.caller.open, "the caller half is closed");

    let mut upstream_first =
        oagw::domain::stream::StreamSession::open_for_incremental(TENANT, UPSTREAM, None);
    upstream_first.upstream_closed();
    assert_eq!(upstream_first.lifecycle, StreamLifecycle::Closed);
    assert_eq!(upstream_first.outcome, Some(StreamOutcome::UpstreamClosed));

    let mut stalled = oagw::domain::stream::StreamSession::open_for_incremental(TENANT, UPSTREAM, None);
    stalled.stalled();
    assert_eq!(stalled.lifecycle, StreamLifecycle::Closed);
    assert_eq!(stalled.outcome, Some(StreamOutcome::Stalled));
    assert!(
        stream::answer_of(stalled.outcome.expect("the stall is recorded")).is_some(),
        "a stalled stream is answered"
    );

    let mut aborted = oagw::domain::stream::StreamSession::open_for_incremental(TENANT, UPSTREAM, None);
    aborted.abort_transfer();
    assert_eq!(aborted.lifecycle, StreamLifecycle::Closed);
    assert_eq!(aborted.outcome, Some(StreamOutcome::Aborted));
}

#[test]
fn the_bytes_moved_are_counted_once_they_are_written() {
    let mut session = oagw::domain::stream::StreamSession::open_for_incremental(TENANT, UPSTREAM, None);
    session.record_moved(7);
    session.record_moved(3);
    assert_eq!(session.moved, 10, "each byte is counted once it has been written");
}

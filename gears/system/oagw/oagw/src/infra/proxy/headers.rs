//! Header rules (`DESIGN` §"Headers Transformation").
//!
//! Three categories cross the data plane:
//!
//! 1. *routing* headers (`X-OAGW-Target-Host`, `X-OAGW-Error-Source`) are
//!    consumed by the gateway and never forwarded;
//! 2. *hop-by-hop* headers (RFC 9110 §7.6.1) are stripped in both directions,
//!    because the gateway terminates one connection and opens another;
//! 3. *passthrough* headers are forwarded upstream according to
//!    `upstream.headers.request.passthrough`.
//!
//! `Host` is always replaced by the endpoint authority, and the outbound
//! framing headers are recomputed by the transport, which streams the body.

use std::collections::BTreeMap;

use http::HeaderMap;

use crate::domain::model::{Passthrough, RequestHeaderRules, ResponseHeaderRules};

/// `Host`, set from the endpoint authority and never taken from the caller.
pub const HOST_HEADER: &str = "host";

/// Headers the gateway consumes while routing; never forwarded.
pub const ROUTING_HEADERS: &[&str] = &["x-oagw-target-host", "x-oagw-error-source"];

/// Headers that belong to one connection and are not forwarded
/// (RFC 9110 §7.6.1, `DESIGN` §"Headers Transformation").
pub const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Framing headers the transport recomputes for the body it forwards.
pub const FRAMING_HEADERS: &[&str] = &["content-length"];

/// `true` when `name` is a header the gateway consumes or re-frames.
#[must_use]
pub fn is_forwarded(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase();
    !(HOP_BY_HOP.contains(&name.as_str())
        || ROUTING_HEADERS.contains(&name.as_str())
        || FRAMING_HEADERS.contains(&name.as_str()))
}

/// `true` when `headers` ask for a protocol upgrade: `Connection: Upgrade` plus
/// an `Upgrade` header naming a protocol.
#[must_use]
pub fn is_upgrade(headers: &HeaderMap) -> bool {
    let connection = headers
        .get(http::header::CONNECTION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if !connection.split(',').any(|token| token.trim() == "upgrade") {
        return false;
    }
    headers
        .get(http::header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.trim().is_empty())
}

/// The headers an upstream request carries.
///
/// The inbound set is filtered by the forwarding rules, `Host` is set from the
/// endpoint authority, and the `set` and `add` verbs are applied last. An
/// upgrade handshake keeps `Connection` and `Upgrade`, which are otherwise
/// hop-by-hop.
///
/// `produced` names the headers the gateway itself wrote — the credentials an
/// auth plugin forwarded, the values a transform set. `DESIGN` §"Headers
/// Transformation" scopes the passthrough rules to the *inbound* headers, so a
/// produced header is forwarded even when `passthrough` would drop the caller's
/// one of the same name.
#[must_use]
pub fn outbound_request_headers(
    inbound: &BTreeMap<String, String>,
    authority: &str,
    rules: Option<&RequestHeaderRules>,
    preserve_upgrade: bool,
    produced: &std::collections::BTreeSet<String>,
) -> HeaderMap {
    let removed: Vec<String> = rules
        .map(|rules| {
            rules
                .remove
                .iter()
                .map(|name| name.trim().to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default();

    let mut outbound = HeaderMap::new();
    for (name, value) in inbound {
        let key = name.trim().to_ascii_lowercase();
        if key.eq_ignore_ascii_case(HOST_HEADER)
            || removed.iter().any(|drop| drop == &key)
            || FRAMING_HEADERS.contains(&key.as_str())
            || ROUTING_HEADERS.contains(&key.as_str())
        {
            continue;
        }
        if HOP_BY_HOP.contains(&key.as_str())
            && !(preserve_upgrade && matches!(key.as_str(), "connection" | "upgrade"))
        {
            continue;
        }
        if !produced.contains(&key) && !passes_through(rules, &key) {
            continue;
        }
        push(&mut outbound, &key, value);
    }

    push(&mut outbound, "host", authority);
    if let Some(rules) = rules {
        apply_set(&mut outbound, &rules.set);
        apply_add(&mut outbound, &rules.add);
    }
    outbound
}

/// The headers a proxied response carries back to the caller.
///
/// Hop-by-hop and framing headers are dropped — the transport re-frames the
/// streamed body — then `remove`, `set` and `add` are applied.
#[must_use]
pub fn outbound_response_headers(
    upstream: &HeaderMap,
    rules: Option<&ResponseHeaderRules>,
) -> HeaderMap {
    let removed: Vec<String> = rules
        .map(|rules| {
            rules
                .remove
                .iter()
                .map(|name| name.trim().to_ascii_lowercase())
                .collect()
        })
        .unwrap_or_default();

    let mut outbound = HeaderMap::new();
    for (name, value) in upstream {
        let key = name.as_str().to_ascii_lowercase();
        if !is_forwarded(&key) || removed.iter().any(|drop| drop == &key) {
            continue;
        }
        outbound.append(name, value.clone());
    }
    if let Some(rules) = rules {
        apply_set(&mut outbound, &rules.set);
        apply_add(&mut outbound, &rules.add);
    }
    outbound
}

/// `true` when an inbound header named `name` is forwarded upstream.
fn passes_through(rules: Option<&RequestHeaderRules>, name: &str) -> bool {
    let Some(rules) = rules else {
        // No rules at all: the proxy forwards the caller's headers.
        return true;
    };
    match rules.passthrough {
        None | Some(Passthrough::None) => false,
        Some(Passthrough::All) => true,
        Some(Passthrough::Allowlist) => rules
            .passthrough_allowlist
            .iter()
            .any(|allowed| allowed.trim().eq_ignore_ascii_case(name)),
    }
}

/// Add a header, skipping either half of a pair the HTTP grammar refuses.
fn push(outbound: &mut HeaderMap, name: &str, value: &str) {
    if let Ok(name) = http::HeaderName::from_bytes(name.as_bytes())
        && let Ok(value) = http::HeaderValue::from_str(value)
    {
        outbound.append(name, value);
    }
}

/// Overwrite every occurrence of the configured headers.
fn apply_set(outbound: &mut HeaderMap, set: &std::collections::BTreeMap<String, String>) {
    for (name, value) in set {
        if let Ok(name) = http::HeaderName::from_bytes(name.trim().to_ascii_lowercase().as_bytes())
            && let Ok(value) = http::HeaderValue::from_str(value)
        {
            outbound.insert(name, value);
        }
    }
}

/// Append the configured headers, keeping any that is already present.
fn apply_add(outbound: &mut HeaderMap, add: &std::collections::BTreeMap<String, String>) {
    for (name, value) in add {
        if let Ok(name) = http::HeaderName::from_bytes(name.trim().to_ascii_lowercase().as_bytes())
            && let Ok(value) = http::HeaderValue::from_str(value)
        {
            outbound.append(name, value);
        }
    }
}

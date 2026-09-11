//! Header transformation of a proxy exchange.
//!
//! Realizes `cpt-cf-oagw-algo-header-transform`: the pure function that turns
//! the validated `ProxyContext` header map and the resolved upstream's
//! `headers` rules into the `OutboundRequest` header map, and turns the
//! upstream response's headers into the ones the caller receives. It is the
//! response half of `cpt-cf-oagw-fr-header-transform` and the request half of
//! `cpt-cf-oagw-dod-header-transformation`.
//!
//! The four categories of DESIGN §3.2 Headers Transformation are handled in
//! order: the routing header is dropped because endpoint selection already
//! consumed it, the eight hop-by-hop headers are dropped, the
//! `headers.request.passthrough` mode decides what of the remainder is
//! forwarded with the caller's `Authorization` never a candidate, and the
//! `set`/`add`/`remove` rules run after the passthrough decision. The
//! `Host`/`:authority` replacement is the endpoint's, and never the routing
//! function of `X-OAGW-Target-Host`.

use crate::domain::error::{DomainError, ErrorKind};
use crate::domain::proxy::{PluginMutations, ProxyContext, SelectedEndpoint};
use crate::domain::stream::{HANDSHAKE_HEADERS, UpgradeDetection};
use crate::domain::upstream::{HeadersConfig, Passthrough};

/// The port the shipped schema documents as an endpoint's default, which an
/// authority does not restate.
const DEFAULT_ENDPOINT_PORT: u16 = 443;

// @cpt-dod:cpt-cf-oagw-dod-header-transformation:p1

/// Builds the outbound request header map.
///
/// `plugin_headers` are the entries the plugin chain added or mutated during
/// the request phase, carried in after the configuration rules so a plugin sees
/// the transformed request and not the inbound one.
///
/// `handshake` is the upgrade detection `cpt-cf-oagw-feature-streaming` made
/// before this routine ran: a detected upgrade request carries the suspension
/// of two of the eight hop-by-hop headers and the admission of the handshake's
/// own request headers, and every other request carries none.
///
/// # Errors
///
/// Returns the `ValidationError` failure when the resulting map carries a
/// value with CR or LF, or an invalid well-known header for the direction.
#[allow(clippy::result_large_err)]
pub fn transform_request(
    context: &ProxyContext,
    headers: &HeadersConfig,
    selected: &SelectedEndpoint,
    mutations: &PluginMutations,
    handshake: Option<UpgradeDetection>,
) -> Result<Vec<(String, String)>, DomainError> {
    let rules = headers.request.clone().unwrap_or_default();
    let mut outbound: Vec<(String, String)> = Vec::new();

    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-routing
    // The routing header is consumed by endpoint selection and never forwarded.
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-routing

    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-hop
    // The eight hop-by-hop headers of DESIGN §3.2's table are stripped. The
    // exception that suspends two of them belongs to the streaming feature and
    // reaches this routine only as the suspension a detected upgrade request
    // carries, so a plain request/response exchange is stripped over all eight.
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-hop
    let suspended: &[&str] = if handshake.is_some() {
        &["upgrade", "connection"]
    } else {
        &[]
    };

    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-passthrough
    // `none`, the shipped-schema default, forwards no inbound header;
    // `allowlist` forwards exactly the names of `passthrough_allowlist`; `all`
    // forwards the remainder. The caller's `Authorization` value is never a
    // passthrough candidate in any mode: the platform middleware consumed it
    // before this routine ran, and no step of the flow reads it again.
    let mode = rules.passthrough.unwrap_or(Passthrough::None);
    let mut allowed: Vec<String> = Vec::new();
    if mode == Passthrough::Allowlist {
        allowed = rules
            .passthrough_allowlist
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect();
    }
    for (name, value) in &context.headers {
        let lowered = name.to_ascii_lowercase();
        if lowered == "x-oagw-target-host" || lowered == "authorization" {
            continue;
        }
        // @cpt-begin:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-six
        // The other six hop-by-hop headers stay stripped exactly as the
        // unconditional rule strips them, because an upgrade changes the
        // meaning of neither, so only the two the suspension names are kept.
        if super::validate::HOP_BY_HOP.contains(&lowered.as_str())
            && !suspended.contains(&lowered.as_str())
        {
            continue;
        }
        // @cpt-end:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-six
        // @cpt-begin:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-sec
        // The WebSocket handshake's own request headers reach the upstream
        // regardless of the resolved `headers.request.passthrough` mode,
        // including at that mode's shipped default of `none`, because they are
        // the fields a handshake is judged by and the default forwards none of
        // them. No other inbound header is admitted by the suspension.
        let admitted = handshake.is_some()
            && (suspended.contains(&lowered.as_str())
                || HANDSHAKE_HEADERS.contains(&lowered.as_str()));
        // @cpt-end:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-sec
        let forwarded = admitted
            || match mode {
                Passthrough::None => false,
                Passthrough::Allowlist => allowed.contains(&lowered),
                Passthrough::All => true,
            };
        if forwarded {
            outbound.push((name.clone(), value.clone()));
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-passthrough

    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-rules
    // The rules run in set, add, remove order, so a set overwrites and an add
    // appends.
    // @cpt-begin:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-rules
    // The configured rules and the authority replacement are applied to a
    // handshake request exactly as to any other, so the handshake request is
    // transformed exactly as a non-upgrade request would be apart from the
    // suspension.
    for (name, value) in &rules.set {
        set_header(&mut outbound, name, value);
    }
    for (name, value) in &rules.add {
        add_header(&mut outbound, name, value);
    }
    for name in &rules.remove {
        remove_header(&mut outbound, name);
    }
    // @cpt-end:cpt-cf-oagw-algo-upgrade-handshake:p1:inst-uh-rules
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-rules

    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-host
    // The endpoint's host replaces `Host` on HTTP/1.1 and `:authority` on
    // HTTP/2; the two are the same replacement at the two protocol layers, and
    // neither ever replaces the routing function of the target-host header.
    let authority = authority_of(&selected.endpoint);
    set_header(&mut outbound, "host", &authority);
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-host

    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-plugin-loop
    for (name, value) in &mutations.set {
        // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-plugin
        set_header(&mut outbound, name, value);
        // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-plugin
    }
    for name in &mutations.removed {
        remove_header(&mut outbound, name);
    }
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-plugin-loop

    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-invalid-if
    if let Some(defect) = invalid_header(&outbound) {
        // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-invalid-return
        return Err(invalid_error(&defect));
        // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-invalid-return
    }
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-invalid-if

    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-invalid-else
    // The ELSE of the validity check: every header the outbound map carries is
    // a well-formed name and value, so the request is forwarded as built.
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-invalid-else

    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-return
    Ok(outbound)
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-return
}

/// Applies the `headers.response` rules to an upstream response's headers and
/// carries the response-phase plugin mutations.
///
/// The upstream's `content-length` and `transfer-encoding` are dropped with the
/// framing: the gateway buffers the body and re-states the length itself, so a
/// declared length that no longer describes the body it travels with is never
/// emitted. The `X-OAGW-Error-Source` tag the classification adds is not a
/// `headers.response` concern and is never removed here.
#[must_use]
pub fn transform_response(
    upstream_headers: &[(String, String)],
    headers: &HeadersConfig,
    mutations: &PluginMutations,
) -> Vec<(String, String)> {
    // @cpt-begin:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-response
    let rules = headers.response.clone().unwrap_or_default();
    let mut outbound: Vec<(String, String)> = upstream_headers
        .iter()
        .filter(|(name, _)| {
            let lowered = name.to_ascii_lowercase();
            lowered != "content-length" && lowered != "transfer-encoding"
        })
        .cloned()
        .collect();
    for (name, value) in &rules.set {
        set_header(&mut outbound, name, value);
    }
    for (name, value) in &rules.add {
        add_header(&mut outbound, name, value);
    }
    for name in &rules.remove {
        remove_header(&mut outbound, name);
    }
    for (name, value) in &mutations.set {
        set_header(&mut outbound, name, value);
    }
    for name in &mutations.removed {
        remove_header(&mut outbound, name);
    }
    outbound
    // @cpt-end:cpt-cf-oagw-algo-header-transform:p1:inst-hdr-response
}

/// Overwrites every entry of one name, keeping the position of the first.
fn set_header(map: &mut Vec<(String, String)>, name: &str, value: &str) {
    let mut written = false;
    map.retain_mut(|entry| {
        if entry.0.eq_ignore_ascii_case(name) {
            if written {
                return false;
            }
            entry.1 = String::from(value);
            written = true;
        }
        true
    });
    if !written {
        map.push((String::from(name), String::from(value)));
    }
}

/// Appends one entry under a name that may already be present.
fn add_header(map: &mut Vec<(String, String)>, name: &str, value: &str) {
    map.push((String::from(name), String::from(value)));
}

/// Removes every entry of one name.
fn remove_header(map: &mut Vec<(String, String)>, name: &str) {
    map.retain(|entry| !entry.0.eq_ignore_ascii_case(name));
}

/// The first invalid entry of the map, if the map carries one.
///
/// A CR or LF in a value is a header-injection vector in every direction; a
/// `Host` or `:authority` value that is not a valid authority is the invalid
/// well-known header of DESIGN §3.2's rule for the request direction.
fn invalid_header(map: &[(String, String)]) -> Option<String> {
    for (name, value) in map {
        if value.contains('\r') || value.contains('\n') || value.contains('\0') {
            return Some(format!("{name} carries a control character in its value"));
        }
        let lowered = name.to_ascii_lowercase();
        if (lowered == "host" || lowered == ":authority") && value.trim().is_empty() {
            return Some(format!("{name} is empty"));
        }
    }
    None
}

/// The 400 failure an invalid header map answers with.
fn invalid_error(defect: &str) -> DomainError {
    let mut error = DomainError::gateway(
        ErrorKind::ValidationError,
        "the transformed header map is not valid for the direction",
    );
    error.detail = String::from(defect);
    error
}

/// The authority the selected endpoint is addressed by.
///
/// The shipped schema documents an endpoint port default of `443`, so a port
/// that carries the default is not restated in the authority and any other is.
#[must_use]
fn authority_of(endpoint: &crate::domain::Endpoint) -> String {
    match endpoint.port {
        Some(port) if port != DEFAULT_ENDPOINT_PORT => {
            format!("{}:{port}", endpoint.host.as_str())
        }
        _ => String::from(endpoint.host.as_str()),
    }
}

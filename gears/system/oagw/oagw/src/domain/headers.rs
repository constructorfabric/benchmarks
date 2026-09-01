//! Header transformation rules (DESIGN §3.1 "Headers Transformation",
//! §3.2 "Transformation Rules").
//!
//! Three categories are handled when building the outbound request:
//!
//! 1. **Routing headers** — consumed by OAGW (`X-OAGW-Target-Host`, `Host`)
//!    and never forwarded verbatim.
//! 2. **Hop-by-hop headers** — stripped per RFC 9110 §7.6.1.
//! 3. **Passthrough headers** — forwarded according to `headers.request`
//!    (`none` / `allowlist` / `all`).
//!
//! The response direction has no passthrough switch in
//! `docs/schemas/upstream.v1.schema.json`: the upstream response is forwarded
//! minus the hop-by-hop set and minus the configured `remove` list, then
//! `set`/`add` are applied.
//!
//! `Content-Type` is forwarded even under `passthrough: none`. It describes
//! the payload that is being forwarded unchanged, so dropping it would turn
//! every default-configured request into an unparsable body; DESIGN §3.1
//! requires well-known headers such as `Content-Type` and `Content-Length` to
//! be "validated, set or adjusted" rather than dropped.

use crate::domain::models::{HeaderPassthrough, HeaderRules};

/// `X-OAGW-Target-Host`, read by routing then stripped.
pub const TARGET_HOST_HEADER: &str = "x-oagw-target-host";

/// Headers stripped from both directions (DESIGN §3.1 hop-by-hop table).
pub const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Inbound headers OAGW consumes for its own routing decisions.
pub const ROUTING_HEADERS: &[&str] = &[TARGET_HOST_HEADER, "host"];

/// Payload-describing headers forwarded regardless of the passthrough mode.
pub const STRUCTURAL_HEADERS: &[&str] = &["content-type"];

/// Whether `name` is stripped from every proxied exchange.
#[must_use]
pub fn is_hop_by_hop(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    HOP_BY_HOP_HEADERS.contains(&lowered.as_str())
}

/// Resolves the effective request passthrough mode.
///
/// * No `headers` block at all → transparent proxy (`all`).
/// * A `headers.request` block without an explicit `passthrough` → the schema
///   default `none`.
#[must_use]
pub fn request_passthrough_mode(rules: Option<&HeaderRules>) -> HeaderPassthrough {
    match rules {
        None => HeaderPassthrough::All,
        Some(rules) => rules.passthrough.unwrap_or(HeaderPassthrough::None),
    }
}

/// Builds the headers of the outbound request.
///
/// `target_authority` is the `host[:port]` of the selected endpoint and
/// replaces the inbound `Host`.
#[must_use]
pub fn build_outbound_request_headers(
    inbound: &[(String, String)],
    target_authority: &str,
    rules: Option<&HeaderRules>,
) -> Vec<(String, String)> {
    let mode = request_passthrough_mode(rules);
    let remove: &[String] = match rules {
        Some(rules) => &rules.remove,
        None => &[],
    };

    let mut outbound: Vec<(String, String)> = Vec::new();
    if mode != HeaderPassthrough::None {
        for (name, value) in inbound {
            let lowered = name.to_ascii_lowercase();
            if is_hop_by_hop(&lowered)
                || ROUTING_HEADERS.contains(&lowered.as_str())
                || remove.iter().any(|dropped| dropped.eq_ignore_ascii_case(&lowered))
            {
                continue;
            }
            if mode == HeaderPassthrough::Allowlist && !is_allowlisted(rules, &lowered) {
                continue;
            }
            push_header(&mut outbound, &lowered, value);
        }
    }

    // Structural headers describe the forwarded payload and survive every
    // mode; they are only added when the passthrough pass did not already
    // forward them.
    for name in STRUCTURAL_HEADERS {
        let forwarded = outbound.iter().any(|(header, _)| header == name);
        if forwarded {
            continue;
        }
        if let Some((_, value)) = inbound
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
        {
            push_header(&mut outbound, name, value);
        }
    }

    if let Some(rules) = rules {
        for (name, value) in &rules.set {
            set_header(&mut outbound, name, value);
        }
        for (name, value) in &rules.add {
            push_header(&mut outbound, &name.to_ascii_lowercase(), value);
        }
        for name in &rules.remove {
            drop_header(&mut outbound, name);
        }
    }

    // `Host` is always replaced by the upstream authority (DESIGN §3.1).
    set_header(&mut outbound, "host", target_authority);
    outbound
}

/// Builds the headers returned to the caller.
#[must_use]
pub fn build_outbound_response_headers(
    upstream_headers: &[(String, String)],
    rules: Option<&HeaderRules>,
) -> Vec<(String, String)> {
    let remove: &[String] = match rules {
        Some(rules) => &rules.remove,
        None => &[],
    };
    let mut outbound: Vec<(String, String)> = Vec::new();
    for (name, value) in upstream_headers {
        let lowered = name.to_ascii_lowercase();
        if is_hop_by_hop(&lowered)
            || remove
                .iter()
                .any(|dropped| dropped.eq_ignore_ascii_case(&lowered))
        {
            continue;
        }
        push_header(&mut outbound, &lowered, value);
    }
    if let Some(rules) = rules {
        for (name, value) in &rules.set {
            set_header(&mut outbound, name, value);
        }
        for (name, value) in &rules.add {
            push_header(&mut outbound, &name.to_ascii_lowercase(), value);
        }
        for name in &rules.remove {
            drop_header(&mut outbound, name);
        }
    }
    outbound
}

/// Renders `rules` as an inline summary for tracing, without logging values.
#[must_use]
pub fn describe_rules(rules: Option<&HeaderRules>) -> String {
    let Some(rules) = rules else {
        return "none".to_owned();
    };
    format!(
        "set={}, add={}, remove={}, passthrough={}",
        rules.set.len(),
        rules.add.len(),
        rules.remove.len(),
        match rules.passthrough {
            Some(mode) => format!("{mode:?}").to_lowercase(),
            None => "none".to_owned(),
        }
    )
}

/// Header names mentioned by the rules, lowercased (used by tests).
#[must_use]
pub fn rule_header_names(rules: &HeaderRules) -> Vec<String> {
    let mut names: Vec<String> = rules
        .set
        .keys()
        .chain(rules.add.keys())
        .map(|name| name.to_ascii_lowercase())
        .collect();
    names.extend(rules.remove.iter().map(|name| name.to_ascii_lowercase()));
    names.extend(
        rules
            .passthrough_allowlist
            .iter()
            .map(|name| name.to_ascii_lowercase()),
    );
    names.sort();
    names.dedup();
    names
}

/// Whether the allowlist admits `name` (an absent allowlist admits nothing).
fn is_allowlisted(rules: Option<&HeaderRules>, name: &str) -> bool {
    rules.is_some_and(|rules| {
        rules
            .passthrough_allowlist
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(name))
    })
}

fn push_header(outbound: &mut Vec<(String, String)>, name: &str, value: &str) {
    outbound.push((name.to_owned(), value.to_owned()));
}

fn set_header(outbound: &mut Vec<(String, String)>, name: &str, value: &str) {
    let lowered = name.to_ascii_lowercase();
    if let Some(slot) = outbound
        .iter_mut()
        .find(|(header, _)| *header == lowered)
    {
        value.clone_into(&mut slot.1);
    } else {
        outbound.push((lowered, value.to_owned()));
    }
}

fn drop_header(outbound: &mut Vec<(String, String)>, name: &str) {
    outbound.retain(|(header, _)| !header.eq_ignore_ascii_case(name));
}

#[cfg(test)]
#[path = "headers_tests.rs"]
mod tests;

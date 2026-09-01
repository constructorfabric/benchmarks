// Created: 2026-08-31 by Constructor Tech
//! Proxy data plane (DESIGN §3.2, §3.5; ADR-0001, ADR-0007).
//!
//! The module is split so that every routing decision is testable without a
//! network:
//!
//! * [`routing`] — alias → upstream, route selection, upstream URL, target
//!   endpoint selection. Pure functions over the domain model.
//! * [`breaker`] — the per-upstream circuit breaker of PRD
//!   `cpt-cf-oagw-nfr-high-availability`: whether a request may be dialled at
//!   all.
//! * [`headers`] — request and response header transformation. Pure functions
//!   over `http` types.
//! * [`chain`]   — tenant-chain resolution behind a narrow seam.
//! * [`ratelimit`] — the token buckets of ADR-0003 and the effective limit of
//!   one request.
//! * [`cors`]   — the built-in CORS handler of ADR-0004: preflight detection,
//!   origin and method enforcement, response headers.
//! * [`plugins`]  — the plugin chain of one request: composition, resolution and
//!   the phase runners of ADR-0002.
//! * [`service`] — the orchestrating [`service::ProxyService`], owning the one
//!   outbound `toolkit_http::HttpClient` and the round-robin counters.
//!
//! # Deviation from the upstream JSON schema
//!
//! `upstream.v1` declares `"passthrough": "none"` as the default of
//! `headers.request`. OAGW follows the schema literally: a `headers.request`
//! object that omits `passthrough` forwards **no** inbound header that is not
//! produced by `set` / `add`. Only a route without a `headers` object at all
//! gets the transparent behaviour of DESIGN §3.2 "Headers Transformation"
//! (forward everything except the hop-by-hop and routing headers).
//!
//! # Known limits of the enforcement
//!
//! * **Unmetered preflights (ADR-0004).** A preflight is answered from the
//!   policy alone and never walks the quota, so a preflight flood reaches the
//!   gateway for free. ADR-0004 leaves preflight metering to the edge controls
//!   this deployment does not have; see [`cors`].
//! * **`ip` is only as honest as its proxy.** The first `x-forwarded-for` hop
//!   is an attacker-supplied value; it is read as an address and length-capped,
//!   and the counters of the unreadable ones share one `unknown` bucket. Behind
//!   a proxy that overwrites the chain it is per client; anywhere else it is a
//!   hint. See [`ratelimit`].
//! * **Bounded quota memory.** The counter map is capped; when it is full the
//!   idle buckets are swept and the surplus callers share one counter until
//!   room returns, so an admitted request is never turned away for lack of
//!   memory. See [`ratelimit`].
//! * **Queue fairness is per key.** A `queue` request takes a per-key gate
//!   before it waits, so a woken waiter claims its token before later arrivals,
//!   but nothing orders two waiters of the same key beyond that. See
//!   [`ratelimit`].

pub mod breaker;
pub mod chain;
pub mod cors;
pub mod headers;
pub mod plugins;
pub mod ratelimit;
pub mod routing;
pub mod service;

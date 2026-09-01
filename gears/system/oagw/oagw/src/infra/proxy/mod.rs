//! The proxy data plane (`DESIGN` §3.2, `infra/proxy/`).
//!
//! The data plane turns `/oagw/v1/proxy/{alias}/{suffix}` into an upstream
//! exchange. The modules are the stages of that path:
//!
//! * [`resolver`] — alias → upstream, walking the tenant chain;
//! * [`endpoint`] — upstream → endpoint, honouring `X-OAGW-Target-Host`;
//! * [`route_match`] — upstream + route table → the route a request belongs to;
//! * [`policy`] — the deployment knobs the path enforces;
//! * [`ratelimit`] — the token-bucket limiter (`ADR`-0003);
//! * [`cors`] — the built-in CORS handler (`ADR`-0004);
//! * [`body`] — the request-body framing checks;
//! * [`headers`] — the header rules of `DESIGN` §"Headers Transformation";
//! * [`ssrf`] — the outbound-target enforcement point;
//! * [`transport`] — the outbound HTTP client (plain HTTP, SSE, WebSocket);
//! * [`forward`] — the pipeline that wires every stage together.
//!
//! The slice plan in `PLAN.md` maps these modules onto the slices of the
//! implementation brief and names the `DESIGN`/`ADR` element each one realises.

pub mod body;
pub mod cors;
pub mod endpoint;
pub mod forward;
pub mod headers;
pub mod policy;
pub mod ratelimit;
pub mod resolver;
pub mod route_match;
pub mod ssrf;
pub mod transport;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod body_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod cors_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod endpoint_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod policy_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod ratelimit_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod resolver_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod route_match_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod ssrf_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod headers_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod transport_tests;

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod forward_tests;

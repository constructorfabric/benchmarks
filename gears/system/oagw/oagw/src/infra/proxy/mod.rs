//! The data-plane adapters the proxy path runs through.
//!
//! `limiter` is the per-instance limiter registry of the rate-limiting
//! feature; `pipeline` is the data-plane proxy pipeline of
//! `cpt-cf-oagw-feature-proxy-pipeline`, the caller that orders the pure
//! decisions of `domain::proxy`, performs the streaming outbound call over the
//! bridge the DESIGN names `DataPlaneServiceImpl` and is the caller of the
//! limiter's check, owning every other stage of the proxy path; `streaming`
//! carries the SSE passthrough handover and the WebSocket tunnel pump of
//! `cpt-cf-oagw-feature-streaming-proxy`.

pub mod limiter;
pub mod pipeline;
pub mod streaming;

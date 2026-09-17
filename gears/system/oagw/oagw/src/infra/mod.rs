//! Infrastructure for the OAGW gear: in-memory persistence, the builtin
//! plugin registry, token-bucket rate limiting, CORS enforcement and the
//! outbound HTTP proxy pipeline.
//!
//! The domain stays storage/runtime-agnostic; every runtime concern
//! (thread-safety, TLS, HTTP forwarding, secret resolution) lives here.

pub mod cors;
pub mod plugins;
pub mod proxy;
pub mod ratelimit;
pub mod storage;

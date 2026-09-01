// Created: 2026-08-29 by Constructor Tech
//! Data-plane proxy engine.

pub mod circuit_breaker;
pub mod headers;
pub mod rate_limiter;
pub mod service;

pub use circuit_breaker::{BreakerCheck, CircuitBreakerRegistry};
pub use headers::{ERROR_SOURCE_HEADER, TARGET_HOST_HEADER};
pub use rate_limiter::{RateDecision, RateKey, RateLimiterRegistry};
pub use service::{DataPlaneService, ProxyBody, ProxyFailure, ProxyOutcome, ProxyRequest};

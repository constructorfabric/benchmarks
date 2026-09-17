//! Policy decisions of the data plane that are *not* routing (DESIGN §3.5
//! "Guard Rules", ADR-0003, ADR-0004).
//!
//! The proxy executes the policies in this order:
//!
//! 1. **CORS preflight** — answered locally, before anything is resolved
//!    ([`cors`]);
//! 2. **rate limiting** — one token bucket per counter key ([`rate_limit`]);
//! 3. **CORS origin/method validation** on the actual request, once the
//!    upstream is known ([`cors`]);
//! 4. the plugin chain ([`crate::infra::plugin`]).
//!
//! Plugins are *not* a policy of this module: CORS is a built-in handler and
//! rate limiting is core data-plane logic by decision of ADR-0002 and
//! ADR-0004 respectively, so neither is reachable through a plugin registry.

pub mod cors;
pub mod rate_limit;

pub use crate::domain::types::CorsConfig;
pub use cors::{
    CorsPreflight, CorsRequest, CorsService, PreflightResponse, cors_response_headers,
    effective_cors, is_preflight, preflight_response,
};
pub use rate_limit::{
    EffectiveRateLimit, RATE_LIMIT_LIMIT_HEADER, RATE_LIMIT_REMAINING_HEADER,
    RATE_LIMIT_RESET_HEADER, RateLimitDecision, RateLimitLimiter, RateLimitRequest,
    RateLimitService, client_ip, counter_key, effective_rate_limit, format_limit, unix_now,
    window_seconds,
};

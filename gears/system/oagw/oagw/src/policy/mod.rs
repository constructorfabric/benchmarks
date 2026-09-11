//! Proxy-time policy engines layered on top of proxy-core: CORS handling
//! and rate limiting.
//!
//! Implemented by DECOMPOSITION entries 2.7 (CORS handling) and 2.8 (rate
//! limiting). This module is a stub aggregator today.

/// Owned by DECOMPOSITION entry 2.7 (cors-handling). Declares the CORS
/// preflight-fast-path, post-resolution origin/method-validation, and
/// post-relay response-header extension points that
/// `crate::proxy::engine` calls as no-ops in this round.
pub(crate) mod cors;

/// Owned by DECOMPOSITION entry 2.8 (rate-limiting). Declares the
/// post-resolution rate-limit-budget extension point that
/// `crate::proxy::engine` calls as a no-op in this round.
pub(crate) mod ratelimit;

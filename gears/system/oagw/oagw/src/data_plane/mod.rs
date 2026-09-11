//! Data Plane of the `oagw` gear.
//!
//! One module per CDSL routine of `cpt-cf-oagw-feature-data-plane-proxy`, free
//! of transport types exactly as `control_plane` is: the handler assembles the
//! transport shapes from the domain entities these routines produce and maps
//! their failures through the foundation's error mapping. The layer holds the
//! two pieces of state `cpt-cf-oagw-adr-state-management` assigns the Data
//! Plane and this feature owns — the L1 configuration cache and the shared
//! outbound client — plus the per-upstream round-robin counter.
//!
//! The request path is, in the order the proxy flow states it:
//! [`cache`] → [`resolve`] → [`match_route`] → [`endpoint`] → [`validate`] →
//! the rate-limit seam → [`execute`] → [`headers`] → [`forward`].

// @cpt-dod:cpt-cf-oagw-dod-low-latency:p1

pub mod cache;
pub mod classify;
pub mod endpoint;
pub mod execute;
pub mod forward;
pub mod headers;
pub mod match_route;
pub mod observability;
pub mod ratelimit;
pub mod resolve;
pub mod sandbox;
pub mod stream;
pub mod validate;

pub use cache::{DpCache, DP_CACHE_CAPACITY};
pub use classify::{classify_upstream, classify_upstream_head};
pub use endpoint::{select_endpoint, RoundRobin};
pub use execute::run_request_phase;
pub use headers::{transform_request, transform_response};
pub use match_route::{match_route, MatchOutcome};
pub use ratelimit::{
    LimitIdentity, LimitVerdict, RateLimitHeaders, RegistryCleanup, RESPONSE_HEADERS_GATE,
    SharedLimits, check, rate_limit_headers, upstream_prefix,
};
pub use resolve::{consume, Resolution};
pub use stream::{incremental, tunnel};
pub use validate::{validate_body, validate_inbound};

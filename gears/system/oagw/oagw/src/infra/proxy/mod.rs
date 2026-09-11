//! Proxy engine of the OAGW gear (entry 2.4).
//!
//! The module is the data plane the FEATURES' proxy-engine contract describes:
//! an in-memory pipeline over one published [`ConfigSnapshot`](crate::infra::storage::ConfigSnapshot)
//! that walks the tenant hierarchy for the addressed alias, matches a route,
//! merges the effective configuration, selects an endpoint of the pool,
//! validates the request framing, transforms the headers, opens the upstream
//! exchange and passes the response through — exactly one outbound request, no
//! retry and no response cache.
//!
//! The stages live in their own submodules and are wired by [`engine`]:
//!
//! | Stage | Module |
//! |---|---|
//! | Request/response context | [`context`] |
//! | Alias walk | [`walk`] |
//! | Route matching | [`route_match`] |
//! | Configuration merge | [`effective`] |
//! | Endpoint selection | [`endpoint`] |
//! | Request and body validation | [`validate`] |
//! | Header transform | [`headers`] |
//! | Upstream call | [`call`] |
//! | Circuit breaker | [`breaker`] |
//! | Response passthrough | [`passthrough`] |
//! | Streamed exchange (entry 2.6) | [`stream`] |
//! | Plugin hook points | [`hooks`] |
//! | Plugin chain execution | [`chain`] |
//! | Credential resolution | [`credentials`] |
//! | Token cache | [`token_cache`] |
//! | Rate limiting | [`rate_limit`] |
//!
//! The pipeline carries no credential material: the request context records
//! routing facts and measurements only, and the effective configuration
//! references plugins by identifier without resolving a secret. The [`chain`]
//! resolves the credentials the merged configuration references, inside the
//! plugins that consume them, and nothing else carries a resolved value.

pub mod breaker;
pub mod call;
pub mod chain;
pub mod context;
pub mod credentials;
pub mod effective;
pub mod endpoint;
pub mod engine;
pub mod headers;
pub mod hooks;
pub mod passthrough;
pub mod rate_limit;
pub mod route_match;
pub mod stream;
pub mod token_cache;
pub mod validate;
pub mod walk;

pub use breaker::{Admission, BreakerState, CallOutcome, CircuitBreaker, Transition};
pub use call::{CallReply, HttpVersion, OutboundRequest, UpstreamCaller};
pub use chain::ChainExecutor;
pub use context::{RequestContext, RequestPhase, ResponseContext, SelectionMethod};
pub use credentials::CredentialSource;
pub use endpoint::{RoundRobin, Selection, TARGET_HOST_HEADER};
pub use engine::{OutcomeKind, ProxyEngine, ProxyOutcome, ProxyRequest};
pub use effective::{EffectiveUpstream, EnforcedConstraint, EnforcedField};
pub use hooks::{HookOutcome, NoPlugins, PluginChains};
pub use passthrough::{Classification, ERROR_SOURCE_GATEWAY, ERROR_SOURCE_UPSTREAM};
pub use rate_limit::{RateDecision, RateLimiter, RateOutcome, RatePlan};
pub use stream::{SseClassifier, SseFacts, StreamReply, inject_upgrade_headers, streamed,
    upgraded, validate_upgrade_headers};
pub use token_cache::{CachedToken, TokenCache};

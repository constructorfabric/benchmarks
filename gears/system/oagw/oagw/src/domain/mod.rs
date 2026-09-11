//! Domain layer of the `oagw` gear.
//!
//! DDD-Light layering: nothing in this module may reference a transport
//! (`http`, `axum`, `hyper`) or persistence (`sqlx`, `sea_orm`) type. HTTP
//! statuses are `u16`, headers are strings, and every type here is marked with
//! `#[toolkit_macros::domain_model]` so a field that pulls in an
//! infrastructure type fails macro expansion.

// @cpt-dod:cpt-cf-oagw-dod-domain-model-types:p1
pub mod alias;
pub mod context;
pub mod cors;
pub mod effective;
pub mod error;
pub mod gear_state;
pub mod observability;
pub mod plugin;
pub mod plugin_contract;
pub mod proxy;
pub mod ratelimit;
pub mod route;
pub mod scheme;
pub mod stream;
pub mod upstream;

pub use alias::{Alias, AliasError, EndpointHost, Hostname};
pub use context::{AuthContext, RequestContext, ResponseContext};
pub use cors::{CorsDecision, CorsDecoration, CorsRefusal, EffectiveCorsPolicy, PreflightAnswer};
pub use effective::{
    AncestorBinding, ChainError, ContributedFamilies, EffectiveAuth, EffectiveCors,
    EffectivePluginChain, EffectiveRateLimit, EffectiveRouteConfig, EffectiveTagSet,
    EffectiveUpstreamConfig, Family, FamilyContribution, FamilyModes, RouteSelector, TenantChain,
};
pub use error::{DomainError, ErrorContext, ErrorKind, ErrorSource, ModelError};
pub use gear_state::{GearFoundationState, InvalidTransition};
pub use observability::{
    AuditEvent, CorrelationContext, CorrelationSource, MetricLabelSet, SamplingDecision,
    AUDIT_EVENTS,
};
pub use plugin::Plugin;
pub use plugin_contract::{
    AuthPlugin, AuthPluginRegistry, GuardDecision, GuardPlugin, GuardPluginRegistry, PluginFailure,
    PluginFamily, PluginPhase, PluginResolveError, SandboxLimits, TransformPlugin,
    TransformPluginRegistry, SANDBOX_LIMITS,
};
pub use ratelimit::{
    AcquireOutcome, BreakerPhase, BudgetAllocation, BudgetMode, BudgetOutcome, CircuitBreakerState,
    EffectiveLimit, LimitLayer, LimitLayers, RateLimiterRegistry, SlidingWindow, TokenBucket,
    allocate_budget, fold, per_common_scale, sliding_window, token_bucket, token_bucket_capped,
    window_millis,
};
pub use proxy::{
    AliasDerivation, EndpointChoice, MatchedRoute, OutboundRequest, ProxyContext, ProxyResponse,
    ResolvedUpstream, RouteCandidate, SelectedEndpoint,
};
pub use route::{GrpcMatch, HttpMatch, MatchConfig, PathSuffixMode, Route};
pub use scheme::Scheme;
pub use stream::{
    answer_of, select_mode, upgrade_detection, HalfSide, HANDSHAKE_HEADERS, IDLE_TIMEOUT,
    IDLE_TIMEOUT_SECS, StreamHalf, StreamLifecycle, StreamOutcome, StreamSession, StreamTransition,
    TransferMode, UpgradeAnswer, UpgradeDetection, UpgradeHandshake,
};
pub use upstream::{
    Algorithm, AuthConfig, Burst, CorsConfig, Endpoint, HeadersConfig, Passthrough, PluginsConfig,
    RateLimitConfig, RateLimitScope, RequestHeaderRules, ResponseHeaderRules, ServerConfig,
    SharingMode, Strategy, Sustained, Upstream, Window,
};

//! `DataPlaneServiceImpl` — the proxy pipeline of entry 2.4
//! (`cpt-cf-oagw-flow-request-proxy-dispatch`).
//!
//! One call runs the whole hot path, in the order the flows fix it:
//!
//! 1. alias resolution through the caller's tenant chain ([`alias_resolver`]);
//! 2. the disabled-upstream gate, which opens no connection;
//! 3. the gRPC gate, which forwards nothing (graded deviation 7);
//! 4. route matching across the tenant chain ([`crate::domain::route_matcher`]);
//! 5. request-surface validation: path suffix, query allowlist, dot segments;
//! 6. the effective-configuration merge (`tenant` over `route` over `upstream`);
//! 7. endpoint selection and the scheme allowlist ([`crate::domain::endpoints`]);
//! 8. the header pipeline ([`crate::domain::headers`]);
//! 9. the upstream call under `proxy_timeout_secs`, with endpoint-level
//!    connection failover and no re-issue of the client request;
//! 10. the response header pipeline and the error-source classification.
//!
//! The plugin chain (2.6) and the rate-limit call-in (2.7) are wired into this
//! pipeline; the CORS call-in (2.8) and the L1 hot-config cache (2.9) are
//! extension points this entry defines and does not fill: the merged
//! [`EffectiveConfig`] is the only configuration view the steps behind it may
//! consume, and the pipeline-boundary observation is exposed and never
//! emitted.
// @cpt-flow:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1
// @cpt-flow:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1
// @cpt-flow:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1
// @cpt-algo:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1
// @cpt-algo:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1
// @cpt-algo:cpt-cf-oagw-algo-request-proxy-timeout:p1
// @cpt-state:cpt-cf-oagw-state-request-proxy-proxy-request:p1

// @cpt-begin:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-1
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-2
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-3
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-4
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-5
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-6
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-7
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-8
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-9
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-1
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-10
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-11
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-12
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-13
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-14
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-15
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-2
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-3
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-4
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-5
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-6
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-7
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-8
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-9
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-timeout:p1:inst-rp-al-timeout-1
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-timeout:p1:inst-rp-al-timeout-2
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-timeout:p1:inst-rp-al-timeout-3
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-timeout:p1:inst-rp-al-timeout-4
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-timeout:p1:inst-rp-al-timeout-5
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-timeout:p1:inst-rp-al-timeout-6
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-timeout:p1:inst-rp-al-timeout-7
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-timeout:p1:inst-rp-al-timeout-8
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-1
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-10
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-11
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-12
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-13
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-2
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-3
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-4
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-5
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-6
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-7
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-8
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-9
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-1
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-10
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-2
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-3
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-4
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-5
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-6
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-7
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-8
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-9
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1:inst-rp-wt-1
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1:inst-rp-wt-2
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1:inst-rp-wt-3
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1:inst-rp-wt-4
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1:inst-rp-wt-5
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1:inst-rp-wt-6
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1:inst-rp-wt-7
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1:inst-rp-wt-8
// @cpt-begin:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-1
// @cpt-begin:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-10
// @cpt-begin:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-2
// @cpt-begin:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-3
// @cpt-begin:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-4
// @cpt-begin:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-5
// @cpt-begin:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-6
// @cpt-begin:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-7
// @cpt-begin:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-8
// @cpt-begin:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-9
//! The rate-limit step sits between the effective-configuration merge (step 6)
//! and the upstream call (step 9): [`crate::infra::proxy::rate_limiter`] owns
//! the counters, [`crate::domain::rate_limit`] owns the algorithms, and this
//! module owns the one call that resolves the key, runs the acquisition and
//! builds the refusal.

use std::sync::Arc;
//
// @cpt-end:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-9
// @cpt-end:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-8
// @cpt-end:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-7
// @cpt-end:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-6
// @cpt-end:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-5
// @cpt-end:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-4
// @cpt-end:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-3
// @cpt-end:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-2
// @cpt-end:cpt-cf-oagw-algo-request-proxy-alias-resolve:p1:inst-rp-al-alias-1
// @cpt-end:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-9
// @cpt-end:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-8
// @cpt-end:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-7
// @cpt-end:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-6
// @cpt-end:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-5
// @cpt-end:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-4
// @cpt-end:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-3
// @cpt-end:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-2
// @cpt-end:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-15
// @cpt-end:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-14
// @cpt-end:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-13
// @cpt-end:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-12
// @cpt-end:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-11
// @cpt-end:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-10
// @cpt-end:cpt-cf-oagw-algo-request-proxy-endpoint-select:p1:inst-rp-al-endpoint-1
// @cpt-end:cpt-cf-oagw-algo-request-proxy-timeout:p1:inst-rp-al-timeout-8
// @cpt-end:cpt-cf-oagw-algo-request-proxy-timeout:p1:inst-rp-al-timeout-7
// @cpt-end:cpt-cf-oagw-algo-request-proxy-timeout:p1:inst-rp-al-timeout-6
// @cpt-end:cpt-cf-oagw-algo-request-proxy-timeout:p1:inst-rp-al-timeout-5
// @cpt-end:cpt-cf-oagw-algo-request-proxy-timeout:p1:inst-rp-al-timeout-4
// @cpt-end:cpt-cf-oagw-algo-request-proxy-timeout:p1:inst-rp-al-timeout-3
// @cpt-end:cpt-cf-oagw-algo-request-proxy-timeout:p1:inst-rp-al-timeout-2
// @cpt-end:cpt-cf-oagw-algo-request-proxy-timeout:p1:inst-rp-al-timeout-1
// @cpt-end:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-9
// @cpt-end:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-8
// @cpt-end:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-7
// @cpt-end:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-6
// @cpt-end:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-5
// @cpt-end:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-4
// @cpt-end:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-3
// @cpt-end:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-2
// @cpt-end:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-13
// @cpt-end:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-12
// @cpt-end:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-11
// @cpt-end:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-10
// @cpt-end:cpt-cf-oagw-flow-request-proxy-plugin-chain:p1:inst-rp-chain-1
// @cpt-end:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-9
// @cpt-end:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-8
// @cpt-end:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-7
// @cpt-end:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-6
// @cpt-end:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-5
// @cpt-end:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-4
// @cpt-end:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-3
// @cpt-end:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-2
// @cpt-end:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-10
// @cpt-end:cpt-cf-oagw-flow-request-proxy-sse-streaming:p1:inst-rp-sse-1
// @cpt-end:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1:inst-rp-wt-8
// @cpt-end:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1:inst-rp-wt-7
// @cpt-end:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1:inst-rp-wt-6
// @cpt-end:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1:inst-rp-wt-5
// @cpt-end:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1:inst-rp-wt-4
// @cpt-end:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1:inst-rp-wt-3
// @cpt-end:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1:inst-rp-wt-2
// @cpt-end:cpt-cf-oagw-flow-request-proxy-webtransport-session:p1:inst-rp-wt-1
// @cpt-end:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-9
// @cpt-end:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-8
// @cpt-end:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-7
// @cpt-end:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-6
// @cpt-end:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-5
// @cpt-end:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-4
// @cpt-end:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-3
// @cpt-end:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-2
// @cpt-end:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-10
// @cpt-end:cpt-cf-oagw-state-request-proxy-proxy-request:p1:inst-rp-st-req-1
//
use std::time::{Duration, Instant};

use async_trait::async_trait;
use toolkit_http::{HttpClient, HttpClientBuilder};
use crate::config::OagwConfig;
use crate::domain::dto::{Endpoint, PathSuffixMode};
use crate::domain::dto::ServerConfig;
use crate::domain::endpoints::{self, SelectedEndpoint};
use crate::domain::error::DomainError;
use crate::domain::gts_helpers::PROTOCOL_GRPC;
use crate::domain::merge::{self, ConfigLayer, EffectiveConfig, OverridePermissions};
use crate::domain::proxy::{
    ErrorSource, PhaseObservation, ProxyBody, ProxyContext, ProxyFailure, ProxyObservation, ProxyResponse,
    RoutingObservation,
    ProxyByteStream, StreamKind, StreamLifecycle,
};
use crate::domain::repo::{RouteRepository, UpstreamRecord, UpstreamRepository};
use crate::domain::route_matcher::{self, CandidateRoute, RouteMatchOutcome};
use crate::domain::services::management::{Actor, AncestorResolver, ManagementAuthorizer};
use crate::domain::services::proxy::DataPlaneService;
use crate::domain::plugin::{compose, ChainLayer};
use crate::domain::proxy::RateLimitObservation;
use crate::domain::rate_limit::{
    counter_key, quota_header_pairs, CounterKeyContext, CounterSpec, RateDecision,
    RateLimitResource, SystemClock,
};
use crate::infra::plugin::executor::{PluginRuntime, ResolvedChain};
use crate::infra::plugin::resolution::TenantChain;
use crate::infra::proxy::alias_resolver;
use crate::infra::proxy::endpoint_selector::EndpointSelector;
use crate::infra::dp_cache::{DpHotConfig, DpValue};
use crate::infra::proxy::rate_limiter::RateLimiterRegistry;
use crate::infra::proxy::stream;

/// The configuration the data plane reads from `OagwConfig`.
///
/// `token_cache_*` is a plugin-runtime setting (entry 2.6) and is not carried.
#[derive(Debug, Clone, Copy)]
pub struct DataPlaneLimits {
    /// `allow_http_upstream`: whether the `http` endpoint scheme is admitted.
    pub allow_http_upstream: bool,
    /// The request body limit, bounded by the 100 MB ceiling.
    pub max_body_size_bytes: u64,
    /// The buffered-exchange timeout in seconds.
    pub proxy_timeout_secs: u64,
}

impl DataPlaneLimits {
    /// The limits of a validated configuration.
    #[must_use]
    pub const fn of(config: &OagwConfig) -> Self {
        Self {
            allow_http_upstream: config.allow_http_upstream,
            max_body_size_bytes: config.max_body_size_bytes,
            proxy_timeout_secs: config.proxy_timeout_secs,
        }
    }
}

/// The hierarchy override permissions the requester holds for the merge
/// (`inst-gf-eff-8`).
///
/// The proxy path consumes the same input the merge engine takes; the seam
/// exists so entry 2.6 or 2.9 can resolve it through `authz_resolver` without
/// the pipeline changing.
#[async_trait]
pub trait HierarchyPermissions: Send + Sync {
    /// The permissions `actor` holds over the ancestor configuration.
    async fn resolve(&self, actor: &Actor) -> OverridePermissions;
}

/// The default: the requester holds none of them, so every `inherit` override
/// of an ancestor layer is refused and every `enforce` value stands.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoHierarchyPermissions;

#[async_trait]
impl HierarchyPermissions for NoHierarchyPermissions {
    async fn resolve(&self, _actor: &Actor) -> OverridePermissions {
        OverridePermissions::NONE
    }
}

/// The data plane the proxy handler dispatches to.
pub struct DataPlaneServiceImpl {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    ancestors: Arc<dyn AncestorResolver>,
    authorizer: Arc<dyn ManagementAuthorizer>,
    hierarchy: Arc<dyn HierarchyPermissions>,
    limits: DataPlaneLimits,
    http: HttpClient,
    selector: EndpointSelector,
    /// The plugin runtime of entry 2.6. `None` only in a data plane built
    /// without one, in which case a composed chain that carries any reference
    /// fails with `PluginNotFound` rather than being silently skipped.
    plugins: Option<Arc<PluginRuntime>>,
    /// The in-memory counter registry of entry 2.7. A data plane is always
    /// built with one; a test replaces it to drive the clock.
    rate_limiters: Arc<RateLimiterRegistry>,
    /// The Data Plane L1 hot-configuration cache of entry 2.9. `None` in a
    /// data plane built without one, which is how the pre-2.9 tests run
    /// (`cpt-cf-oagw-dod-observability-and-state-dp-cache`).
    hot_config: Option<Arc<DpHotConfig>>,
    /// The transport observer of entry 2.9, which the shared client reports the
    /// reachability of every selected endpoint to. `None` in a data plane built
    /// without one, which is how the pre-2.9 tests run.
    transport_observer: Option<Arc<dyn TransportObserver>>,
    /// The per-host connection usage of the shared client's pool
    /// (`inst-os-client-3`).
    pool: PoolUsage,
    /// The per-host connection ceiling the shared client's pool is built with,
    /// read from the client configuration and never from `oagw.config`
    /// (`cpt-cf-oagw-algo-observability-and-state-deployment-mode`).
    pool_max_per_host: u64,
}

/// The per-host connection usage the shared client accumulates while its
/// exchanges are in flight.
#[derive(Default)]
struct PoolUsage {
    in_flight: std::sync::Mutex<std::collections::HashMap<String, u64>>,
}

impl PoolUsage {
    /// Move the in-flight count of `host` by `delta` and return the resulting
    /// `active` count.
    fn move_by(&self, host: &str, delta: i64) -> u64 {
        let mut in_flight = self
            .in_flight
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = in_flight.entry(host.to_owned()).or_insert(0);
        *entry = (*entry as i64 + delta).max(0) as u64;
        let active = *entry;
        // The count is dropped with its key once nothing is in flight, so the
        // map's cardinality follows the hosts actually in use rather than every
        // host the process has ever reported on.
        if active == 0 {
            in_flight.remove(host);
        }
        drop(in_flight);
        active
    }
}

/// The transport reachability one endpoint selection observed
/// (`cpt-cf-oagw-flow-observability-and-state-request-metrics`): the seam the
/// shared client reports through, so `oagw_upstream_available{host, endpoint}`
/// reflects the connection outcome and not the upstream's response status.
pub trait TransportObserver: Send + Sync {
    /// Report the reachability `available` of `endpoint_host` of `upstream_id`,
    /// under the addressed alias `host`.
    fn observed(&self, host: &str, upstream_id: &str, endpoint_host: &str, available: bool);

    /// Report the per-host connection usage of the shared client's pool:
    /// `idle` connections ready for reuse, `active` ones in an exchange and
    /// `max` the pool's per-host ceiling
    /// (`inst-os-client-3`, `inst-os-req-8`).
    fn connections(&self, host: &str, idle: u64, active: u64, max: u64) {
        let _ = (host, idle, active, max);
    }
}

/// One per-host connection an upstream exchange holds, released on every exit
/// path of the exchange.
struct ConnectionGuard<'a> {
    plane: &'a DataPlaneServiceImpl,
    host: String,
}

impl<'a> ConnectionGuard<'a> {
    /// Take one connection for `host` out of the shared client's pool.
    fn take(plane: &'a DataPlaneServiceImpl, host: &str) -> ConnectionGuard<'a> {
        plane.report_pool(host, 1);
        Self { plane, host: host.to_owned() }
    }
}

impl Drop for ConnectionGuard<'_> {
    fn drop(&mut self) {
        self.plane.report_pool(&self.host, -1);
    }
}

impl std::fmt::Debug for DataPlaneServiceImpl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DataPlaneServiceImpl")
            .field("limits", &self.limits)
            .finish()
    }
}

impl DataPlaneServiceImpl {
    /// Build the data plane over the repositories, the tenant-hierarchy
    /// resolver, the authorizer and the gear configuration.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        ancestors: Arc<dyn AncestorResolver>,
        authorizer: Arc<dyn ManagementAuthorizer>,
        limits: DataPlaneLimits,
    ) -> Self {
        let config = toolkit_http::HttpClientConfig::proxy();
        // The pool ceiling is the one the shared client is built with, read
        // here and never from `oagw.config`
        // (`cpt-cf-oagw-algo-observability-and-state-deployment-mode`).
        let pool_max_per_host = config.pool_max_idle_per_host as u64;
        Self {
            upstreams,
            routes,
            ancestors,
            authorizer,
            hierarchy: Arc::new(NoHierarchyPermissions),
            limits,
            http: HttpClientBuilder::with_config(config)
                .build()
                .expect("the proxy client builds"),
            selector: EndpointSelector::new(),
            plugins: None,
            rate_limiters: Arc::new(RateLimiterRegistry::new(Arc::new(SystemClock))),
            hot_config: None,
            transport_observer: None,
            pool: PoolUsage::default(),
            pool_max_per_host,
        }
    }

    /// Install the transport observer of entry 2.9, which the shared client
    /// reports every endpoint reachability outcome to
    /// (`cpt-cf-oagw-dod-observability-and-state-metrics`).
    #[must_use]
    pub fn with_transport_observer(mut self, observer: Arc<dyn TransportObserver>) -> Self {
        self.transport_observer = Some(observer);
        self
    }

    /// Install the Data Plane L1 hot-configuration cache of entry 2.9
    /// (`cpt-cf-oagw-dod-observability-and-state-dp-cache`).
    #[must_use]
    pub fn with_hot_config(mut self, hot_config: Arc<DpHotConfig>) -> Self {
        self.hot_config = Some(hot_config);
        self
    }

    /// The hot-configuration cache the alias resolution reads through, when
    /// the data plane was built with one.
    #[must_use]
    pub fn hot_config(&self) -> Option<Arc<DpHotConfig>> {
        self.hot_config.clone()
    }

    /// Replace the counter registry of entry 2.7, which is how a test drives
    /// the clock the counters refill on.
    #[must_use]
    pub fn with_rate_limiters(mut self, rate_limiters: Arc<RateLimiterRegistry>) -> Self {
        self.rate_limiters = rate_limiters;
        self
    }

    /// The counter registry the rate-limit gate enforces through.
    #[must_use]
    pub fn rate_limiters(&self) -> Arc<RateLimiterRegistry> {
        Arc::clone(&self.rate_limiters)
    }

    /// Install the plugin runtime of entry 2.6
    /// (`cpt-cf-oagw-dod-plugin-system-plugin-traits`).
    #[must_use]
    pub fn with_plugin_runtime(mut self, plugins: Arc<PluginRuntime>) -> Self {
        self.plugins = Some(plugins);
        self
    }

    /// The plugin runtime the exchange executes its chain through.
    #[must_use]
    pub fn plugin_runtime(&self) -> Option<Arc<PluginRuntime>> {
        self.plugins.clone()
    }

    /// Replace the hierarchy-permission seam.
    #[must_use]
    pub fn with_hierarchy_permissions(
        mut self,
        hierarchy: Arc<dyn HierarchyPermissions>,
    ) -> Self {
        self.hierarchy = hierarchy;
        self
    }

    /// The authorizer the permission gate of the handler evaluates through.
    #[must_use]
    pub fn authorizer(&self) -> Arc<dyn ManagementAuthorizer> {
        Arc::clone(&self.authorizer)
    }

    /// The body limit the handler enforces before it buffers the body.
    #[must_use]
    pub const fn max_body_size_bytes(&self) -> u64 {
        self.limits.max_body_size_bytes
    }

    /// The proxy timeout of the buffered exchange.
    #[must_use]
    pub const fn proxy_timeout(&self) -> Duration {
        Duration::from_secs(self.limits.proxy_timeout_secs)
    }

    /// Run one proxied exchange, with an inbound upgrade when the request is a
    /// WebSocket session.
    ///
    /// # Errors
    ///
    /// Returns the [`ProxyFailure`] the handler renders.
    pub async fn execute(
        &self,
        context: ProxyContext,
        upgrade: Option<crate::infra::proxy::upgrade::InboundUpgrade>,
    ) -> Result<ProxyResponse, ProxyFailure> {
        let started = Instant::now();
        let trace_id = context.trace_id.clone();
        let actor = Actor {
            tenant_id: context.tenant_id,
            principal_id: context.principal_id,
        };

        // -- alias resolution -------------------------------------------
        let walk = self.resolve_alias(&actor, &context).await?;
        let selected = walk.upstream().clone();

        // -- the disabled-upstream gate ---------------------------------
        if walk.any_disabled() {
            return Err(ProxyFailure::Domain(DomainError::LinkUnavailable {
                upstream_id: Some(selected.upstream.id.to_string()),
                host: None,
                path: Some(context.request_path()),
                trace_id: trace_id.clone(),
            }));
        }

        // -- the gRPC gate ---------------------------------------------
        if selected.upstream.protocol == PROTOCOL_GRPC {
            return Err(ProxyFailure::Domain(DomainError::ProtocolError {
                upstream_id: Some(selected.upstream.id.to_string()),
                host: None,
                path: Some(context.request_path()),
                trace_id: trace_id.clone(),
            }));
        }

        // -- route matching over the tenant chain ----------------------
        let route_match_started = Instant::now();
        let matched = self.select_route(&walk, &context)?;
        let route = matched.route.clone();
        let route_match_ms = Some(elapsed_ms(route_match_started));

        // -- request-surface validation --------------------------------
        self.validate_surface(&route, &context)?;

        // -- the effective configuration --------------------------------
        let (effective, permissions) = self.merged(&walk, &route, &actor).await;

        // -- the actual-request CORS check ------------------------------
        // Entry 2.8: the origin is matched first and the method second, after
        // the upstream and route are resolved and the effective configuration
        // is merged, and before the plugin chain and the upstream call, so a
        // rejected request never reaches the upstream.
        let cors_gate = match self.enforce_cors(&effective, &context, &trace_id) {
            Ok(gate) => gate,
            Err((refusal, outcome)) => {
                let mut refusal = refusal;
                let error_type = refusal.error.as_ref().map(type_of);
                refusal.observation = ProxyObservation {
                    status: refusal.status,
                    duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    request_size: context.body.len() as u64,
                    response_size: 0,
                    error_type,
                    rate_limit: None,
                    cors: Some(outcome),
                    host: Some(selected.upstream.alias.clone()),
                    route: Some(route.match_.http.as_ref().map_or_else(String::new, |http| http.path.clone())),
                    routing: None,
                    phases: PhaseObservation { route_match_ms, ..PhaseObservation::default() },
                };
                return Ok(refusal);
            }
        };

        // -- endpoint selection and the scheme allowlist ----------------
        let target_host = context
            .headers
            .iter()
            .find(|(name, _)| name == crate::domain::headers::TARGET_HOST_HEADER)
            .map(|(_, value)| value.as_str());
        let selected_endpoint = endpoints::select_endpoint(
            &selected.upstream.server,
            Some(selected.upstream.id.to_string()),
            &context.alias,
            target_host,
            self.limits.allow_http_upstream,
            trace_id.clone(),
            |_length| self.selector.next(selected.upstream.id, &selected.upstream.server.endpoints),
        )
        .map_err(ProxyFailure::Domain)?;

        // -- the rate-limit gate ----------------------------------------
        // Entry 2.7: the decision is taken after the upstream and route are
        // resolved and the effective configuration is merged, and before the
        // upstream call is issued, so a refused request never reaches the
        // upstream. The quota headers of an admitted request are attached to
        // whatever response the exchange produces.
        let gate = match self.enforce_rate_limit(
            &walk,
            &route,
            &actor,
            &permissions,
            &context,
            &trace_id,
        ) {
            Ok(gate) => gate,
            Err((refusal, observation)) => {
                let mut refusal = refusal;
                let error_type = refusal.error.as_ref().map(type_of);
                refusal.observation = ProxyObservation {
                    status: refusal.status,
                    duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    request_size: context.body.len() as u64,
                    response_size: 0,
                    error_type,
                    rate_limit: Some(observation),
                    cors: None,
                    host: Some(selected.upstream.alias.clone()),
                    route: Some(route.match_.http.as_ref().map_or_else(String::new, |http| http.path.clone())),
                    routing: None,
                    phases: PhaseObservation { route_match_ms, ..PhaseObservation::default() },
                };
                return Ok(refusal);
            }
        };

        // -- the header pipeline ----------------------------------------
        let outbound = crate::domain::headers::build_request_headers(
            &context.headers,
            effective.headers.as_ref(),
            &selected_endpoint.endpoint,
            upgrade.is_some(),
        )
        .map_err(ProxyFailure::Domain)?;

        // -- the plugin chain -------------------------------------------
        // The chain is composed and resolved before the request phases run, and
        // the same resolved chain serves the response phases, so one request
        // resolves its whole composed chain against one consistent view.
        // The route is read under the tenant of the level that matched it: the
        // matcher records the distance of the owning upstream, and distances
        // are unique along the walk.
        let route_tenant_id = walk
            .levels
            .iter()
            .find(|level| level.distance == matched.tenant_distance)
            .map(|level| level.tenant_id)
            .unwrap_or(actor.tenant_id);
        let mut resolved = self
            .plugin_chain(&walk, &route, &effective, route_tenant_id)
            .await?;
        let plugin_request_started = Instant::now();
        let (outbound, query) = self
            .run_plugin_request(
                &mut resolved,
                &context,
                &selected_endpoint,
                outbound,
                trace_id.as_deref().unwrap_or(""),
            )
            .await?;
        let plugin_chain_request_ms = Some(elapsed_ms(plugin_request_started));
        // The query surface the plugins extended is what the upstream URL is
        // built from, so the transformed context, not the inbound one, is
        // handed to the upstream call.
        let mut proxied = context.clone();
        proxied.query = query;

        // -- the upstream call ------------------------------------------
        let pool = walk.upstream().upstream.server.clone();
        let upstream_started = Instant::now();
        let mut response = self
            .forward(
                &proxied,
                &effective,
                &selected_endpoint,
                &selected.upstream.id.to_string(),
                pool,
                outbound,
                upgrade,
            )
            .await?;
        let upstream_call_ms = Some(elapsed_ms(upstream_started));
        let plugin_response_started = Instant::now();
        self.run_plugin_response(&mut resolved, &mut response, trace_id.as_deref().unwrap_or(""))
            .await?;
        let plugin_chain_response_ms = Some(elapsed_ms(plugin_response_started));
        // The CORS response headers ride on the response the caller receives:
        // `Vary: Origin` is appended to the list the upstream set rather than
        // overwriting it, and the computed `Access-Control-Allow-Origin` is set
        // only when the upstream did not set one
        // (`cpt-cf-oagw-algo-cors-response-headers` steps 4 and 5).
        apply_cors_gate(&mut response, &cors_gate);
        // The quota headers ride on the response the caller receives, and the
        // rate-limit outcome rides on the pipeline-boundary observation entry
        // 2.9 consumes.
        append_headers(&mut response, &gate.headers);
        let observation = ProxyObservation {
            status: response.status,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            request_size: context.body.len() as u64,
            response_size: 0,
            error_type: response.error.as_ref().map(|error| type_of(error)),
            rate_limit: gate.observation,
            cors: None,
            host: Some(selected.upstream.alias.clone()),
            route: Some(route.match_.http.as_ref().map_or_else(String::new, |http| http.path.clone())),
            routing: Some(RoutingObservation {
                upstream_id: selected.upstream.id.to_string(),
                endpoint_host: endpoint_host(&selected_endpoint.endpoint),
                selection_method: selected_endpoint.method,
                target_host_used: target_host.is_some(),
            }),
            phases: PhaseObservation {
                route_match_ms,
                plugin_chain_request_ms,
                upstream_call_ms,
                plugin_chain_response_ms,
                response_ms: Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
            },
        };
        Ok(ProxyResponse { observation, ..response })
    }

    // -- the alias resolution of entry 2.9 --------------------------
    /// Resolve the alias through the tenant chain, reading the Data Plane L1
    /// hot-configuration cache at every level
    /// (`cpt-cf-oagw-flow-observability-and-state-dp-cache-read`,
    /// `inst-os-algo-key-4c`).
    ///
    /// The cached entry is keyed `upstream:{tenant_id}:{alias}` — the one
    /// documented family the alias lookup expresses, with `tenant_id` the
    /// **owning** tenant of the level — so every level of the walk is one
    /// independent entry and a hit still reproduces the whole level list the
    /// route match and the merge consume. A route-match scan is not one of the
    /// three families and bypasses the cache (`inst-os-algo-key-5`).
    async fn resolve_alias(
        &self,
        actor: &Actor,
        context: &ProxyContext,
    ) -> Result<alias_resolver::AliasWalk, ProxyFailure> {
        let alias = crate::domain::alias::normalize_alias(&context.alias);
        if alias.is_empty() {
            return Err(ProxyFailure::Domain(DomainError::NotFound { resource_type: "upstream" }));
        }
        let mut chain = vec![actor.tenant_id];
        chain.extend(
            self.ancestors
                .ancestors(actor, actor.tenant_id)
                .await
                .map_err(ProxyFailure::Domain)?,
        );
        let mut levels = Vec::with_capacity(chain.len());
        for (distance, tenant_id) in chain.into_iter().enumerate() {
            let record = self.level_record(tenant_id, &alias)?;
            levels.push(alias_resolver::AliasLevel { tenant_id, distance, record });
        }
        let Some(selected) = levels.iter().position(|level| level.record.is_some()) else {
            return Err(ProxyFailure::Domain(DomainError::NotFound { resource_type: "upstream" }));
        };
        Ok(alias_resolver::AliasWalk { levels, selected })
    }

    /// The upstream one tenant level holds under an alias, read from the Data
    /// Plane L1 cache first and from the Control Plane repository — itself the
    /// caching decorator — on a miss.
    fn level_record(
        &self,
        tenant_id: uuid::Uuid,
        alias: &str,
    ) -> Result<Option<UpstreamRecord>, ProxyFailure> {
        let Some(cache) = &self.hot_config else {
            return self.store_record(tenant_id, alias);
        };
        if let Some(record) = cache.get_upstream(tenant_id, alias) {
            return Ok(Some((*record).clone()));
        }
        // The dependency generations are sampled *before* the store read: the
        // guard `put` runs then compares the generations the resolution
        // observed against the store's current ones, so a population that raced
        // a flush cannot insert a pre-write value.
        let key = DpHotConfig::upstream_key(tenant_id, alias);
        let observed = cache.observe(std::slice::from_ref(&key));
        let Some(record) = self.store_record(tenant_id, alias)? else {
            return Ok(None);
        };
        cache.put(key, DpValue::Upstream(Arc::new(record.clone())), observed);
        Ok(Some(record))
    }

    /// The Control Plane read of one level, with the not-found outcome that
    /// contributes no level.
    fn store_record(
        &self,
        tenant_id: uuid::Uuid,
        alias: &str,
    ) -> Result<Option<UpstreamRecord>, ProxyFailure> {
        match self.upstreams.get_by_alias(tenant_id, alias) {
            Ok(record) => Ok(Some(record)),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(ProxyFailure::Domain(error)),
        }
    }

    // -- the rate-limit gate of entry 2.7 ---------------------------
    // @cpt-begin:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-1
    // `inst-rl-chk-1` .. `-15`, `inst-rl-eff-1` .. `-9`: the decision is a
    // step of the proxy pipeline, taken after the upstream and route are
    // resolved and the effective configuration is merged, and before the
    // upstream call is issued. It is served from in-memory state with no
    // control-plane call per request, and it alters neither the plugin
    // execution order nor the request-validation behavior of entries 2.4 and
    // 2.6. A request whose resolved configuration carries no `rate_limit`
    // proceeds with no counter consulted, no usage observed and no quota
    // headers added (`inst-rl-chk-3`/`-4`, `inst-rl-eff-8`).
    //
    // The counter key is derived once per request and is the only key-sized
    // allocation the hot path makes (`inst-rl-key-9`,
    // `cpt-cf-oagw-dod-rate-limiting-hot-path-cost`); the registry holds it
    // under its own per-key lock, so the check-and-deduct of one counter is
    // atomic (`inst-rl-scp-10`).
    fn enforce_rate_limit(
        &self,
        walk: &alias_resolver::AliasWalk,
        route: &crate::domain::dto::Route,
        actor: &Actor,
        permissions: &OverridePermissions,
        context: &ProxyContext,
        trace_id: &Option<String>,
    ) -> Result<RateLimitGate, (ProxyResponse, RateLimitObservation)> {
        // `inst-rl-eff-8`: no layer contributes a limit, so the limiter is
        // inactive for this request.
        let Some(config) = self.rate_limit_view(walk, route, actor, permissions) else {
            return Ok(RateLimitGate::inactive());
        };

        let config = &config;
        let selected = walk.upstream();
        // `inst-rl-key-1`: the owning resource identity leads the key, so an
        // upstream-level and a route-level counter for the same caller are
        // distinct.
        let resource = if route.rate_limit.is_some() {
            RateLimitResource::Route { route_id: route.id.to_string() }
        } else {
            RateLimitResource::Upstream { upstream_id: selected.upstream.id.to_string() }
        };
        let key = counter_key(&CounterKeyContext {
            resource: &resource,
            scope: config.scope,
            tenant_id: &context.tenant_id.to_string(),
            principal_id: Some(&context.principal_id.to_string()),
            peer_addr: context.peer_addr.as_deref(),
            route_id: Some(&route.id.to_string()),
        });
        let spec = CounterSpec::of(config);
        // `inst-rl-chk-5`/`-6`: the acquisition, refill-on-read under
        // `token_bucket` and admission against the trailing window under
        // `sliding_window`.
        let decision = self.rate_limiters.acquire(&key, &spec);

        let observation = RateLimitObservation {
            host: context.alias.clone(),
            path: route_pattern(route),
            refused: !decision.allowed,
            usage_ratio_parts_per_million: usage_ratio_parts_per_million(decision.usage_ratio),
            retry_after_seconds: decision.retry_after_seconds,
        };
        if !decision.allowed {
            // `inst-rl-chk-11` .. `-14`: the only executable strategy is
            // `reject`, so a configured `queue` or `degrade` resolves to the
            // `reject` outcome and the upstream is never called.
            return Err((rate_limit_refusal(config, &decision, observation.clone(), trace_id), observation));
        }
        // `inst-rl-chk-8`/`-9`: the quota headers ride on the response, and the
        // proxy path continues toward the upstream call.
        let headers = if config.response_headers {
            quota_header_pairs(&decision)
        } else {
            Vec::new()
        };
        Ok(RateLimitGate { headers, observation: Some(observation) })
    }
    // @cpt-end:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-1

    // -- the actual-request CORS gate of entry 2.8 ------------------
    // @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-1
    // `inst-cors-act-1` .. `-15`, `inst-cors-alg-ev-1` .. `-9`,
    // `inst-cors-alg-hd-1` .. `-6`, `inst-cors-hier-1` .. `-7`: the check runs
    // after the alias has been resolved across the tenant hierarchy and the
    // route has been matched — the merged effective configuration is the CORS
    // configuration the merge engine produced — and before the plugin chain and
    // the upstream call. Only a request carrying an `Origin` is a CORS subject;
    // the origin is matched first and the method second, so a disallowed origin
    // is never reported as a method failure; a disabled configuration forwards
    // the request with no `Access-Control-*` header while `Vary: Origin` is
    // still appended; and a merged configuration combining credentials with a
    // wildcard origin is fail-closed rather than served, persisting no merge
    // result (`inst-cors-hier-5`/`-6`). No CORS outcome is recorded for a
    // forwarded request: the four labels this feature supplies are the preflight
    // short-circuit and the three rejections.
    fn enforce_cors(
        &self,
        effective: &EffectiveConfig,
        context: &ProxyContext,
        trace_id: &Option<String>,
    ) -> Result<CorsGate, (ProxyResponse, crate::domain::cors::CorsObservation)> {
        let origin = context
            .headers
            .iter()
            .find(|(name, _)| name == ORIGIN_HEADER)
            .map(|(_, value)| value.as_str());
        let evaluation = crate::domain::cors::evaluate(
            effective.cors.as_ref(),
            origin,
            &context.method,
            Some(&context.request_path()),
            trace_id.as_deref(),
        );
        if let Some(error) = evaluation.rejection {
            let outcome = crate::domain::cors::CorsObservation {
                outcome: evaluation.outcome.unwrap_or(
                    crate::domain::cors::CorsOutcome::OriginNotAllowed,
                ),
            };
            // `Vary: Origin` is rendered by the shared error contract, which
            // stamps it on a CORS rejection of its own.
            return Err((ProxyResponse::gateway_error(error, CORS_REJECTION_STATUS), outcome));
        }
        Ok(CorsGate { headers: evaluation.headers, vary: evaluation.vary })
    }
    // @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-1

    // -- the preflight call-in of entry 2.8 -------------------------
    // @cpt-begin:cpt-cf-oagw-flow-cors-preflight:p1:inst-cors-pf-1
    // `inst-cors-pf-1` .. `-8`, `inst-cors-alg-pf-1` .. `-7`: the preflight is
    // answered here, at the preflight-detection point of the canonical order,
    // with no plugin execution, no endpoint selection and no upstream contact,
    // so the browser receives an answer even when the upstream is unavailable.
    // The response is permissive by design because it grants nothing on its
    // own: the actual request is re-checked by the CORS gate above. The
    // credential input is delivered only when the caller was already
    // identified by the platform edge and the addressed alias carries a `cors`
    // block of its own; with no caller, no alias, or no `cors` block the answer
    // stays permissive and credential-free, and no tenant hierarchy is walked.
    #[must_use]
    pub fn preflight(
        &self,
        alias: Option<&str>,
        tenant: Option<uuid::Uuid>,
        headers: &[(String, String)],
    ) -> ProxyResponse {
        let origin = headers
            .iter()
            .find(|(name, _)| name == ORIGIN_HEADER)
            .map(|(_, value)| value.as_str());
        let requested_method = header_value(headers, "access-control-request-method");
        let requested_headers = header_value(headers, "access-control-request-headers");
        let cors = alias
            .zip(tenant)
            .and_then(|(alias, tenant)| self.upstreams.get_by_alias(tenant, alias).ok())
            .and_then(|record| record.upstream.cors)
            .map(|cors| {
                let exact = origin.is_some_and(|origin| {
                    matches!(
                        crate::domain::cors::match_origin(
                            origin,
                            cors.allowed_origins.as_deref().unwrap_or(&[]),
                        ),
                        crate::domain::cors::OriginVerdict::Exact(_)
                    )
                });
                // A `cors` block whose `enabled` is false grants nothing, so it
                // carries no credential-bearing preflight answer either: the
                // effective `allow_credentials` of a disabled policy is false.
                crate::domain::cors::PreflightCors {
                    allow_credentials: cors.enabled && cors.allow_credentials,
                    exact,
                }
            });
        ProxyResponse::preflight(crate::domain::cors::preflight_response_headers(
            origin,
            requested_method,
            requested_headers,
            cors,
        ))
    }
    // @cpt-end:cpt-cf-oagw-flow-cors-preflight:p1:inst-cors-pf-1

    // -- the plugin chain of entry 2.6 ------------------------------
    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-1
    // `inst-ps-comp-1` .. `-10`: the effective chain is composed from the
    // ordered upstream-level bindings and the ordered route-level bindings,
    // `[U1, U2] + [R1, R2] => [U1, U2, R1, R2]`, with an enforced ancestor
    // binding retained and a private ancestor binding omitted; every reference
    // in the composed chain is then resolved before execution begins, so an
    // unresolvable one fails the request with `PluginNotFound` rather than
    // being silently skipped. The single auth plugin comes from the upstream
    // `auth` block, never from the chain.
    async fn plugin_chain(
        &self,
        walk: &alias_resolver::AliasWalk,
        route: &crate::domain::dto::Route,
        effective: &EffectiveConfig,
        route_tenant_id: uuid::Uuid,
    ) -> Result<ResolvedChain, ProxyFailure> {
        // The ancestor upstream levels, base -> most specific, each carrying the
        // sharing mode its own `plugins` field declared: a `private` level
        // contributes nothing to a descendant requester and an `enforce` one
        // cannot be removed.
        let mut ancestors: Vec<ChainLayer> = Vec::new();
        for level in walk.shadowed() {
            let Some(record) = level.record.as_ref() else {
                continue;
            };
            let Some(plugins) = &record.upstream.plugins else {
                continue;
            };
            let bindings = self
                .upstreams
                .get(level.tenant_id, record.upstream.id)
                .map(|stored| stored.plugin_bindings)
                .unwrap_or_default();
            ancestors.push(ChainLayer::inherited(bindings, plugins.sharing));
        }
        // The bindings of a level are read under the tenant that owns the
        // level, not under the caller's: the selected alias level and the
        // matched route may both be inherited from an ancestor, and a read
        // keyed by the caller's tenant would find nothing and silently drop the
        // configured guard and transform bindings
        // (`inst-rp-chain-5`/`-6`). A repository failure is propagated rather
        // than read as an empty binding set.
        let selected = walk.upstream();
        let selected_tenant_id = walk.levels[walk.selected].tenant_id;
        let upstream = self
            .upstreams
            .get(selected_tenant_id, selected.upstream.id)
            .map_err(ProxyFailure::Domain)?
            .plugin_bindings;
        let route_bindings = self
            .routes
            .get(route_tenant_id, route.id)
            .map_err(ProxyFailure::Domain)?
            .plugin_bindings;
        let composed = compose(&ancestors, &upstream, &route_bindings, effective.auth.as_ref())?;
        // The posture of a data plane built with no plugin runtime: a chain that
        // carries no reference is a no-op, and one that carries any reference is
        // a resolve failure, never a silent skip.
        let Some(plugins) = self.plugins.as_ref() else {
            if composed.is_empty() {
                return Ok(ResolvedChain { auth: None, entries: Vec::new(), phases: Vec::new() });
            }
            let reference = composed
                .auth_ref
                .clone()
                .or_else(|| composed.bindings.first().map(|binding| binding.plugin_ref.clone()))
                .unwrap_or_default();
            return Err(ProxyFailure::Domain(DomainError::PluginNotFound { plugin_ref: reference }));
        };
        // The proxy path walks the tenant chain base -> most specific, so a
        // custom-plugin record an ancestor bound resolves against the owning
        // tenant's row (`inst-ps-res-13`/`-14`).
        let tenant_chain =
            TenantChain::new(walk.levels.iter().rev().map(|level| level.tenant_id).collect());
        plugins
            .resolve(
                &composed,
                &tenant_chain,
                effective.auth.as_ref().and_then(|auth| auth.config.as_ref()),
            )
            .map_err(ProxyFailure::Domain)
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-composition:p1:inst-ps-comp-1

    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-1
    // `inst-ps-exec-1` .. `-9`: the resolved chain runs auth, then every guard,
    // then every transform(request); the first rejection or failure stops the
    // chain and makes no upstream call. The phases run over the outbound header
    // set the header pipeline produced, so an injected credential and a
    // rewritten correlation identifier are what the upstream receives, and the
    // query surface a `query`-location auth plugin extends is what the upstream
    // URL is built from.
    async fn run_plugin_request(
        &self,
        resolved: &mut ResolvedChain,
        context: &ProxyContext,
        _endpoint: &SelectedEndpoint,
        outbound: crate::domain::headers::OutboundRequestHeaders,
        trace_id: &str,
    ) -> Result<(crate::domain::headers::OutboundRequestHeaders, Option<String>), ProxyFailure> {
        if resolved.is_empty() {
            return Ok((outbound, context.query.clone()));
        }
        let plugins = self.plugins.as_ref().expect("a resolved chain needs the runtime");
        let inbound = parse_query(context.query.as_deref().unwrap_or(""));
        let mut headers = outbound.forwarded;
        let transformed = plugins
            .run_request(
                resolved,
                context.method.as_str(),
                &context.request_path(),
                &inbound,
                &mut headers,
                context.body.clone(),
                crate::domain::plugin::Principal {
                    subject_id: Some(context.principal_id),
                    tenant_id: Some(context.tenant_id),
                    scopes: Vec::new(),
                },
                trace_id,
            )
            .await
            .map_err(ProxyFailure::Domain)?;
        // The transformed query extends the raw query string the upstream URL is
        // built from, in plugin order after the inbound order.
        let query = if transformed.query.is_empty() {
            context.query.clone()
        } else {
            Some(serialize_query(
                &inbound,
                &transformed.query,
            ))
        };
        Ok((
            crate::domain::headers::OutboundRequestHeaders {
                host: outbound.host,
                forwarded: transformed.headers,
            },
            query,
        ))
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-1

    // @cpt-begin:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-11
    // `inst-ps-exec-11` .. `-21`: the response phases run the guards and the
    // transforms against the upstream response, or the transforms against the
    // error context when the upstream call failed; a response-phase transform
    // failure discards the upstream response and maps to the downstream-error
    // class, and an error-phase transform failure falls back to the
    // untransformed error context without masking the original error type.
    async fn run_plugin_response(
        &self,
        resolved: &mut ResolvedChain,
        response: &mut ProxyResponse,
        trace_id: &str,
    ) -> Result<(), ProxyFailure> {
        if resolved.is_empty() {
            return Ok(());
        }
        let plugins = self.plugins.as_ref().expect("a resolved chain needs the runtime");
        plugins
            .run_response(
                resolved,
                response.status,
                &mut response.headers,
                response.error.as_ref(),
                trace_id,
            )
            .await
            .map_err(ProxyFailure::Domain)
    }
    // @cpt-end:cpt-cf-oagw-flow-plugin-system-chain-execution:p1:inst-ps-exec-11

    /// Walk the tenant chain upstream by upstream and stop at the first route
    /// set that yields a match (`inst-rp-match-5` .. `-10`).
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-5
// `inst-rp-match-5` .. `-10`: the route sets are walked upstream by upstream
// and the first level that yields a match wins outright, so a descendant route
// shadows an ancestor route on the same effective path.
    fn select_route(
        &self,
        walk: &alias_resolver::AliasWalk,
        context: &ProxyContext,
    ) -> Result<route_matcher::MatchedRoute, ProxyFailure> {
        let path = context.request_path();
        let mut not_allowed: Option<Vec<&'static str>> = None;
        for level in &walk.levels {
            let Some(record) = level.record.as_ref() else {
                continue;
            };
            let stored = self
                .routes
                .list_for_upstream(level.tenant_id, record.upstream.id)
                .map_err(ProxyFailure::Domain)?;
            let candidates: Vec<CandidateRoute> = stored
                .into_iter()
                .map(|record| CandidateRoute { route: record.route, tenant_distance: level.distance })
                .collect();
            match route_matcher::select(&candidates, &context.method, &path, |route| {
                route.match_.http.as_ref().map_or(PathSuffixMode::Append, |http| http.path_suffix_mode)
            }) {
                RouteMatchOutcome::Matched(matched) => return Ok(matched),
                RouteMatchOutcome::NotFound => {}
                RouteMatchOutcome::MethodNotAllowed { allowed } => {
                    // The closest level that matches the path but not the
                    // method is the one that names the allowed methods.
                    not_allowed.get_or_insert(allowed);
                }
            }
        }
        if let Some(allowed) = not_allowed {
            return Err(ProxyFailure::MethodNotAllowed { path: Some(path), allowed });
        }
        Err(ProxyFailure::Domain(DomainError::RouteNotFound {
            path: Some(path),
            trace_id: context.trace_id.clone(),
        }))
    }

// @cpt-end:cpt-cf-oagw-flow-request-proxy-route-matching:p1:inst-rp-match-5
    /// Validate the request surface against the matched route
    /// (`inst-rp-validate-1` .. `-6`).
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-1
// `inst-rp-validate-1` .. `-11`: the request surface — the suffix mode, the
// prefix escape and the query allowlist are checked against the matched route
// before anything else happens.
    fn validate_surface(
        &self,
        route: &crate::domain::dto::Route,
        context: &ProxyContext,
    ) -> Result<(), ProxyFailure> {
        let Some(http) = &route.match_.http else {
            return Err(ProxyFailure::Domain(DomainError::RouteNotFound {
                path: Some(context.request_path()),
                trace_id: context.trace_id.clone(),
            }));
        };
        if http.path_suffix_mode == PathSuffixMode::Disabled
            && !route_matcher::suffix_of(&http.path, &context.request_path()).is_empty()
        {
            return Err(ProxyFailure::Domain(DomainError::field_rejection(
                "path_suffix",
                "the route does not accept a path suffix",
            )));
        }
        route_matcher::reject_suffix_escape(&http.path, &context.request_path())
            .map_err(ProxyFailure::Domain)?;
        route_matcher::query_is_allowed(route, context.query.as_deref()).map_err(ProxyFailure::Domain)?;
        Ok(())
    }

// @cpt-end:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-1
    /// Merge the upstream, route and ancestor layers in the documented order
    /// (`inst-rp-al-config-1` .. `-6`).
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-effective-config:p1:inst-rp-al-config-1
// `inst-rp-al-config-1` .. `-6`: the effective configuration — the upstream
// layer, the route layer and every ancestor layer merged in the documented
// order with the selected layer overriding.
    /// The rate-limit view of the same layers the effective configuration was
    /// merged from (`inst-rl-eff-1` .. `-7`).
    ///
    /// The route layer is the resource the caller is invoking, so the
    /// `oagw:upstream:override_rate` gate that guards a descendant tenant's
    /// override of an *ancestor tenant's* inherited limit does not apply to it:
    /// without this a route-level limit would be silently ignored on the proxy
    /// path whenever the upstream also declares one. The sharing modes are
    /// evaluated before the permission gate, so a `private` ancestor layer
    /// still contributes nothing and an `enforce` ancestor limit still stands
    /// absolutely (`inst-rl-eff-3`/`-4`).
    fn rate_limit_view(
        &self,
        walk: &alias_resolver::AliasWalk,
        route: &crate::domain::dto::Route,
        actor: &Actor,
        permissions: &OverridePermissions,
    ) -> Option<crate::domain::dto::RateLimitConfig> {
        let layers = self.config_layers(walk, route);
        let permissions =
            OverridePermissions { override_rate: true, ..*permissions };
        merge::merge(&layers, actor.tenant_id, &permissions).rate_limit
    }

    /// The effective configuration and the hierarchy permissions it was merged
    /// under, so a step that needs a per-field view of the same layers can
    /// re-merge them instead of resolving the permissions twice.
    async fn merged(
        &self,
        walk: &alias_resolver::AliasWalk,
        route: &crate::domain::dto::Route,
        actor: &Actor,
    ) -> (EffectiveConfig, OverridePermissions) {
        let layers = self.config_layers(walk, route);
        let permissions = self.hierarchy.resolve(actor).await;
        let effective = merge::merge(&layers, actor.tenant_id, &permissions);
        (effective, permissions)
    }

    /// The layer list of one request, base -> most specific: the selected
    /// upstream, the matched route, then every shadowed ancestor level.
    fn config_layers(
        &self,
        walk: &alias_resolver::AliasWalk,
        route: &crate::domain::dto::Route,
    ) -> Vec<ConfigLayer> {
        let selected = walk.upstream();
        let mut layers: Vec<ConfigLayer> = vec![
            merge::upstream_base_layer(&selected.upstream),
            merge::route_layer(route),
        ];
        for ancestor in alias_resolver::ancestor_layers(walk) {
            let mut layer = merge::upstream_base_layer(&ancestor.upstream);
            layer.base = false;
            layers.push(layer);
        }
        layers
    }

    /// Send the prepared request to the selected endpoint, with endpoint-level
    /// connection failover and no re-issue of the client request
    /// (`inst-rp-forward-1` .. `-13`).
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
// @cpt-end:cpt-cf-oagw-algo-request-proxy-effective-config:p1:inst-rp-al-config-1
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-1
// `inst-rp-forward-1` .. `-13`, `inst-rp-st-req-1` .. `-10`, `inst-rp-st-conn-1`
// .. `-6`: the upstream call — the endpoint selection, the single pass over the
// pool that failover admits, the timeout that is never retried, and the
// response classification into a buffered body and a relayed stream.
    async fn forward(
        &self,
        context: &ProxyContext,
        effective: &EffectiveConfig,
        selected: &SelectedEndpoint,
        upstream_id: &str,
        pool: ServerConfig,
        outbound: crate::domain::headers::OutboundRequestHeaders,
        upgrade: Option<crate::infra::proxy::upgrade::InboundUpgrade>,
    ) -> Result<ProxyResponse, ProxyFailure> {
        let endpoint = &selected.endpoint;
        if let Some(inbound) = upgrade {
            return crate::infra::proxy::upgrade::relay(
                &self.upgrade_client(),
                context,
                endpoint,
                outbound,
                effective.headers.as_ref(),
                inbound,
                context.trace_id.clone(),
            )
            .await
            .map_err(ProxyFailure::Domain);
        }
        let failover: Vec<Endpoint> = pool
            .endpoints
            .into_iter()
            .filter(|candidate| {
                candidate.host != endpoint.host || candidate.port != endpoint.port
            })
            .collect();
        self.forward_buffered(context, effective, endpoint, upstream_id, failover, outbound).await
    }

    /// The client the WebSocket relay leg connects through.
    fn upgrade_client(&self) -> crate::infra::proxy::upgrade::UpstreamClient {
        crate::infra::proxy::upgrade::UpstreamClient::new()
    }

    async fn forward_buffered(
        &self,
        context: &ProxyContext,
        effective: &EffectiveConfig,
        endpoint: &Endpoint,
        upstream_id: &str,
        failover: Vec<Endpoint>,
        outbound: crate::domain::headers::OutboundRequestHeaders,
    ) -> Result<ProxyResponse, ProxyFailure> {
        let pool: Vec<Endpoint> = std::iter::once(endpoint.clone()).chain(failover).collect();
        let deadline = Instant::now() + self.proxy_timeout();
        let mut last: Option<ProxyFailure> = None;
        for candidate in &pool {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.send_once(candidate, context, effective, upstream_id, &outbound, remaining).await {
                Ok(response) => return Ok(response),
                Err(failure) => {
                    // A timeout is not a connection failure: failover never
                    // turns a timeout into a retry.
                    if matches!(failure, ProxyFailure::Domain(DomainError::RequestTimeout { .. })) {
                        return Err(failure);
                    }
                    last = Some(failure);
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            ProxyFailure::Domain(DomainError::DownstreamError {
                upstream_id: Some(upstream_id.to_owned()),
                host: None,
                path: Some(context.request_path()),
                trace_id: context.trace_id.clone(),
                retriable: false,
            })
        }))
    }

    /// Move the shared client's per-host connection count by `delta` and report
    /// the resulting pool state through the transport observer
    /// (`inst-os-client-3`).
    fn report_pool(&self, host: &str, delta: i64) {
        let Some(observer) = &self.transport_observer else {
            return;
        };
        let active = self.pool.move_by(host, delta);
        let active = active.min(self.pool_max_per_host);
        let idle = self.pool_max_per_host - active;
        observer.connections(host, idle, active, self.pool_max_per_host);
    }

    async fn send_once(
        &self,
        endpoint: &Endpoint,
        context: &ProxyContext,
        effective: &EffectiveConfig,
        upstream_id: &str,
        outbound: &crate::domain::headers::OutboundRequestHeaders,
        budget: Duration,
    ) -> Result<ProxyResponse, ProxyFailure> {
        // One per-host connection is held for the whole exchange and released on
        // every exit path, so the connection gauge of the shared client follows
        // the pool rather than the response status (`inst-os-client-3`).
        // The gauge is keyed on the host the connection is actually opened to:
        // the addressed alias is a client-controlled string whose casing and
        // spelling variants all resolve to this endpoint, and keying the gauge
        // on it would fan one endpoint out over unbounded distinct keys.
        let _connection = ConnectionGuard::take(self, &endpoint.host);
        self.exchange(endpoint, context, effective, upstream_id, outbound, budget).await
    }

    /// The one upstream exchange `send_once` guards with a pool connection.
    async fn exchange(
        &self,
        endpoint: &Endpoint,
        context: &ProxyContext,
        effective: &EffectiveConfig,
        upstream_id: &str,
        outbound: &crate::domain::headers::OutboundRequestHeaders,
        budget: Duration,
    ) -> Result<ProxyResponse, ProxyFailure> {
        let target = crate::domain::headers::upstream_url(
            endpoint,
            &context.request_path(),
            context.query.as_deref(),
        )
        .map_err(ProxyFailure::Domain)?;
        let builder = builder_for(&self.http, &context.method, &target)?;
        let builder = builder.headers(outbound.forwarded.clone());
        let builder = builder.header(crate::domain::headers::HOST_HEADER, outbound.host.as_str());
        let builder = builder.body_bytes(context.body.clone());

        let exchange = builder.send();
        let response = match tokio::time::timeout(budget, exchange).await {
            Ok(outcome) => outcome,
            Err(_) => {
                observe_transport(&self.transport_observer, context, upstream_id, endpoint, false);
                return Err(ProxyFailure::Domain(DomainError::RequestTimeout {
                    upstream_id: Some(upstream_id.to_owned()),
                    host: Some(endpoint.host.clone()),
                    guidance_secs: Some(self.limits.proxy_timeout_secs),
                    trace_id: context.trace_id.clone(),
                }));
            }
        }
        .map_err(|error| {
            // @cpt-begin:cpt-cf-oagw-algo-error-handling-retriability:p1:inst-eh-retry-5
            // `inst-eh-retry-5`: the guidance value of a `504` is the
            // configured `proxy_timeout_secs`, delivered here at the one place
            // the budget is known.
            observe_transport(&self.transport_observer, context, upstream_id, endpoint, false);
            transport_failure(
                error,
                endpoint,
                context,
                upstream_id,
                Some(self.limits.proxy_timeout_secs),
            )
            // @cpt-end:cpt-cf-oagw-algo-error-handling-retriability:p1:inst-eh-retry-5
        })?;
        observe_transport(&self.transport_observer, context, upstream_id, endpoint, true);

        self.render(response, context, effective, endpoint, upstream_id, budget)
            .await
    }

// @cpt-end:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-1
    /// Turn an upstream response into a proxy response
    /// (`inst-rp-forward-7` .. `-10`, `inst-rp-forward-13`).
    async fn render(
        &self,
        response: toolkit_http::HttpResponse,
        context: &ProxyContext,
        effective: &EffectiveConfig,
        endpoint: &Endpoint,
        upstream_id: &str,
        budget: Duration,
    ) -> Result<ProxyResponse, ProxyFailure> {
        let status = response.status().as_u16();
        let upstream_headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.to_str().unwrap_or("").to_owned()))
            .collect();
        let is_sse = upstream_headers.iter().any(|(name, value)| {
            name == "content-type" && value.split(';').any(|token| token.trim().eq_ignore_ascii_case("text/event-stream"))
        });
        let lifecycle = StreamLifecycle::shared();
        // The shared client caps nothing (`max_body_size: usize::MAX`), so the
        // gear's own body ceiling bounds the buffered leg here rather than the
        // client's, and the read is bounded by the same budget that bounded the
        // response head: a slow-drip or stalled body must not hold a pool
        // connection and a resident buffer open past `proxy_timeout_secs`.
        let max_bytes = usize::try_from(self.limits.max_body_size_bytes).unwrap_or(usize::MAX);
        let body = response.into_limited_body();
        let headers =
            crate::domain::headers::build_response_headers(&upstream_headers, effective.headers.as_ref());

        if is_sse {
            let relayed: ProxyByteStream = stream::relay(
                body,
                Arc::clone(&lifecycle),
                stream::IDLE_WINDOW,
                context.trace_id.clone(),
            );
            return Ok(ProxyResponse {
                status,
                headers,
                body: ProxyBody::Stream(relayed),
                source: ErrorSource::Upstream,
                stream: StreamKind::Sse,
                lifecycle,
                error: None,
                observation: ProxyObservation::default(),
            });
        }

        let bytes = match tokio::time::timeout(budget, stream::collect(body, max_bytes, context.trace_id.clone())).await
        {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => return Err(ProxyFailure::Domain(match error {
                // The ceiling breach is enriched here, where the upstream
                // identity is known, and stays non-retriable so failover never
                // retries a body the gear has already rejected.
                DomainError::DownstreamError { trace_id, .. } => DomainError::DownstreamError {
                    upstream_id: Some(upstream_id.to_owned()),
                    host: Some(endpoint.host.clone()),
                    path: Some(context.request_path()),
                    trace_id,
                    retriable: false,
                },
                other => other,
            })),
            Err(_) => {
                return Err(ProxyFailure::Domain(DomainError::RequestTimeout {
                    upstream_id: Some(upstream_id.to_owned()),
                    host: Some(endpoint.host.clone()),
                    guidance_secs: Some(self.limits.proxy_timeout_secs),
                    trace_id: context.trace_id.clone(),
                }));
            }
        };
        Ok(ProxyResponse {
            status,
            headers,
            body: ProxyBody::Buffered(bytes),
            source: ErrorSource::Upstream,
            stream: StreamKind::None,
            lifecycle,
            error: None,
            observation: ProxyObservation::default(),
        })
    }
}

// @cpt-begin:cpt-cf-oagw-algo-request-proxy-effective-config:p1:inst-rp-al-config-2
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-effective-config:p1:inst-rp-al-config-3
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-effective-config:p1:inst-rp-al-config-4
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-effective-config:p1:inst-rp-al-config-5
// @cpt-begin:cpt-cf-oagw-algo-request-proxy-effective-config:p1:inst-rp-al-config-6
// @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-10
// @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-11
// @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-12
// @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-13
// @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-14
// @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-15
// @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-2
// @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-3
// @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-4
// @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-5
// @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-6
// @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-7
// @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-8
// @cpt-begin:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-9
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-10
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-11
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-12
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-13
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-14
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-15
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-2
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-3
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-4
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-5
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-6
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-7
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-8
// @cpt-begin:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-9
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-10
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-11
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-2
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-3
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-4
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-5
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-6
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-7
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-8
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-9
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-10
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-11
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-12
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-13
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-2
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-3
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-4
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-5
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-6
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-7
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-8
// @cpt-begin:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-9
#[async_trait]
impl DataPlaneService for DataPlaneServiceImpl {
    async fn proxy(
        &self,
        context: ProxyContext,
    ) -> Result<ProxyResponse, ProxyFailure> {
        self.execute(context, None).await
    }
}
//
// @cpt-end:cpt-cf-oagw-algo-request-proxy-effective-config:p1:inst-rp-al-config-6
// @cpt-end:cpt-cf-oagw-algo-request-proxy-effective-config:p1:inst-rp-al-config-5
// @cpt-end:cpt-cf-oagw-algo-request-proxy-effective-config:p1:inst-rp-al-config-4
// @cpt-end:cpt-cf-oagw-algo-request-proxy-effective-config:p1:inst-rp-al-config-3
// @cpt-end:cpt-cf-oagw-algo-request-proxy-effective-config:p1:inst-rp-al-config-2
// @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-9
// @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-8
// @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-7
// @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-6
// @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-5
// @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-4
// @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-3
// @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-2
// @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-15
// @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-14
// @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-13
// @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-12
// @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-11
// @cpt-end:cpt-cf-oagw-flow-cors-actual-request:p1:inst-cors-act-10
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-9
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-8
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-7
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-6
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-5
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-4
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-3
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-2
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-15
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-14
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-13
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-12
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-11
// @cpt-end:cpt-cf-oagw-flow-rate-limiting-proxy-check:p1:inst-rl-chk-10
// @cpt-end:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-9
// @cpt-end:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-8
// @cpt-end:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-7
// @cpt-end:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-6
// @cpt-end:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-5
// @cpt-end:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-4
// @cpt-end:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-3
// @cpt-end:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-2
// @cpt-end:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-11
// @cpt-end:cpt-cf-oagw-flow-request-proxy-request-validation:p1:inst-rp-validate-10
// @cpt-end:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-9
// @cpt-end:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-8
// @cpt-end:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-7
// @cpt-end:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-6
// @cpt-end:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-5
// @cpt-end:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-4
// @cpt-end:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-3
// @cpt-end:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-2
// @cpt-end:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-13
// @cpt-end:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-12
// @cpt-end:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-11
// @cpt-end:cpt-cf-oagw-flow-request-proxy-upstream-forwarding:p1:inst-rp-forward-10
//

/// The builder the request method selects.
///
/// The proxy path is method-agnostic, so every method the route allowlist
/// admits plus the two method-agnostic ones is mapped; anything else is a
/// protocol failure rather than a silently rewritten request.
fn builder_for(
    http: &HttpClient,
    method: &str,
    target: &str,
) -> Result<toolkit_http::RequestBuilder, ProxyFailure> {
    let builder = match method {
        "GET" => http.get(target),
        "POST" => http.post(target),
        "PUT" => http.put(target),
        "DELETE" => http.delete(target),
        "PATCH" => http.patch(target),
        "HEAD" => http.head(target),
        "OPTIONS" => http.options(target),
        _ => {
            return Err(ProxyFailure::Domain(DomainError::field_rejection(
                "method",
                "the request method is not supported by the proxy surface",
            )));
        }
    };
    Ok(builder)
}

/// Map a transport failure onto the domain error the shared table names.
///
/// `guidance_secs` is the retry guidance the `504` rows carry: the configured
/// `proxy_timeout_secs`, or `None` where the caller has none.
// @cpt-begin:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-2
// `inst-eh-td-2` .. `-6`: the transport classification is mapped here and
// nowhere else — the three timeouts onto their own retriable `504` types, a
// protocol-level failure onto `502 protocol.error.v1`, an upstream service
// failure onto `502 downstream.error.v1` with the retriability the error
// carries — with the resolved upstream, the failing host, the request path and
// the trace identifier populated so the failing endpoint is identifiable. A
// complete upstream error response is never routed here: that is passthrough.
// @cpt-begin:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-1
// @cpt-begin:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-3
// @cpt-begin:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-4
// @cpt-begin:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-5
// @cpt-begin:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-6
// @cpt-begin:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-7
// @cpt-begin:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-8
// @cpt-begin:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-9
fn transport_failure(
    error: toolkit_http::HttpError,
    endpoint: &Endpoint,
    context: &ProxyContext,
    upstream_id: &str,
    guidance_secs: Option<u64>,
) -> ProxyFailure {
    let host = Some(endpoint.host.clone());
    let path = Some(context.request_path());
    let trace_id = context.trace_id.clone();
    let error = match error {
        toolkit_http::HttpError::Timeout(_) | toolkit_http::HttpError::DeadlineExceeded(_) => {
            DomainError::RequestTimeout {
                upstream_id: Some(upstream_id.to_owned()),
                host,
                guidance_secs,
                trace_id,
            }
        }
        toolkit_http::HttpError::InsecureTransport { .. } | toolkit_http::HttpError::InvalidScheme { .. } => {
            DomainError::ValidationError {
                detail: "the endpoint scheme is not admitted by the allowlist".to_owned(),
                path: Some("server.endpoints.scheme".to_owned()),
                trace_id,
            }
        }
        _ => DomainError::DownstreamError {
            upstream_id: Some(upstream_id.to_owned()),
            host,
            path,
            trace_id,
            retriable: false,
        },
    };
    ProxyFailure::Domain(error)
}
//
// @cpt-end:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-9
// @cpt-end:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-8
// @cpt-end:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-7
// @cpt-end:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-6
// @cpt-end:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-5
// @cpt-end:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-4
// @cpt-end:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-3
// @cpt-end:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-1
//
// @cpt-end:cpt-cf-oagw-flow-error-handling-timeout-downstream:p1:inst-eh-td-2

/// The `endpoint_host` label of one selected endpoint: `host:port`, which is
/// the address the connection is opened to, never a credential or a tenant.
#[must_use]
pub fn endpoint_host(endpoint: &crate::domain::dto::Endpoint) -> String {
    format!("{}:{}", endpoint.host, endpoint.port)
}

/// Report the reachability of one endpoint to the observer, when the data
/// plane was built with one (`inst-os-req-7`, `inst-os-req-8`).
fn observe_transport(
    observer: &Option<Arc<dyn TransportObserver>>,
    context: &ProxyContext,
    upstream_id: &str,
    endpoint: &crate::domain::dto::Endpoint,
    available: bool,
) {
    if let Some(observer) = observer {
        observer.observed(&context.alias, upstream_id, &endpoint_host(endpoint), available);
    }
}

/// The elapsed milliseconds of one pipeline step.
#[must_use]
fn elapsed_ms(started: std::time::Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The GTS error type of a domain error, for the observation.
fn type_of(error: &DomainError) -> &'static str {
    crate::api::rest::error::mapping_of(error).0
}

/// The rate-limit outcome of one request: the quota headers the response
/// carries, and the observation the pipeline-boundary record carries.
#[derive(Debug, Default)]
struct RateLimitGate {
    /// The `X-RateLimit-*` headers, in order, empty when the effective limit
    /// sets `response_headers: false`.
    headers: Vec<(String, String)>,
    /// The observation, and `None` when no `rate_limit` was configured.
    observation: Option<RateLimitObservation>,
}

impl RateLimitGate {
    fn inactive() -> Self {
        Self::default()
    }
}

/// The CORS outcome of a forwarded request (`cpt-cf-oagw-algo-cors-response-headers`).
#[derive(Debug, Default)]
struct CorsGate {
    /// The `Access-Control-*` pairs to add to the upstream response, empty when
    /// the effective configuration is disabled.
    headers: Vec<(String, String)>,
    /// Whether `Vary: Origin` is appended to the response: every request that
    /// carries an `Origin` is CORS-relevant.
    vary: bool,
}

/// Apply a [`CorsGate`] to the upstream response
/// (`cpt-cf-oagw-algo-cors-response-headers` steps 4 and 5): the computed
/// `Access-Control-*` value is set only when the upstream did not already set
/// one, and `Vary: Origin` is appended to the upstream list rather than
/// overwriting it.
fn apply_cors_gate(response: &mut ProxyResponse, gate: &CorsGate) {
    if gate.vary {
        crate::domain::cors::append_vary_origin(&mut response.headers);
    }
    for (name, value) in &gate.headers {
        if !response.headers.iter().any(|(existing, _)| existing == name) {
            response.headers.push((name.clone(), value.clone()));
        }
    }
}

/// The status the rejection of an exhausted counter carries
/// (`cpt-cf-oagw-dod-rate-limiting-reject-response`).
const RATE_LIMIT_STATUS: u16 = 429;

/// The status a CORS origin or method rejection carries
/// (`cpt-cf-oagw-dod-cors-origin-enforcement`).
const CORS_REJECTION_STATUS: u16 = 403;

/// The header name of the request origin, compared exactly as the framework
/// lowercased it (`cpt-cf-oagw-algo-cors-request-evaluation`).
const ORIGIN_HEADER: &str = "origin";

/// The value of a lowercased request header, in arrival order.
fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(header, _)| header == name).map(|(_, value)| value.as_str())
}

/// The parts per million the consumed fraction of a decision carries.
const PARTS_PER_MILLION: f64 = 1_000_000.0;

fn usage_ratio_parts_per_million(ratio: f64) -> u64 {
    (ratio.clamp(0.0, 1.0) * PARTS_PER_MILLION) as u64
}

/// The normalized route match pattern the `path` metric label carries, never
/// the raw request path (`inst-rl-obs-2`/`-4`).
fn route_pattern(route: &crate::domain::dto::Route) -> String {
    route
        .match_
        .http
        .as_ref()
        .map(|http| http.path.clone())
        .or_else(|| route.match_.grpc.as_ref().map(|grpc| grpc.service.clone()))
        .unwrap_or_else(|| "/".to_owned())
}

/// Append the quota headers of a decided rate limit to a response, after the
/// response rules have produced their own header set.
fn append_headers(response: &mut ProxyResponse, headers: &[(String, String)]) {
    response.headers.extend(headers.iter().cloned());
}

/// The `429` response of an exhausted counter, with its retry guidance and its
/// quota headers, rendered through the shared response pipeline.
fn rate_limit_refusal(
    config: &crate::domain::dto::RateLimitConfig,
    decision: &RateDecision,
    observation: RateLimitObservation,
    trace_id: &Option<String>,
) -> ProxyResponse {
    let error = DomainError::RateLimitExceeded {
        upstream_id: None,
        host: Some(observation.host.clone()),
        retry_after_seconds: decision.retry_after_seconds,
        trace_id: trace_id.clone(),
    };
    let mut response = ProxyResponse::gateway_error(error, RATE_LIMIT_STATUS);
    if config.response_headers {
        response.headers = quota_header_pairs(decision);
    }
    response.observation.rate_limit = Some(observation);
    response
}

/// The `(name, value)` pairs of a raw query string, in arrival order.
fn parse_query(query: &str) -> Vec<(String, String)> {
    if query.is_empty() {
        return Vec::new();
    }
    form_urlencoded::parse(query.as_bytes())
        .map(|(name, value)| (name.into_owned(), value.into_owned()))
        .collect()
}

/// The raw query string of the inbound pairs with the plugin-extended pairs
/// appended, in that order.
fn serialize_query(inbound: &[(String, String)], extended: &[(String, String)]) -> String {
    let mut pairs = inbound.to_vec();
    for (name, value) in extended {
        if !pairs.iter().any(|(existing, _)| existing == name) {
            pairs.push((name.clone(), value.clone()));
        }
    }
    form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs.iter().map(|(name, value)| (name.as_str(), value.as_str())))
        .finish()
}

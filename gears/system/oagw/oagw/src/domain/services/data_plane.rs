//! Data-plane contracts: what the proxy handler needs from the services.

use uuid::Uuid;

use crate::domain::dto::{Cors, Route, Upstream};
use crate::domain::error::DomainError;
use crate::domain::layering::EffectiveConfig;
use crate::infra::authz::TenantChain;

/// An upstream resolved for a proxy request.
#[derive(Debug, Clone)]
pub struct ResolvedUpstream {
    /// Tenant owning the selected upstream.
    pub owner_tenant_id: Uuid,
    /// The selected upstream.
    pub upstream: Upstream,
    /// The tenant chain the resolution walked.
    pub chain: TenantChain,
}

impl ResolvedUpstream {
    /// The GTS id of the resolved upstream.
    pub fn gts_id(&self) -> String {
        crate::domain::gts_helpers::anonymous_gts_id(
            crate::domain::gts_helpers::UPSTREAM_TYPE,
            self.upstream.id.clone().unwrap_or_default(),
        )
    }

    /// The alias of the resolved upstream.
    pub fn alias(&self) -> &str {
        self.upstream.alias_str()
    }
}

/// The routes belonging to a resolved upstream, with their owning tenant.
#[derive(Debug, Clone)]
pub struct RouteCandidate {
    /// Owning tenant of the route.
    pub owner_tenant_id: Uuid,
    /// The route.
    pub route: Route,
}

/// Outcome of a data-plane decision.
#[derive(Debug, Clone)]
pub struct DataPlaneDecision {
    /// The resolved upstream.
    pub resolved: ResolvedUpstream,
    /// The matched route.
    pub route: Option<Route>,
    /// The folded configuration.
    pub config: EffectiveConfig,
    /// The endpoint host the request is routed to.
    pub target_host: String,
    /// The port of the selected endpoint.
    pub target_port: u16,
    /// Whether the selected endpoint is a TLS endpoint.
    pub target_tls: bool,
    /// The path forwarded upstream.
    pub forward_path: String,
    /// The query string forwarded upstream, when any.
    pub forward_query: Option<String>,
}

/// The data-plane orchestration contract.
#[async_trait::async_trait]
pub trait DataPlaneService: Send + Sync {
    /// Resolves an alias to the closest enabled upstream in the tenant chain.
    async fn resolve(
        &self,
        chain: &TenantChain,
        alias: &str,
    ) -> Result<Option<ResolvedUpstream>, DomainError>;

    /// Selects the matching route for a request.
    async fn match_route(
        &self,
        chain: &TenantChain,
        resolved: &ResolvedUpstream,
        method: &str,
        path: &str,
        query: &[(String, String)],
    ) -> Result<Option<RouteCandidate>, DomainError>;

    /// Folds the effective configuration for the request.
    async fn effective_config(
        &self,
        chain: &TenantChain,
        resolved: &ResolvedUpstream,
        route: Option<&Route>,
    ) -> Result<EffectiveConfig, DomainError>;

    /// The effective CORS policy of the request.
    fn effective_cors(config: &EffectiveConfig) -> Option<&Cors> {
        config.cors.as_ref()
    }
}

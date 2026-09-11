//! The Data Plane proxy entities (FEATURE §5, `cpt-cf-oagw-dod-proxy-entities`).
//!
//! Six entities carry one proxy request from the handler to the upstream and
//! back: [`ProxyContext`] is what the caller sent, [`ResolvedUpstream`] is what
//! the tenant chain resolved, [`MatchedRoute`] is the route that matched it,
//! [`SelectedEndpoint`] is the endpoint that was chosen, [`OutboundRequest`] is
//! what was sent, and [`ProxyResponse`] is what came back. Two smaller types
//! travel with them: the alias derivation kind the endpoint-selection matrix
//! keys on, and one route candidate of the set that candidate is selected from.
//!
//! Every member is a domain type. The transport half of a proxy request — the
//! header map the HTTP layer holds, the connector's peer, the response body
//! stream — is assembled in the API layer from these values and never appears
//! here, which is what keeps the entities free of transport and persistence
//! types. The effective-configuration types the resolution produces
//! (`EffectiveUpstreamConfig`, `EffectiveRouteConfig`) are referenced from
//! `cpt-cf-oagw-feature-hierarchical-config`, and the error context from the
//! foundation, rather than redeclared.

use uuid::Uuid;

use crate::domain::effective::{EffectivePluginChain, EffectiveRateLimit};
use crate::domain::error::ErrorSource;
use crate::domain::route::Route;
use crate::domain::upstream::{Endpoint, HeadersConfig};

// @cpt-dod:cpt-cf-oagw-dod-proxy-entities:p1

/// How the resolved upstream's alias relates to its endpoint set.
///
/// `cpt-cf-oagw-algo-alias-derive` of `cpt-cf-oagw-feature-control-plane-config`
/// records at write time whether the alias was derived from a common suffix;
/// the fact is recomputable from the endpoint set alone, so the Data Plane
/// carries it as this kind and never persists it.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasDerivation {
    /// The alias was derived from the endpoint set's common suffix, which makes
    /// `X-OAGW-Target-Host` required for a multi-endpoint pool.
    Derived,
    /// The alias was supplied, which makes the header optional.
    Explicit,
}

/// The upstream a proxy request resolved to, with the route candidates of its
/// chain.
///
/// The per-family sharing modes the hierarchy walk carried are not restated:
/// the merged families are the ones `cpt-cf-oagw-feature-hierarchical-config`
/// produced, and the rate-limit family is carried here for
/// `cpt-cf-oagw-feature-rate-limiting`, which consumes this entity from inside
/// the resolved proxy context.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone)]
pub struct ResolvedUpstream {
    /// The tenant that owns the routing target.
    pub tenant_id: Uuid,
    /// The routing target's identifier.
    pub upstream_id: Uuid,
    /// The normalized alias the request addressed.
    pub alias: String,
    /// The alias derivation kind the endpoint-selection matrix keys on.
    pub alias_derivation: AliasDerivation,
    /// The endpoint pool, homogeneous in scheme, port, and protocol.
    pub endpoints: Vec<Endpoint>,
    /// The upstream protocol literal: `cf.core.oagw.http.v1` or
    /// `cf.core.oagw.grpc.v1`.
    pub protocol: String,
    /// The effective `enabled` state: the target's own flag conjoined with
    /// every matched ancestor row's.
    pub enabled: bool,
    /// The header transformation rules in both directions.
    pub headers: HeadersConfig,
    /// The merged rate-limit family, carried for the rate-limiting feature.
    pub rate_limit: Option<EffectiveRateLimit>,
    /// The merged plugin family of the upstream layer.
    pub plugins: Option<EffectivePluginChain>,
    /// The merged CORS family of the upstream layer, carried for
    /// `cpt-cf-oagw-feature-cors`, which consumes this entity from inside the
    /// resolved proxy context.
    pub cors: Option<crate::domain::effective::EffectiveCors>,
    /// The ordered route candidate set of the chain, most distant first.
    pub route_candidates: Vec<RouteCandidate>,
}

impl ResolvedUpstream {
    /// Whether the resolved upstream is a gRPC one.
    ///
    /// No HTTP match key is evaluated for such an upstream, so the request is
    /// answered before matching, per the §1.5 deviation.
    #[must_use]
    pub fn is_grpc(&self) -> bool {
        self.protocol == crate::gts::PROTOCOL_GRPC
    }
}

/// One route of the candidate set `cpt-cf-oagw-algo-route-match` selects from.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone)]
pub struct RouteCandidate {
    /// The tenant that owns the route row.
    pub tenant_id: Uuid,
    /// The chain depth of the element that holds it: `0` is the routing
    /// target, larger values are the ancestors.
    pub depth: usize,
    /// The route row as stored.
    pub route: Route,
}

/// The route `cpt-cf-oagw-algo-route-match` selected, with the outbound path it
/// produced.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone)]
pub struct MatchedRoute {
    /// The tenant that owns the selected route.
    pub tenant_id: Uuid,
    /// The selected route's identifier.
    pub route_id: Uuid,
    /// The selected route's priority, the ascending tie-break of §1.5.
    pub priority: Option<i64>,
    /// The path the outbound request carries.
    pub outbound_path: String,
    /// The route's normalized match pattern, which the request path was
    /// matched against and which the `http.route` label of
    /// `cpt-cf-oagw-feature-observability` carries.
    pub match_pattern: String,
    /// The route's query allowlist, which admits no parameter when empty.
    pub query_allowlist: Vec<String>,
    /// The merged rate-limit family of the route layer.
    pub rate_limit: Option<EffectiveRateLimit>,
    /// The merged plugin family of the route layer.
    pub plugins: Option<EffectivePluginChain>,
    /// The merged CORS family of the route layer, carried for
    /// `cpt-cf-oagw-feature-cors`.
    pub cors: Option<crate::domain::effective::EffectiveCors>,
}

/// How the endpoint was chosen, as the ADR 0001 matrix records it.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointChoice {
    /// The pool holds one endpoint; no load balancing ran.
    Only,
    /// The `X-OAGW-Target-Host` header named it.
    Header,
    /// The round-robin counter selected it.
    LoadBalanced,
}

/// The endpoint a request is sent to.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone)]
pub struct SelectedEndpoint {
    /// The endpoint itself.
    pub endpoint: Endpoint,
    /// How it was chosen.
    pub choice: EndpointChoice,
}

/// The proxy request as the caller issued it, before any resolution.
///
/// Headers are kept as the ordered pairs the HTTP layer read, with the names as
/// they arrived; every consumer matches on the lowercased name.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Default)]
pub struct ProxyContext {
    /// The request method as issued.
    pub method: String,
    /// The alias path segment as issued, before normalization.
    pub alias: String,
    /// The path suffix after the alias, when the request carried one.
    pub path_suffix: Option<String>,
    /// The raw query string, when the request carried one.
    pub query: Option<String>,
    /// The request headers, in arrival order.
    pub headers: Vec<(String, String)>,
    /// The value of `X-OAGW-Target-Host`, read before it is stripped.
    pub target_host: Option<String>,
    /// The calling tenant.
    pub tenant_id: Uuid,
    /// The authenticated subject, when the token carried one.
    pub subject_id: Option<Uuid>,
    /// The correlation state `cpt-cf-oagw-feature-observability` assigned at
    /// the path's entry. A request that left the path before that step ran —
    /// the CORS preflight is the one — carries none, and the absence is the
    /// reason the preflight is recorded by no record and no series.
    pub correlation: Option<crate::domain::observability::CorrelationContext>,
}

impl ProxyContext {
    /// The request path the route match and the plugin contexts read: the path
    /// the request carried beyond the alias, which is the space a route's
    /// `match.http.path` addresses. The alias itself is the routing key that
    /// selected the upstream and is never part of the match.
    #[must_use]
    pub fn request_path(&self) -> String {
        match self.path_suffix.as_deref() {
            Some(suffix) if !suffix.is_empty() => format!("/{suffix}"),
            _ => String::from("/"),
        }
    }

    /// Reads the first value of a header by its lowercased name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.to_ascii_lowercase() == lower)
            .map(|(_, value)| value.as_str())
    }

    /// Reads every value of a header by its lowercased name.
    #[must_use]
    pub fn header_values(&self, name: &str) -> Vec<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .filter(|(candidate, _)| candidate.to_ascii_lowercase() == lower)
            .map(|(_, value)| value.as_str())
            .collect()
    }
}

/// The header entries the plugin chain added or mutated, and the names it
/// removed, in one phase.
///
/// `cpt-cf-oagw-algo-header-transform` carries the entries into the outbound
/// map after the configuration rules have run, so a plugin sees the transformed
/// request and not the inbound one; the removed names are dropped from it for
/// the same reason.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginMutations {
    /// The entries the phase added or wrote a new value for.
    pub set: Vec<(String, String)>,
    /// The names the phase removed.
    pub removed: Vec<String>,
}

/// The request as transformed and ready to send.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone)]
pub struct OutboundRequest {
    /// The method to send.
    pub method: String,
    /// The endpoint's scheme, checked at dial time.
    pub scheme: crate::domain::scheme::Scheme,
    /// The selected endpoint's host.
    pub host: String,
    /// The selected endpoint's port, when it declares one.
    pub port: Option<u16>,
    /// The outbound path, with the allowed query appended.
    pub path: String,
    /// The transformed header map, in the order it was built.
    pub headers: Vec<(String, String)>,
    /// The validated body.
    pub body: Vec<u8>,
}

/// The response the caller is answered with.
#[toolkit_macros::domain_model]
#[derive(Debug, Clone)]
pub struct ProxyResponse {
    /// The status to answer with.
    pub status: u16,
    /// The response headers, in the order they were assembled.
    pub headers: Vec<(String, String)>,
    /// The response body.
    pub body: Vec<u8>,
    /// Who produced the body, which is what the error-source header states.
    pub source: ErrorSource,
}

impl ProxyResponse {
    /// Builds an upstream-sourced response: the status, headers, and body the
    /// upstream produced, passed through as received.
    #[must_use]
    pub fn upstream(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> Self {
        Self {
            status,
            headers,
            body,
            source: ErrorSource::Upstream,
        }
    }

    /// Reads the first value of a header by its lowercased name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let lower = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.to_ascii_lowercase() == lower)
            .map(|(_, value)| value.as_str())
    }
}

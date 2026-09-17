//! Data-plane resolution: alias walk, route match and endpoint selection.
//!
//! This module turns a proxy request into a [`ProxyResolution`]: the upstream
//! that serves the alias, the endpoint to dial, the matched route and the
//! effective configuration after the tenant chain is merged.

use std::sync::Arc;

use tenant_resolver_sdk::api::TenantResolverClient;
use tenant_resolver_sdk::models::{GetAncestorsOptions, TenantId};
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::matcher::{self, MatchInput};
use crate::domain::merge::{self, EffectiveConfig};
use crate::domain::model::{Endpoint, Route, Upstream};
use crate::domain::repo::{RouteRepository, UpstreamRepository};

/// Everything the proxy transport needs to execute a request.
#[derive(Debug, Clone)]
pub struct ProxyResolution {
    /// Upstream that serves the alias.
    pub upstream: Upstream,
    /// Endpoint selected from the upstream's pool.
    pub endpoint: Endpoint,
    /// Route matched for the request, when the upstream is HTTP.
    pub route: Option<Route>,
    /// Path forwarded upstream.
    pub path: String,
    /// Configuration after the tenant chain and route are merged.
    pub effective: EffectiveConfig,
}

/// Resolution service over the in-memory repositories and the tenant chain.
pub struct DataPlaneService {
    upstreams: Arc<dyn UpstreamRepository>,
    routes: Arc<dyn RouteRepository>,
    tenants: Option<Arc<dyn TenantResolverClient>>,
    round_robin: matcher::RoundRobin,
}

impl DataPlaneService {
    /// Assemble the service.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        routes: Arc<dyn RouteRepository>,
        tenants: Option<Arc<dyn TenantResolverClient>>,
    ) -> Self {
        Self {
            upstreams,
            routes,
            tenants,
            round_robin: matcher::RoundRobin::new(),
        }
    }

    /// Tenant chain for `tenant`, descendant first; falls back to the caller's
    /// own tenant when the resolver is unavailable or fails.
    async fn tenant_chain(&self, context: &SecurityContext) -> Vec<Uuid> {
        let own = context.subject_tenant_id();
        let Some(resolver) = &self.tenants else {
            return vec![own];
        };
        match resolver
            .get_ancestors(context, TenantId(own), &GetAncestorsOptions::default())
            .await
        {
            Ok(response) => {
                let mut chain = vec![response.tenant.id.0];
                chain.extend(response.ancestors.iter().map(|tenant| tenant.id.0));
                chain
            }
            Err(error) => {
                tracing::warn!(tenant = %own, error = %error, "tenant chain unavailable; proxying with the caller's own tenant only");
                vec![own]
            }
        }
    }

    /// Upstream specs along `chain`, root inward, stopping at the resolved one.
    fn specs_for_chain(&self, chain: &[Uuid], upstream: &Upstream) -> Vec<Upstream> {
        let mut found = Vec::new();
        for tenant in chain {
            let Ok(Some(candidate)) = self.upstreams.find_by_alias(*tenant, &upstream.alias) else {
                continue;
            };
            let reached = candidate.id == upstream.id;
            found.push(candidate);
            if reached {
                break;
            }
        }
        found
    }

    /// Routes along `chain` for `upstream`, descendant first.
    fn routes_for_chain(
        &self,
        chain: &[Uuid],
        upstream: &Upstream,
    ) -> Result<Vec<Route>, DomainError> {
        let mut routes = Vec::new();
        for tenant in chain {
            let Ok(Some(candidate)) = self.upstreams.find_by_alias(*tenant, &upstream.alias) else {
                continue;
            };
            let reached = candidate.id == upstream.id;
            routes.extend(self.routes.list(*tenant, Some(candidate.id))?);
            if reached {
                break;
            }
        }
        Ok(routes)
    }

    /// Resolve the alias across the tenant chain, closest enabled upstream
    /// first (DESIGN.md §3.1 "Shadowing Behavior").
    ///
    /// # Errors
    ///
    /// Returns a not-found error when no level carries the alias and
    /// `cf.oagw.link.unavailable.v1` when a level's upstream with that alias
    /// is disabled — an ancestor disable is never bypassed by shadowing.
    pub async fn resolve_upstream(
        &self,
        context: &SecurityContext,
        requested: &str,
    ) -> Result<(Upstream, Vec<Uuid>), DomainError> {
        let wanted = crate::domain::model::normalize_alias(requested);
        let chain = self.tenant_chain(context).await;
        let mut seen_disabled = false;
        for tenant in &chain {
            if let Some(upstream) = self.upstreams.find_by_alias(*tenant, &wanted)? {
                if !upstream.spec.enabled {
                    seen_disabled = true;
                    continue;
                }
                return Ok((upstream, chain));
            }
        }
        if seen_disabled {
            return Err(DomainError::link_unavailable(format!(
                "upstream `{wanted}` is disabled"
            )));
        }
        Err(DomainError::resource_not_found(format!(
            "no upstream answers to alias `{wanted}`"
        )))
    }

    /// Build the full resolution for a proxy request.
    ///
    /// # Errors
    ///
    /// Propagates the alias, route and endpoint selection errors.
    pub async fn resolve(
        &self,
        context: &SecurityContext,
        requested_alias: &str,
        method: &str,
        path: &str,
        query_keys: &[String],
        target_host: Option<&str>,
    ) -> Result<ProxyResolution, DomainError> {
        let (upstream, chain) = self.resolve_upstream(context, requested_alias).await?;
        let endpoint = matcher::select_endpoint(
            &upstream.spec.server.endpoints,
            &upstream.alias,
            upstream.alias_explicit,
            target_host,
            &self.round_robin,
        )?
        .clone();

        let matched = self.match_route(
            &chain,
            &upstream,
            &MatchInput {
                method,
                path,
                query_keys,
            },
        )?;

        let specs: Vec<crate::domain::model::UpstreamSpec> = self
            .specs_for_chain(&chain, &upstream)
            .into_iter()
            .map(|candidate| candidate.spec)
            .collect();
        let refs: Vec<&crate::domain::model::UpstreamSpec> = specs.iter().collect();
        let effective = merge::merge(&refs, matched.as_ref().map(|(route, _)| &route.spec));

        Ok(ProxyResolution {
            upstream,
            endpoint,
            route: matched.as_ref().map(|(route, _)| route.clone()),
            path: matched.map_or_else(|| path.to_owned(), |(_, rewritten)| rewritten),
            effective,
        })
    }

    /// Match a route, walking the tenant chain descendant-first.
    ///
    /// An upstream that declares no route at all serves every path
    /// (ADR-0001, "Single-Endpoint Upstream" example configures no route);
    /// once routes are declared, an unmatched path is a 404.
    fn match_route(
        &self,
        chain: &[Uuid],
        upstream: &Upstream,
        input: &MatchInput<'_>,
    ) -> Result<Option<(Route, String)>, DomainError> {
        let routes = self.routes_for_chain(chain, upstream)?;
        if routes.is_empty() {
            return Ok(None);
        }
        let candidates: Vec<&Route> = routes.iter().collect();
        matcher::select_route(&candidates, input)
            .map(|selection| Some((selection.route.clone(), selection.path)))
    }
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod proxy_tests;

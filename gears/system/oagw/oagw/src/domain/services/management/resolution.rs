use super::alias::normalize_alias;
use super::merge::{compute_effective_config, is_visible_to_descendant};

use crate::domain::error::DomainError;
use crate::domain::model::{Route, Upstream};
use crate::domain::repo::RouteRepository;

use super::ControlPlaneServiceImpl;

use toolkit_security::SecurityContext;
use uuid::Uuid;

impl ControlPlaneServiceImpl {
    /// Build the ordered tenant chain `[self, parent, ..., root]`.
    ///
    /// Index 0 is always the requesting tenant. Callers that only need
    /// ancestors (e.g. permission checks) can skip `&chain[1..]`.
    pub(crate) async fn build_tenant_chain(
        &self,
        ctx: &SecurityContext,
    ) -> Result<Vec<Uuid>, DomainError> {
        let tenant_id = ctx.subject_tenant_id();
        let ancestors_resp = self
            .tenant_resolver
            .get_ancestors(
                ctx,
                tenant_resolver_sdk::TenantId(tenant_id),
                &tenant_resolver_sdk::GetAncestorsOptions::default(),
            )
            .await?;

        let mut chain = Vec::with_capacity(1 + ancestors_resp.ancestors.len());
        chain.push(tenant_id);
        for ancestor in &ancestors_resp.ancestors {
            chain.push(ancestor.id.0);
        }
        Ok(chain)
    }

    /// Alias resolution: find the winning upstream by alias across the tenant
    /// chain, collect the merge chain, optionally resolve a route, and return
    /// the effective config.
    ///
    /// Performs a **single walk** over the tenant chain, collecting all visible
    /// upstreams in one pass. The winning (closest enabled) upstream is selected
    /// and ancestors above it form the merge chain — no second pass needed.
    ///
    /// When `method_path` is `Some((method, path))`, a route is also resolved
    /// across the tenant chain (searching by each ancestor upstream ID) and
    /// folded into the effective config via `compute_effective_config`.
    pub(crate) async fn resolve_alias(
        &self,
        ctx: &SecurityContext,
        tenant_chain: &[Uuid],
        alias: &str,
        method_path: Option<(&str, &str)>,
    ) -> Result<(Upstream, Option<Route>), DomainError> {
        let tenant_id = ctx.subject_tenant_id();
        // Normalize the incoming alias for case-insensitive matching.
        let alias = normalize_alias(alias);

        // Single walk: collect all visible upstreams keyed by chain index.
        let mut found: Vec<(usize, Upstream)> = Vec::new();
        let mut disabled_alias: Option<String> = None;

        for (i, &tid) in tenant_chain.iter().enumerate() {
            match self.upstreams.get_by_alias(tid, &alias).await {
                Ok(upstream) => {
                    if tid != tenant_id && !is_visible_to_descendant(&upstream) {
                        continue;
                    }
                    if !upstream.enabled {
                        if disabled_alias.is_none() {
                            disabled_alias = Some(upstream.alias.clone());
                        }
                        continue;
                    }
                    found.push((i, upstream));
                }
                Err(crate::domain::repo::RepositoryError::NotFound { .. }) => continue,
                Err(e) => return Err(DomainError::from(e)),
            }
        }

        // The winning upstream is the closest (lowest index) enabled match.
        let (_, selected_upstream) = match found.first() {
            Some(pair) => pair.clone(),
            None => {
                if let Some(alias) = disabled_alias {
                    return Err(DomainError::upstream_disabled(alias));
                }
                return Err(DomainError::not_found("upstream", Uuid::nil()));
            }
        };

        // Ancestors above the selected one form the merge chain (already collected).
        let merge_chain: Vec<&Upstream> = found[1..].iter().map(|(_, u)| u).collect();

        // Resolve route if method+path provided.
        // Search by each upstream ID in the chain — routes may be attached to
        // the selected upstream or any ancestor upstream.
        let route = if let Some((method, path)) = method_path {
            let mut route_found: Option<Route> = None;

            // Try selected upstream's ID first (most specific).
            match Self::find_route_in_chain(
                &*self.routes,
                tenant_chain,
                selected_upstream.id,
                method,
                path,
            )
            .await
            {
                Ok(r) => route_found = Some(r),
                Err(DomainError::NotFound { .. }) => {}
                Err(e) => return Err(e),
            }

            // Fall back to ancestor upstream IDs (closest ancestor first).
            if route_found.is_none() {
                for ancestor in &merge_chain {
                    match Self::find_route_in_chain(
                        &*self.routes,
                        tenant_chain,
                        ancestor.id,
                        method,
                        path,
                    )
                    .await
                    {
                        Ok(r) => {
                            route_found = Some(r);
                            break;
                        }
                        Err(DomainError::NotFound { .. }) => continue,
                        Err(e) => return Err(e),
                    }
                }
            }

            Some(route_found.ok_or_else(|| DomainError::not_found("route", Uuid::nil()))?)
        } else {
            None
        };

        // Build effective config.
        if merge_chain.is_empty() {
            // Single upstream → apply route overrides directly if present.
            if let Some(ref route) = route {
                let effective = compute_effective_config(
                    std::slice::from_ref(&selected_upstream),
                    Some(route),
                )?;
                return Ok((effective, Some(route.clone())));
            }
            return Ok((selected_upstream, None));
        }

        // Root-first order for merge: reverse ancestors, append selected.
        let mut merge_vec: Vec<Upstream> = merge_chain.into_iter().rev().cloned().collect();
        merge_vec.push(selected_upstream);

        let effective = compute_effective_config(&merge_vec, route.as_ref())?;
        Ok((effective, route))
    }

    /// Find a matching route for `upstream_id` by searching across tenant scopes.
    ///
    /// Walks the tenant chain from closest (index 0) to root, returning the
    /// first matching route. This gives descendant route definitions priority
    /// over ancestor ones.
    pub(crate) async fn find_route_in_chain(
        routes: &dyn RouteRepository,
        tenant_chain: &[Uuid],
        upstream_id: Uuid,
        method: &str,
        path: &str,
    ) -> Result<Route, DomainError> {
        for &tid in tenant_chain {
            match routes.find_matching(tid, upstream_id, method, path).await {
                Ok(route) => return Ok(route),
                Err(crate::domain::repo::RepositoryError::NotFound { .. }) => continue,
                Err(e) => return Err(DomainError::from(e)),
            }
        }
        Err(DomainError::not_found("route", Uuid::nil()))
    }
}

//! Alias resolution for the data plane (`DESIGN` §"Alias Resolution").
//!
//! Upstreams are addressed by alias, and aliases are unique per tenant. A
//! proxy request therefore has to walk the tenant hierarchy from the calling
//! tenant up to the root and take the closest definition of the alias — that
//! is what lets a descendant shadow an ancestor's routing target while the
//! ancestor's enforced constraints still apply.
//!
//! The walk needs an ancestor port ([`TenantHierarchy`]) because the domain
//! repositories are strictly tenant-scoped: an ancestor row is unreachable by
//! design, so the chain has to come from the platform's tenant resolver.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::domain::alias::normalize_alias;
use crate::domain::error::DomainError;
use crate::domain::model::Upstream;
use crate::domain::repo::UpstreamRepository;

/// Port over the tenant hierarchy.
///
/// Implementations answer with the chain **descendant → root**, starting at
/// `tenant` itself. `toolkit`'s tenant resolver answers parent → root, so
/// adapters prepend the calling tenant.
#[async_trait::async_trait]
pub trait TenantHierarchy: Send + Sync {
    /// The tenant and its ancestors, descendant first.
    ///
    /// # Errors
    /// Returns [`DomainError::ServiceUnavailable`] when the resolver cannot be
    /// reached: an unanswerable hierarchy must fail closed rather than route
    /// on a partial view.
    async fn ancestors(&self, tenant: uuid::Uuid) -> Result<Vec<uuid::Uuid>, DomainError>;
}

/// In-process hierarchy for tests and single-tenant deployments.
///
/// The edges are `(child, parent)`; a missing edge means the child is a root.
#[derive(Debug, Clone, Default)]
pub struct StaticHierarchy {
    edges: Vec<(uuid::Uuid, uuid::Uuid)>,
}

impl StaticHierarchy {
    /// Build a hierarchy from `(child, parent)` edges.
    #[must_use]
    pub fn new(edges: Vec<(uuid::Uuid, uuid::Uuid)>) -> Self {
        Self { edges }
    }
}

#[async_trait::async_trait]
impl TenantHierarchy for StaticHierarchy {
    async fn ancestors(&self, tenant: uuid::Uuid) -> Result<Vec<uuid::Uuid>, DomainError> {
        let mut chain = vec![tenant];
        let mut seen = BTreeSet::from([tenant]);
        let mut cursor = tenant;
        while let Some(&(_, parent)) = self.edges.iter().find(|(child, _)| *child == cursor) {
            if parent == uuid::Uuid::nil() || !seen.insert(parent) {
                break;
            }
            chain.push(parent);
            cursor = parent;
        }
        Ok(chain)
    }
}

/// The upstream an alias resolves to, plus the same-alias ancestors.
#[derive(Debug, Clone)]
pub struct ResolvedUpstream {
    /// The routing target: the closest definition of the alias.
    pub selected: Upstream,
    /// Ancestors that also define the alias, descendant → root. They are kept
    /// because `DESIGN` §"Hierarchical Configuration" merges their enforced
    /// limits into the effective configuration.
    pub ancestors: Vec<Upstream>,
}

/// Resolves a proxy alias to an upstream, walking the tenant chain.
#[derive(Clone)]
pub struct AliasResolver {
    upstreams: Arc<dyn UpstreamRepository>,
    hierarchy: Arc<dyn TenantHierarchy>,
}

impl AliasResolver {
    /// Bind the resolver to its ports.
    #[must_use]
    pub fn new(
        upstreams: Arc<dyn UpstreamRepository>,
        hierarchy: Arc<dyn TenantHierarchy>,
    ) -> Self {
        Self {
            upstreams,
            hierarchy,
        }
    }

    /// Resolve `alias` for `tenant`.
    ///
    /// # Errors
    /// Returns [`DomainError::UnknownTargetHost`] when no tenant of the chain
    /// defines the alias, [`DomainError::LinkUnavailable`] when the closest
    /// definition is disabled, and propagates repository or hierarchy
    /// failures.
    pub async fn resolve(
        &self,
        tenant: uuid::Uuid,
        alias: &str,
    ) -> Result<ResolvedUpstream, DomainError> {
        let alias = normalize_alias(alias);
        let chain = self.hierarchy.ancestors(tenant).await?;

        let mut definitions: Vec<Upstream> = Vec::new();
        for member in chain {
            if let Some(row) = self.upstreams.find_by_alias(member, &alias).await? {
                definitions.push(row);
            }
        }

        let Some(selected) = definitions.first().cloned() else {
            // `DESIGN` §3.3: `UnknownTargetHost` is the `X-OAGW-Target-Host`
            // value not matching a configured endpoint. An alias nothing in the
            // chain defines is a routing miss, which is `404`.
            return Err(DomainError::RouteNotFound {
                alias: alias.clone(),
                path: String::new(),
            });
        };

        if !selected.enabled {
            return Err(DomainError::LinkUnavailable {
                detail: format!("upstream '{alias}' is disabled"),
                retry_after: None,
            });
        }

        let ancestors = definitions.iter().skip(1).cloned().collect();
        Ok(ResolvedUpstream {
            selected,
            ancestors,
        })
    }
}

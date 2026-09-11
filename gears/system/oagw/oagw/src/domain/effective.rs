//! Effective configuration — the result types of the hierarchical
//! configuration feature.
//!
//! Every type here is what a resolution answers with and nothing else: the
//! ordered chain the platform tenant-resolver supplies, the alias-match link
//! between a descendant's row and a more distant ancestor's row, the sharing
//! modes a row declares per field family, and the two per-layer results the
//! downstream features consume. No transport and no persistence type appears,
//! and no type here carries a value an ancestor marked `private`, because an
//! [`AncestorBinding`] has no field to put one in.
//!
//! The names are the DECOMPOSITION §2.3 names — `EffectiveUpstreamConfig` and
//! `EffectiveRouteConfig` — not the `EffectiveUpstream` / `MatchedRoute` names
//! of the ADR 0006 request-flow sketch.

// @cpt-dod:cpt-cf-oagw-dod-effective-config-result:p1

use std::collections::BTreeSet;

use toolkit_macros::domain_model;
use uuid::Uuid;

use crate::domain::upstream::{AuthConfig, CorsConfig, PluginsConfig, RateLimitConfig, SharingMode};

/// One configuration field family the sharing modes and the merge address.
///
/// The four variants are exactly the four families the shipped schemas give a
/// `sharing` member; `tags` carries no sharing field and never reaches a
/// sharing-mode decision.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Family {
    /// The authentication plugin binding.
    Auth,
    /// The rate limiting configuration.
    RateLimit,
    /// The plugin chain.
    Plugins,
    /// The CORS configuration.
    Cors,
}

impl Family {
    /// The descendant override permission the four-permission table of DESIGN
    /// §3.2 names for this family, or `None` for the CORS family, which the
    /// sharing mode alone governs.
    #[must_use]
    pub const fn override_permission(self) -> Option<&'static str> {
        match self {
            Self::Auth => Some(crate::gts::PERMISSION_OVERRIDE_AUTH),
            Self::RateLimit => Some(crate::gts::PERMISSION_OVERRIDE_RATE),
            Self::Plugins => Some(crate::gts::PERMISSION_ADD_PLUGINS),
            // The four-permission table names no permission for CORS: inventing
            // a fifth one is outside this feature's authority, so the sharing
            // mode alone decides.
            Self::Cors => None,
        }
    }
}

/// The sharing modes one row declares for the four sharing-bearing families.
///
/// A family the row does not declare takes the schema default `private`, which
/// is why every field is a plain [`SharingMode`] and never an `Option`.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FamilyModes {
    /// The mode of the `auth` family.
    pub auth: SharingMode,
    /// The mode of the `rate_limit` family.
    pub rate_limit: SharingMode,
    /// The mode of the `plugins` family.
    pub plugins: SharingMode,
    /// The mode of the `cors` family.
    pub cors: SharingMode,
}

impl FamilyModes {
    /// Builds the modes from the four `sharing` members a row carries, taking
    /// the schema default for the ones it omits.
    #[must_use]
    pub fn new(
        auth: Option<SharingMode>,
        rate_limit: Option<SharingMode>,
        plugins: Option<SharingMode>,
        cors: Option<SharingMode>,
    ) -> Self {
        let fallback = || SharingMode::Private;
        Self {
            auth: auth.unwrap_or_else(fallback),
            rate_limit: rate_limit.unwrap_or_else(fallback),
            plugins: plugins.unwrap_or_else(fallback),
            cors: cors.unwrap_or_else(fallback),
        }
    }

    /// The mode one family carries.
    #[must_use]
    pub const fn mode_of(self, family: Family) -> SharingMode {
        match family {
            Family::Auth => self.auth,
            Family::RateLimit => self.rate_limit,
            Family::Plugins => self.plugins,
            Family::Cors => self.cors,
        }
    }
}

/// The ordered ancestor chain the platform tenant-resolver supplies.
///
/// The chain runs from the calling tenant to the platform root, inclusive of
/// both ends, without a repeated element, and the calling tenant is its first
/// element — so its depth is zero and its rows are the closest candidates. A
/// chain the resolver answers with a repeated element, or with the calling
/// tenant anywhere but first, is an unavailable chain: [`TenantChain::from_resolver`]
/// answers `None` and the caller fails closed rather than ordering candidates
/// against a chain it cannot order.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantChain {
    tenants: Vec<Uuid>,
}

impl TenantChain {
    /// Builds the chain the resolver's ancestor answer produces.
    ///
    /// Tenants the resolver retired — `status: deleted` — are dropped before
    /// the chain is ordered, because a retired tenant is not an active
    /// participant of any resolution. `None` answers an unordered or cyclic
    /// answer, which the caller fails closed on.
    #[must_use]
    pub fn from_resolver(calling_tenant: Uuid, ancestors: &[Uuid]) -> Option<Self> {
        let mut tenants: Vec<Uuid> = Vec::with_capacity(ancestors.len() + 1);
        match ancestors.first() {
            // The resolver already answered the calling tenant first.
            Some(first) if *first == calling_tenant => tenants.extend_from_slice(ancestors),
            // The calling tenant appears anywhere else: the resolver's answer
            // runs from the root towards the leaf, which is the same answer a
            // cycle in the tree produces, and neither can be ordered.
            _ if ancestors.contains(&calling_tenant) => return None,
            // The resolver answered the ancestors only, or nothing at all.
            _ => {
                tenants.push(calling_tenant);
                tenants.extend_from_slice(ancestors);
            }
        }
        if has_duplicate(&tenants) {
            return None;
        }
        Some(Self { tenants })
    }

    /// Builds the chain from the tenant identifiers, calling tenant first.
    ///
    /// # Errors
    ///
    /// Answers [`ChainError::Cyclic`] when the chain repeats an element, and
    /// [`ChainError::Empty`] when it carries none; both are unavailable chains
    /// the caller fails closed on.
    pub fn from_ordered(tenants: Vec<Uuid>) -> Result<Self, ChainError> {
        if tenants.is_empty() {
            return Err(ChainError::Empty);
        }
        if has_duplicate(&tenants) {
            return Err(ChainError::Cyclic);
        }
        Ok(Self { tenants })
    }

    /// The tenants of the chain, calling tenant first, root last.
    #[must_use]
    pub fn tenants(&self) -> &[Uuid] {
        &self.tenants
    }

    /// The calling tenant, which is always the first element.
    #[must_use]
    pub fn calling_tenant(&self) -> Uuid {
        self.tenants[0]
    }

    /// The depth of one tenant in the chain: `0` for the calling tenant and
    /// growing towards the root. `None` for a tenant outside the chain, for
    /// which no lookup is ever issued.
    #[must_use]
    pub fn depth_of(&self, tenant: Uuid) -> Option<usize> {
        self.tenants.iter().position(|known| *known == tenant)
    }

    /// Whether the chain carries one tenant.
    #[must_use]
    pub fn contains(&self, tenant: Uuid) -> bool {
        self.tenants.contains(&tenant)
    }
}

/// Why a chain could not be ordered.
#[domain_model]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainError {
    /// The chain carried no tenant.
    Empty,
    /// The chain repeated an element, so it is a cycle.
    Cyclic,
}

/// Whether one list repeats an element.
fn has_duplicate(tenants: &[Uuid]) -> bool {
    let seen: BTreeSet<&Uuid> = tenants.iter().collect();
    seen.len() != tenants.len()
}

/// One family value an ancestor contributes to a merge, with the mode that
/// decided the contribution.
///
/// A family the ancestor marks `private` contributes nothing at all, so it
/// produces no [`FamilyContribution`] and the value is never read into a
/// result, copied onto any row, or echoed in any answer.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyContribution<V> {
    /// The sharing mode the contributing row declares.
    pub mode: SharingMode,
    /// The value the contributing row carries.
    pub value: V,
}

/// The families one ancestor row contributes to a merge.
///
/// `None` is the structural form of `private`: the family has no field to
/// carry a value in, so an ancestor value marked `private` cannot reach any
/// result. `auth` is always `None` for a route-layer binding, because a route
/// carries no authentication family.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct ContributedFamilies {
    /// The authentication binding, when the ancestor contributes it.
    pub auth: Option<FamilyContribution<AuthConfig>>,
    /// The rate limit, when the ancestor contributes it.
    pub rate_limit: Option<FamilyContribution<RateLimitConfig>>,
    /// The plugin chain, when the ancestor contributes it.
    pub plugins: Option<FamilyContribution<PluginsConfig>>,
    /// The CORS configuration, when the ancestor contributes it.
    pub cors: Option<FamilyContribution<CorsConfig>>,
    /// The tags, which always contribute: `tags` carries no sharing field.
    pub tags: Option<Vec<String>>,
}

/// The alias-match link between a descendant's row and a more distant
/// ancestor's row with the same normalized alias.
///
/// The binding is not materialized: no table, column, or join row records it,
/// and it exists only for as long as the configuration that produced it does.
/// It carries the contributed families of the ancestor row, never the families
/// that ancestor marked `private`.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct AncestorBinding {
    /// The tenant that owns the ancestor row.
    pub tenant_id: Uuid,
    /// The depth of that tenant in the chain; always greater than the
    /// descendant's.
    pub depth: usize,
    /// The identifier of the ancestor's upstream row.
    pub upstream_id: Uuid,
    /// The ancestor row's `enabled` flag, which participates in the effective
    /// enabled state regardless of every sharing mode.
    pub enabled: bool,
    /// The families the ancestor row contributes.
    pub contributed: ContributedFamilies,
}

impl AncestorBinding {
    /// The mode the binding's row declares for one family, taken from the
    /// contribution when it contributes and `private` when it does not.
    #[must_use]
    pub fn mode_of(&self, family: Family) -> SharingMode {
        let mode = match family {
            Family::Auth => self.contributed.auth.as_ref().map(|item| item.mode),
            Family::RateLimit => self.contributed.rate_limit.as_ref().map(|item| item.mode),
            Family::Plugins => self.contributed.plugins.as_ref().map(|item| item.mode),
            Family::Cors => self.contributed.cors.as_ref().map(|item| item.mode),
        };
        mode.unwrap_or(SharingMode::Private)
    }

    /// Whether the binding contributes one family at all.
    #[must_use]
    pub fn contributes(&self, family: Family) -> bool {
        match family {
            Family::Auth => self.contributed.auth.is_some(),
            Family::RateLimit => self.contributed.rate_limit.is_some(),
            Family::Plugins => self.contributed.plugins.is_some(),
            Family::Cors => self.contributed.cors.is_some(),
        }
    }
}

/// The authentication family of one per-layer result.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveAuth {
    /// The tenant whose authentication object is the effective one.
    pub owner: Uuid,
    /// The sharing mode that produced the result.
    pub mode: SharingMode,
    /// The effective authentication object.
    pub auth: AuthConfig,
}

/// The rate-limit family of one per-layer result.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveRateLimit {
    /// The tenant whose limit the effective value descends from.
    pub owner: Uuid,
    /// The sharing mode that produced the result.
    pub mode: SharingMode,
    /// The effective limit: the minimum of the visible sustained rates and the
    /// minimum of the visible burst capacities, with the remaining members
    /// carried unchanged.
    pub rate_limit: RateLimitConfig,
}

/// The plugin family of one per-layer result.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectivePluginChain {
    /// The tenant whose chain items are the nearest of the effective ones.
    pub owner: Uuid,
    /// The sharing mode that produced the result.
    pub mode: SharingMode,
    /// The effective chain: the ancestors' items followed by the
    /// descendant's, in that order.
    pub items: Vec<String>,
    /// The tenants whose items are in the chain, most distant first.
    pub contributors: Vec<Uuid>,
}

/// The CORS family of one per-layer result.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveCors {
    /// The tenant whose CORS object is the effective one.
    pub owner: Uuid,
    /// The sharing mode that produced the result.
    pub mode: SharingMode,
    /// The effective CORS configuration.
    pub cors: CorsConfig,
}

/// The tag family of one per-layer result.
///
/// `tags` carries no sharing field, so there is no mode to report: the result
/// is the add-only union of every contributor's tags.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EffectiveTagSet {
    /// The effective tags: the union of the ancestors' and the descendant's.
    pub tags: Vec<String>,
    /// The tenants whose tags are in the set, most distant first.
    pub contributors: Vec<Uuid>,
}

/// The upstream-layer result of one resolution.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveUpstreamConfig {
    /// The tenant that owns the routing target: the resolved ownership the
    /// consumer's own authorization check needs.
    pub tenant_id: Uuid,
    /// The routing target's identifier.
    pub upstream_id: Uuid,
    /// The authentication family, when the merged families produced one.
    pub auth: Option<EffectiveAuth>,
    /// The rate-limit family, when the merged families produced one.
    pub rate_limit: Option<EffectiveRateLimit>,
    /// The plugin family, when the merged families produced one.
    pub plugins: Option<EffectivePluginChain>,
    /// The CORS family, when the merged families produced one.
    pub cors: Option<EffectiveCors>,
    /// The tag family.
    pub tags: EffectiveTagSet,
}

/// The route-layer result of one resolution.
///
/// A route carries no authentication family, so the result has no `auth`
/// member to skip.
#[domain_model]
#[derive(Debug, Clone, PartialEq)]
pub struct EffectiveRouteConfig {
    /// The tenant that owns the matched route.
    pub tenant_id: Uuid,
    /// The matched route's identifier.
    pub route_id: Uuid,
    /// The upstream the matched route belongs to.
    pub upstream_id: Uuid,
    /// The rate-limit family, when the merged families produced one.
    pub rate_limit: Option<EffectiveRateLimit>,
    /// The plugin family, when the merged families produced one.
    pub plugins: Option<EffectivePluginChain>,
    /// The CORS family, when the merged families produced one.
    pub cors: Option<EffectiveCors>,
    /// The tag family.
    pub tags: EffectiveTagSet,
}

/// The route selector a resolution matches against, ADR 0006's `method` and
/// `path`.
///
/// The matching this type drives is the candidate selection of the route layer
/// alone: which chain element's route the resolution resolves. Applying the
/// result to a proxy request belongs to the data-plane proxy feature.
#[domain_model]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteSelector {
    /// An HTTP request: the request method and the request path.
    Http {
        /// The request method.
        method: String,
        /// The request path.
        path: String,
    },
    /// A gRPC request: the fully qualified service and the RPC method.
    Grpc {
        /// The fully qualified service name.
        service: String,
        /// The RPC method name.
        rpc: String,
    },
}

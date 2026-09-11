//! Effective-configuration merge engine
//! (`cpt-cf-oagw-algo-gear-foundation-config-merge`).
//!
//! Priority order is **Upstream (base) < Route < Tenant**, and the tenant
//! chain is walked root -> leaf. The result is computed per request and is
//! **never** cached downstream: the L1 caches key resolved upstreams and
//! routes, not merged configuration documents.
//!
//! # The fourteen steps
//!
//! 1. start from the upstream base layer as the initial effective value
//!    (`inst-gf-merge-1`);
//! 2. iterate the remaining layers in increasing priority — route, then
//!    tenant chain root to leaf (`inst-gf-merge-2`);
//! 3. `sharing: private` on an ancestor layer field, with a descendant
//!    requester, skips the field (`inst-gf-merge-3`/`-4`);
//! 4. `sharing: enforce` keeps the ancestor value and discards every
//!    descendant value, including across alias shadowing
//!    (`inst-gf-merge-5`/`-6`);
//! 5. auth merges by **override**, gated on `oagw:upstream:override_auth`
//!    (`inst-gf-merge-7`);
//! 6. rate limits merge by **`min(ancestor, descendant)`**, gated on
//!    `oagw:upstream:override_rate` (`inst-gf-merge-8`);
//! 7. tags merge by **add-only union** (`inst-gf-merge-9`);
//! 8. CORS origins merge by **union** under `inherit`, and stay as-is under
//!    `enforce` (`inst-gf-merge-10`);
//! 9. plugin chains merge by **concatenation**, gated on
//!    `oagw:upstream:add_plugins` (`inst-gf-merge-11`);
//! 10. scalar fields with no sharing semantics take the more specific value
//!     (`inst-gf-merge-12`);
//! 11. a field no layer specifies stays absent (`inst-gf-merge-13`);
//! 12. the effective configuration is returned (`inst-gf-merge-14`).
//!
//! The override permissions are resolved through `authz_resolver` *before*
//! any `inherit` override is applied (`inst-gf-eff-8`): they are an input to
//! [`merge`], not a call this module makes, because the domain layer carries
//! no infrastructure dependency. A descendant that holds no permission
//! receives the ancestor value as-is and **no error is surfaced**.

use uuid::Uuid;

use crate::domain::dto::{
    AuthConfig, CorsConfig, HeadersConfig, PluginsConfig, RateLimitConfig, ServerConfig,
};
use crate::domain::gts_helpers::{PERM_ADD_PLUGINS, PERM_OVERRIDE_AUTH, PERM_OVERRIDE_RATE};
use crate::domain::validation::bound_invalid_value;

/// Sharing mode one layer declares for one mergeable field.
///
/// The DTO-level [`crate::domain::dto::SharingMode`] is the *wire* form; this
/// is the merge-engine view of the same three values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sharing {
    /// Invisible to a descendant requester.
    #[default]
    Private,
    /// Descendants may override, gated on the corresponding permission.
    Inherit,
    /// Descendants cannot override.
    Enforce,
}

impl Sharing {
    /// Map the wire form onto the merge-engine form.
    #[must_use]
    pub const fn from_sharing_mode(mode: crate::domain::dto::SharingMode) -> Self {
        match mode {
            crate::domain::dto::SharingMode::Private => Self::Private,
            crate::domain::dto::SharingMode::Inherit => Self::Inherit,
            crate::domain::dto::SharingMode::Enforce => Self::Enforce,
        }
    }
}

/// Per-field sharing declaration of one layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayerSharing {
    /// `enabled`, `protocol`, `server`, `headers`.
    pub scalars: Sharing,
    pub auth: Sharing,
    pub rate_limit: Sharing,
    pub cors: Sharing,
    pub plugins: Sharing,
    pub tags: Sharing,
}

// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-effective-config:p1:inst-gf-eff-8
// `inst-gf-eff-8`: the override permissions are resolved through
// `authz_resolver` before any `inherit` override is applied.
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-effective-config:p1:inst-gf-eff-1
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-effective-config:p1:inst-gf-eff-3
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-effective-config:p1:inst-gf-eff-4
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-effective-config:p1:inst-gf-eff-5
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-effective-config:p1:inst-gf-eff-6
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-effective-config:p1:inst-gf-eff-8
/// Override permissions resolved through `authz_resolver` before the merge
/// (`inst-gf-eff-8`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OverridePermissions {
    /// `oagw:upstream:override_auth`
    pub override_auth: bool,
    /// `oagw:upstream:override_rate`
    pub override_rate: bool,
    /// `oagw:upstream:add_plugins`
    pub add_plugins: bool,
}
//
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-effective-config:p1:inst-gf-eff-6
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-effective-config:p1:inst-gf-eff-5
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-effective-config:p1:inst-gf-eff-4
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-effective-config:p1:inst-gf-eff-3
// @cpt-end:cpt-cf-oagw-flow-gear-foundation-effective-config:p1:inst-gf-eff-1
//

impl OverridePermissions {
    /// The requester holds none of the ancestor's override permissions; the
    /// ancestor values stand with no error surfaced.
    pub const NONE: Self = Self { override_auth: false, override_rate: false, add_plugins: false };

    /// All three override permissions.
    pub const ALL: Self =
        Self { override_auth: true, override_rate: true, add_plugins: true };

    /// Project the hierarchy permissions a requester holds.
    #[must_use]
    pub fn from_granted<'a>(granted: impl IntoIterator<Item = &'a str>) -> Self {
        let mut this = Self::NONE;
        for permission in granted {
            match permission {
                PERM_OVERRIDE_AUTH => this.override_auth = true,
                PERM_OVERRIDE_RATE => this.override_rate = true,
                PERM_ADD_PLUGINS => this.add_plugins = true,
                _ => {}
            }
        }
        this
    }
}

/// One layer of the ordered layer list.
///
/// `owner_tenant_id` is the tenant the layer's values belong to. A layer whose
/// owner differs from the requesting tenant is an *ancestor* layer relative to
/// the requester: its `private` fields are invisible and its `inherit`
/// overrides are permission-gated.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConfigLayer {
    pub owner_tenant_id: Uuid,
    /// `true` only for the upstream base layer, the first layer of the ordered
    /// list. `inst-gf-merge-1` makes it the initial effective value, so it
    /// contributes its own values unconditionally; every later layer's
    /// `inherit` override is permission-gated (`inst-gf-merge-7`, `-8`, `-11`).
    pub base: bool,
    pub sharing: LayerSharing,
    pub enabled: Option<bool>,
    pub protocol: Option<String>,
    pub server: Option<ServerConfig>,
    pub headers: Option<HeadersConfig>,
    pub auth: Option<AuthConfig>,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: Option<CorsConfig>,
    pub plugins: Option<PluginsConfig>,
    pub tags: Option<Vec<String>>,
}

impl ConfigLayer {
    /// A layer owned by `owner_tenant_id` with no values.
    #[must_use]
    pub fn new(owner_tenant_id: Uuid) -> Self {
        Self { owner_tenant_id, ..Self::default() }
    }

    /// Whether this layer is owned by `tenant_id`.
    #[must_use]
    pub fn is_owned_by(&self, tenant_id: Uuid) -> bool {
        self.owner_tenant_id == tenant_id
    }
}

/// The single effective configuration the merge produces.
///
/// Absent fields stay absent — there is no implicit substitution
/// (`inst-gf-merge-13`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EffectiveConfig {
    pub enabled: Option<bool>,
    pub protocol: Option<String>,
    pub server: Option<ServerConfig>,
    pub headers: Option<HeadersConfig>,
    pub auth: Option<AuthConfig>,
    pub rate_limit: Option<RateLimitConfig>,
    pub cors: Option<CorsConfig>,
    pub plugins: Option<PluginsConfig>,
    /// Add-only union of every visible layer's tags.
    pub tags: Vec<String>,
}

/// One mergeable field's accumulated state: the effective value plus the
/// sharing mode the *contributing* layer declared, which is what later layers
/// are measured against when enforcing.
struct Slot<T> {
    value: Option<T>,
    sharing: Option<Sharing>,
}

impl<T> Slot<T> {
    const fn new() -> Self {
        Self { value: None, sharing: None }
    }

    /// `true` while the accumulated value came from an `enforce` layer.
    const fn is_enforced(&self) -> bool {
        matches!(self.sharing, Some(Sharing::Enforce))
    }
}

/// Whether a layer's field is visible to the requesting tenant: a field
/// carrying `private` on a layer the requester does not own is invisible.
const fn visible(owned: bool, sharing: Sharing) -> bool {
    owned || !matches!(sharing, Sharing::Private)
}

/// Merge the ordered layer list into one effective configuration.
///
/// `layers` must be ordered base -> most specific: the upstream base
/// configuration first, then the route configuration, then the tenant chain
/// root to leaf.
// @cpt-begin:cpt-cf-oagw-flow-gear-foundation-effective-config:p1:inst-gf-eff-2
// `inst-gf-eff-2`: the layers are walked from base to most specific and each
// per-field rule of `cpt-cf-oagw-algo-gear-foundation-config-merge` applies.
#[must_use]
pub fn merge(
    layers: &[ConfigLayer],
    requester_tenant_id: Uuid,
    permissions: &OverridePermissions,
) -> EffectiveConfig {
    let mut scalars = Slot::<()>::new();
    let mut enabled = Slot::<bool>::new();
    let mut protocol = Slot::<String>::new();
    let mut server = Slot::<ServerConfig>::new();
    let mut headers = Slot::<HeadersConfig>::new();
    let mut auth = Slot::<AuthConfig>::new();
    let mut rate_limit = Slot::<RateLimitConfig>::new();
    let mut cors = Slot::<CorsConfig>::new();
    let mut plugins = Slot::<PluginsConfig>::new();
    let mut tags: Vec<String> = Vec::new();

    for layer in layers {
        let owned = layer.is_owned_by(requester_tenant_id);
        let share = &layer.sharing;

        // ---------------------------------------------------------------
        // Step 3: `private` on an ancestor layer is invisible.
        // ---------------------------------------------------------------
        if visible(owned, share.scalars) {
            // -----------------------------------------------------------
            // Step 4: `enforce` wins; every descendant value is discarded.
            // -----------------------------------------------------------
            let enforce = matches!(share.scalars, Sharing::Enforce);
            if enforce {
                scalars.value = Some(());
                scalars.sharing = Some(Sharing::Enforce);
            }
            if !scalars.is_enforced() {
                // -------------------------------------------------------
                // Step 10: scalars take the more specific value.
                // -------------------------------------------------------
                if let Some(value) = layer.enabled {
                    enabled.value = Some(value);
                    enabled.sharing = Some(share.scalars);
                }
                if let Some(value) = &layer.protocol {
                    protocol.value = Some(value.clone());
                    protocol.sharing = Some(share.scalars);
                }
                if let Some(value) = &layer.server {
                    server.value = Some(value.clone());
                    server.sharing = Some(share.scalars);
                }
                if let Some(value) = &layer.headers {
                    headers.value = Some(value.clone());
                    headers.sharing = Some(share.scalars);
                }
            }
        }

        // ---------------------------------------------------------------
        // Step 5: auth by override, gated on `oagw:upstream:override_auth`.
        // ---------------------------------------------------------------
        if let Some(candidate) = &layer.auth {
            if visible(owned, share.auth) && !auth.is_enforced() {
                // A descendant value replaces an `inherit` ancestor value only
                // when the requester holds the override permission. With no
                // inherited value there is nothing to override, so the
                // descendant's value stands; the base layer is the initial
                // effective value and always contributes.
                let nothing_inherited = auth.value.is_none();
                if nothing_inherited || layer.base || permissions.override_auth {
                    auth.value = Some(candidate.clone());
                    auth.sharing = Some(share.auth);
                }
                // Without the permission the ancestor value stands and no
                // error is surfaced.
            }
        }

        // ---------------------------------------------------------------
        // Step 6: rate limits by `min(ancestor, descendant)`, gated on
        // `oagw:upstream:override_rate`; enforced ancestor limits survive
        // alias shadowing.
        // ---------------------------------------------------------------
        if let Some(candidate) = &layer.rate_limit {
            if visible(owned, share.rate_limit) && !rate_limit.is_enforced() {
                let contributes =
                    layer.base || rate_limit.value.is_none() || permissions.override_rate;
                if contributes {
                    rate_limit.value = Some(match &rate_limit.value {
                        Some(ancestor) => ancestor.merge_stricter(candidate),
                        None => candidate.clone(),
                    });
                    rate_limit.sharing = Some(share.rate_limit);
                }
            }
        }

        // ---------------------------------------------------------------
        // Step 8: CORS origins by union under `inherit`; the enforced
        // ancestor origin set stays as-is under `enforce`.
        // ---------------------------------------------------------------
        if let Some(candidate) = &layer.cors {
            if visible(owned, share.cors) && !cors.is_enforced() {
                match share.cors {
                    Sharing::Enforce => {
                        cors.value = Some(candidate.clone());
                        cors.sharing = Some(Sharing::Enforce);
                    }
                    Sharing::Inherit => {
                        let merged = match &cors.value {
                            Some(previous) => union_cors_origins(previous, candidate),
                            None => candidate.clone(),
                        };
                        cors.value = Some(merged);
                        cors.sharing = Some(share.cors);
                    }
                    // A private CORS block owned by the requester (it is the
                    // owner) behaves like `inherit` for itself.
                    Sharing::Private => {
                        if cors.value.is_none() {
                            cors.value = Some(candidate.clone());
                            cors.sharing = Some(share.cors);
                        }
                    }
                }
            }
        }

        // ---------------------------------------------------------------
        // Step 9: plugin chains by concatenation, ancestor-then-descendant,
        // gated on `oagw:upstream:add_plugins`.
        // ---------------------------------------------------------------
        if let Some(candidate) = &layer.plugins {
            if visible(owned, share.plugins) && !plugins.is_enforced() {
                let appends =
                    layer.base || plugins.value.is_none() || permissions.add_plugins;
                match &plugins.value {
                    Some(previous) => {
                        if appends {
                            let mut items = previous.items.clone();
                            items.extend(candidate.items.iter().cloned());
                            plugins.value = Some(PluginsConfig {
                                sharing: candidate.sharing,
                                items,
                            });
                            plugins.sharing = Some(share.plugins);
                        }
                    }
                    None => {
                        plugins.value = Some(candidate.clone());
                        plugins.sharing = Some(share.plugins);
                    }
                }
            }
        }

        // ---------------------------------------------------------------
        // Step 7: tags by add-only union; inherited tags cannot be removed.
        // ---------------------------------------------------------------
        if let Some(candidate) = &layer.tags {
            if visible(owned, share.tags) && !matches!(share.tags, Sharing::Enforce) {
                for tag in candidate {
                    if !tags.contains(tag) {
                        tags.push(tag.clone());
                    }
                }
            }
        }
    }

    // @cpt-begin:cpt-cf-oagw-flow-gear-foundation-effective-config:p1:inst-gf-eff-7
    // `inst-gf-eff-7`: the resolved layer values are assembled into the one
    // effective configuration the pipeline consumes.
    EffectiveConfig {
        enabled: enabled.value,
        protocol: protocol.value,
        server: server.value,
        headers: headers.value,
        auth: auth.value,
        rate_limit: rate_limit.value,
        cors: cors.value,
        plugins: plugins.value,
        tags,
    }
    // @cpt-end:cpt-cf-oagw-flow-gear-foundation-effective-config:p1:inst-gf-eff-7
    // @cpt-end:cpt-cf-oagw-flow-gear-foundation-effective-config:p1:inst-gf-eff-2
}

/// Add-only union of two CORS origin sets, plus the four scalar CORS rules
/// (`cpt-cf-oagw-algo-cors-origin-set-merge` step 7): the ancestor's origin
/// entries are retained and the descendant's are appended, the method and
/// exposed-header lists union, `enabled` comes from the more specific layer
/// present, and `allow_credentials` escalates monotonically.
///
/// The one implementation of the field-level CORS merge lives in
/// [`crate::domain::cors::merge_fields`]; the *layer-level* semantics — which
/// layer contributes at all, and whether an ancestor `enforce` discards a
/// descendant addition — are decided by step 8 above.
// @cpt-begin:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-1
// `inst-cors-alg-mg-1` .. `-12-1`: the merge engine hands the two CORS layers
// it selected to the one field-level merge of the built-in CORS handler, so an
// inherited origin set can only grow and the scalar rules hold at every depth
// of the tenant chain.
fn union_cors_origins(ancestor: &CorsConfig, descendant: &CorsConfig) -> CorsConfig {
    crate::domain::cors::merge_fields(ancestor, descendant)
}
// @cpt-end:cpt-cf-oagw-algo-cors-origin-set-merge:p1:inst-cors-alg-mg-1

/// Build the base layer of an ordered layer list from an upstream record.
#[must_use]
pub fn upstream_base_layer(upstream: &crate::domain::dto::Upstream) -> ConfigLayer {
    ConfigLayer {
        base: true,
        owner_tenant_id: upstream.tenant_id,
        sharing: crate::domain::merge::LayerSharing {
            scalars: Sharing::Inherit,
            auth: Sharing::from_sharing_mode(
                upstream.auth.as_ref().map_or(crate::domain::dto::SharingMode::Private, |a| a.sharing),
            ),
            rate_limit: Sharing::from_sharing_mode(
                upstream
                    .rate_limit
                    .as_ref()
                    .map_or(crate::domain::dto::SharingMode::Private, |r| r.sharing),
            ),
            cors: Sharing::from_sharing_mode(
                upstream.cors.as_ref().map_or(crate::domain::dto::SharingMode::Private, |c| c.sharing),
            ),
            plugins: Sharing::from_sharing_mode(
                upstream
                    .plugins
                    .as_ref()
                    .map_or(crate::domain::dto::SharingMode::Private, |p| p.sharing),
            ),
            tags: Sharing::Inherit,
        },
        enabled: Some(upstream.enabled),
        protocol: Some(upstream.protocol.clone()),
        server: Some(upstream.server.clone()),
        headers: upstream.headers.clone(),
        auth: upstream.auth.clone(),
        rate_limit: upstream.rate_limit.clone(),
        cors: upstream.cors.clone(),
        plugins: upstream.plugins.clone(),
        tags: Some(upstream.tags.clone()),
    }
}

/// Build a route layer from a route record (second position of the ordered
/// layer list).
#[must_use]
pub fn route_layer(route: &crate::domain::dto::Route) -> ConfigLayer {
    ConfigLayer {
        base: false,
        owner_tenant_id: route.tenant_id,
        sharing: crate::domain::merge::LayerSharing {
            scalars: Sharing::Inherit,
            auth: Sharing::Private,
            rate_limit: Sharing::from_sharing_mode(
                route
                    .rate_limit
                    .as_ref()
                    .map_or(crate::domain::dto::SharingMode::Private, |r| r.sharing),
            ),
            cors: Sharing::from_sharing_mode(
                route.cors.as_ref().map_or(crate::domain::dto::SharingMode::Private, |c| c.sharing),
            ),
            plugins: Sharing::from_sharing_mode(
                route
                    .plugins
                    .as_ref()
                    .map_or(crate::domain::dto::SharingMode::Private, |p| p.sharing),
            ),
            tags: Sharing::Inherit,
        },
        enabled: Some(route.enabled),
        protocol: None,
        server: None,
        headers: None,
        auth: None,
        rate_limit: route.rate_limit.clone(),
        cors: route.cors.clone(),
        plugins: route.plugins.clone(),
        tags: Some(route.tags.clone()),
    }
}

/// A tenant-chain layer.
#[must_use]
pub fn tenant_layer(owner_tenant_id: Uuid) -> ConfigLayer {
    ConfigLayer::new(owner_tenant_id)
}

/// The `invalid_value` echo the error contract permits, bounded before it can
/// ever reach a rendered body.
#[must_use]
pub fn bound_target_host_echo(value: &str) -> String {
    bound_invalid_value(value)
}

#[cfg(test)]
#[path = "merge_tests.rs"]
mod tests;

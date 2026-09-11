//! Effective configuration across the tenant chain (`DESIGN.md` §
//! "Hierarchical Configuration").
//!
//! Alias resolution picks the closest upstream on the chain; the ancestor
//! constraints that shadowing must not bypass are merged onto it here, so the
//! rest of the pipeline consumes one upstream and never re-derives the
//! inheritance rules.

use crate::domain::model::{AuthConfig, Cors, PluginSet, RateLimit, SharingMode, Upstream};

/// A participant of the chain above the selected upstream.
#[derive(Debug, Clone, Copy)]
pub struct Ancestor<'a> {
    /// The ancestor upstream.
    pub upstream: &'a Upstream,
}

impl<'a> Ancestor<'a> {
    /// Wraps an upstream as a chain participant.
    #[must_use]
    pub fn new(upstream: &'a Upstream) -> Self {
        Self { upstream }
    }

    /// The rate limit the ancestor imposes on its descendants.
    ///
    /// `None` when it declares none, or declares it `private`.
    #[must_use]
    pub fn enforced_rate_limit(&self) -> Option<&'a RateLimit> {
        match self.upstream.rate_limit.as_ref() {
            Some(limit) if limit.sharing == SharingMode::Enforce => Some(limit),
            _ => None,
        }
    }

    /// The rate limit a descendant may inherit when it declares none of its own.
    #[must_use]
    pub fn offered_rate_limit(&self) -> Option<&'a RateLimit> {
        match self.upstream.rate_limit.as_ref() {
            Some(limit) if limit.sharing == SharingMode::Inherit => Some(limit),
            _ => None,
        }
    }

    /// The auth binding an ancestor offers its descendants.
    #[must_use]
    pub fn offered_auth(&self) -> Option<&'a AuthConfig> {
        match self.upstream.auth.as_ref() {
            Some(auth) if auth.sharing == SharingMode::Inherit => Some(auth),
            _ => None,
        }
    }

    /// The auth binding a descendant cannot override.
    #[must_use]
    pub fn enforced_auth(&self) -> Option<&'a AuthConfig> {
        match self.upstream.auth.as_ref() {
            Some(auth) if auth.sharing == SharingMode::Enforce => Some(auth),
            _ => None,
        }
    }

    /// The plugin bindings an ancestor contributes to the chain.
    #[must_use]
    pub fn offered_plugins(&self) -> Option<&'a PluginSet> {
        match self.upstream.plugins.as_ref() {
            Some(set) if set.sharing != SharingMode::Private => Some(set),
            _ => None,
        }
    }

    /// The CORS policy an ancestor offers its descendants.
    #[must_use]
    pub fn offered_cors(&self) -> Option<&'a Cors> {
        match self.upstream.cors.as_ref() {
            Some(cors) if cors.sharing == SharingMode::Inherit => Some(cors),
            _ => None,
        }
    }

    /// The CORS policy a descendant cannot override.
    #[must_use]
    pub fn enforced_cors(&self) -> Option<&'a Cors> {
        match self.upstream.cors.as_ref() {
            Some(cors) if cors.sharing == SharingMode::Enforce => Some(cors),
            _ => None,
        }
    }
}

/// The configuration one request actually runs under: the closest upstream on
/// the chain with every enforced ancestor constraint folded in.
///
/// `ancestors` runs closest first, matching [`crate::domain::services::TenantChain::chain`].
#[must_use]
pub fn effective(selected: &Upstream, ancestors: &[Ancestor<'_>]) -> Upstream {
    let mut merged = selected.clone();

    // Tags have no sharing mode: they accumulate from the root down, and a
    // descendant can add to them but never remove.
    let mut tags = Vec::new();
    for ancestor in ancestors.iter().rev() {
        tags = union(&tags, &ancestor.upstream.tags);
    }
    merged.tags = union(&tags, &selected.tags);

    // Auth: the closest enforced binding replaces the descendant's own; an
    // inherited one only fills a gap.
    if let Some(forced) = ancestors.iter().find_map(Ancestor::enforced_auth) {
        merged.auth = Some(forced.clone());
    } else if merged.auth.is_none() {
        merged.auth = ancestors.iter().find_map(Ancestor::offered_auth).cloned();
    }

    // Rate limits: stricter always wins, and an enforced ancestor is never
    // bypassed by shadowing.
    for ancestor in ancestors {
        if let Some(limit) = ancestor.enforced_rate_limit() {
            merged.rate_limit = Some(narrower(limit, merged.rate_limit.as_ref()));
        }
    }
    if merged.rate_limit.is_none() {
        merged.rate_limit = ancestors
            .iter()
            .find_map(Ancestor::offered_rate_limit)
            .copied();
    }

    // Plugins: the ancestor chain runs ahead of the descendant's own.
    let mut items = Vec::new();
    for ancestor in ancestors.iter().rev() {
        if let Some(set) = ancestor.offered_plugins() {
            items.extend(set.items.iter().cloned());
        }
    }
    if !items.is_empty() {
        if let Some(own) = selected.plugins.as_ref() {
            items.extend(own.items.iter().cloned());
        }
        merged.plugins = Some(PluginSet {
            sharing: SharingMode::Private,
            items,
        });
    }

    // CORS: a forced policy replaces the descendant's; an inherited one widens it.
    if let Some(forced) = ancestors.iter().find_map(Ancestor::enforced_cors) {
        merged.cors = Some(forced.clone());
    } else {
        for ancestor in ancestors.iter().rev() {
            let Some(offered) = ancestor.offered_cors() else {
                continue;
            };
            merged.cors = Some(match merged.cors.take() {
                Some(own) => widen(&own, offered),
                None => offered.clone(),
            });
        }
    }

    merged
}

/// Tags accumulate: `base` keeps its order, `additions` extend it.
fn union(base: &[String], additions: &[String]) -> Vec<String> {
    let mut tags = base.to_vec();
    for tag in additions {
        if !tags.contains(tag) {
            tags.push(tag.clone());
        }
    }
    tags
}

/// The stricter of two rate limits: fewer tokens per second, and on a tie, the
/// smaller burst.
fn narrower(ancestor: &RateLimit, descendant: Option<&RateLimit>) -> RateLimit {
    let Some(descendant) = descendant else {
        return *ancestor;
    };
    let ancestor_rate = ancestor.rate_per_second();
    let descendant_rate = descendant.rate_per_second();
    if ancestor_rate < descendant_rate
        || (ancestor_rate == descendant_rate && ancestor.capacity() < descendant.capacity())
    {
        *ancestor
    } else {
        *descendant
    }
}

/// Widens a descendant's CORS policy with an inherited ancestor's.
fn widen(descendant: &Cors, ancestor: &Cors) -> Cors {
    let mut merged = descendant.clone();
    for origin in &ancestor.allowed_origins {
        if !merged.allowed_origins.contains(origin) {
            merged.allowed_origins.push(origin.clone());
        }
    }
    if ancestor.enabled && !merged.enabled {
        merged.enabled = true;
    }
    if ancestor.allow_credentials {
        merged.allow_credentials = true;
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::model::{Endpoint, PluginBinding, Protocol, Scheme, ServerConfig};

    fn upstream(tags: &[&str]) -> Upstream {
        Upstream {
            id: Some(uuid::Uuid::new_v4()),
            enabled: true,
            alias: Some("vendor.test".to_owned()),
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Https,
                    host: "vendor.test".to_owned(),
                    port: Some(443),
                }],
            },
            protocol: Protocol::Http,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tenant_id: uuid::Uuid::new_v4(),
            created_at: 0,
            updated_at: 0,
        }
    }

    fn rate(rate: u32, window: crate::domain::model::RateWindow) -> RateLimit {
        RateLimit {
            sustained: crate::domain::model::SustainedRate { rate, window },
            ..RateLimit::default()
        }
    }

    fn auth(sharing: SharingMode) -> AuthConfig {
        AuthConfig {
            sharing,
            ..AuthConfig::default()
        }
    }

    #[test]
    fn an_enforced_ancestor_rate_limit_constrains_a_stricter_descendant() {
        let parent = upstream(&[]);
        let mut parent = parent;
        parent.rate_limit = Some(rate(2, crate::domain::model::RateWindow::Hour));
        parent.rate_limit.as_mut().expect("rate").sharing = SharingMode::Enforce;

        let mut child = upstream(&[]);
        child.rate_limit = Some(rate(1000, crate::domain::model::RateWindow::Second));

        let merged = effective(&child, &[Ancestor::new(&parent)]);
        let limit = merged.rate_limit.expect("inherited the constraint");
        assert_eq!(limit.sustained.rate, 2);
        assert_eq!(
            limit.sustained.window,
            crate::domain::model::RateWindow::Hour
        );
    }

    #[test]
    fn a_descendant_that_is_already_stricter_stays_in_charge() {
        let mut parent = upstream(&[]);
        parent.rate_limit = Some(rate(1000, crate::domain::model::RateWindow::Second));
        parent.rate_limit.as_mut().expect("rate").sharing = SharingMode::Enforce;

        let mut child = upstream(&[]);
        child.rate_limit = Some(rate(1, crate::domain::model::RateWindow::Second));

        let merged = effective(&child, &[Ancestor::new(&parent)]);
        let limit = merged.rate_limit.expect("own limit");
        assert_eq!(limit.sustained.rate, 1);
        assert_eq!(
            limit.sustained.window,
            crate::domain::model::RateWindow::Second
        );
    }

    #[test]
    fn a_private_ancestor_rate_limit_is_not_inherited() {
        let mut parent = upstream(&[]);
        parent.rate_limit = Some(rate(2, crate::domain::model::RateWindow::Hour));

        let child = upstream(&[]);
        let merged = effective(&child, &[Ancestor::new(&parent)]);
        assert!(merged.rate_limit.is_none(), "private is not shared");
    }

    #[test]
    fn an_inherited_auth_fills_a_gap_and_an_own_binding_overrides_it() {
        let mut parent = upstream(&[]);
        parent.auth = Some(auth(SharingMode::Inherit));
        parent.auth.as_mut().expect("auth").kind = "parent-auth".to_owned();

        let orphan = upstream(&[]);
        assert_eq!(
            effective(&orphan, &[Ancestor::new(&parent)])
                .auth
                .expect("inherited")
                .kind,
            "parent-auth"
        );

        let mut child = upstream(&[]);
        child.auth = Some(auth(SharingMode::Private));
        child.auth.as_mut().expect("auth").kind = "own-auth".to_owned();
        assert_eq!(
            effective(&child, &[Ancestor::new(&parent)])
                .auth
                .expect("own")
                .kind,
            "own-auth"
        );
    }

    #[test]
    fn an_enforced_auth_binding_beats_a_descendant_of_its_own() {
        let mut parent = upstream(&[]);
        parent.auth = Some(auth(SharingMode::Enforce));
        parent.auth.as_mut().expect("auth").kind = "parent-auth".to_owned();

        let mut child = upstream(&[]);
        child.auth = Some(auth(SharingMode::Private));
        child.auth.as_mut().expect("auth").kind = "own-auth".to_owned();

        assert_eq!(
            effective(&child, &[Ancestor::new(&parent)])
                .auth
                .expect("forced")
                .kind,
            "parent-auth"
        );
    }

    #[test]
    fn plugins_run_ancestor_first_and_private_ones_do_not_leak() {
        let binding = PluginBinding::Ref("gts.cf.core.oagw.guard_plugin.v1~x".to_owned());
        let mut parent = upstream(&[]);
        parent.plugins = Some(PluginSet {
            sharing: SharingMode::Inherit,
            items: vec![binding.clone()],
        });

        let mut child = upstream(&[]);
        child.plugins = Some(PluginSet {
            sharing: SharingMode::Private,
            items: vec![binding.clone()],
        });

        let merged = effective(&child, &[Ancestor::new(&parent)]);
        assert_eq!(merged.plugins.expect("plugins").items.len(), 2);

        let mut hidden = upstream(&[]);
        hidden.plugins = Some(PluginSet {
            sharing: SharingMode::Private,
            items: vec![binding],
        });
        let merged = effective(&child, &[Ancestor::new(&hidden)]);
        assert_eq!(merged.plugins.expect("own only").items.len(), 1);
    }

    #[test]
    fn tags_accumulate_across_the_chain_and_never_shrink() {
        let root = upstream(&["tier-1", "platform"]);
        let middle = upstream(&["tier-1", "edge"]);
        let leaf = upstream(&["edge", "tenant-a"]);

        let merged = effective(&leaf, &[Ancestor::new(&middle), Ancestor::new(&root)]);
        assert_eq!(merged.tags, vec!["tier-1", "platform", "edge", "tenant-a"]);
    }

    #[test]
    fn an_inherited_cors_policy_unions_origins() {
        let mut parent = upstream(&[]);
        parent.cors = Some(Cors {
            enabled: true,
            allowed_origins: vec!["https://platform.example".to_owned()],
            ..Cors::default()
        });
        parent.cors.as_mut().expect("cors").sharing = SharingMode::Inherit;

        let mut child = upstream(&[]);
        child.cors = Some(Cors {
            enabled: true,
            allowed_origins: vec!["https://tenant.example".to_owned()],
            ..Cors::default()
        });

        let merged = effective(&child, &[Ancestor::new(&parent)]);
        let cors = merged.cors.expect("cors");
        assert!(
            cors.allowed_origins
                .contains(&"https://platform.example".to_owned())
        );
        assert!(
            cors.allowed_origins
                .contains(&"https://tenant.example".to_owned())
        );
    }

    #[test]
    fn an_enforced_cors_policy_replaces_the_descendants_own() {
        let mut parent = upstream(&[]);
        parent.cors = Some(Cors {
            enabled: true,
            allowed_origins: vec!["https://platform.example".to_owned()],
            ..Cors::default()
        });
        parent.cors.as_mut().expect("cors").sharing = SharingMode::Enforce;

        let mut child = upstream(&[]);
        child.cors = Some(Cors {
            enabled: true,
            allowed_origins: vec!["https://tenant.example".to_owned()],
            ..Cors::default()
        });

        let merged = effective(&child, &[Ancestor::new(&parent)]);
        assert_eq!(
            merged.cors.expect("cors").allowed_origins,
            vec!["https://platform.example".to_owned()]
        );
    }

    #[test]
    fn the_selected_upstream_identity_survives_the_merge() {
        let mut parent = upstream(&["platform"]);
        parent.rate_limit = Some(rate(2, crate::domain::model::RateWindow::Hour));
        parent.rate_limit.as_mut().expect("rate").sharing = SharingMode::Enforce;

        let child = upstream(&["tenant"]);
        let merged = effective(&child, &[Ancestor::new(&parent)]);
        assert_eq!(merged.id, child.id);
        assert_eq!(merged.tenant_id, child.tenant_id);
        assert_eq!(merged.alias, child.alias);
    }
}

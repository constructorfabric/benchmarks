//! Merge the Effective Configuration Across the Hierarchy
//! (`cpt-cf-oagw-algo-proxy-merge-config`).

use crate::model::route::Route;
use crate::model::upstream::{AuthConfig, CorsConfig, HeadersConfig, RateLimitConfig, Sharing};
use crate::policy::ratelimit::limit::{EffectiveRateLimit, effective_from_config};
use crate::proxy::constants::{
    ADD_PLUGINS_PERMISSION, OVERRIDE_AUTH_PERMISSION, OVERRIDE_RATE_PERMISSION,
};
use crate::proxy::resolve::AncestorLevel;
use toolkit_security::SecurityContext;

/// RF-003: the effective rate-limit budget, carrying the full merged
/// `EffectiveRateLimit` (scope/strategy/burst/algorithm/cost) rather than a
/// bare `Option<u32>`, plus which resource contributed it -- needed by
/// `crate::proxy::engine` to build the real
/// `crate::policy::ratelimit::key::ScopeContext` (RF-003's "no
/// resource/route identity" gap).
#[derive(Debug, Clone)]
pub(crate) struct RateLimitPlan {
    pub effective: EffectiveRateLimit,
    /// `true` when the winning candidate was the matched Route's own
    /// declaration; `false` when it was the selected Upstream's own
    /// declaration or an ancestor's enforced one.
    pub from_route: bool,
}

/// One effective configuration object, merged per the documented
/// sharing-mode semantics, carrying the fields this path and the features
/// layered on it (2.7-2.9) consume.
#[derive(Debug, Clone, Default)]
pub(crate) struct EffectiveConfig {
    /// Consumed by DECOMPOSITION entry 2.9 (plugin-execution)
    /// (`crate::proxy::engine` builds the auth `PluginBinding` from this
    /// value's `auth_type`/`config`), which resolves and injects the
    /// credential this binding names; this feature only computes the
    /// merged value.
    pub auth: Option<AuthConfig>,
    /// The effective rate-limit budget, already reduced to
    /// `min(selected, route, enforced ancestors)` by normalized per-second
    /// rate, carrying the winning level's full `scope`/`strategy`/`burst`/
    /// `algorithm`/`cost` (RF-003) rather than a bare rate number; `None`
    /// when no level declares a rate limit at all.
    pub rate_limit: Option<RateLimitPlan>,
    /// Concatenated plugin bindings, ancestor-then-descendant and, within
    /// one level, upstream-bound then route-bound (RF-001: each binding's
    /// `config` object survives here rather than being discarded, even
    /// though the frozen `upstream.v1.schema.json`/`route.v1.schema.json`
    /// `plugins.items[]` shape carries only bare identifier strings for a
    /// guard/transform binding today -- see
    /// `crate::plugins::binding::PluginBinding::without_config`'s doc
    /// comment for that structural limitation. `EffectiveConfig::auth`
    /// above is the one binding kind the frozen schema *does* carry a
    /// `config` object for.
    pub plugins: Vec<crate::plugins::binding::PluginBinding>,
    pub cors: Option<CorsConfig>,
    /// Taken verbatim from the selected upstream: no cross-level merge.
    pub headers: Option<HeadersConfig>,
    /// Union of every visible level's tags; not consumed by this path's own
    /// forwarding decision, carried for observability/labelling by features
    /// layered on top.
    #[allow(dead_code)]
    pub tags: Vec<String>,
    pub timeout_secs: u32,
}

fn has_permission(ctx: &SecurityContext, permission: &str) -> bool {
    ctx.token_scopes()
        .iter()
        .any(|s| s == "*" || s == permission)
}

/// Fold `auth` root-to-child: `private` hides an ancestor's declaration from
/// descendants, `inherit` lets the selected (closest) level's own value
/// override it with the override permission, `enforce` fixes the value and
/// discards every deeper declaration (`inst-proxy-merge-foreach-auth`
/// through `inst-proxy-merge-auth-enforce`).
// @cpt-begin:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-foreach-auth
// @cpt-begin:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-auth-private
// @cpt-begin:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-auth-inherit
// @cpt-begin:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-auth-enforce
fn merge_auth(chain: &[AncestorLevel], ctx: &SecurityContext) -> Option<AuthConfig> {
    let mut effective: Option<AuthConfig> = None;
    let mut locked = false;
    let last_idx = chain.len().saturating_sub(1);
    for (idx, level) in chain.iter().enumerate() {
        if locked {
            continue;
        }
        let Some(declared) = &level.upstream.auth else {
            continue;
        };
        if idx == last_idx {
            // The routing target's own declaration: its own `sharing` value
            // governs propagation to descendants beyond it, which does not
            // exist here, so it plays no role in this decision. Only an
            // *ancestor's* sharing constrains whether this level's value is
            // allowed to take effect.
            if effective.is_none() || has_permission(ctx, OVERRIDE_AUTH_PERMISSION) {
                effective = Some(declared.clone());
            }
            continue;
        }
        match declared.sharing {
            Sharing::Enforce => {
                effective = Some(declared.clone());
                locked = true;
            }
            Sharing::Inherit => {
                effective = Some(declared.clone());
            }
            Sharing::Private => {
                // Hidden from every descendant; leaves `effective` unchanged.
            }
        }
    }
    effective
}
// @cpt-end:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-auth-enforce
// @cpt-end:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-auth-inherit
// @cpt-end:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-auth-private
// @cpt-end:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-foreach-auth

fn union_origins(a: &[String], b: &[String]) -> Vec<String> {
    let mut out = a.to_vec();
    for origin in b {
        if !out.contains(origin) {
            out.push(origin.clone());
        }
    }
    out
}

/// Fold `cors` root-to-child with the same `private`/`inherit`/`enforce`
/// plumbing as `auth`, except `inherit` unions origin sets rather than
/// overriding (`inst-proxy-merge-cors`).
// @cpt-begin:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-cors
fn merge_cors(chain: &[AncestorLevel]) -> Option<CorsConfig> {
    let mut effective: Option<CorsConfig> = None;
    let mut locked = false;
    let last_idx = chain.len().saturating_sub(1);
    for (idx, level) in chain.iter().enumerate() {
        if locked {
            continue;
        }
        let Some(declared) = &level.upstream.cors else {
            continue;
        };
        if idx == last_idx {
            // The routing target's own declaration always unions in (no
            // permission gate is documented for `cors`, unlike `auth`); its
            // own `sharing` value governs propagation beyond it, which does
            // not exist here.
            effective = Some(match effective {
                Some(prev) => CorsConfig {
                    allowed_origins: union_origins(
                        &prev.allowed_origins,
                        &declared.allowed_origins,
                    ),
                    ..declared.clone()
                },
                None => declared.clone(),
            });
            continue;
        }
        match declared.sharing {
            Sharing::Enforce => {
                effective = Some(declared.clone());
                locked = true;
            }
            Sharing::Inherit => {
                effective = Some(match effective {
                    Some(prev) => CorsConfig {
                        allowed_origins: union_origins(
                            &prev.allowed_origins,
                            &declared.allowed_origins,
                        ),
                        ..declared.clone()
                    },
                    None => declared.clone(),
                });
            }
            Sharing::Private => {
                // Hidden from every descendant; leaves `effective` unchanged.
            }
        }
    }
    effective
}
// @cpt-end:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-cors

/// `effective_rate = min(selected_rate, route_rate, all ancestor enforced
/// rates)` (`inst-proxy-merge-rate`), comparing each candidate's normalized
/// per-second rate (`crate::policy::ratelimit::limit::effective_from_config`)
/// rather than a raw, window-mixed `sustained.rate` number, so a
/// `sustained.window` difference between levels can never invert the
/// intended "tightest wins" comparison. A descendant's own declared rate
/// only enters the computation when there is no enforced ancestor rate to
/// override, or the calling tenant holds `oagw:upstream:override_rate`.
///
/// RF-003: the winning candidate's full `RateLimitConfig` -- `scope`,
/// `strategy`, `burst`, `algorithm`, `cost` -- survives into the returned
/// [`RateLimitPlan`] instead of being collapsed to a bare `u32`, and
/// [`RateLimitPlan::from_route`] records whether the Route's own
/// declaration won, so `crate::proxy::engine` can attribute the rate-limit
/// counter key to the correct resource.
// @cpt-begin:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-rate
fn merge_rate_limit(
    chain: &[AncestorLevel],
    route: &Route,
    ctx: &SecurityContext,
) -> Option<RateLimitPlan> {
    let selected = chain.last();
    let ancestor_enforced: Vec<&RateLimitConfig> = chain[..chain.len().saturating_sub(1)]
        .iter()
        .filter_map(|level| {
            let rl = level.upstream.rate_limit.as_ref()?;
            (rl.sharing == Sharing::Enforce).then_some(rl)
        })
        .collect();

    // `(config, from_route)` candidates, in the same precedence set the
    // pre-RF-003 implementation used: every enforcing ancestor, the
    // winning Route's own declaration (unconditional), and the selected
    // Upstream's own declaration (gated the same way as before).
    let mut candidates: Vec<(&RateLimitConfig, bool)> =
        ancestor_enforced.iter().map(|rl| (*rl, false)).collect();
    if let Some(route_rate) = route.rate_limit.as_ref() {
        candidates.push((route_rate, true));
    }
    if let Some(own_rate) = selected.and_then(|s| s.upstream.rate_limit.as_ref())
        && (ancestor_enforced.is_empty() || has_permission(ctx, OVERRIDE_RATE_PERMISSION))
    {
        candidates.push((own_rate, false));
    }

    candidates
        .into_iter()
        .map(|(config, from_route)| (effective_from_config(config), from_route))
        .min_by(|(a, _), (b, _)| a.rate_per_second.total_cmp(&b.rate_per_second))
        .map(|(effective, from_route)| RateLimitPlan {
            effective,
            from_route,
        })
}
// @cpt-end:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-rate

/// Concatenate ancestor-then-descendant plugin bindings, and within one
/// level upstream-bound items followed by the winning route's items when
/// that route belongs to this level (`inst-proxy-merge-plugins`). A level's
/// upstream plugins are hidden from a more-descendant level when
/// `sharing: private`; a descendant's own additions require
/// `oagw:upstream:add_plugins`.
// @cpt-begin:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-plugins
fn merge_plugins(
    chain: &[AncestorLevel],
    route: &Route,
    ctx: &SecurityContext,
) -> Vec<crate::plugins::binding::PluginBinding> {
    use crate::plugins::binding::PluginBinding;

    let can_add = has_permission(ctx, ADD_PLUGINS_PERMISSION);
    let last_idx = chain.len().saturating_sub(1);
    let mut out = Vec::new();
    for (idx, level) in chain.iter().enumerate() {
        let is_selected = idx == last_idx;
        if let Some(binding) = &level.upstream.plugins {
            let visible = binding.sharing != Sharing::Private || is_selected;
            let allowed = !is_selected || can_add || binding.items.is_empty();
            if visible && allowed {
                // RF-001: `items[]` is a bare identifier string on the
                // frozen wire format (`upstream.v1.schema.json`'s
                // `plugins.items[]` `oneOf` has no object/config branch),
                // so every binding built here is honestly config-less --
                // see `PluginBinding::without_config`'s doc comment.
                out.extend(
                    binding
                        .items
                        .iter()
                        .cloned()
                        .map(PluginBinding::without_config),
                );
            }
        }
        if route.upstream_id == level.upstream.id.unwrap_or_default()
            && let Some(route_plugins) = &route.plugins
        {
            let allowed = !is_selected || can_add || route_plugins.items.is_empty();
            if allowed {
                out.extend(
                    route_plugins
                        .items
                        .iter()
                        .cloned()
                        .map(PluginBinding::without_config),
                );
            }
        }
    }
    out
}
// @cpt-end:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-plugins

/// Union the tags of every visible level; no sharing mode, add-only
/// (`inst-proxy-merge-tags`).
// @cpt-begin:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-tags
fn merge_tags(chain: &[AncestorLevel]) -> Vec<String> {
    let mut out = Vec::new();
    for level in chain {
        for tag in &level.upstream.tags {
            if !out.contains(tag) {
                out.push(tag.clone());
            }
        }
    }
    out
}
// @cpt-end:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-tags

/// `cpt-cf-oagw-algo-proxy-merge-config`: produce one effective
/// configuration for the request, in priority order Upstream (base) <
/// Route < Tenant (`inst-proxy-merge-order`), walking the ancestor chain
/// root-to-child (`inst-proxy-merge-walk`).
// @cpt-algo:cpt-cf-oagw-algo-proxy-merge-config:p2
// @cpt-dod:cpt-cf-oagw-dod-proxy-config-merge:p1
// @cpt-begin:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-order
// @cpt-begin:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-walk
// @cpt-begin:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-headers
// @cpt-begin:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-timeout
// @cpt-begin:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-return
pub(crate) fn merge_config(
    chain: &[AncestorLevel],
    route: &Route,
    ctx: &SecurityContext,
    proxy_timeout_secs: u32,
) -> EffectiveConfig {
    let selected = chain.last();
    EffectiveConfig {
        auth: merge_auth(chain, ctx),
        rate_limit: merge_rate_limit(chain, route, ctx),
        plugins: merge_plugins(chain, route, ctx),
        cors: merge_cors(chain),
        headers: selected.and_then(|s| s.upstream.headers.clone()),
        tags: merge_tags(chain),
        timeout_secs: proxy_timeout_secs,
    }
}
// @cpt-end:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-return
// @cpt-end:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-timeout
// @cpt-end:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-headers
// @cpt-end:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-walk
// @cpt-end:cpt-cf-oagw-algo-proxy-merge-config:p2:inst-proxy-merge-order

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::route::{HttpMatch, PathSuffixMode, RouteMatch, RoutePluginsBinding};
    use crate::model::upstream::{
        Endpoint, EndpointScheme, PluginsBinding, RateLimitConfig, RateLimitWindow, ServerConfig,
        SustainedRate, Upstream,
    };
    use std::sync::Arc;
    use uuid::Uuid;

    fn ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .unwrap()
    }

    fn ctx_with(permission: &str) -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .token_scopes(vec![permission.to_owned()])
            .build()
            .unwrap()
    }

    fn base_upstream(tenant_id: Uuid) -> Upstream {
        Upstream {
            id: Some(Uuid::new_v4()),
            enabled: true,
            alias: Some("svc".to_owned()),
            tags: Vec::new(),
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Https,
                    host: "example.com".to_owned(),
                    port: 443,
                }],
            },
            protocol: "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1".to_owned(),
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tenant_id,
        }
    }

    fn level(distance: u32, upstream: Upstream) -> AncestorLevel {
        AncestorLevel {
            tenant_id: upstream.tenant_id,
            distance,
            upstream: Arc::new(upstream),
        }
    }

    fn route_for(upstream_id: Uuid) -> Route {
        Route {
            id: Some(Uuid::new_v4()),
            tenant_id: Uuid::new_v4(),
            tags: Vec::new(),
            upstream_id,
            route_match: RouteMatch {
                http: Some(HttpMatch {
                    methods: Vec::new(),
                    path: "/v1".to_owned(),
                    query_allowlist: Vec::new(),
                    path_suffix_mode: PathSuffixMode::Append,
                }),
                grpc: None,
            },
            plugins: None,
            rate_limit: None,
            enabled: true,
            priority: Some(1),
        }
    }

    #[test]
    fn enforce_ancestor_auth_survives_descendant_override_attempt() {
        let ancestor_tenant = Uuid::new_v4();
        let mut ancestor_up = base_upstream(ancestor_tenant);
        ancestor_up.auth = Some(AuthConfig {
            auth_type: Some("ancestor".to_owned()),
            sharing: Sharing::Enforce,
            config: serde_json::Value::Null,
        });
        let mut child_up = base_upstream(Uuid::new_v4());
        child_up.auth = Some(AuthConfig {
            auth_type: Some("child".to_owned()),
            sharing: Sharing::Private,
            config: serde_json::Value::Null,
        });
        let chain = vec![level(1, ancestor_up), level(0, child_up)];
        let effective = merge_auth(&chain, &ctx());
        assert_eq!(effective.unwrap().auth_type.as_deref(), Some("ancestor"));
    }

    #[test]
    fn inherit_ancestor_auth_is_overridden_only_with_permission() {
        let ancestor_tenant = Uuid::new_v4();
        let mut ancestor_up = base_upstream(ancestor_tenant);
        ancestor_up.auth = Some(AuthConfig {
            auth_type: Some("ancestor".to_owned()),
            sharing: Sharing::Inherit,
            config: serde_json::Value::Null,
        });
        let mut child_up = base_upstream(Uuid::new_v4());
        child_up.auth = Some(AuthConfig {
            auth_type: Some("child".to_owned()),
            sharing: Sharing::Private,
            config: serde_json::Value::Null,
        });
        let chain = vec![level(1, ancestor_up.clone()), level(0, child_up.clone())];

        let without_permission = merge_auth(&chain, &ctx());
        assert_eq!(
            without_permission.unwrap().auth_type.as_deref(),
            Some("ancestor")
        );

        let with_permission = merge_auth(&chain, &ctx_with(OVERRIDE_AUTH_PERMISSION));
        assert_eq!(with_permission.unwrap().auth_type.as_deref(), Some("child"));
    }

    #[test]
    fn rate_limit_takes_the_minimum_across_levels() {
        let mut ancestor_up = base_upstream(Uuid::new_v4());
        ancestor_up.rate_limit = Some(RateLimitConfig {
            sharing: Sharing::Enforce,
            algorithm: Default::default(),
            sustained: SustainedRate {
                rate: 50,
                window: RateLimitWindow::Second,
            },
            burst: None,
            scope: Default::default(),
            strategy: Default::default(),
            cost: 1,
        });
        let child_up = base_upstream(Uuid::new_v4());
        let chain = vec![level(1, ancestor_up), level(0, child_up.clone())];
        let mut route = route_for(child_up.id.unwrap());
        route.rate_limit = Some(RateLimitConfig {
            sharing: Sharing::Private,
            algorithm: Default::default(),
            sustained: SustainedRate {
                rate: 100,
                window: RateLimitWindow::Second,
            },
            burst: None,
            scope: Default::default(),
            strategy: Default::default(),
            cost: 1,
        });

        let effective = merge_rate_limit(&chain, &route, &ctx()).unwrap();
        assert_eq!(effective.effective.display_rate, 50);
        assert!(!effective.from_route);
    }

    /// RF-003: the winning candidate's `scope`/`strategy`/`burst` survive
    /// into the merged plan instead of being collapsed to a bare rate
    /// number, and the Route is correctly attributed as the contributor
    /// when its own (tighter) declaration wins.
    #[test]
    fn rate_limit_preserves_the_winning_levels_scope_strategy_and_attributes_the_route() {
        let upstream = base_upstream(Uuid::new_v4());
        let chain = vec![level(0, upstream.clone())];
        let mut route = route_for(upstream.id.unwrap());
        route.rate_limit = Some(RateLimitConfig {
            sharing: Sharing::Private,
            algorithm: Default::default(),
            sustained: SustainedRate {
                rate: 10,
                window: RateLimitWindow::Second,
            },
            burst: Some(crate::model::upstream::BurstConfig { capacity: Some(20) }),
            scope: crate::model::upstream::RateLimitScope::Ip,
            strategy: crate::model::upstream::RateLimitStrategy::Degrade,
            cost: 2,
        });

        let plan = merge_rate_limit(&chain, &route, &ctx()).unwrap();
        assert!(plan.from_route);
        assert_eq!(plan.effective.display_rate, 10);
        assert_eq!(plan.effective.burst_capacity, 20);
        assert_eq!(
            plan.effective.scope,
            crate::model::upstream::RateLimitScope::Ip
        );
        assert_eq!(
            plan.effective.strategy,
            crate::model::upstream::RateLimitStrategy::Degrade
        );
        assert_eq!(plan.effective.cost, 2);
    }

    #[test]
    fn plugins_concatenate_ancestor_then_descendant() {
        let mut ancestor_up = base_upstream(Uuid::new_v4());
        ancestor_up.plugins = Some(PluginsBinding {
            sharing: Sharing::Enforce,
            items: vec!["u1".to_owned(), "u2".to_owned()],
        });
        let mut child_up = base_upstream(Uuid::new_v4());
        child_up.plugins = Some(PluginsBinding {
            sharing: Sharing::Private,
            items: vec!["u3".to_owned()],
        });
        let chain = vec![level(1, ancestor_up), level(0, child_up.clone())];
        let mut route = route_for(child_up.id.unwrap());
        route.plugins = Some(RoutePluginsBinding {
            sharing: Sharing::Private,
            items: vec!["r1".to_owned()],
        });

        let plugins = merge_plugins(&chain, &route, &ctx_with(ADD_PLUGINS_PERMISSION));
        let refs: Vec<&str> = plugins.iter().map(|b| b.plugin_ref.as_str()).collect();
        assert_eq!(refs, vec!["u1", "u2", "u3", "r1"]);
    }

    /// RF-001: every binding `merge_plugins` produces from `plugins.items[]`
    /// is honestly config-less (`Value::Null`) -- the frozen wire format has
    /// no per-item `config` slot for a guard/transform binding (unlike
    /// `auth.config`, which does exist on the schema).
    #[test]
    fn merged_plugin_bindings_carry_no_config_per_the_frozen_wire_format() {
        let upstream = base_upstream(Uuid::new_v4());
        let mut upstream_with_plugins = upstream.clone();
        upstream_with_plugins.plugins = Some(PluginsBinding {
            sharing: Sharing::Private,
            items: vec!["some-guard-ref".to_owned()],
        });
        let chain = vec![level(0, upstream_with_plugins.clone())];
        let route = route_for(upstream_with_plugins.id.unwrap());
        let plugins = merge_plugins(&chain, &route, &ctx_with(ADD_PLUGINS_PERMISSION));
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].config, serde_json::Value::Null);
    }

    #[test]
    fn tags_union_additively_without_removal() {
        let mut ancestor_up = base_upstream(Uuid::new_v4());
        ancestor_up.tags = vec!["a".to_owned()];
        let mut child_up = base_upstream(Uuid::new_v4());
        child_up.tags = vec!["b".to_owned()];
        let chain = vec![level(1, ancestor_up), level(0, child_up)];
        assert_eq!(merge_tags(&chain), vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn headers_taken_verbatim_from_selected_upstream() {
        let mut child_up = base_upstream(Uuid::new_v4());
        child_up.headers = Some(HeadersConfig::default());
        let chain = vec![level(0, child_up.clone())];
        let route = route_for(child_up.id.unwrap());
        let effective = merge_config(&chain, &route, &ctx(), 2);
        assert!(effective.headers.is_some());
        assert_eq!(effective.timeout_secs, 2);
    }
}

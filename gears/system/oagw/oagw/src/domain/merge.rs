//! Hierarchical configuration merge (`DESIGN.md` §3.1).
//!
//! Effective configuration is computed from the tenant chain (root →
//! descendant):
//!
//! | field | strategy |
//! |---|---|
//! | auth | override if `inherit`; forced if `enforce`; invisible if `private` |
//! | rate limits | `min(ancestor, descendant)` |
//! | plugins | concatenation: ancestor chain + descendant chain |
//! | CORS | union origins if `inherit`; forced if `enforce` |
//! | headers | ancestor mandates, then the descendant's own rules |
//! | tags | add-only union (no sharing mode) |
//!
//! `inherit` ancestors are *defaults*: the closest configuration that declares
//! something of its own wins, and an ancestor value applies only when the
//! descendant contributes nothing of its own. `enforce` ancestors are
//! authoritative and are never bypassed by alias shadowing; `private`
//! ancestors are invisible.

use crate::domain::model::{
    AuthConfig, CorsConfig, HeadersConfig, PassthroughMode, PluginsConfig, RateLimitConfig,
    RequestHeaderRules, ResponseHeaderRules, SharingMode,
};

/// A snapshot of an ancestor upstream configuration, root first.
#[derive(Debug, Clone)]
pub struct AncestorConfig<'a> {
    /// Tenant the ancestor upstream belongs to.
    pub tenant_id: uuid::Uuid,
    /// The ancestor's upstream.
    pub upstream: &'a crate::domain::model::Upstream,
}

/// Builds the effective auth configuration for a selected upstream.
///
/// `chain` is root → descendant, excluding the selected upstream itself.
///
/// * `private` ancestors stay invisible.
/// * an `inherit` ancestor is a **default**: the selected upstream's own auth
///   wins whenever it declares one (`PRD.md` §5.5 "With `sharing: inherit`,
///   descendant with permission can use own credentials").
/// * an `enforce` ancestor is authoritative: it overrides the descendant's own
///   auth, and the root-most `enforce` ancestor wins when several enforce.
#[must_use]
pub fn effective_auth<'a>(
    chain: &[AncestorConfig<'a>],
    own: Option<&'a AuthConfig>,
) -> Option<&'a AuthConfig> {
    let mut inherited = None;
    let mut enforced = None;
    for ancestor in chain {
        if let Some(auth) = &ancestor.upstream.auth {
            match auth.sharing {
                SharingMode::Private => {}
                SharingMode::Inherit => {
                    if inherited.is_none() {
                        inherited = Some(auth);
                    }
                }
                SharingMode::Enforce => {
                    if enforced.is_none() {
                        enforced = Some(auth);
                    }
                }
            }
        }
    }
    // Ancestor `enforce` forces, then the descendant's own auth, then the
    // nearest `inherit` ancestor as a fallback.
    enforced.or(own).or(inherited)
}

/// The root-most ancestor header configuration that actually configures rules.
///
/// [`HeadersConfig`] carries no `sharing` mode in the contract, so an
/// ancestor's header rules are **mandates**: they apply whenever the ancestor
/// declares them, regardless of any sharing mode elsewhere on the upstream.
///
/// Returns `None` when no ancestor on the chain configures header rules.
#[must_use]
pub fn ancestor_header_mandate<'a>(chain: &[AncestorConfig<'a>]) -> Option<&'a HeadersConfig> {
    chain.iter().find_map(|ancestor| {
        let headers = ancestor.upstream.headers.as_ref()?;
        configures_headers(headers).then_some(headers)
    })
}

/// Whether a request rule set declares a passthrough policy of its own.
///
/// `PassthroughMode::None` is both the serde default and the "do not forward"
/// policy, so an explicitly configured `none` is indistinguishable from an
/// absent field; the composed policy is therefore the **stricter** of the two
/// ([`strictest`]), which keeps an ancestor mandate a floor rather than a
/// default.
fn strictest(a: PassthroughMode, b: PassthroughMode) -> PassthroughMode {
    use PassthroughMode::{All, Allowlist, None};
    match (a, b) {
        (None, _) | (_, None) => None,
        (Allowlist, _) | (_, Allowlist) => Allowlist,
        (All, All) => All,
    }
}

/// Whether a header configuration declares at least one rule.
fn configures_headers(headers: &HeadersConfig) -> bool {
    let request = headers.request.as_ref().is_some_and(|rules| {
        !rules.set.is_empty()
            || !rules.add.is_empty()
            || !rules.remove.is_empty()
            || rules.passthrough != PassthroughMode::None
            || !rules.passthrough_allowlist.is_empty()
    });
    let response = headers.response.as_ref().is_some_and(|rules| {
        !rules.set.is_empty() || !rules.add.is_empty() || !rules.remove.is_empty()
    });
    request || response
}

/// Builds the effective header rules for the selected upstream.
///
/// [`HeadersConfig`] carries no sharing mode, so an ancestor's header rules are
/// a **mandate**: they always apply, and a descendant's own rules are composed
/// *after* them rather than replacing them. The returned configuration carries
/// the ancestor's `remove`/`set`/`add` first and the descendant's second (for
/// `set` the descendant therefore overwrites the ancestor, for `add` both
/// values are appended). The inbound passthrough policy is the **stricter** of
/// the two, so an ancestor can lock down what a descendant forwards but never
/// widen it. With no ancestor contribution the descendant's own configuration
/// is returned unchanged, and with no descendant contribution the root-most
/// ancestor mandate applies as-is.
#[must_use]
pub fn effective_headers<'a>(
    chain: &[AncestorConfig<'a>],
    own: Option<&'a HeadersConfig>,
) -> Option<HeadersConfig> {
    let mandate = ancestor_header_mandate(chain);
    match (mandate, own) {
        (None, None) => None,
        (None, Some(own)) => Some(own.clone()),
        (Some(mandate), None) => Some(mandate.clone()),
        (Some(mandate), Some(own)) => Some(compose_headers(mandate, own)),
    }
}

/// Ancestor mandate first, descendant's own rules second.
fn compose_headers(mandate: &HeadersConfig, own: &HeadersConfig) -> HeadersConfig {
    HeadersConfig {
        request: compose_request_rules(mandate.request.as_ref(), own.request.as_ref()),
        response: compose_response_rules(mandate.response.as_ref(), own.response.as_ref()),
    }
}

fn compose_request_rules(
    mandate: Option<&RequestHeaderRules>,
    own: Option<&RequestHeaderRules>,
) -> Option<RequestHeaderRules> {
    match (mandate, own) {
        (None, None) => None,
        (None, Some(own)) => Some(own.clone()),
        (Some(mandate), None) => Some(mandate.clone()),
        (Some(mandate), Some(own)) => {
            let mut remove = mandate.remove.clone();
            remove.extend(own.remove.iter().cloned());
            let mut set = mandate.set.clone();
            for (name, value) in &own.set {
                set.insert(name.clone(), value.clone());
            }
            let mut add = mandate.add.clone();
            for (name, value) in &own.add {
                add.insert(name.clone(), value.clone());
            }
            // The stricter passthrough policy governs: an ancestor mandate can
            // lock down what a descendant forwards, never the reverse.
            let passthrough = strictest(mandate.passthrough, own.passthrough);
            let passthrough_allowlist = if passthrough == PassthroughMode::Allowlist {
                let mut allowlist = Vec::new();
                if mandate.passthrough == PassthroughMode::Allowlist {
                    allowlist.extend(mandate.passthrough_allowlist.iter().cloned());
                }
                if own.passthrough == PassthroughMode::Allowlist {
                    allowlist.extend(own.passthrough_allowlist.iter().cloned());
                }
                dedup(allowlist)
            } else {
                Vec::new()
            };
            Some(RequestHeaderRules {
                remove: dedup(remove),
                set,
                add,
                passthrough,
                passthrough_allowlist,
            })
        }
    }
}

fn compose_response_rules(
    mandate: Option<&ResponseHeaderRules>,
    own: Option<&ResponseHeaderRules>,
) -> Option<ResponseHeaderRules> {
    match (mandate, own) {
        (None, None) => None,
        (None, Some(own)) => Some(own.clone()),
        (Some(mandate), None) => Some(mandate.clone()),
        (Some(mandate), Some(own)) => {
            let mut remove = mandate.remove.clone();
            remove.extend(own.remove.iter().cloned());
            let mut set = mandate.set.clone();
            for (name, value) in &own.set {
                set.insert(name.clone(), value.clone());
            }
            let mut add = mandate.add.clone();
            for (name, value) in &own.add {
                add.insert(name.clone(), value.clone());
            }
            Some(ResponseHeaderRules {
                remove: dedup(remove),
                set,
                add,
            })
        }
    }
}

/// Builds the effective plugin chain.
///
/// Every ancestor whose plugin sharing mode is `inherit` or `enforce`
/// contributes its chain, root → descendant (`DESIGN.md` §3.2
/// "Concatenate: `ancestor.plugins + descendant.plugins`"), then the selected
/// upstream's own chain, then the matched route's chain. `private` ancestors
/// are excluded, and an `enforce` ancestor's bindings cannot be removed by a
/// descendant (`PRD.md` §5.5 "enforced plugins cannot be removed").
#[must_use]
pub fn effective_plugins<'a>(
    chain: &[AncestorConfig<'a>],
    own: Option<&'a PluginsConfig>,
    route: Option<&'a PluginsConfig>,
) -> PluginsConfig {
    let mut items: Vec<crate::domain::model::PluginBinding> = Vec::new();
    let mut enforced = false;
    for ancestor in chain {
        if let Some(plugins) = &ancestor.upstream.plugins
            && plugins.sharing != SharingMode::Private
        {
            items.extend(plugins.items.iter().cloned());
            enforced |= plugins.sharing == SharingMode::Enforce;
        }
    }
    let mut sharing = if enforced {
        SharingMode::Enforce
    } else {
        SharingMode::Private
    };
    if let Some(plugins) = own {
        items.extend(plugins.items.iter().cloned());
        sharing = plugins.sharing;
    }
    if enforced {
        // An ancestor `enforce` block cannot be relaxed by the descendant's own
        // `sharing` mode: the enforced bindings stay non-removable.
        sharing = SharingMode::Enforce;
    }
    if let Some(plugins) = route {
        items.extend(plugins.items.iter().cloned());
    }
    PluginsConfig { sharing, items }
}

/// Builds the effective CORS configuration.
///
/// * an `inherit` ancestor is a **fallback baseline**: its origins are unioned
///   with the descendant's own (ADR-0004 "With `inherit`, child origins are
///   unioned with parent origins"), and its whole configuration applies when
///   the descendant contributes no CORS of its own.
/// * an `enforce` ancestor is **authoritative**: origins, allowed methods,
///   exposed headers and `allow_credentials` all come from the ancestor, so a
///   descendant cannot widen any dimension of the policy (ADR-0004 "With
///   `sharing: enforce`, child cannot add origins").
/// * with no ancestor contribution, the descendant's own configuration (route
///   first, then upstream) applies unchanged.
#[must_use]
pub fn effective_cors<'a>(
    chain: &[AncestorConfig<'a>],
    own: Option<&'a CorsConfig>,
    route: Option<&'a CorsConfig>,
) -> Option<CorsConfig> {
    let mut inherited: Option<&CorsConfig> = None;
    let mut enforced: Option<CorsConfig> = None;

    for ancestor in chain {
        let Some(cors) = &ancestor.upstream.cors else {
            continue;
        };
        if !cors.enabled || cors.sharing == SharingMode::Private {
            continue;
        }
        if cors.sharing == SharingMode::Enforce {
            enforced = Some(match &enforced {
                // Several `enforce` ancestors union into one wider mandate.
                Some(previous) => union_cors(previous, cors),
                None => cors.clone(),
            });
        } else if inherited.is_none() {
            inherited = Some(cors);
        }
    }

    let base = route.or(own);

    if let Some(forced) = enforced {
        // Ancestor `enforce` is authoritative in every dimension: the
        // descendant cannot add origins, methods, exposed headers or
        // credentials, and cannot switch CORS off either.
        return Some(CorsConfig {
            sharing: SharingMode::Enforce,
            enabled: true,
            ..forced
        });
    }

    let Some(own_config) = base else {
        // Only ancestors contribute: an `inherit` ancestor's configuration
        // applies as-is to a descendant that declares nothing.
        return inherited.map(|cors| CorsConfig {
            sharing: SharingMode::Inherit,
            ..cors.clone()
        });
    };

    if !own_config.enabled {
        // The descendant explicitly disabled CORS, so the `inherit` ancestor
        // baseline is not merged in either.
        return None;
    }

    let mut origins = inherited.map_or(Vec::new(), |config| config.allowed_origins.clone());
    origins.extend(own_config.allowed_origins.iter().cloned());
    Some(CorsConfig {
        sharing: own_config.sharing,
        enabled: true,
        allowed_origins: dedup(origins),
        allowed_methods: dedup(own_config.allowed_methods.clone()),
        expose_headers: dedup(own_config.expose_headers.clone()),
        allow_credentials: own_config.allow_credentials,
    })
}

/// Union of two `enforce` CORS mandates: the widest of every dimension.
fn union_cors(previous: &CorsConfig, next: &CorsConfig) -> CorsConfig {
    let mut origins = previous.allowed_origins.clone();
    origins.extend(next.allowed_origins.iter().cloned());
    let mut methods = previous.allowed_methods.clone();
    methods.extend(next.allowed_methods.iter().cloned());
    let mut expose = previous.expose_headers.clone();
    expose.extend(next.expose_headers.iter().cloned());
    CorsConfig {
        sharing: SharingMode::Enforce,
        enabled: true,
        allowed_origins: dedup(origins),
        allowed_methods: dedup(methods),
        expose_headers: dedup(expose),
        allow_credentials: previous.allow_credentials || next.allow_credentials,
    }
}

/// Builds the effective rate limit by delegating to [`crate::domain::ratelimit`].
#[must_use]
pub fn effective_rate_limit<'a>(
    chain: &[AncestorConfig<'a>],
    own: Option<&'a RateLimitConfig>,
    route: Option<&'a RateLimitConfig>,
) -> Option<RateLimitConfig> {
    let contributions: Vec<crate::domain::ratelimit::LimitContribution<'a>> = chain
        .iter()
        .filter_map(|ancestor| {
            ancestor.upstream.rate_limit.as_ref().map(|limit| {
                crate::domain::ratelimit::LimitContribution {
                    tenant_id: ancestor.tenant_id,
                    limit,
                }
            })
        })
        .collect();
    crate::domain::ratelimit::effective_limit(&contributions, own, route)
}

/// Union of ancestor and descendant tags (add-only, descendants cannot remove).
#[must_use]
pub fn effective_tags<'a>(chain: &[AncestorConfig<'a>], own: Option<&'a [String]>) -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for ancestor in chain {
        for tag in &ancestor.upstream.tags {
            if !tags.iter().any(|t| t == tag) {
                tags.push(tag.clone());
            }
        }
    }
    if let Some(own) = own {
        for tag in own {
            if !tags.iter().any(|t| t == tag) {
                tags.push(tag.clone());
            }
        }
    }
    tags
}

fn dedup(values: Vec<String>) -> Vec<String> {
    let mut seen: Vec<String> = Vec::with_capacity(values.len());
    for value in values {
        if !seen.iter().any(|v| v == &value) {
            seen.push(value);
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::domain::gts;
    use crate::domain::model::RequestHeaderRules;
    use crate::domain::model::{
        BurstCapacity, Endpoint, EndpointScheme, RateWindow, ServerConfig, SustainedRate, Upstream,
    };
    use std::collections::BTreeMap;

    fn upstream(mode: SharingMode, rate: Option<u32>, tags: &[&str]) -> Upstream {
        Upstream {
            id: uuid::Uuid::new_v4(),
            tenant_id: uuid::Uuid::new_v4(),
            alias: "vendor.com".to_owned(),
            protocol: gts::PROTOCOL_HTTP.to_owned(),
            enabled: true,
            server: ServerConfig {
                endpoints: vec![Endpoint {
                    scheme: EndpointScheme::Https,
                    host: "api.vendor.com".to_owned(),
                    port: 443,
                }],
            },
            auth: None,
            headers: None,
            rate_limit: rate.map(|rate| RateLimitConfig {
                sharing: mode,
                algorithm: crate::domain::model::RateAlgorithm::TokenBucket,
                sustained: SustainedRate {
                    rate,
                    window: RateWindow::Second,
                },
                burst: Some(BurstCapacity { capacity: rate }),
                scope: crate::domain::model::RateScope::Tenant,
                strategy: crate::domain::model::RateStrategy::Reject,
                cost: 1,
                response_headers: true,
            }),
            cors: None,
            plugins: None,
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
            created_at: 0,
        }
    }

    #[test]
    fn tags_are_additive_union() {
        let root = upstream(SharingMode::Enforce, None, &["llm", "vendor"]);
        let own = vec!["vendor".to_owned(), "extra".to_owned()];
        let chain = vec![AncestorConfig {
            tenant_id: root.tenant_id,
            upstream: &root,
        }];
        let tags = effective_tags(&chain, Some(&own));
        assert_eq!(
            tags,
            vec!["llm".to_owned(), "vendor".to_owned(), "extra".to_owned()]
        );
    }

    #[test]
    fn enforced_rate_limit_is_never_bypassed() {
        let root = upstream(SharingMode::Enforce, Some(10), &[]);
        let own = upstream(SharingMode::Inherit, Some(1000), &[]);
        let chain = vec![AncestorConfig {
            tenant_id: root.tenant_id,
            upstream: &root,
        }];
        let effective = effective_rate_limit(&chain, own.rate_limit.as_ref(), None).unwrap();
        assert_eq!(effective.sustained.rate, 10);
    }

    #[test]
    fn private_ancestor_rate_limit_is_invisible() {
        let root = upstream(SharingMode::Private, Some(10), &[]);
        let own = upstream(SharingMode::Private, Some(1000), &[]);
        let chain = vec![AncestorConfig {
            tenant_id: root.tenant_id,
            upstream: &root,
        }];
        let effective = effective_rate_limit(&chain, own.rate_limit.as_ref(), None).unwrap();
        assert_eq!(effective.sustained.rate, 1000);
    }

    fn ancestor<'a>(upstream: &'a Upstream) -> AncestorConfig<'a> {
        AncestorConfig {
            tenant_id: upstream.tenant_id,
            upstream,
        }
    }

    fn chain_of<'a>(items: &[&'a Upstream]) -> Vec<AncestorConfig<'a>> {
        items.iter().map(|upstream| ancestor(upstream)).collect()
    }

    fn upstream_with_headers(headers: HeadersConfig) -> Upstream {
        let mut upstream = upstream(SharingMode::Enforce, None, &[]);
        upstream.headers = Some(headers);
        upstream
    }

    fn auth(mode: SharingMode, secret: &str) -> AuthConfig {
        AuthConfig {
            auth_type: Some(gts::AUTH_PLUGIN_APIKEY.to_owned()),
            sharing: mode,
            config: Some(serde_json::json!({ "secret_ref": secret })),
        }
    }

    fn plugins(mode: SharingMode, refs: &[&str]) -> PluginsConfig {
        PluginsConfig {
            sharing: mode,
            items: refs
                .iter()
                .map(|reference| crate::domain::model::PluginBinding::Bare((*reference).to_owned()))
                .collect(),
        }
    }

    fn cors(mode: SharingMode, origins: &[&str], credentials: bool) -> CorsConfig {
        CorsConfig {
            sharing: mode,
            enabled: true,
            allowed_origins: origins.iter().map(|o| (*o).to_owned()).collect(),
            allowed_methods: vec!["GET".to_owned()],
            expose_headers: vec!["x-request-id".to_owned()],
            allow_credentials: credentials,
        }
    }

    #[test]
    fn descendant_own_auth_overrides_an_inherit_ancestor() {
        let root = upstream(SharingMode::Inherit, None, &[]);
        let mut ancestor_upstream = root;
        ancestor_upstream.auth = Some(auth(SharingMode::Inherit, "cred://partner-key"));

        let chain = chain_of(&[&ancestor_upstream]);
        let own = auth(SharingMode::Private, "cred://my-own-key");
        let effective = effective_auth(&chain, Some(&own)).expect("own auth wins");
        assert_eq!(
            effective.config,
            Some(serde_json::json!({ "secret_ref": "cred://my-own-key" }))
        );
    }

    #[test]
    fn inherit_ancestor_auth_is_the_fallback_when_the_descendant_has_none() {
        let mut ancestor_upstream = upstream(SharingMode::Inherit, None, &[]);
        ancestor_upstream.auth = Some(auth(SharingMode::Inherit, "cred://partner-key"));
        let chain = chain_of(&[&ancestor_upstream]);
        assert!(effective_auth(&chain, None).is_some());
        assert!(effective_auth(&chain, None).unwrap().config.is_some());
    }

    #[test]
    fn enforced_ancestor_auth_overrides_the_descendant() {
        let mut ancestor_upstream = upstream(SharingMode::Enforce, None, &[]);
        ancestor_upstream.auth = Some(auth(SharingMode::Enforce, "cred://partner-key"));
        let chain = chain_of(&[&ancestor_upstream]);
        let own = auth(SharingMode::Private, "cred://my-own-key");
        let effective = effective_auth(&chain, Some(&own)).expect("enforced auth wins");
        assert_ne!(
            effective.config,
            Some(serde_json::json!({ "secret_ref": "cred://my-own-key" }))
        );
    }

    #[test]
    fn private_ancestor_auth_stays_invisible() {
        let mut ancestor_upstream = upstream(SharingMode::Private, None, &[]);
        ancestor_upstream.auth = Some(auth(SharingMode::Private, "cred://partner-key"));
        let chain = chain_of(&[&ancestor_upstream]);
        assert!(
            effective_auth(&chain, None).is_none(),
            "private is invisible"
        );
        let own = auth(SharingMode::Private, "cred://my-own-key");
        assert_eq!(
            effective_auth(&chain, Some(&own)).unwrap().config,
            own.config
        );
    }

    #[test]
    fn inherit_ancestor_plugins_are_concatenated() {
        let mut ancestor_upstream = upstream(SharingMode::Inherit, None, &[]);
        ancestor_upstream.plugins = Some(plugins(
            SharingMode::Inherit,
            &["gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1"],
        ));
        let chain = chain_of(&[&ancestor_upstream]);
        let own = plugins(
            SharingMode::Private,
            &["gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1"],
        );
        let effective = effective_plugins(&chain, Some(&own), None);
        assert_eq!(
            effective.items.len(),
            2,
            "ancestor chain is concatenated first"
        );
        assert_eq!(
            effective.items[0].plugin_ref(),
            gts::GUARD_PLUGIN_REQUIRED_HEADERS
        );
        assert_eq!(
            effective.items[1].plugin_ref(),
            gts::TRANSFORM_PLUGIN_REQUEST_ID
        );
    }

    #[test]
    fn private_ancestor_plugins_are_excluded() {
        let mut ancestor_upstream = upstream(SharingMode::Private, None, &[]);
        ancestor_upstream.plugins = Some(plugins(
            SharingMode::Private,
            &[gts::GUARD_PLUGIN_REQUIRED_HEADERS],
        ));
        let chain = chain_of(&[&ancestor_upstream]);
        let effective = effective_plugins(&chain, None, None);
        assert!(effective.items.is_empty());
    }

    #[test]
    fn enforced_ancestor_plugins_cannot_be_removed() {
        let mut ancestor_upstream = upstream(SharingMode::Enforce, None, &[]);
        ancestor_upstream.plugins = Some(plugins(
            SharingMode::Enforce,
            &[gts::GUARD_PLUGIN_REQUIRED_HEADERS],
        ));
        let chain = chain_of(&[&ancestor_upstream]);
        let own = plugins(SharingMode::Private, &[gts::TRANSFORM_PLUGIN_REQUEST_ID]);
        let effective = effective_plugins(&chain, Some(&own), None);
        assert_eq!(effective.items.len(), 2);
        assert_eq!(effective.sharing, SharingMode::Enforce);
    }

    #[test]
    fn inherit_ancestor_cors_origins_are_unioned() {
        let mut ancestor_upstream = upstream(SharingMode::Inherit, None, &[]);
        ancestor_upstream.cors = Some(cors(
            SharingMode::Inherit,
            &["https://app.example.com"],
            false,
        ));
        let chain = chain_of(&[&ancestor_upstream]);
        let own = cors(SharingMode::Private, &["https://admin.example.com"], false);
        let effective = effective_cors(&chain, Some(&own), None).expect("merged");
        assert_eq!(
            effective.allowed_origins,
            vec![
                "https://app.example.com".to_owned(),
                "https://admin.example.com".to_owned()
            ]
        );
        // The descendant's own methods and credentials are preserved.
        assert_eq!(effective.allowed_methods, own.allowed_methods);
        assert!(!effective.allow_credentials);
    }

    #[test]
    fn inherit_ancestor_cors_applies_when_the_descendant_has_none() {
        let mut ancestor_upstream = upstream(SharingMode::Inherit, None, &[]);
        ancestor_upstream.cors = Some(cors(
            SharingMode::Inherit,
            &["https://app.example.com"],
            true,
        ));
        ancestor_upstream.cors.as_mut().unwrap().allowed_methods = vec!["PUT".to_owned()];
        let chain = chain_of(&[&ancestor_upstream]);
        let effective = effective_cors(&chain, None, None).expect("ancestor baseline");
        assert_eq!(
            effective.allowed_origins,
            vec!["https://app.example.com".to_owned()]
        );
        assert_eq!(effective.allowed_methods, vec!["PUT".to_owned()]);
        assert!(effective.allow_credentials);
        assert_eq!(effective.sharing, SharingMode::Inherit);
    }

    #[test]
    fn enforced_ancestor_cors_cannot_be_widened() {
        let mut ancestor_upstream = upstream(SharingMode::Enforce, None, &[]);
        ancestor_upstream.cors = Some(cors(
            SharingMode::Enforce,
            &["https://app.example.com"],
            false,
        ));
        ancestor_upstream.cors.as_mut().unwrap().allowed_methods = vec!["GET".to_owned()];
        let chain = chain_of(&[&ancestor_upstream]);
        let mut own = cors(SharingMode::Private, &["https://evil.example.com"], true);
        own.allowed_methods = vec!["DELETE".to_owned(), "PATCH".to_owned()];
        own.expose_headers = vec!["x-secret".to_owned()];

        let effective = effective_cors(&chain, Some(&own), None).expect("enforced config");
        assert_eq!(effective.sharing, SharingMode::Enforce);
        assert_eq!(
            effective.allowed_origins,
            vec!["https://app.example.com".to_owned()],
            "a descendant cannot add origins to an enforce ancestor"
        );
        assert_eq!(effective.allowed_methods, vec!["GET".to_owned()]);
        assert_eq!(effective.expose_headers, vec!["x-request-id".to_owned()]);
        assert!(!effective.allow_credentials);
    }

    #[test]
    fn descendant_cannot_re_enable_an_enforced_ancestor_cors() {
        let mut ancestor_upstream = upstream(SharingMode::Enforce, None, &[]);
        ancestor_upstream.cors = Some(cors(
            SharingMode::Enforce,
            &["https://app.example.com"],
            false,
        ));
        let chain = chain_of(&[&ancestor_upstream]);
        let mut own = cors(SharingMode::Private, &["https://other.example.com"], false);
        own.enabled = false;
        assert!(effective_cors(&chain, Some(&own), None).is_some());
    }

    #[test]
    fn private_ancestor_cors_is_invisible() {
        let mut ancestor_upstream = upstream(SharingMode::Private, None, &[]);
        ancestor_upstream.cors = Some(cors(
            SharingMode::Private,
            &["https://app.example.com"],
            false,
        ));
        let chain = chain_of(&[&ancestor_upstream]);
        assert!(effective_cors(&chain, None, None).is_none());
    }

    #[test]
    fn ancestor_header_mandate_is_returned_when_the_descendant_has_none() {
        let mut ancestor_upstream = upstream(SharingMode::Enforce, None, &[]);
        ancestor_upstream.headers = Some(HeadersConfig {
            request: Some(RequestHeaderRules {
                set: BTreeMap::from([("x-mandated".to_owned(), "1".to_owned())]),
                ..RequestHeaderRules::default()
            }),
            response: None,
        });
        let chain = chain_of(&[&ancestor_upstream]);
        let mandate = effective_headers(&chain, None).expect("ancestor mandate");
        assert!(
            mandate
                .request
                .as_ref()
                .unwrap()
                .set
                .contains_key("x-mandated")
        );
    }

    #[test]
    fn ancestor_and_descendant_header_rules_are_composed() {
        let mut ancestor_upstream = upstream(SharingMode::Enforce, None, &[]);
        // The ancestor mandates headers even without any `enforce` auth block.
        ancestor_upstream.headers = Some(HeadersConfig {
            request: Some(RequestHeaderRules {
                set: BTreeMap::from([("x-ancestor".to_owned(), "1".to_owned())]),
                add: BTreeMap::from([("x-both-add".to_owned(), "a".to_owned())]),
                remove: vec!["x-dropped".to_owned()],
                ..RequestHeaderRules::default()
            }),
            response: Some(ResponseHeaderRules {
                set: BTreeMap::from([("x-resp-ancestor".to_owned(), "1".to_owned())]),
                ..ResponseHeaderRules::default()
            }),
        });
        let chain = chain_of(&[&ancestor_upstream]);
        let own = HeadersConfig {
            request: Some(RequestHeaderRules {
                set: BTreeMap::from([("x-own".to_owned(), "1".to_owned())]),
                add: BTreeMap::from([("x-both-add".to_owned(), "b".to_owned())]),
                ..RequestHeaderRules::default()
            }),
            response: Some(ResponseHeaderRules {
                add: BTreeMap::from([("x-resp-own".to_owned(), "1".to_owned())]),
                ..ResponseHeaderRules::default()
            }),
        };
        let effective = effective_headers(&chain, Some(&own)).expect("composed config");
        let request = effective.request.as_ref().expect("request rules");
        assert!(
            request.set.contains_key("x-ancestor") && request.set.contains_key("x-own"),
            "the ancestor mandate and the descendant's rules both apply"
        );
        // `add` is a multiset: both values survive, ancestor's first.
        assert_eq!(
            request.add.get("x-both-add").map(String::as_str),
            Some("b"),
            "the descendant's `add` value is applied after the ancestor's"
        );
        assert_eq!(request.remove, vec!["x-dropped".to_owned()]);
        let response = effective.response.as_ref().expect("response rules");
        assert!(response.set.contains_key("x-resp-ancestor"));
        assert!(response.add.contains_key("x-resp-own"));
    }

    #[test]
    fn the_stricter_passthrough_policy_governs_the_composition() {
        let mandate_with = |passthrough| HeadersConfig {
            request: Some(RequestHeaderRules {
                passthrough,
                passthrough_allowlist: vec!["accept".to_owned()],
                ..RequestHeaderRules::default()
            }),
            response: None,
        };
        let own_with = |passthrough| HeadersConfig {
            request: Some(RequestHeaderRules {
                passthrough,
                passthrough_allowlist: vec!["x-custom".to_owned()],
                ..RequestHeaderRules::default()
            }),
            response: None,
        };
        let composed = |mandate: HeadersConfig, own: HeadersConfig| {
            let ancestor = upstream_with_headers(mandate);
            let chain = chain_of(&[&ancestor]);
            effective_headers(&chain, Some(&own)).expect("composed config")
        };

        // A descendant cannot widen an ancestor mandate.
        let effective = composed(
            mandate_with(PassthroughMode::None),
            own_with(PassthroughMode::All),
        );
        assert_eq!(
            effective.request.as_ref().unwrap().passthrough,
            PassthroughMode::None
        );

        // Neither side declaring a policy keeps the safe default.
        let effective = composed(
            mandate_with(PassthroughMode::None),
            own_with(PassthroughMode::None),
        );
        assert_eq!(
            effective.request.as_ref().unwrap().passthrough,
            PassthroughMode::None
        );

        // The allowlists are unioned when both sides allowlist.
        let effective = composed(
            mandate_with(PassthroughMode::Allowlist),
            own_with(PassthroughMode::Allowlist),
        );
        let request = effective.request.as_ref().unwrap();
        assert_eq!(request.passthrough, PassthroughMode::Allowlist);
        assert_eq!(
            request.passthrough_allowlist,
            vec!["accept".to_owned(), "x-custom".to_owned()]
        );

        // A single allowlisting side supplies the allowlist.
        let effective = composed(
            mandate_with(PassthroughMode::Allowlist),
            own_with(PassthroughMode::All),
        );
        let request = effective.request.as_ref().unwrap();
        assert_eq!(request.passthrough, PassthroughMode::Allowlist);
        assert_eq!(request.passthrough_allowlist, vec!["accept".to_owned()]);
    }

    #[test]
    fn an_ancestor_headers_config_needs_no_auth_sharing_mode() {
        let mut ancestor_upstream = upstream(SharingMode::Private, None, &[]);
        ancestor_upstream.auth = None;
        ancestor_upstream.headers = Some(HeadersConfig {
            request: Some(RequestHeaderRules {
                remove: vec!["x-internal".to_owned()],
                ..RequestHeaderRules::default()
            }),
            response: None,
        });
        let chain = chain_of(&[&ancestor_upstream]);
        assert!(
            effective_headers(&chain, None).is_some(),
            "header mandates are independent of the auth sharing mode"
        );
    }
}

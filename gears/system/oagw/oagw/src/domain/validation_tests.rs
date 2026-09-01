#![allow(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(coverage_nightly, coverage(off))]

use std::sync::Arc;

use super::{
    CORS_METHODS, MAX_ENDPOINTS, MAX_PLUGIN_SOURCE_BYTES, PayloadRules, all_hostnames,
    ensure_match_rule_unique, validate_cors, validate_endpoints, validate_hostname,
    validate_http_match, validate_plugin_chain, validate_plugin_ref, validate_plugin_source,
    validate_rate_limit, validate_route, validate_tags, validate_upstream,
    validate_upstream_with_posture,
};
use crate::domain::error::DomainError;
use crate::domain::model::{
    AUTH_PLUGIN_TYPE, AuthConfig, Burst, CorsConfig, Endpoint, EndpointScheme, GUARD_PLUGIN_TYPE,
    GrpcMatch, HttpMatch, HttpMethod, MatchConfig, PathSuffixMode, Plugin, PluginRef, PluginType,
    Protocol, RateLimitConfig, Route, ServerConfig, SharingMode, SustainedRate, Upstream,
};
use crate::domain::plugin::ChainKind;
use crate::domain::repo::{PluginRepository, RouteRepository};
use crate::infra::storage::{
    MemoryPluginRepository, MemoryRouteRepository, MemoryStore, MemoryUpstreamRepository,
};

const ROW: &str = "3f2c1b2a-1b1c-2d3e-4f50-61728394a5b6";

fn endpoint(host: &str, port: u16) -> Endpoint {
    Endpoint {
        scheme: EndpointScheme::Https,
        host: host.to_owned(),
        port,
    }
}

fn endpoints(hosts: &[(&str, u16)]) -> Vec<Endpoint> {
    hosts
        .iter()
        .map(|(host, port)| endpoint(host, *port))
        .collect()
}

/// A cleartext endpoint pool: what an `allow_http_upstream` deployment
/// registers.
fn cleartext_endpoints(hosts: &[(&str, u16)]) -> Vec<Endpoint> {
    hosts
        .iter()
        .map(|(host, port)| Endpoint {
            scheme: EndpointScheme::Http,
            host: (*host).to_owned(),
            port: *port,
        })
        .collect()
}

fn http_match(path: &str, methods: &[HttpMethod]) -> MatchConfig {
    MatchConfig {
        http: Some(HttpMatch {
            methods: methods.to_vec(),
            path: path.to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: None,
    }
}

fn upstream(tenant: uuid::Uuid, alias: &str) -> Upstream {
    Upstream {
        id: uuid::Uuid::new_v4(),
        tenant_id: tenant,
        alias: alias.to_owned(),
        protocol: Protocol::Http,
        enabled: true,
        server: ServerConfig {
            endpoints: endpoints(&[("api.openai.com", 443)]),
        },
        auth: None,
        headers: None,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        updated_at: "2026-01-01T00:00:00Z".to_owned(),
    }
}

fn plugin_row(tenant: uuid::Uuid, kind: PluginType) -> Plugin {
    Plugin {
        id: ROW.parse().unwrap(),
        tenant_id: tenant,
        plugin_type: kind,
        name: "guarded".to_owned(),
        config_schema: None,
        source_code: "def apply(ctx):\n    return ctx\n".to_owned(),
        phases: Vec::new(),
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        updated_at: "2026-01-01T00:00:00Z".to_owned(),
        last_used_at: None,
        gc_eligible_at: None,
    }
}

/// A plugin repository with exactly one custom guard plugin of `tenant`.
async fn seeded_plugins(tenant: uuid::Uuid, kind: PluginType) -> Arc<MemoryPluginRepository> {
    let store = Arc::new(MemoryStore::new());
    let repo = Arc::new(MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store)))
            as Arc<dyn crate::domain::repo::UpstreamRepository>,
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store)))
            as Arc<dyn crate::domain::repo::RouteRepository>,
    ));
    repo.insert(&plugin_row(tenant, kind)).await.unwrap();
    repo
}

// ── hostname validation ────────────────────────────────────────────────────

#[test]
fn hostname_normalizes_case_and_the_trailing_root_dot() {
    assert_eq!(
        validate_hostname("API.OPENAI.COM.").unwrap(),
        "api.openai.com"
    );
    assert_eq!(
        validate_hostname("  api.openai.com  ").unwrap(),
        "api.openai.com"
    );
}

#[test]
fn hostname_accepts_ip_literals() {
    assert_eq!(validate_hostname("10.0.0.1").unwrap(), "10.0.0.1");
    assert_eq!(validate_hostname("2001:db8::1").unwrap(), "2001:db8::1");
}

#[test]
fn hostname_rejects_malformed_hosts() {
    let long_label = "a".repeat(64);
    for host in [
        "",
        "   ",
        ".",
        "api..openai.com",
        "-api.openai.com",
        "api.openai.com-",
        "api openai com",
        "api/openai.com",
        "api:8080",
        long_label.as_str(),
        "api.-openai.com",
    ] {
        assert!(validate_hostname(host).is_err(), "{host}");
    }
}

#[test]
fn hostname_accepts_the_maximum_label_and_host_lengths() {
    let label = "a".repeat(63);
    assert_eq!(validate_hostname(&label).unwrap(), label);
    let two_labels = format!("{label}.{}", "b".repeat(63));
    assert_eq!(validate_hostname(&two_labels).unwrap(), two_labels);
    let too_long = format!("{two_labels}.{}", "c".repeat(64));
    assert!(validate_hostname(&too_long).is_err());
}

// ── endpoint pool ──────────────────────────────────────────────────────────

#[test]
fn endpoints_must_be_non_empty_and_homogeneous() {
    assert!(validate_endpoints(&[]).is_err());
    assert!(validate_endpoints(&endpoints(&[("api.openai.com", 443)])).is_ok());
    assert!(
        validate_endpoints(&endpoints(&[
            ("api.openai.com", 443),
            ("api.openai.com", 8443)
        ]))
        .is_err()
    );
    assert!(validate_endpoints(&endpoints(&[("api.openai.com", 443), ("1.2.3.4", 443)])).is_ok());
}

#[test]
fn endpoints_are_capped() {
    let pool: Vec<Endpoint> = (0..=MAX_ENDPOINTS)
        .map(|index| endpoint(&format!("h{index}.openai.com"), 443))
        .collect();
    assert!(validate_endpoints(&pool).is_err());
    assert!(validate_endpoints(&pool[..MAX_ENDPOINTS]).is_ok());
}

// ── plugin chains ──────────────────────────────────────────────────────────

fn chain(items: &[&str]) -> crate::domain::model::PluginsConfig {
    crate::domain::model::PluginsConfig {
        sharing: SharingMode::Private,
        items: items
            .iter()
            .map(|item| PluginRef::Id((*item).to_owned()))
            .collect(),
    }
}

const REQUIRED_HEADERS: &str = "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.required_headers.v1";
const REQUEST_ID: &str = "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.request_id.v1";
const NOOP_AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.noop.v1";
const APIKEY_AUTH: &str = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";

/// `F1` — both chains bind the guard and the transform built-ins.
#[tokio::test]
async fn plugin_chain_accepts_the_bindable_builtins_of_both_families() {
    let store = Arc::new(MemoryStore::new());
    let repo = MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
    );
    for kind in [ChainKind::Upstream, ChainKind::Route] {
        assert!(
            validate_plugin_chain(
                kind,
                &chain(&[REQUIRED_HEADERS]),
                &repo,
                uuid::Uuid::new_v4()
            )
            .await
            .is_ok()
        );
        assert!(
            validate_plugin_chain(kind, &chain(&[REQUEST_ID]), &repo, uuid::Uuid::new_v4())
                .await
                .is_ok()
        );
    }
}

/// `F1` — an auth id is never a chain entry: `auth` is an upstream field.
#[tokio::test]
async fn plugin_chain_rejects_an_auth_plugin_id() {
    let store = Arc::new(MemoryStore::new());
    let repo = MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
    );
    for kind in [ChainKind::Upstream, ChainKind::Route] {
        let error = validate_plugin_chain(kind, &chain(&[NOOP_AUTH]), &repo, uuid::Uuid::new_v4())
            .await
            .unwrap_err();
        assert!(matches!(error, DomainError::Validation { .. }), "{error:?}");
        let detail = error.to_string();
        assert!(
            detail.contains(&format!("unknown {kind} plugin")),
            "{detail}"
        );
        assert!(detail.contains(GUARD_PLUGIN_TYPE), "{detail}");
    }
}

/// `F5` — the catalog-only identifiers exist for the types registry and may not
/// be bound.
#[tokio::test]
async fn plugin_chain_rejects_the_catalog_only_builtins() {
    let store = Arc::new(MemoryStore::new());
    let repo = MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
    );
    for reference in [
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.timeout.v1",
        "gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1",
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.logging.v1",
        "gts.cf.core.oagw.transform_plugin.v1~cf.core.oagw.metrics.v1",
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
    ] {
        for kind in [ChainKind::Upstream, ChainKind::Route] {
            let error =
                validate_plugin_chain(kind, &chain(&[reference]), &repo, uuid::Uuid::new_v4())
                    .await
                    .unwrap_err();
            assert!(
                matches!(error, DomainError::Validation { .. }),
                "{reference}"
            );
        }
    }
}

/// `F6` — the ADR-0009 object form parses and validates next to the string form.
#[tokio::test]
async fn plugin_chain_accepts_the_object_binding_form() {
    let store = Arc::new(MemoryStore::new());
    let repo = MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
    );
    let chain = crate::domain::model::PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![PluginRef::Binding {
            plugin_ref: REQUIRED_HEADERS.to_owned(),
            config: Some(serde_json::json!({
                "required_request_headers": "x-correlation-id,accept",
                "required_response_headers": "content-type"
            })),
        }],
    };
    assert!(
        validate_plugin_chain(ChainKind::Upstream, &chain, &repo, uuid::Uuid::new_v4())
            .await
            .is_ok()
    );
}

/// `F6` — a mixed chain keeps both spellings.
#[tokio::test]
async fn plugin_chain_accepts_a_mixed_array_of_both_forms() {
    let store = Arc::new(MemoryStore::new());
    let repo = MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
    );
    let chain = crate::domain::model::PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![
            PluginRef::Id(REQUEST_ID.to_owned()),
            PluginRef::Binding {
                plugin_ref: REQUIRED_HEADERS.to_owned(),
                config: None,
            },
        ],
    };
    assert!(
        validate_plugin_chain(ChainKind::Route, &chain, &repo, uuid::Uuid::new_v4())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn plugin_chain_rejects_an_unresolvable_custom_row() {
    let tenant = uuid::Uuid::new_v4();
    let repo = seeded_plugins(tenant, PluginType::Guard).await;
    let reference = format!("{AUTH_PLUGIN_TYPE}{ROW}");
    let error = validate_plugin_ref(ChainKind::Upstream, &reference, repo.as_ref(), tenant)
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
}

#[tokio::test]
async fn plugin_chain_accepts_a_custom_row_of_the_tenant() {
    let tenant = uuid::Uuid::new_v4();
    let repo = seeded_plugins(tenant, PluginType::Guard).await;
    let reference = format!("{GUARD_PLUGIN_TYPE}{ROW}");
    assert!(
        validate_plugin_ref(ChainKind::Route, &reference, repo.as_ref(), tenant)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn plugin_chain_rejects_a_row_of_another_tenant() {
    let owner = uuid::Uuid::new_v4();
    let other = uuid::Uuid::new_v4();
    let repo = seeded_plugins(owner, PluginType::Guard).await;
    let reference = format!("{GUARD_PLUGIN_TYPE}{ROW}");
    let error = validate_plugin_ref(ChainKind::Route, &reference, repo.as_ref(), other)
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
}

/// `F7` — a route chain names the base type; only an upstream chain accepts a
/// bare UUID tail.
#[tokio::test]
async fn a_route_chain_rejects_a_bare_uuid_reference() {
    let tenant = uuid::Uuid::new_v4();
    let repo = seeded_plugins(tenant, PluginType::Guard).await;
    let error = validate_plugin_ref(ChainKind::Route, ROW, repo.as_ref(), tenant)
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
    assert!(error.to_string().contains("guard_plugin.v1~ or"));
    // The same tail is accepted on the upstream chain, whose schema allows it.
    assert!(
        validate_plugin_ref(ChainKind::Upstream, ROW, repo.as_ref(), tenant)
            .await
            .is_ok()
    );
}

// ── upstream payload ───────────────────────────────────────────────────────

#[tokio::test]
async fn upstream_validation_accepts_a_well_formed_payload() {
    let tenant = uuid::Uuid::new_v4();
    let repo = seeded_plugins(tenant, PluginType::Guard).await;
    let binding = crate::domain::model::PluginsConfig {
        sharing: SharingMode::Private,
        items: vec![PluginRef::Id(format!("{GUARD_PLUGIN_TYPE}{ROW}"))],
    };
    assert!(
        validate_upstream(
            &endpoints(&[("api.openai.com", 443)]),
            Protocol::Http,
            None,
            PayloadRules {
                plugins: Some(&binding),
                rate_limit: None,
                cors: None,
                tags: &["primary".to_owned()],
            },
            repo.as_ref(),
            tenant,
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn upstream_validation_rejects_a_protocol_the_pool_does_not_speak() {
    let store = Arc::new(MemoryStore::new());
    let repo = MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
    );
    let pool = endpoints(&[("api.openai.com", 443)]);
    assert!(
        validate_upstream(
            &pool,
            Protocol::Grpc,
            None,
            PayloadRules {
                plugins: None,
                rate_limit: None,
                cors: None,
                tags: &[],
            },
            &repo,
            uuid::Uuid::new_v4(),
        )
        .await
        .is_err()
    );
}

/// The cleartext posture is a deployment opt-in, not a default: the same
/// payload is refused until the operator sets `allow_http_upstream`.
#[tokio::test]
async fn upstream_validation_admits_cleartext_only_when_the_deployment_opts_in() {
    let store = Arc::new(MemoryStore::new());
    let repo = MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
    );
    let pool = cleartext_endpoints(&[("api.openai.com", 8080)]);
    let tags: Vec<String> = Vec::new();
    let rules = PayloadRules {
        plugins: None,
        rate_limit: None,
        cors: None,
        tags: &tags,
    };

    let refused = validate_upstream_with_posture(
        &pool,
        Protocol::Http,
        None,
        rules,
        &repo,
        uuid::Uuid::new_v4(),
        false,
    )
    .await;
    assert!(refused.is_err(), "HTTPS-only is the default posture");

    let admitted = validate_upstream_with_posture(
        &pool,
        Protocol::Http,
        None,
        PayloadRules {
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: &tags,
        },
        &repo,
        uuid::Uuid::new_v4(),
        true,
    )
    .await;
    assert!(admitted.is_ok(), "{admitted:?}");
}

#[tokio::test]
async fn upstream_validation_rejects_malformed_tags() {
    let store = Arc::new(MemoryStore::new());
    let repo = MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
    );
    let pool = endpoints(&[("api.openai.com", 443)]);
    let error = validate_upstream(
        &pool,
        Protocol::Http,
        None,
        PayloadRules {
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: &["Not A Tag".to_owned()],
        },
        &repo,
        uuid::Uuid::new_v4(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
}

// ── auth.type (F2) ─────────────────────────────────────────────────────────

fn auth(reference: &str) -> AuthConfig {
    AuthConfig {
        plugin_type: Some(reference.to_owned()),
        sharing: SharingMode::Private,
        config: None,
    }
}

#[tokio::test]
async fn an_absent_auth_type_is_forwarded_as_is() {
    let store = Arc::new(MemoryStore::new());
    let repo = MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
    );
    // An upstream whose `auth` carries no `type` at all is forwarded as-is.
    let untyped = AuthConfig {
        plugin_type: None,
        sharing: SharingMode::Private,
        config: None,
    };
    assert!(
        super::validate_auth(&untyped, &repo, uuid::Uuid::new_v4())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn the_named_builtins_resolve_and_the_reserved_ones_do_not() {
    let store = Arc::new(MemoryStore::new());
    let repo = MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
    );
    let tenant = uuid::Uuid::new_v4();

    for reference in [
        NOOP_AUTH,
        APIKEY_AUTH,
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred.v1",
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.oauth2_client_cred_basic.v1",
    ] {
        assert!(
            super::validate_auth(&auth(reference), &repo, tenant)
                .await
                .is_ok(),
            "{reference}"
        );
    }
    // `basic.v1` and `bearer.v1` are cataloged with no implementation.
    for reference in [
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.basic.v1",
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1",
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.no_such_auth.v1",
    ] {
        let error = super::validate_auth(&auth(reference), &repo, tenant)
            .await
            .unwrap_err();
        assert!(
            matches!(error, DomainError::Validation { .. }),
            "{reference}"
        );
        assert!(error.to_string().contains("unknown auth plugin"), "{error}");
    }
}

#[tokio::test]
async fn auth_type_must_carry_the_auth_plugin_base_type() {
    let store = Arc::new(MemoryStore::new());
    let repo = MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
    );
    let tenant = uuid::Uuid::new_v4();
    // A bare UUID is not a plugin identity.
    let error = super::validate_auth(&auth(ROW), &repo, tenant)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown auth plugin"), "{error}");
    // Nor is a guard id.
    let error = super::validate_auth(&auth(REQUIRED_HEADERS), &repo, tenant)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown auth plugin"), "{error}");
}

#[tokio::test]
async fn a_custom_auth_plugin_must_resolve_to_an_auth_row() {
    let tenant = uuid::Uuid::new_v4();
    // A dangling UUID tail names no row of the tenant.
    let store = Arc::new(MemoryStore::new());
    let empty = MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
    );
    let reference = format!("{AUTH_PLUGIN_TYPE}{ROW}");
    let error = super::validate_auth(&auth(&reference), &empty, tenant)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown auth plugin"), "{error}");

    // A resolvable row of the wrong kind is refused as well.
    let guards = seeded_plugins(tenant, PluginType::Guard).await;
    let error = super::validate_auth(&auth(&reference), guards.as_ref(), tenant)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unknown auth plugin"), "{error}");

    // A resolvable auth row resolves.
    let auths = seeded_plugins(tenant, PluginType::Auth).await;
    assert!(
        super::validate_auth(&auth(&reference), auths.as_ref(), tenant)
            .await
            .is_ok()
    );
}

// ── cors and rate limit (F3) ───────────────────────────────────────────────

fn cors_config(enabled: bool) -> CorsConfig {
    CorsConfig {
        sharing: SharingMode::Private,
        enabled,
        allowed_origins: Vec::new(),
        allowed_methods: Vec::new(),
        expose_headers: Vec::new(),
        allow_credentials: false,
    }
}

#[test]
fn cors_lists_require_cors_to_be_enabled() {
    let mut config = cors_config(false);
    config.allowed_origins = vec!["https://studio.example".to_owned()];
    let error = validate_cors(&config).unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }));
    assert!(
        error.to_string().contains("require cors.enabled"),
        "{error}"
    );

    // The same configuration with CORS enabled is accepted.
    config.enabled = true;
    assert!(validate_cors(&config).is_ok());
    // A disabled CORS with no lists at all is accepted.
    assert!(validate_cors(&cors_config(false)).is_ok());
}

#[test]
fn cors_credentials_require_specific_origins() {
    let wildcard = CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: vec!["*".to_owned()],
        allowed_methods: vec!["GET".to_owned()],
        expose_headers: Vec::new(),
        allow_credentials: true,
    };
    let error = validate_cors(&wildcard).unwrap_err();
    assert!(
        error.to_string().contains("must not contain '*'"),
        "{error}"
    );

    let empty = CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: Vec::new(),
        allowed_methods: vec!["GET".to_owned()],
        expose_headers: Vec::new(),
        allow_credentials: true,
    };
    let error = validate_cors(&empty).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("requires at least one allowed origin"),
        "{error}"
    );
}

#[test]
fn cors_origins_must_be_the_wildcard_or_a_uri() {
    let mut config = cors_config(true);
    config.allowed_origins = vec!["https://studio.example".to_owned(), "*".to_owned()];
    assert!(validate_cors(&config).is_ok());

    config.allowed_origins = vec!["studio.example".to_owned()];
    let error = validate_cors(&config).unwrap_err();
    assert!(error.to_string().contains("'*' or a URI"), "{error}");
}

#[test]
fn cors_methods_come_from_the_documented_enum() {
    for method in CORS_METHODS {
        let mut config = cors_config(true);
        config.allowed_methods = vec![(*method).to_owned()];
        assert!(validate_cors(&config).is_ok(), "{method}");
    }
    let mut config = cors_config(true);
    config.allowed_methods = vec!["TRACE".to_owned()];
    let error = validate_cors(&config).unwrap_err();
    assert!(error.to_string().contains("must be one of"), "{error}");
}

/// The `sharing` discriminator is a serde enum, so the wire domain of
/// `{private, inherit, enforce}` is already enforced at the boundary.
#[test]
fn cors_sharing_is_constrained_by_the_deserializer() {
    let value: CorsConfig = serde_json::from_value(serde_json::json!({
        "sharing": "enforce",
        "enabled": false
    }))
    .unwrap();
    assert_eq!(value.sharing, SharingMode::Enforce);
    assert!(
        serde_json::from_value::<CorsConfig>(serde_json::json!({
            "sharing": "public",
            "enabled": false
        }))
        .is_err()
    );
}

fn rate_limit(rate: u64, burst: Option<u64>) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: crate::domain::model::RateAlgorithm::default(),
        sustained: SustainedRate {
            rate,
            window: crate::domain::model::RateWindow::default(),
        },
        burst: burst.map(|capacity| Burst { capacity }),
        scope: crate::domain::model::RateScope::default(),
        strategy: crate::domain::model::RateStrategy::default(),
        cost: 1,
    }
}

#[test]
fn rate_limit_requires_a_positive_sustained_rate() {
    assert!(validate_rate_limit(&rate_limit(1, None)).is_ok());
    assert!(validate_rate_limit(&rate_limit(0, None)).is_err());
}

#[test]
fn burst_capacity_must_not_be_smaller_than_the_sustained_rate() {
    assert!(validate_rate_limit(&rate_limit(10, Some(10))).is_ok());
    let error = validate_rate_limit(&rate_limit(10, Some(5))).unwrap_err();
    assert!(error.to_string().contains("must be at least"), "{error}");
}

/// `F3` — the upstream payload carries both `cors` and `rate_limit`, so both are
/// validated as part of the upstream aggregate.
#[tokio::test]
async fn upstream_validation_rejects_an_invalid_cors_configuration() {
    let store = Arc::new(MemoryStore::new());
    let repo = MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
    );
    let pool = endpoints(&[("api.openai.com", 443)]);
    let invalid = CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: vec!["*".to_owned()],
        allowed_methods: vec!["GET".to_owned()],
        expose_headers: Vec::new(),
        allow_credentials: true,
    };
    let error = validate_upstream(
        &pool,
        Protocol::Http,
        None,
        PayloadRules {
            plugins: None,
            rate_limit: None,
            cors: Some(&invalid),
            tags: &[],
        },
        &repo,
        uuid::Uuid::new_v4(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }), "{error:?}");

    let valid = CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: vec!["https://studio.example".to_owned()],
        allowed_methods: vec!["GET".to_owned(), "POST".to_owned()],
        expose_headers: vec!["x-request-id".to_owned()],
        allow_credentials: true,
    };
    assert!(
        validate_upstream(
            &pool,
            Protocol::Http,
            None,
            PayloadRules {
                plugins: None,
                rate_limit: Some(&rate_limit(5, Some(20))),
                cors: Some(&valid),
                tags: &[],
            },
            &repo,
            uuid::Uuid::new_v4(),
        )
        .await
        .is_ok()
    );
}

/// `F8` — a route carries its own `cors` override, validated like the upstream's.
#[tokio::test]
async fn route_cors_is_validated_like_the_upstream_cors() {
    let store = Arc::new(MemoryStore::new());
    let repo = MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
    );
    let tenant = uuid::Uuid::new_v4();

    let valid = CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: vec!["https://studio.example".to_owned()],
        allowed_methods: vec!["GET".to_owned()],
        expose_headers: Vec::new(),
        allow_credentials: false,
    };
    assert!(
        validate_route(
            &upstream(tenant, "x"),
            &http_match("/v1", &[HttpMethod::Get]),
            1,
            PayloadRules {
                plugins: None,
                rate_limit: None,
                cors: Some(&valid),
                tags: &[],
            },
            &repo,
            tenant,
        )
        .await
        .is_ok()
    );

    let invalid = CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: vec!["*".to_owned()],
        allowed_methods: vec!["GET".to_owned()],
        expose_headers: Vec::new(),
        allow_credentials: true,
    };
    let error = validate_route(
        &upstream(tenant, "x"),
        &http_match("/v1", &[HttpMethod::Get]),
        1,
        PayloadRules {
            plugins: None,
            rate_limit: None,
            cors: Some(&invalid),
            tags: &[],
        },
        &repo,
        tenant,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }), "{error:?}");

    // A route rate limit is validated too.
    let error = validate_route(
        &upstream(tenant, "x"),
        &http_match("/v1", &[HttpMethod::Get]),
        1,
        PayloadRules {
            plugins: None,
            rate_limit: Some(&rate_limit(0, None)),
            cors: None,
            tags: &[],
        },
        &repo,
        tenant,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DomainError::Validation { .. }), "{error:?}");
}

// ── tags and plugin source ─────────────────────────────────────────────────

#[test]
fn tags_must_match_the_documented_shape() {
    assert!(validate_tags(&["primary".to_owned(), "tier-1".to_owned()]).is_ok());
    assert!(validate_tags(&["Tier 1".to_owned()]).is_err());
    assert!(validate_tags(&[String::new()]).is_err());
    assert!(validate_tags(&["a".repeat(65)]).is_err());
    let too_many: Vec<String> = (0..33).map(|index| format!("t{index}")).collect();
    assert!(validate_tags(&too_many).is_err());
}

#[test]
fn plugin_source_is_bounded_and_non_empty() {
    assert!(validate_plugin_source("def apply(ctx):\n    return ctx\n").is_ok());
    assert!(validate_plugin_source("   \n").is_err());
    let oversized = "x".repeat(MAX_PLUGIN_SOURCE_BYTES + 1);
    assert!(validate_plugin_source(&oversized).is_err());
    assert!(validate_plugin_source(&"x".repeat(MAX_PLUGIN_SOURCE_BYTES)).is_ok());
}

// ── match rules ────────────────────────────────────────────────────────────

#[test]
fn http_match_requires_methods_and_a_leading_slash() {
    let valid = HttpMatch {
        methods: vec![HttpMethod::Get, HttpMethod::Post],
        path: "/v1/models".to_owned(),
        query_allowlist: Vec::new(),
        path_suffix_mode: PathSuffixMode::Append,
    };
    assert!(validate_http_match(&valid).is_ok());

    let empty_methods = HttpMatch {
        methods: Vec::new(),
        ..valid.clone()
    };
    assert!(validate_http_match(&empty_methods).is_err());

    let relative_path = HttpMatch {
        path: "v1/models".to_owned(),
        ..valid.clone()
    };
    assert!(validate_http_match(&relative_path).is_err());

    let blank_path = HttpMatch {
        path: "   ".to_owned(),
        ..valid.clone()
    };
    let _ = &valid;
    assert!(validate_http_match(&blank_path).is_err());

    let duplicate_methods = HttpMatch {
        methods: vec![HttpMethod::Get, HttpMethod::Get],
        ..valid.clone()
    };
    assert!(validate_http_match(&duplicate_methods).is_err());

    let blank_query_name = HttpMatch {
        query_allowlist: vec!["  ".to_owned()],
        ..valid
    };
    assert!(validate_http_match(&blank_query_name).is_err());
}

#[test]
fn grpc_match_requires_a_service_and_a_method() {
    let valid = GrpcMatch {
        service: "example.v1.Echo".to_owned(),
        method: "Echo".to_owned(),
    };
    assert!(super::validate_grpc_match(&valid).is_ok());
    assert!(
        super::validate_grpc_match(&GrpcMatch {
            service: " ".to_owned(),
            method: "Echo".to_owned(),
        })
        .is_err()
    );
    assert!(
        super::validate_grpc_match(&GrpcMatch {
            service: "svc".to_owned(),
            method: String::new(),
        })
        .is_err()
    );
}

/// `F15` — the match-rule shape has one definition, reached through
/// `validate_route`.
#[tokio::test]
async fn match_config_must_carry_exactly_one_protocol_block() {
    let store = Arc::new(MemoryStore::new());
    let repo = MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
    );
    let tenant = uuid::Uuid::new_v4();

    assert!(
        validate_route(
            &upstream(tenant, "x"),
            &http_match("/v1", &[HttpMethod::Get]),
            1,
            PayloadRules {
                plugins: None,
                rate_limit: None,
                cors: None,
                tags: &[],
            },
            &repo,
            tenant,
        )
        .await
        .is_ok()
    );

    let empty = MatchConfig {
        http: None,
        grpc: None,
    };
    assert!(
        validate_route(
            &upstream(tenant, "x"),
            &empty,
            1,
            PayloadRules {
                plugins: None,
                rate_limit: None,
                cors: None,
                tags: &[],
            },
            &repo,
            tenant,
        )
        .await
        .is_err()
    );

    let both = MatchConfig {
        http: Some(HttpMatch {
            methods: vec![HttpMethod::Get],
            path: "/v1".to_owned(),
            query_allowlist: Vec::new(),
            path_suffix_mode: PathSuffixMode::Append,
        }),
        grpc: Some(GrpcMatch {
            service: "svc".to_owned(),
            method: "m".to_owned(),
        }),
    };
    assert!(
        validate_route(
            &upstream(tenant, "x"),
            &both,
            1,
            PayloadRules {
                plugins: None,
                rate_limit: None,
                cors: None,
                tags: &[],
            },
            &repo,
            tenant,
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn route_validation_enforces_the_upstream_protocol() {
    let tenant = uuid::Uuid::new_v4();
    let store = Arc::new(MemoryStore::new());
    let repo = MemoryPluginRepository::new(
        Arc::clone(&store),
        Arc::new(MemoryUpstreamRepository::new(Arc::clone(&store))),
        Arc::new(MemoryRouteRepository::new(Arc::clone(&store))),
    );
    let mut grpc_upstream = upstream(tenant, "grpc.svc");
    grpc_upstream.protocol = Protocol::Grpc;
    grpc_upstream.server = ServerConfig {
        endpoints: vec![Endpoint {
            scheme: EndpointScheme::Grpc,
            host: "grpc.svc".to_owned(),
            port: 443,
        }],
    };

    let http_rules = http_match("/v1", &[HttpMethod::Get]);
    assert!(
        validate_route(
            &grpc_upstream,
            &http_rules,
            1,
            PayloadRules {
                plugins: None,
                rate_limit: None,
                cors: None,
                tags: &[],
            },
            &repo,
            tenant,
        )
        .await
        .is_err()
    );
    assert!(
        validate_route(
            &upstream(tenant, "x"),
            &http_rules,
            1,
            PayloadRules {
                plugins: None,
                rate_limit: None,
                cors: None,
                tags: &[],
            },
            &repo,
            tenant,
        )
        .await
        .is_ok()
    );
}

// ── match-rule uniqueness ──────────────────────────────────────────────────

fn route(tenant: uuid::Uuid, upstream: uuid::Uuid, path: &str, priority: u32) -> Route {
    Route {
        id: uuid::Uuid::new_v4(),
        tenant_id: tenant,
        upstream_id: upstream,
        r#match: http_match(path, &[HttpMethod::Get]),
        priority,
        enabled: true,
        rate_limit: None,
        cors: None,
        plugins: None,
        tags: Vec::new(),
        created_at: "2026-01-01T00:00:00Z".to_owned(),
        updated_at: "2026-01-01T00:00:00Z".to_owned(),
    }
}

#[tokio::test]
async fn match_rule_duplication_conflicts() {
    let store = Arc::new(MemoryStore::new());
    let routes = MemoryRouteRepository::new(store);
    let tenant = uuid::Uuid::new_v4();
    let upstream = uuid::Uuid::new_v4();
    routes
        .insert(&route(tenant, upstream, "/v1/models", 10))
        .await
        .unwrap();

    let candidate = http_match("/v1/models", &[HttpMethod::Get]);
    let error = ensure_match_rule_unique(&routes, tenant, upstream, &candidate, 10, None)
        .await
        .unwrap_err();
    assert!(matches!(error, DomainError::Conflict { .. }));

    // A different priority or a different path is free.
    assert!(
        ensure_match_rule_unique(&routes, tenant, upstream, &candidate, 11, None)
            .await
            .is_ok()
    );
    let other_path = http_match("/v1/chat", &[HttpMethod::Get]);
    assert!(
        ensure_match_rule_unique(&routes, tenant, upstream, &other_path, 10, None)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn match_rule_uniqueness_ignores_the_route_being_replaced() {
    let store = Arc::new(MemoryStore::new());
    let routes = MemoryRouteRepository::new(store);
    let tenant = uuid::Uuid::new_v4();
    let upstream = uuid::Uuid::new_v4();
    let existing = route(tenant, upstream, "/v1/models", 10);
    routes.insert(&existing).await.unwrap();

    let candidate = http_match("/v1/models", &[HttpMethod::Get]);
    assert!(
        ensure_match_rule_unique(&routes, tenant, upstream, &candidate, 10, Some(existing.id))
            .await
            .is_ok()
    );
}

// ── helpers ────────────────────────────────────────────────────────────────

#[test]
fn all_hostnames_distinguishes_hostnames_from_ip_literals() {
    assert!(all_hostnames(&endpoints(&[("api.openai.com", 443)])));
    assert!(!all_hostnames(&endpoints(&[("1.2.3.4", 443)])));
}

//! OAGW provisioning first-pass tests over a recording `ServiceGatewayClientV1` and a
//! scripted `authn_resolver`; no network is used.
//!
//! Slice S2: algorithm `oagw-provisioning`, state machine `provider-provisioning`,
//! definition of done `provider-provisioning`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;

use oagw_sdk::api::ServiceGatewayClientV1;
use oagw_sdk::{
    APIKEY_AUTH_PLUGIN_ID, HTTP_PROTOCOL_ID, HttpMethod, ListQuery, PathSuffixMode, Scheme,
    SharingMode,
};
use secrecy::SecretString;
use serde_json::{Value, json};
use tracing_test::traced_test;
use uuid::Uuid;

use super::{
    EntryOutcome, ProvisionTarget, ProvisioningError, alias_conflicts, chat_route_match,
    exchange_s2s, first_pass, provision_entry, provision_targets, rag_route_matches,
    upstream_request,
};
use crate::config::{ClientCredentials, MiniChatConfig};
use crate::domain::provider_registry::{EffectiveProvider, ProviderRegistry};
use crate::test_support::s2::{
    AuthNScript, RecordingOagw, S2S_SUBJECT, ScriptedAuthN, UpstreamScript, s2s_ctx,
};
use crate::test_support::{TENANT_A, TENANT_B};

fn credentials() -> ClientCredentials {
    ClientCredentials {
        client_id: "mini-chat".to_owned(),
        client_secret: SecretString::from("dev-secret"),
    }
}

fn registry_of(providers: Value) -> ProviderRegistry {
    let mut config = json!({
        "client_credentials": { "client_id": "mini-chat", "client_secret": "dev-secret" }
    });
    if !providers.is_null() {
        config["providers"] = providers;
    }
    let parsed = MiniChatConfig::from_value(config).expect("valid config");
    ProviderRegistry::build(&parsed.providers)
}

fn default_registry() -> ProviderRegistry {
    registry_of(Value::Null)
}

fn provider(registry: &ProviderRegistry, id: &str) -> EffectiveProvider {
    registry.entry(id).expect("entry").base.clone()
}

fn target_of(registry: &ProviderRegistry, id: &str) -> ProvisionTarget {
    ProvisionTarget {
        label: id.to_owned(),
        provider: provider(registry, id),
    }
}

/// `(method, path, suffix mode, query allowlist)` of a route request.
fn route_shape(m: &oagw_sdk::MatchRules) -> (Vec<HttpMethod>, String, PathSuffixMode, Vec<String>) {
    let http = m.http.as_ref().expect("http match");
    assert!(m.grpc.is_none());
    (
        http.methods.clone(),
        http.path.clone(),
        http.path_suffix_mode,
        http.query_allowlist.clone(),
    )
}

// ── upstream request ─────────────────────────────────────────────────────────

#[test]
fn the_upstream_request_carries_scheme_host_port_alias_protocol_and_auth() {
    let registry = default_registry();
    let request = upstream_request(&provider(&registry, "openai"));

    assert_eq!(request.alias(), Some("api.openai.com"));
    assert_eq!(request.protocol(), HTTP_PROTOCOL_ID);
    let endpoints = &request.server().endpoints;
    assert_eq!(endpoints.len(), 1);
    assert_eq!(endpoints[0].scheme, Scheme::Https);
    assert_eq!(endpoints[0].host, "api.openai.com");
    assert_eq!(endpoints[0].port, 443);

    let auth = request.auth().expect("auth plugin configured");
    assert_eq!(auth.plugin_type, APIKEY_AUTH_PLUGIN_ID);
    assert_eq!(auth.sharing, SharingMode::Private);
    let config = auth.config.as_ref().expect("auth config");
    assert_eq!(
        config.get("secret_ref").map(String::as_str),
        Some("cred://openai-key")
    );
    assert_eq!(
        config.get("header").map(String::as_str),
        Some("Authorization")
    );
    assert_eq!(config.get("prefix").map(String::as_str), Some("Bearer "));
}

#[test]
fn use_http_selects_the_http_scheme_and_an_explicit_alias_is_passed() {
    let registry = registry_of(json!({
        "mock": {
            "kind": "openai_responses", "host": "127.0.0.1", "port": 8089, "use_http": true,
            "upstream_alias": "mock-llm", "storage_kind": "openai"
        }
    }));
    let request = upstream_request(&provider(&registry, "mock"));
    assert_eq!(request.alias(), Some("mock-llm"));
    let endpoint = &request.server().endpoints[0];
    assert_eq!(endpoint.scheme, Scheme::Http);
    assert_eq!(endpoint.host, "127.0.0.1");
    assert_eq!(endpoint.port, 8089);
}

#[test]
fn an_entry_without_an_auth_plugin_creates_an_upstream_without_auth() {
    let registry = registry_of(json!({
        "bare": { "kind": "vllm_responses", "host": "llm.internal", "storage_kind": "openai" }
    }));
    let request = upstream_request(&provider(&registry, "bare"));
    assert!(request.auth().is_none());
    assert_eq!(request.alias(), Some("llm.internal"));
}

// ── routes ───────────────────────────────────────────────────────────────────

#[test]
fn the_chat_route_is_a_post_on_the_prefix_derived_from_api_path() {
    // (api_path, route path, query allowlist)
    let cases = vec![
        ("/v1/responses", "/v1/responses", vec![]),
        ("/v1/chat/completions", "/v1/chat/completions", vec![]),
        (
            "/openai/v1/responses?api-version=preview",
            "/openai/v1/responses",
            vec!["api-version"],
        ),
        (
            "/openai/deployments/{model}/chat/completions?api-version=2024-10-21",
            "/openai/deployments",
            vec!["api-version"],
        ),
        (
            "/v1/responses?api-version=1&trace=on",
            "/v1/responses",
            vec!["api-version", "trace"],
        ),
    ];
    for (api_path, path, allowlist) in cases {
        let registry = registry_of(json!({
            "p": { "kind": "openai_responses", "host": "h.example.com", "api_path": api_path, "storage_kind": "openai" }
        }));
        let (methods, route_path, mode, query) =
            route_shape(&chat_route_match(&provider(&registry, "p")));
        assert_eq!(methods, vec![HttpMethod::Post], "{api_path}");
        assert_eq!(route_path, path, "{api_path}");
        assert_eq!(mode, PathSuffixMode::Append, "{api_path}");
        assert_eq!(query, allowlist, "{api_path}");
    }
}

#[test]
fn the_openai_rag_routes_use_the_v1_prefix_without_query_parameters() {
    let registry = default_registry();
    let shapes: Vec<_> = rag_route_matches(&provider(&registry, "openai"))
        .iter()
        .map(route_shape)
        .collect();
    let expected = vec![
        (
            vec![HttpMethod::Post],
            "/v1/files",
            PathSuffixMode::Disabled,
        ),
        (
            vec![HttpMethod::Delete],
            "/v1/files",
            PathSuffixMode::Append,
        ),
        (
            vec![HttpMethod::Post],
            "/v1/vector_stores",
            PathSuffixMode::Append,
        ),
        (
            vec![HttpMethod::Delete],
            "/v1/vector_stores",
            PathSuffixMode::Append,
        ),
        (
            vec![HttpMethod::Get],
            "/v1/vector_stores",
            PathSuffixMode::Append,
        ),
    ];
    assert_eq!(shapes.len(), expected.len(), "{shapes:?}");
    for (methods, path, mode) in expected {
        let found = shapes
            .iter()
            .find(|(m, p, _, _)| *m == methods && p == path)
            .unwrap_or_else(|| panic!("missing RAG route {methods:?} {path}: {shapes:?}"));
        assert_eq!(found.2, mode, "{methods:?} {path}");
        assert!(
            found.3.is_empty(),
            "openai RAG routes allow no query: {found:?}"
        );
    }
}

#[test]
fn the_azure_rag_routes_use_the_openai_prefix_and_allow_the_api_version_query() {
    let registry = registry_of(json!({
        "azure": {
            "kind": "openai_responses", "host": "res.openai.azure.com", "storage_kind": "azure",
            "api_version": "2025-03-01-preview"
        }
    }));
    let shapes: Vec<_> = rag_route_matches(&provider(&registry, "azure"))
        .iter()
        .map(route_shape)
        .collect();
    let paths: BTreeSet<(String, String)> = shapes
        .iter()
        .map(|(m, p, _, _)| (format!("{m:?}"), p.clone()))
        .collect();
    let expected: BTreeSet<(String, String)> = [
        ("[Post]", "/openai/files"),
        ("[Delete]", "/openai/files"),
        ("[Post]", "/openai/vector_stores"),
        ("[Delete]", "/openai/vector_stores"),
        ("[Get]", "/openai/vector_stores"),
    ]
    .iter()
    .map(|(m, p)| ((*m).to_owned(), (*p).to_owned()))
    .collect();
    assert_eq!(paths, expected);
    for (_, path, _, query) in &shapes {
        assert_eq!(query, &vec!["api-version".to_owned()], "{path}");
    }
}

// ── targets ──────────────────────────────────────────────────────────────────

#[test]
fn every_entry_and_every_override_with_its_own_host_or_alias_is_a_target() {
    let registry = registry_of(json!({
        "main": {
            "kind": "openai_responses", "host": "api.example.com", "storage_kind": "openai",
            "tenant_overrides": {
                (TENANT_A.to_string()): { "host": "tenant-a.example.com" },
                (TENANT_B.to_string()): { "host": "10.0.0.7", "upstream_alias": "tenant-b-alias" }
            }
        },
        "second": { "kind": "vllm_responses", "host": "llm.internal", "storage_kind": "openai" }
    }));
    let targets = provision_targets(&registry);
    assert_eq!(targets.len(), 4);
    let aliases: BTreeSet<String> = targets
        .iter()
        .map(|t| t.provider.upstream_alias.clone())
        .collect();
    assert_eq!(
        aliases,
        BTreeSet::from([
            "api.example.com".to_owned(),
            "tenant-a.example.com".to_owned(),
            "tenant-b-alias".to_owned(),
            "llm.internal".to_owned(),
        ])
    );
    let labels: BTreeSet<String> = targets.iter().map(|t| t.label.clone()).collect();
    assert!(labels.contains("main"), "{labels:?}");
    assert!(labels.contains("second"), "{labels:?}");
    assert!(
        labels
            .iter()
            .any(|l| l.starts_with("main/") && l.contains(&TENANT_A.to_string())),
        "an override is labelled with its provider and tenant: {labels:?}"
    );
}

// ── exchange ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_s2s_exchange_sends_the_configured_credentials_and_returns_the_context() {
    let authn = ScriptedAuthN::ok();
    let ctx = exchange_s2s(authn.as_ref(), &credentials()).await.unwrap();
    assert_eq!(ctx.subject_id(), S2S_SUBJECT);
    let exchanges = authn.exchanges.lock().unwrap().clone();
    assert_eq!(
        exchanges,
        vec![("mini-chat".to_owned(), "dev-secret".to_owned())]
    );
}

#[tokio::test]
async fn a_failed_s2s_exchange_is_a_startup_error_that_never_shows_the_secret() {
    for script in [AuthNScript::TokenFailure, AuthNScript::Unavailable] {
        let authn = ScriptedAuthN::new(script.clone());
        let error = exchange_s2s(authn.as_ref(), &credentials())
            .await
            .unwrap_err();
        assert!(
            matches!(error, ProvisioningError::S2sExchange(_)),
            "{error:?}"
        );
        assert!(!format!("{error} {error:?}").contains("dev-secret"));
    }
}

// ── first pass ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn the_first_pass_creates_the_upstream_and_the_chat_and_rag_routes_under_the_s2s_context() {
    let registry = default_registry();
    let authn = ScriptedAuthN::ok();
    let oagw = RecordingOagw::accepting();

    let outcome = first_pass(authn.as_ref(), oagw.as_ref(), &credentials(), &registry)
        .await
        .unwrap();

    assert!(outcome.deferred.is_empty());
    assert_eq!(outcome.s2s.subject_id(), S2S_SUBJECT);
    assert_eq!(authn.exchange_count(), 1, "one exchange for the whole pass");

    assert_eq!(oagw.upstream_aliases(), vec!["api.openai.com".to_owned()]);
    let upstream_id = oagw
        .list_upstreams(s2s_ctx(), &ListQuery::default())
        .await
        .unwrap()[0]
        .id;
    let routes = oagw.routes();
    assert_eq!(routes.len(), 6, "one chat route and five RAG routes");
    for route in &routes {
        assert_eq!(route.request.upstream_id(), upstream_id);
        assert_eq!(route.subject, S2S_SUBJECT);
    }
    for call in oagw.upstream_calls.lock().unwrap().iter() {
        assert_eq!(call.subject, S2S_SUBJECT);
        assert_eq!(call.tenant, TENANT_A);
    }
    // the upstream and the routes are the ones built from the entry; the chat route is first
    let openai = provider(&registry, "openai");
    assert_eq!(
        oagw.upstream_calls.lock().unwrap()[0].request,
        upstream_request(&openai)
    );
    assert_eq!(routes[0].request.match_rules(), &chat_route_match(&openai));
    let rag: Vec<_> = routes[1..]
        .iter()
        .map(|r| r.request.match_rules().clone())
        .collect();
    assert_eq!(rag, rag_route_matches(&openai));
}

#[tokio::test]
async fn the_first_pass_provisions_every_entry_and_override() {
    let registry = registry_of(json!({
        "main": {
            "kind": "openai_responses", "host": "api.example.com", "storage_kind": "openai",
            "tenant_overrides": { (TENANT_A.to_string()): { "host": "tenant-a.example.com" } }
        },
        "second": { "kind": "vllm_responses", "host": "llm.internal", "storage_kind": "openai" }
    }));
    let oagw = RecordingOagw::accepting();
    first_pass(
        ScriptedAuthN::ok().as_ref(),
        oagw.as_ref(),
        &credentials(),
        &registry,
    )
    .await
    .unwrap();
    let aliases: BTreeSet<String> = oagw.upstream_aliases().into_iter().collect();
    assert_eq!(
        aliases,
        BTreeSet::from([
            "api.example.com".to_owned(),
            "tenant-a.example.com".to_owned(),
            "llm.internal".to_owned(),
        ])
    );
    assert_eq!(oagw.route_call_count(), 3 * 6);
}

#[tokio::test]
async fn an_upstream_that_already_exists_is_reused_for_the_routes() {
    let registry = default_registry();
    let oagw = RecordingOagw::accepting();
    oagw.script("api.openai.com", UpstreamScript::AlreadyExists);
    let existing = oagw.with_existing("api.openai.com");

    let outcome = first_pass(
        ScriptedAuthN::ok().as_ref(),
        oagw.as_ref(),
        &credentials(),
        &registry,
    )
    .await
    .unwrap();

    assert!(outcome.deferred.is_empty());
    assert_eq!(oagw.upstream_call_count(), 1, "no second create attempt");
    let routes = oagw.routes();
    assert_eq!(routes.len(), 6);
    assert!(routes.iter().all(|r| r.request.upstream_id() == existing));
}

#[tokio::test]
async fn an_unreadable_secret_defers_the_entry_without_failing_the_pass_or_retrying() {
    let registry = registry_of(json!({
        "ready": { "kind": "openai_responses", "host": "ready.example.com", "storage_kind": "openai" },
        "waiting": { "kind": "openai_responses", "host": "waiting.example.com", "storage_kind": "openai" }
    }));
    let oagw = RecordingOagw::accepting();
    oagw.script(
        "waiting.example.com",
        UpstreamScript::SecretUnreadableForever,
    );

    let outcome = first_pass(
        ScriptedAuthN::ok().as_ref(),
        oagw.as_ref(),
        &credentials(),
        &registry,
    )
    .await
    .unwrap();

    assert_eq!(outcome.deferred.len(), 1);
    assert_eq!(outcome.deferred[0].label, "waiting");
    assert_eq!(
        outcome.deferred[0].provider.upstream_alias,
        "waiting.example.com"
    );
    // the ready entry is provisioned, the deferred one is attempted exactly once
    let mut aliases = oagw.upstream_aliases();
    aliases.sort();
    assert_eq!(aliases, vec!["ready.example.com", "waiting.example.com"]);
    assert_eq!(
        oagw.route_call_count(),
        6,
        "routes only for the ready entry"
    );
}

#[tokio::test]
async fn any_other_oagw_failure_is_a_startup_error_naming_the_provider() {
    let scripts = vec![
        UpstreamScript::Invalid,
        UpstreamScript::Unavailable,
        UpstreamScript::Internal,
        UpstreamScript::OtherPrecondition,
    ];
    for script in scripts {
        let registry = registry_of(json!({
            "broken-provider": { "kind": "openai_responses", "host": "broken.example.com", "storage_kind": "openai" }
        }));
        let oagw = RecordingOagw::accepting();
        oagw.script("broken.example.com", script.clone());
        let error = first_pass(
            ScriptedAuthN::ok().as_ref(),
            oagw.as_ref(),
            &credentials(),
            &registry,
        )
        .await
        .unwrap_err();
        match &error {
            ProvisioningError::Misconfigured { provider, .. } => {
                assert!(provider.contains("broken-provider"), "{script:?}: {error}");
            }
            other @ ProvisioningError::S2sExchange(_) => panic!("{script:?}: {other:?}"),
        }
        assert!(error.to_string().contains("broken-provider"), "{script:?}");
    }
}

#[tokio::test]
async fn a_misconfigured_entry_fails_the_pass_even_when_others_are_fine() {
    let registry = registry_of(json!({
        "a-good": { "kind": "openai_responses", "host": "good.example.com", "storage_kind": "openai" },
        "b-bad": { "kind": "openai_responses", "host": "bad.example.com", "storage_kind": "openai" }
    }));
    let oagw = RecordingOagw::accepting();
    oagw.script("bad.example.com", UpstreamScript::Invalid);
    let result = first_pass(
        ScriptedAuthN::ok().as_ref(),
        oagw.as_ref(),
        &credentials(),
        &registry,
    )
    .await;
    assert!(matches!(
        result,
        Err(ProvisioningError::Misconfigured { .. })
    ));
}

#[tokio::test]
async fn a_failed_chat_route_is_a_startup_error_but_a_failed_rag_route_is_not() {
    let registry = default_registry();

    let oagw = RecordingOagw::accepting();
    oagw.fail_routes_containing("/v1/responses");
    let result = first_pass(
        ScriptedAuthN::ok().as_ref(),
        oagw.as_ref(),
        &credentials(),
        &registry,
    )
    .await;
    assert!(
        matches!(result, Err(ProvisioningError::Misconfigured { .. })),
        "{result:?}"
    );

    let oagw = RecordingOagw::accepting();
    oagw.fail_routes_containing("vector_stores");
    let outcome = first_pass(
        ScriptedAuthN::ok().as_ref(),
        oagw.as_ref(),
        &credentials(),
        &registry,
    )
    .await
    .expect("a missing RAG route only degrades RAG");
    assert!(outcome.deferred.is_empty());
}

#[tokio::test]
async fn a_failed_s2s_exchange_fails_the_pass_before_any_oagw_call() {
    for script in [AuthNScript::TokenFailure, AuthNScript::Unavailable] {
        let oagw = RecordingOagw::accepting();
        let error = first_pass(
            ScriptedAuthN::new(script).as_ref(),
            oagw.as_ref(),
            &credentials(),
            &default_registry(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(error, ProvisioningError::S2sExchange(_)),
            "{error:?}"
        );
        assert_eq!(oagw.upstream_call_count(), 0);
        assert_eq!(oagw.route_call_count(), 0);
    }
}

#[tokio::test]
async fn provisioning_works_on_a_copy_so_the_alias_oagw_returns_never_reaches_the_registry() {
    let registry = registry_of(json!({
        "main": { "kind": "openai_responses", "host": "api.example.com", "storage_kind": "openai" }
    }));
    let before = registry.entry("main").unwrap().clone();
    let oagw = RecordingOagw::accepting();
    oagw.rewrite_returned_alias("-normalised");

    first_pass(
        ScriptedAuthN::ok().as_ref(),
        oagw.as_ref(),
        &credentials(),
        &registry,
    )
    .await
    .unwrap();

    assert_eq!(registry.entry("main").unwrap(), &before);
    assert_eq!(
        registry
            .resolve("main", Uuid::from_u128(1))
            .unwrap()
            .upstream_alias,
        "api.example.com"
    );
}

// ── one entry ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn provisioning_one_entry_reports_provisioned_or_deferred() {
    let registry = default_registry();
    let target = target_of(&registry, "openai");

    let oagw = RecordingOagw::accepting();
    assert_eq!(
        provision_entry(oagw.as_ref(), &s2s_ctx(), &target)
            .await
            .unwrap(),
        EntryOutcome::Provisioned
    );

    let oagw = RecordingOagw::accepting();
    oagw.script("api.openai.com", UpstreamScript::SecretUnreadableForever);
    assert_eq!(
        provision_entry(oagw.as_ref(), &s2s_ctx(), &target)
            .await
            .unwrap(),
        EntryOutcome::Deferred
    );
    assert_eq!(oagw.route_call_count(), 0);

    let oagw = RecordingOagw::accepting();
    oagw.script("api.openai.com", UpstreamScript::Invalid);
    assert!(matches!(
        provision_entry(oagw.as_ref(), &s2s_ctx(), &target).await,
        Err(ProvisioningError::Misconfigured { .. })
    ));
}

// ── route and alias reuse (a reused upstream already holds the routes) ───────

#[tokio::test]
#[traced_test]
async fn a_duplicate_target_with_the_same_alias_and_api_path_provisions_ok() {
    // two entries on one host share the alias, hence the upstream, and the identical routes
    let registry = registry_of(json!({
        "first": { "kind": "openai_responses", "host": "api.example.com", "storage_kind": "openai" },
        "second": { "kind": "openai_responses", "host": "api.example.com", "storage_kind": "openai" }
    }));
    let oagw = RecordingOagw::accepting();
    oagw.script("api.example.com", UpstreamScript::AlreadyExists);
    let existing = oagw.with_existing("api.example.com");

    let outcome = first_pass(
        ScriptedAuthN::ok().as_ref(),
        oagw.as_ref(),
        &credentials(),
        &registry,
    )
    .await
    .expect("identical duplicates do not fail the startup");

    assert!(outcome.deferred.is_empty());
    assert_eq!(oagw.upstream_call_count(), 2);
    assert_eq!(oagw.route_call_count(), 12, "both entries try every route");
    assert!(
        oagw.routes()
            .iter()
            .all(|r| r.request.upstream_id() == existing)
    );
    // the second entry meets OAGW's route overlap check; that is neither an error nor a
    // degraded RAG
    assert!(!logs_contain("RAG is degraded"));
    assert!(!logs_contain("share an OAGW upstream alias"));
}

#[tokio::test]
#[traced_test]
async fn a_route_that_already_exists_counts_as_provisioned() {
    let registry = default_registry();
    let oagw = RecordingOagw::accepting();
    oagw.script("api.openai.com", UpstreamScript::AlreadyExists);
    oagw.with_existing("api.openai.com");
    let target = target_of(&registry, "openai");

    // the first run creates the routes, a restart finds them all
    for _ in 0..2 {
        let outcome = provision_entry(oagw.as_ref(), &s2s_ctx(), &target)
            .await
            .expect("an existing route is not a misconfiguration");
        assert_eq!(outcome, EntryOutcome::Provisioned);
    }
    assert_eq!(oagw.route_call_count(), 12);
    assert!(!logs_contain("RAG is degraded"));
}

#[tokio::test]
#[traced_test]
async fn a_rag_route_failing_for_another_reason_still_only_degrades_rag() {
    let registry = default_registry();
    let oagw = RecordingOagw::accepting();
    oagw.fail_routes_containing("vector_stores");
    let outcome = provision_entry(oagw.as_ref(), &s2s_ctx(), &target_of(&registry, "openai"))
        .await
        .unwrap();
    assert_eq!(outcome, EntryOutcome::Provisioned);
    assert!(logs_contain("RAG is degraded"));
}

fn shared_alias_registry(first: Value, second: Value) -> ProviderRegistry {
    let entry = |extra: Value| {
        let mut entry = json!({
            "kind": "openai_responses", "upstream_alias": "shared", "storage_kind": "openai"
        });
        for (key, value) in extra.as_object().unwrap() {
            entry[key] = value.clone();
        }
        entry
    };
    registry_of(json!({ "a": entry(first), "b": entry(second) }))
}

#[test]
fn targets_sharing_an_alias_with_another_endpoint_or_auth_are_reported() {
    let apikey = "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1";
    // (first, second, expected reason fragment)
    let cases = [
        (
            json!({ "host": "10.0.0.1" }),
            json!({ "host": "10.0.0.2" }),
            Some("host"),
        ),
        (
            json!({ "host": "10.0.0.1", "port": 8080 }),
            json!({ "host": "10.0.0.1", "port": 8081 }),
            Some("port"),
        ),
        (
            json!({ "host": "10.0.0.1", "use_http": true, "port": 443 }),
            json!({ "host": "10.0.0.1", "port": 443 }),
            Some("scheme"),
        ),
        (
            json!({ "host": "10.0.0.1", "auth_plugin_type": apikey, "auth_config": { "secret_ref": "cred://a" } }),
            json!({ "host": "10.0.0.1", "auth_plugin_type": apikey, "auth_config": { "secret_ref": "cred://b" } }),
            Some("secret_ref"),
        ),
        (
            json!({ "host": "10.0.0.1", "auth_plugin_type": apikey }),
            json!({ "host": "10.0.0.1" }),
            Some("plugin type"),
        ),
        // identical duplicates (the api_path may differ) are fine
        (
            json!({ "host": "10.0.0.1", "api_path": "/v1/responses" }),
            json!({ "host": "10.0.0.1", "api_path": "/v1/other" }),
            None,
        ),
    ];
    for (first, second, expected) in cases {
        let registry = shared_alias_registry(first.clone(), second.clone());
        let conflicts = alias_conflicts(&provision_targets(&registry));
        match expected {
            Some(fragment) => {
                assert_eq!(conflicts.len(), 1, "{first} vs {second}");
                assert_eq!(conflicts[0].alias, "shared");
                assert_eq!(
                    (conflicts[0].first.as_str(), conflicts[0].second.as_str()),
                    ("a", "b")
                );
                assert!(conflicts[0].reason.contains(fragment), "{:?}", conflicts[0]);
            }
            None => assert!(conflicts.is_empty(), "{first} vs {second}: {conflicts:?}"),
        }
    }
}

#[tokio::test]
#[traced_test]
async fn the_first_pass_warns_naming_both_targets_but_does_not_fail() {
    let registry =
        shared_alias_registry(json!({ "host": "10.0.0.1" }), json!({ "host": "10.0.0.2" }));
    let oagw = RecordingOagw::accepting();
    oagw.script("shared", UpstreamScript::AlreadyExists);
    oagw.with_existing("shared");
    first_pass(
        ScriptedAuthN::ok().as_ref(),
        oagw.as_ref(),
        &credentials(),
        &registry,
    )
    .await
    .expect("a conflicting alias is a warning, not a startup error");
    assert!(logs_contain("share an OAGW upstream alias"));
    assert!(logs_contain("first=a"));
    assert!(logs_contain("second=b"));
}

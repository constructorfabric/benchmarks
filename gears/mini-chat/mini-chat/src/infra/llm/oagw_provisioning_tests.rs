#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Tests of `cpt-cf-mini-chat-algo-gear-foundation-provision-oagw` and the
//! state machine `cpt-cf-mini-chat-state-gear-foundation-provider-provisioning`
//! over a fake OAGW and a fake S2S exchange.

use std::sync::Arc;
use std::time::Duration;

use oagw_sdk::{HttpMethod, PathSuffixMode, ServiceGatewayClientV1};
use serde_json::{Value, json};
use tracing::Level;
use uuid::Uuid;

use super::{
    OAGW_CALL_TIMEOUT, OagwProvisioner, ProvisionTarget, ProvisioningError, ProvisioningState,
    ProvisioningTimeouts, S2S_EXCHANGE_TIMEOUT, S2sFailureCause,
};
use crate::config::{ProviderRegistry, StorageKind};
use crate::infra::llm::provider_registry::ProviderResolver;
use crate::infra::llm::reconcile::TargetRetry;
use crate::test_doubles::{
    APIKEY_PLUGIN, AuthnReply, FakeAuthn, FakeOagw, Fault, LogCapture, OagwCall, S2S_SUBJECT,
    credentials, provider_json, registry, registry_ordered, s2s_ctx,
};

const OVERRIDE_TENANT: Uuid = Uuid::from_u128(0xA1);
const AUTH_SENTINEL: &str = "cred://AUTH-CONFIG-SENTINEL";
const CLIENT_SECRET: &str = "S3CRET-SENTINEL";

fn fast() -> ProvisioningTimeouts {
    ProvisioningTimeouts {
        s2s_exchange: Duration::from_millis(50),
        oagw_call: Duration::from_millis(50),
    }
}

fn provisioner(oagw: &Arc<FakeOagw>, authn: &Arc<FakeAuthn>) -> OagwProvisioner {
    OagwProvisioner::new(
        Arc::clone(oagw) as Arc<dyn ServiceGatewayClientV1>,
        Arc::clone(authn) as Arc<dyn authn_resolver_sdk::AuthNResolverClient>,
        credentials(),
    )
    .with_timeouts(fast())
}

fn ok_authn() -> Arc<FakeAuthn> {
    FakeAuthn::new(AuthnReply::Ok)
}

fn openai_entry(host: &str) -> Value {
    let mut entry = provider_json(host, "openai");
    entry["auth_config"]["secret_ref"] = json!(AUTH_SENTINEL);
    entry
}

fn single() -> ProviderRegistry {
    registry(&json!({ "primary": openai_entry("api.openai.com") }))
}

/// `(method, path)` pairs as sortable text.
fn texts(routes: Vec<(HttpMethod, String)>) -> Vec<String> {
    let mut out: Vec<String> = routes
        .into_iter()
        .map(|(method, path)| format!("{} {path}", method_name(method)))
        .collect();
    out.sort();
    out
}

fn method_name(method: HttpMethod) -> &'static str {
    match method {
        HttpMethod::Get => "GET",
        HttpMethod::Post => "POST",
        HttpMethod::Put => "PUT",
        HttpMethod::Delete => "DELETE",
        HttpMethod::Patch => "PATCH",
    }
}

fn expected(routes: &[&str]) -> Vec<String> {
    let mut out: Vec<String> = routes.iter().map(|r| (*r).to_owned()).collect();
    out.sort();
    out
}

const OPENAI_RAG: [&str; 5] = [
    "POST /v1/files",
    "DELETE /v1/files",
    "POST /v1/vector_stores",
    "DELETE /v1/vector_stores",
    "GET /v1/vector_stores",
];

fn openai_all() -> Vec<String> {
    let mut all = vec!["POST /v1/responses"];
    all.extend(OPENAI_RAG);
    expected(&all)
}

// ---- bounds ------------------------------------------------------------------------

#[test]
fn the_bounds_are_ten_seconds() {
    assert_eq!(S2S_EXCHANGE_TIMEOUT, Duration::from_secs(10));
    assert_eq!(OAGW_CALL_TIMEOUT, Duration::from_secs(10));
    assert_eq!(
        ProvisioningTimeouts::default(),
        ProvisioningTimeouts {
            s2s_exchange: Duration::from_secs(10),
            oagw_call: Duration::from_secs(10),
        }
    );
}

// ---- inst-s2s-exchange ---------------------------------------------------------------

#[tokio::test]
async fn the_s2s_exchange_uses_the_client_credentials_once() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());

    provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("provisions");

    assert_eq!(
        authn.exchanges.lock().unwrap().clone(),
        vec![("mini-chat-client".to_owned(), CLIENT_SECRET.to_owned())]
    );
}

#[tokio::test]
async fn every_oagw_call_uses_the_s2s_security_context() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());

    let outcome = provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("provisions");

    assert!(!oagw.calls().is_empty());
    assert_eq!(oagw.call_subjects(), [S2S_SUBJECT].into_iter().collect());
    assert_eq!(outcome.s2s_context.subject_id(), S2S_SUBJECT);
}

#[tokio::test]
async fn a_failed_s2s_exchange_names_the_client_id_and_calls_no_oagw_endpoint() {
    let (oagw, authn) = (
        FakeOagw::new(),
        FakeAuthn::new(AuthnReply::Fail("idp down".to_owned())),
    );

    let err = provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect_err("exchange fails");

    match &err {
        ProvisioningError::S2sExchange { client_id, cause } => {
            assert_eq!(client_id, "mini-chat-client");
            match cause {
                S2sFailureCause::Failed(message) => {
                    assert!(message.contains("idp down"), "{message}");
                }
                S2sFailureCause::Timeout => panic!("expected Failed, got a timeout"),
            }
        }
        ProvisioningError::Misconfigured { .. } => panic!("expected S2sExchange, got {err:?}"),
    }
    assert_eq!(authn.exchange_count(), 1);
    assert!(err.to_string().contains("client_id"), "{err}");
    assert!(err.to_string().contains("mini-chat-client"), "{err}");
    assert!(!err.to_string().contains(CLIENT_SECRET), "{err}");
    assert!(oagw.calls().is_empty());
}

#[tokio::test]
async fn a_timed_out_s2s_exchange_names_the_client_id_and_timeout() {
    let (oagw, authn) = (FakeOagw::new(), FakeAuthn::new(AuthnReply::Hang));

    let err = tokio::time::timeout(
        Duration::from_secs(5),
        provisioner(&oagw, &authn).provision(&single()),
    )
    .await
    .expect("the provisioner enforces its own bound")
    .expect_err("exchange times out");

    match &err {
        ProvisioningError::S2sExchange { client_id, cause } => {
            assert_eq!(client_id, "mini-chat-client");
            assert_eq!(*cause, S2sFailureCause::Timeout);
        }
        ProvisioningError::Misconfigured { .. } => panic!("expected S2sExchange, got {err:?}"),
    }
    assert!(err.to_string().contains("client_id"), "{err}");
    assert!(err.to_string().contains("timeout"), "{err}");
    assert!(oagw.calls().is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_default_s2s_bound_is_ten_seconds() {
    let (oagw, authn) = (FakeOagw::new(), FakeAuthn::new(AuthnReply::Hang));
    let provisioner = OagwProvisioner::new(
        Arc::clone(&oagw) as Arc<dyn ServiceGatewayClientV1>,
        Arc::clone(&authn) as Arc<dyn authn_resolver_sdk::AuthNResolverClient>,
        credentials(),
    );
    let started = tokio::time::Instant::now();

    let err = provisioner
        .provision(&single())
        .await
        .expect_err("exchange times out");

    assert!(matches!(
        err,
        ProvisioningError::S2sExchange {
            cause: S2sFailureCause::Timeout,
            ..
        }
    ));
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_secs(10) && elapsed < Duration::from_secs(11),
        "{elapsed:?}"
    );
}

// ---- inst-build-targets ----------------------------------------------------------------

#[test]
fn targets_are_built_per_entry_and_override_in_configuration_order() {
    let mut openai = openai_entry("api.openai.com");
    openai["tenant_overrides"] =
        json!({ OVERRIDE_TENANT.to_string(): { "host": "tenant-a.example.com" } });
    let mut azure = provider_json("res.openai.azure.com", "azure");
    azure["tenant_overrides"] = json!({ "00000000-0000-0000-0000-0000000000b2": {
        "upstream_alias": "azure-tenant-b",
        "host": "tenant-b.openai.azure.com"
    } });
    let registry = registry_ordered(&[("openai", openai), ("azure", azure)]);

    let targets = ProvisionTarget::from_registry(&registry);

    let labels: Vec<String> = targets.iter().map(ProvisionTarget::label).collect();
    assert_eq!(
        labels,
        vec![
            "providers.openai".to_owned(),
            format!("providers.openai.tenant_overrides.{OVERRIDE_TENANT}"),
            "providers.azure".to_owned(),
            "providers.azure.tenant_overrides.00000000-0000-0000-0000-0000000000b2".to_owned(),
        ]
    );
    let aliases: Vec<&str> = targets.iter().map(|t| t.alias.as_str()).collect();
    assert_eq!(
        aliases,
        vec![
            "api.openai.com",
            "tenant-a.example.com",
            "res.openai.azure.com",
            "tenant-b.openai.azure.com"
        ]
    );
}

#[test]
fn an_override_target_inherits_storage_kind_api_version_port_and_api_path() {
    let mut azure = provider_json("res.openai.azure.com", "azure");
    azure["api_path"] =
        json!("/openai/deployments/{model}/chat/completions?api-version=2025-01-01");
    azure["tenant_overrides"] =
        json!({ OVERRIDE_TENANT.to_string(): { "host": "tenant-a.openai.azure.com" } });
    let registry = registry(&json!({ "azure": azure }));

    let targets = ProvisionTarget::from_registry(&registry);

    assert_eq!(targets.len(), 2);
    let over = &targets[1];
    assert_eq!(
        over.tenant_id.as_deref(),
        Some(OVERRIDE_TENANT.to_string().as_str())
    );
    assert_eq!(over.host, "tenant-a.openai.azure.com");
    assert_eq!(over.storage_kind, StorageKind::Azure);
    assert_eq!(over.api_version.as_deref(), Some("2025-01-01-preview"));
    assert_eq!(over.port, 443);
    assert_eq!(over.api_path, targets[0].api_path);
    assert_eq!(over.auth_plugin_type.as_deref(), Some(APIKEY_PLUGIN));
    assert_eq!(over.auth_config, targets[0].auth_config);
}

// ---- inst-create-upstream / inst-create-chat-route ---------------------------------------

#[tokio::test]
async fn a_single_entry_gets_an_upstream_the_chat_route_and_the_rag_routes() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());

    let outcome = provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("provisions");

    assert_eq!(oagw.upstream_creates().len(), 1);
    assert_eq!(texts(oagw.created_routes()), openai_all());
    assert_eq!(outcome.targets.len(), 1);
    assert_eq!(outcome.targets[0].state, ProvisioningState::Provisioned);
    assert!(outcome.deferred().is_empty());
    assert_eq!(outcome.provisioned().len(), 1);
}

#[tokio::test]
async fn the_upstream_carries_alias_endpoint_and_auth_of_the_target() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());

    provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("provisions");

    assert_eq!(
        oagw.upstream_creates(),
        vec![OagwCall::CreateUpstream {
            alias: "api.openai.com".to_owned(),
            host: "api.openai.com".to_owned(),
            port: 443,
            https: true,
            plugin_type: Some(APIKEY_PLUGIN.to_owned()),
            auth_config: vec![
                ("header".to_owned(), "Authorization".to_owned()),
                ("prefix".to_owned(), "Bearer ".to_owned()),
                ("secret_ref".to_owned(), AUTH_SENTINEL.to_owned()),
            ],
        }]
    );
}

#[tokio::test]
async fn an_http_entry_uses_the_http_scheme_and_its_port() {
    let mut entry = openai_entry("llm.internal");
    entry["use_http"] = json!(true);
    entry["port"] = json!(8080);
    let (oagw, authn) = (FakeOagw::new(), ok_authn());

    provisioner(&oagw, &authn)
        .provision(&registry(&json!({ "local": entry })))
        .await
        .expect("provisions");

    match &oagw.upstream_creates()[0] {
        OagwCall::CreateUpstream {
            alias,
            host,
            port,
            https,
            ..
        } => {
            assert_eq!(alias, "llm.internal:8080", "derived by OAGW");
            assert_eq!(host, "llm.internal");
            assert_eq!(*port, 8080);
            assert!(!*https);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_chat_route_is_a_post_on_the_api_path_with_no_allowlist() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());

    provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("provisions");

    let chat: Vec<OagwCall> = oagw
        .route_creates()
        .into_iter()
        .filter(
            |call| matches!(call, OagwCall::CreateRoute { path, .. } if path == "/v1/responses"),
        )
        .collect();
    assert_eq!(chat.len(), 1);
    match &chat[0] {
        OagwCall::CreateRoute {
            method,
            query_allowlist,
            ..
        } => {
            assert_eq!(*method, HttpMethod::Post);
            assert!(query_allowlist.is_empty(), "{query_allowlist:?}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn the_chat_route_prefix_stops_at_the_model_placeholder_and_the_query_is_allowlisted() {
    let mut azure = provider_json("res.openai.azure.com", "azure");
    azure["api_path"] = json!(
        "/openai/deployments/{model}/chat/completions?api-version=2025-01-01-preview&alt=sse"
    );
    let (oagw, authn) = (FakeOagw::new(), ok_authn());

    provisioner(&oagw, &authn)
        .provision(&registry(&json!({ "azure": azure })))
        .await
        .expect("provisions");

    let chat: Vec<OagwCall> = oagw
        .route_creates()
        .into_iter()
        .filter(|call| matches!(call, OagwCall::CreateRoute { path, .. } if path == "/openai/deployments"))
        .collect();
    assert_eq!(chat.len(), 1, "{:?}", oagw.created_routes());
    match &chat[0] {
        OagwCall::CreateRoute {
            method,
            query_allowlist,
            suffix,
            ..
        } => {
            assert_eq!(*method, HttpMethod::Post);
            assert_eq!(
                query_allowlist,
                &vec!["api-version".to_owned(), "alt".to_owned()]
            );
            assert_eq!(*suffix, PathSuffixMode::Append);
        }
        other => panic!("{other:?}"),
    }
}

// ---- inst-dedupe-targets -----------------------------------------------------------------

#[tokio::test]
async fn two_entries_sharing_an_alias_provision_the_upstream_and_each_route_once() {
    let registry = registry(&json!({
        "first": openai_entry("api.openai.com"),
        "second": openai_entry("api.openai.com"),
    }));
    let (oagw, authn) = (FakeOagw::new(), ok_authn());

    let outcome = provisioner(&oagw, &authn)
        .provision(&registry)
        .await
        .expect("provisions");

    assert_eq!(oagw.upstream_creates().len(), 1);
    assert_eq!(texts(oagw.created_routes()), openai_all());
    assert_eq!(outcome.targets.len(), 2);
    assert!(
        outcome
            .targets
            .iter()
            .all(|t| t.state == ProvisioningState::Provisioned)
    );
}

#[tokio::test]
async fn a_tenant_override_sharing_the_entry_alias_adds_no_upstream_and_no_route() {
    let mut openai = openai_entry("api.openai.com");
    openai["tenant_overrides"] =
        json!({ OVERRIDE_TENANT.to_string(): { "upstream_alias": "api.openai.com" } });
    let (oagw, authn) = (FakeOagw::new(), ok_authn());

    let outcome = provisioner(&oagw, &authn)
        .provision(&registry(&json!({ "openai": openai })))
        .await
        .expect("provisions");

    assert_eq!(oagw.upstream_creates().len(), 1);
    assert_eq!(texts(oagw.created_routes()), openai_all());
    assert_eq!(outcome.targets.len(), 2);
    assert!(
        outcome
            .targets
            .iter()
            .all(|t| t.state == ProvisioningState::Provisioned)
    );
}

#[tokio::test]
async fn distinct_aliases_get_their_own_upstream_and_routes() {
    let mut openai = openai_entry("api.openai.com");
    openai["tenant_overrides"] =
        json!({ OVERRIDE_TENANT.to_string(): { "host": "tenant-a.example.com" } });
    let (oagw, authn) = (FakeOagw::new(), ok_authn());

    provisioner(&oagw, &authn)
        .provision(&registry(&json!({ "openai": openai })))
        .await
        .expect("provisions");

    assert_eq!(oagw.upstream_creates().len(), 2);
    assert_eq!(oagw.route_creates().len(), 12);
    assert_eq!(oagw.upstream_count(), 2);
    assert_eq!(
        texts(oagw.stored_routes("tenant-a.example.com")),
        openai_all()
    );
}

#[tokio::test]
async fn every_target_of_a_group_takes_the_group_outcome() {
    let registry = registry_ordered(&[
        ("first", openai_entry("api.openai.com")),
        ("second", openai_entry("api.openai.com")),
        ("other", openai_entry("other.example.com")),
    ]);
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_upstream("api.openai.com", Fault::SecretNotReadable);

    let outcome = provisioner(&oagw, &authn)
        .provision(&registry)
        .await
        .expect("deferral does not fail");

    let states: Vec<(String, ProvisioningState)> = outcome
        .targets
        .iter()
        .map(|t| (t.target.provider_id.clone(), t.state))
        .collect();
    assert_eq!(
        states,
        vec![
            ("first".to_owned(), ProvisioningState::Deferred),
            ("second".to_owned(), ProvisioningState::Deferred),
            ("other".to_owned(), ProvisioningState::Provisioned),
        ]
    );
    assert_eq!(
        oagw.upstream_creates()
            .iter()
            .filter(|call| matches!(call, OagwCall::CreateUpstream { alias, .. } if alias == "api.openai.com"))
            .count(),
        1,
        "the group's upstream is attempted once"
    );
}

// ---- inst-list-routes / idempotency ---------------------------------------------------------

#[tokio::test]
async fn an_already_existing_upstream_is_reused_and_its_routes_are_listed() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    let upstream_id = oagw.seed_upstream("api.openai.com");

    let outcome = provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("a reused upstream is not an error");

    assert_eq!(outcome.targets[0].state, ProvisioningState::Provisioned);
    let calls = oagw.calls();
    assert!(
        calls.contains(&OagwCall::ListRoutes { upstream_id }),
        "{calls:?}"
    );
    assert_eq!(oagw.upstream_count(), 1);
    assert_eq!(texts(oagw.created_routes()), openai_all());
    for call in oagw.route_creates() {
        match call {
            OagwCall::CreateRoute {
                upstream_id: id, ..
            } => assert_eq!(id, upstream_id),
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn the_routes_are_listed_before_any_route_is_created() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.seed_upstream("api.openai.com");

    provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("provisions");

    let calls = oagw.calls();
    let list = calls
        .iter()
        .position(|c| matches!(c, OagwCall::ListRoutes { .. }))
        .expect("listed");
    let first_create = calls
        .iter()
        .position(|c| matches!(c, OagwCall::CreateRoute { .. }))
        .expect("created");
    assert!(list < first_create, "{calls:?}");
}

#[tokio::test]
async fn a_freshly_created_upstream_has_no_routes_to_list() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());

    provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("provisions");

    assert!(
        !oagw
            .calls()
            .iter()
            .any(|c| matches!(c, OagwCall::ListRoutes { .. })),
        "{:?}",
        oagw.calls()
    );
}

#[tokio::test]
async fn existing_routes_with_the_same_method_and_path_are_skipped() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    let upstream_id = oagw.seed_upstream("api.openai.com");
    oagw.seed_route(upstream_id, HttpMethod::Post, "/v1/responses");
    oagw.seed_route(upstream_id, HttpMethod::Delete, "/v1/files");
    oagw.seed_route(upstream_id, HttpMethod::Get, "/v1/vector_stores");

    let outcome = provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("provisions");

    assert_eq!(outcome.targets[0].state, ProvisioningState::Provisioned);
    assert_eq!(
        texts(oagw.created_routes()),
        expected(&[
            "POST /v1/files",
            "POST /v1/vector_stores",
            "DELETE /v1/vector_stores",
        ])
    );
}

#[tokio::test]
async fn an_existing_route_with_another_method_or_path_does_not_suppress_creation() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    let upstream_id = oagw.seed_upstream("api.openai.com");
    oagw.seed_route(upstream_id, HttpMethod::Get, "/v1/responses");
    oagw.seed_route(upstream_id, HttpMethod::Post, "/v1/other");

    provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("provisions");

    assert_eq!(texts(oagw.created_routes()), openai_all());
}

#[tokio::test]
async fn a_conflict_on_the_chat_route_counts_as_success() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_route("/v1/responses", Fault::Conflict);

    let outcome = provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("a conflict is not an error");

    assert_eq!(outcome.targets[0].state, ProvisioningState::Provisioned);
    assert_eq!(
        texts(oagw.created_routes()),
        openai_all(),
        "the rag routes are still created after the conflict"
    );
}

#[tokio::test]
async fn a_conflict_on_a_rag_route_counts_as_success_without_a_warning() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_route("/v1/vector_stores", Fault::Conflict);
    let logs = LogCapture::start();

    let outcome = provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("a conflict is not an error");

    assert_eq!(outcome.targets[0].state, ProvisioningState::Provisioned);
    assert!(
        logs.at(Level::WARN).is_empty(),
        "{:?}",
        logs.at(Level::WARN)
    );
    assert!(logs.at(Level::ERROR).is_empty());
}

#[tokio::test]
async fn provisioning_against_an_oagw_that_already_holds_everything_succeeds_without_creating_routes()
 {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    let provisioner = provisioner(&oagw, &authn);
    provisioner.provision(&single()).await.expect("first run");
    let routes_after_first = oagw.route_creates().len();
    assert_eq!(routes_after_first, 6);

    let outcome = provisioner.provision(&single()).await.expect("restart");

    assert_eq!(outcome.targets[0].state, ProvisioningState::Provisioned);
    assert_eq!(oagw.route_creates().len(), routes_after_first);
    assert_eq!(oagw.upstream_count(), 1);
}

#[tokio::test]
async fn the_restart_scenario_with_an_override_and_a_shared_host_converges() {
    let mut openai = openai_entry("api.openai.com");
    openai["tenant_overrides"] = json!({
        OVERRIDE_TENANT.to_string(): { "upstream_alias": "api.openai.com" },
        "00000000-0000-0000-0000-0000000000b2": { "host": "tenant-b.example.com" },
    });
    let registry = registry(&json!({
        "openai": openai,
        "mirror": openai_entry("api.openai.com"),
    }));
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    let provisioner = provisioner(&oagw, &authn);

    provisioner.provision(&registry).await.expect("first run");
    let outcome = provisioner.provision(&registry).await.expect("restart");

    assert_eq!(outcome.targets.len(), 4);
    assert!(
        outcome
            .targets
            .iter()
            .all(|t| t.state == ProvisioningState::Provisioned)
    );
    assert_eq!(oagw.upstream_count(), 2);
    assert_eq!(oagw.route_creates().len(), 12, "each route exactly once");
}

// ---- inst-catch-secret / inst-catch-timeout -------------------------------------------------

#[tokio::test]
async fn an_unreadable_secret_defers_the_target_without_failing_start() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_upstream("api.openai.com", Fault::SecretNotReadable);

    let outcome = provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("deferral is not an error");

    assert_eq!(outcome.targets[0].state, ProvisioningState::Deferred);
    assert_eq!(outcome.deferred().len(), 1);
    assert_eq!(outcome.deferred()[0].provider_id, "primary");
    assert!(
        oagw.route_creates().is_empty(),
        "no routes for a deferred target"
    );
}

#[tokio::test]
async fn a_deferred_target_does_not_block_the_other_targets() {
    let registry = registry(&json!({
        "late": openai_entry("late.example.com"),
        "ready": openai_entry("ready.example.com"),
    }));
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_upstream("late.example.com", Fault::SecretNotReadable);

    let outcome = provisioner(&oagw, &authn)
        .provision(&registry)
        .await
        .expect("provisions");

    assert_eq!(outcome.targets[0].state, ProvisioningState::Deferred);
    assert_eq!(outcome.targets[1].state, ProvisioningState::Provisioned);
    assert_eq!(texts(oagw.stored_routes("ready.example.com")), openai_all());
}

#[tokio::test]
async fn an_oagw_call_that_exceeds_its_bound_defers_the_target_and_warns() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_upstream("api.openai.com", Fault::Hang);
    let logs = LogCapture::start();

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        provisioner(&oagw, &authn).provision(&single()),
    )
    .await
    .expect("every OAGW call is bounded")
    .expect("a timeout defers");

    assert_eq!(outcome.targets[0].state, ProvisioningState::Deferred);
    let warnings = logs.at_containing(Level::WARN, "primary");
    assert!(!warnings.is_empty(), "{:?}", logs.records());
}

#[tokio::test]
async fn a_canonical_deadline_exceeded_defers_the_target_and_warns() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_upstream("api.openai.com", Fault::DeadlineExceeded);
    let logs = LogCapture::start();

    let outcome = provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("a timeout defers");

    assert_eq!(outcome.targets[0].state, ProvisioningState::Deferred);
    assert!(!logs.at_containing(Level::WARN, "primary").is_empty());
}

#[tokio::test]
async fn a_timeout_listing_the_routes_of_a_reused_upstream_defers_the_target() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.seed_upstream("api.openai.com");
    oagw.fail_list_routes(Fault::Hang);

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        provisioner(&oagw, &authn).provision(&single()),
    )
    .await
    .expect("bounded")
    .expect("a timeout defers");

    assert_eq!(outcome.targets[0].state, ProvisioningState::Deferred);
    assert!(oagw.route_creates().is_empty());
}

#[tokio::test]
async fn a_timeout_creating_the_chat_route_defers_the_target() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_route("/v1/responses", Fault::Hang);

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        provisioner(&oagw, &authn).provision(&single()),
    )
    .await
    .expect("bounded")
    .expect("a timeout defers");

    assert_eq!(outcome.targets[0].state, ProvisioningState::Deferred);
}

// ---- inst-catch-misconfig ---------------------------------------------------------------------

#[tokio::test]
async fn any_other_upstream_error_is_a_misconfiguration_naming_the_provider() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_upstream("api.openai.com", Fault::InvalidArgument);

    let err = provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect_err("misconfigured");

    match &err {
        ProvisioningError::Misconfigured {
            provider_id,
            tenant_id,
            ..
        } => {
            assert_eq!(provider_id, "primary");
            assert_eq!(*tenant_id, None);
        }
        ProvisioningError::S2sExchange { .. } => panic!("expected Misconfigured, got {err:?}"),
    }
    assert!(err.to_string().contains("primary"), "{err}");
}

#[tokio::test]
async fn an_internal_oagw_error_is_a_misconfiguration() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_upstream("api.openai.com", Fault::Internal);

    let err = provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect_err("misconfigured");

    assert!(
        matches!(err, ProvisioningError::Misconfigured { .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_failing_tenant_override_names_the_provider_and_the_tenant() {
    let mut openai = openai_entry("api.openai.com");
    openai["tenant_overrides"] =
        json!({ OVERRIDE_TENANT.to_string(): { "host": "tenant-a.example.com" } });
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_upstream("tenant-a.example.com", Fault::InvalidArgument);

    let err = provisioner(&oagw, &authn)
        .provision(&registry(&json!({ "openai": openai })))
        .await
        .expect_err("misconfigured");

    match &err {
        ProvisioningError::Misconfigured {
            provider_id,
            tenant_id,
            ..
        } => {
            assert_eq!(provider_id, "openai");
            assert_eq!(
                tenant_id.as_deref(),
                Some(OVERRIDE_TENANT.to_string().as_str())
            );
        }
        ProvisioningError::S2sExchange { .. } => panic!("expected Misconfigured, got {err:?}"),
    }
    assert!(
        err.to_string().contains(&OVERRIDE_TENANT.to_string()),
        "{err}"
    );
}

#[tokio::test]
async fn a_chat_route_error_that_is_not_a_conflict_is_a_misconfiguration() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_route("/v1/responses", Fault::InvalidArgument);

    let err = provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect_err("misconfigured");

    assert!(
        matches!(err, ProvisioningError::Misconfigured { .. }),
        "{err:?}"
    );
    assert!(
        texts(oagw.created_routes()) == expected(&["POST /v1/responses"]),
        "the rag routes are not attempted after a failed chat route: {:?}",
        oagw.created_routes()
    );
}

#[tokio::test]
async fn a_misconfiguration_wins_over_a_deferral_of_another_target() {
    let registry = registry_ordered(&[
        ("late", openai_entry("late.example.com")),
        ("broken", openai_entry("broken.example.com")),
    ]);
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_upstream("late.example.com", Fault::SecretNotReadable);
    oagw.fail_upstream("broken.example.com", Fault::InvalidArgument);

    let err = provisioner(&oagw, &authn)
        .provision(&registry)
        .await
        .expect_err("misconfigured");

    assert!(
        matches!(&err, ProvisioningError::Misconfigured { provider_id, .. } if provider_id == "broken"),
        "{err:?}"
    );
}

#[tokio::test]
async fn no_error_warning_or_error_log_carries_a_secret_value() {
    let registry = registry_ordered(&[
        ("late", openai_entry("late.example.com")),
        ("slow", openai_entry("slow.example.com")),
        ("broken", openai_entry("broken.example.com")),
    ]);
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_upstream("late.example.com", Fault::SecretNotReadable);
    oagw.fail_upstream("slow.example.com", Fault::Hang);
    oagw.fail_upstream("broken.example.com", Fault::InvalidArgument);
    let logs = LogCapture::start();

    let err = provisioner(&oagw, &authn)
        .provision(&registry)
        .await
        .expect_err("misconfigured");
    assert!(
        matches!(err, ProvisioningError::Misconfigured { .. }),
        "{err:?}"
    );
    let failed_exchange = provisioner(&oagw, &FakeAuthn::new(AuthnReply::Fail("nope".into())))
        .provision(&registry)
        .await
        .expect_err("exchange fails");
    assert!(
        matches!(failed_exchange, ProvisioningError::S2sExchange { .. }),
        "{failed_exchange:?}"
    );

    for text in [
        err.to_string(),
        format!("{err:?}"),
        failed_exchange.to_string(),
        format!("{failed_exchange:?}"),
        logs.all_text(),
    ] {
        assert!(!text.contains("AUTH-CONFIG-SENTINEL"), "{text}");
        assert!(!text.contains(CLIENT_SECRET), "{text}");
    }
}

// ---- inst-create-rag-routes ---------------------------------------------------------------------

#[tokio::test]
async fn openai_rag_routes_use_the_v1_prefix() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());

    provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("provisions");

    let rag: Vec<OagwCall> = oagw
        .route_creates()
        .into_iter()
        .filter(
            |call| !matches!(call, OagwCall::CreateRoute { path, .. } if path == "/v1/responses"),
        )
        .collect();
    assert_eq!(rag.len(), 5);
    for call in rag {
        match call {
            OagwCall::CreateRoute {
                method,
                path,
                query_allowlist,
                suffix,
                ..
            } => {
                assert!(query_allowlist.is_empty(), "{path}: {query_allowlist:?}");
                let expected_suffix = if method == HttpMethod::Post && path == "/v1/files" {
                    PathSuffixMode::Disabled
                } else {
                    PathSuffixMode::Append
                };
                assert_eq!(suffix, expected_suffix, "{} {path}", method_name(method));
            }
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn azure_rag_routes_use_the_openai_prefix_and_allow_api_version() {
    let azure = provider_json("res.openai.azure.com", "azure");
    let (oagw, authn) = (FakeOagw::new(), ok_authn());

    provisioner(&oagw, &authn)
        .provision(&registry(&json!({ "azure": azure })))
        .await
        .expect("provisions");

    let rag: Vec<OagwCall> = oagw
        .route_creates()
        .into_iter()
        .filter(
            |call| !matches!(call, OagwCall::CreateRoute { path, .. } if path == "/v1/responses"),
        )
        .collect();
    assert_eq!(
        texts(
            rag.iter()
                .map(|call| match call {
                    OagwCall::CreateRoute { method, path, .. } => (*method, path.clone()),
                    other => panic!("{other:?}"),
                })
                .collect()
        ),
        expected(&[
            "POST /openai/files",
            "DELETE /openai/files",
            "POST /openai/vector_stores",
            "DELETE /openai/vector_stores",
            "GET /openai/vector_stores",
        ])
    );
    for call in rag {
        match call {
            OagwCall::CreateRoute {
                query_allowlist, ..
            } => {
                assert_eq!(query_allowlist, vec!["api-version".to_owned()]);
            }
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
async fn a_rag_route_failure_logs_a_warning_and_provisioning_continues() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_route("/v1/files", Fault::Internal);
    let logs = LogCapture::start();

    let outcome = provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("a rag failure is not an error");

    assert_eq!(outcome.targets[0].state, ProvisioningState::Provisioned);
    assert!(!logs.at_containing(Level::WARN, "primary").is_empty());
    assert_eq!(
        texts(oagw.created_routes()),
        expected(&[
            "POST /v1/responses",
            "POST /v1/files",
            "DELETE /v1/files",
            "POST /v1/vector_stores",
            "DELETE /v1/vector_stores",
            "GET /v1/vector_stores",
        ]),
        "every rag route is attempted"
    );
    assert_eq!(
        oagw.stored_routes("api.openai.com").len(),
        4,
        "the two failed routes are not stored"
    );
}

#[tokio::test]
async fn a_rag_route_timeout_logs_a_warning_and_does_not_defer_the_target() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_route("/v1/vector_stores", Fault::Hang);
    let logs = LogCapture::start();

    let outcome = tokio::time::timeout(
        Duration::from_secs(5),
        provisioner(&oagw, &authn).provision(&single()),
    )
    .await
    .expect("every call is bounded")
    .expect("a rag timeout is not an error");

    assert_eq!(outcome.targets[0].state, ProvisioningState::Provisioned);
    assert!(outcome.deferred().is_empty());
    assert!(!logs.at_containing(Level::WARN, "primary").is_empty());
}

// ---- inst-write-copy ------------------------------------------------------------------------------

#[tokio::test]
async fn the_alias_returned_by_oagw_is_written_to_the_copy_only() {
    let registry = single();
    let before = registry.clone();
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.rename_alias("api.openai.com", "oagw-assigned");

    let outcome = provisioner(&oagw, &authn)
        .provision(&registry)
        .await
        .expect("provisions");

    assert_eq!(outcome.targets[0].target.alias, "oagw-assigned");
    assert_eq!(registry, before, "the configured entries are untouched");
    let resolution = ProviderResolver::new(&registry)
        .resolve("primary", Uuid::from_u128(1))
        .map(|r| r.alias);
    assert_eq!(resolution.ok().as_deref(), Some("api.openai.com"));
}

#[tokio::test]
async fn every_target_of_a_group_gets_the_returned_alias() {
    let registry = registry(&json!({
        "first": openai_entry("api.openai.com"),
        "second": openai_entry("api.openai.com"),
    }));
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.rename_alias("api.openai.com", "oagw-assigned");

    let outcome = provisioner(&oagw, &authn)
        .provision(&registry)
        .await
        .expect("provisions");

    let aliases: Vec<&str> = outcome
        .targets
        .iter()
        .map(|t| t.target.alias.as_str())
        .collect();
    assert_eq!(aliases, vec!["oagw-assigned", "oagw-assigned"]);
}

#[tokio::test]
async fn a_deferred_target_keeps_its_configured_alias() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_upstream("api.openai.com", Fault::SecretNotReadable);

    let outcome = provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("deferral");

    assert_eq!(outcome.targets[0].target.alias, "api.openai.com");
}

// ---- retry of a deferred target (building block of the reconcile task) ----------------------------

#[tokio::test]
async fn retrying_a_deferred_target_provisions_it_once_the_secret_is_readable() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_upstream("api.openai.com", Fault::SecretNotReadable);
    let provisioner = provisioner(&oagw, &authn);
    let outcome = provisioner.provision(&single()).await.expect("deferral");
    let target = outcome.deferred()[0].clone();

    let still_failing = provisioner.retry(&outcome.s2s_context, &target).await;
    oagw.clear_faults();
    let recovered = provisioner.retry(&outcome.s2s_context, &target).await;

    let failure = still_failing.expect_err("the secret is still unreadable");
    assert!(!failure.cause.is_empty());
    assert!(!failure.cause.contains("AUTH-CONFIG-SENTINEL"));
    recovered.expect("provisioned");
    assert_eq!(texts(oagw.stored_routes("api.openai.com")), openai_all());
}

#[tokio::test]
async fn a_retry_that_times_out_fails_instead_of_hanging() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    oagw.fail_upstream("api.openai.com", Fault::Hang);
    let provisioner = provisioner(&oagw, &authn);
    let target = ProvisionTarget::from_registry(&single()).remove(0);

    let result = tokio::time::timeout(
        Duration::from_secs(5),
        provisioner.retry(&s2s_ctx(), &target),
    )
    .await
    .expect("every call is bounded");

    assert!(result.is_err());
}

#[tokio::test]
async fn a_retry_applies_the_idempotency_rule() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    let provisioner = provisioner(&oagw, &authn);
    let outcome = provisioner.provision(&single()).await.expect("provisions");
    let creates = oagw.route_creates().len();
    oagw.fail_route("/v1/vector_stores", Fault::Conflict);

    provisioner
        .retry(&outcome.s2s_context, &outcome.targets[0].target)
        .await
        .expect("an already provisioned target retries cleanly");

    assert_eq!(
        oagw.route_creates().len(),
        creates,
        "no route is re-created"
    );
    assert_eq!(oagw.upstream_count(), 1);
}

#[tokio::test]
async fn a_retry_reuses_the_upstream_of_a_group_member_and_skips_its_routes() {
    let registry = registry(&json!({
        "first": openai_entry("api.openai.com"),
        "second": openai_entry("api.openai.com"),
    }));
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    let provisioner = provisioner(&oagw, &authn);
    let outcome = provisioner.provision(&registry).await.expect("provisions");
    let creates = oagw.route_creates().len();

    provisioner
        .retry(&outcome.s2s_context, &outcome.targets[1].target)
        .await
        .expect("retry of the second member");

    assert_eq!(oagw.route_creates().len(), creates);
    assert_eq!(oagw.upstream_count(), 1);
}

// ---- secret masking in failure texts -------------------------------------------------------------------

#[test]
fn mask_secrets_replaces_every_auth_config_value() {
    let registry = single();
    let target = ProvisionTarget::from_registry(&registry).remove(0);

    let masked = super::mask_secrets(
        &format!("invalid secret_ref '{AUTH_SENTINEL}' for {AUTH_SENTINEL}"),
        &target,
    );

    assert!(!masked.contains("AUTH-CONFIG-SENTINEL"), "{masked}");
    assert!(masked.contains("***"), "{masked}");
}

#[test]
fn mask_secrets_leaves_a_text_without_secrets_untouched() {
    let registry = single();
    let target = ProvisionTarget::from_registry(&registry).remove(0);

    assert_eq!(
        super::mask_secrets("upstream failed", &target),
        "upstream failed"
    );
}

// ---- OAGW-derived aliases (B-L57-001 / B-L57-002) ---------------------------------------

#[tokio::test]
async fn a_hostname_endpoint_on_a_non_standard_port_provisions_with_the_derived_alias() {
    let mut entry = provider_json("localhost", "openai");
    entry["port"] = json!(18418);
    entry["use_http"] = json!(true);
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    let reg = registry(&json!({ "local": entry }));

    let outcome = provisioner(&oagw, &authn)
        .provision(&reg)
        .await
        .expect("a valid hostname config provisions");

    assert_eq!(outcome.targets[0].state, ProvisioningState::Provisioned);
    assert_eq!(outcome.targets[0].target.alias, "localhost:18418");
    assert_eq!(
        ProviderResolver::new(&reg)
            .resolve("local", Uuid::nil())
            .unwrap()
            .alias,
        "localhost:18418",
        "the resolver alias equals the alias OAGW stores"
    );
    assert_eq!(oagw.stored_routes("localhost:18418").len(), 6);
}

#[tokio::test]
async fn an_explicit_alias_on_a_hostname_endpoint_does_not_crash_provisioning() {
    let mut entry = provider_json("api.example.com", "openai");
    entry["upstream_alias"] = json!("my-own-alias");
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    let reg = registry(&json!({ "p": entry }));

    let outcome = provisioner(&oagw, &authn)
        .provision(&reg)
        .await
        .expect("provisions");

    assert_eq!(outcome.targets[0].target.alias, "api.example.com");
}

#[tokio::test]
async fn an_ip_literal_endpoint_is_provisioned_with_its_normalized_explicit_alias() {
    let mut entry = provider_json("10.0.0.9", "openai");
    entry["upstream_alias"] = json!("Internal-LLM.");
    let (oagw, authn) = (FakeOagw::new(), ok_authn());

    let outcome = provisioner(&oagw, &authn)
        .provision(&registry(&json!({ "p": entry })))
        .await
        .expect("provisions");

    assert_eq!(outcome.targets[0].target.alias, "internal-llm");
    assert!(matches!(
        &oagw.upstream_creates()[0],
        OagwCall::CreateUpstream { alias, .. } if alias == "internal-llm"
    ));
}

#[tokio::test]
async fn an_ip_literal_endpoint_without_an_alias_gets_the_derived_form() {
    let mut entry = provider_json("10.0.0.9", "openai");
    entry["port"] = json!(8080);
    entry["use_http"] = json!(true);
    let (oagw, authn) = (FakeOagw::new(), ok_authn());

    let outcome = provisioner(&oagw, &authn)
        .provision(&registry(&json!({ "p": entry })))
        .await
        .expect("provisions");

    assert_eq!(outcome.targets[0].target.alias, "10.0.0.9:8080");
}

#[tokio::test]
async fn an_existing_upstream_is_found_through_its_normalized_alias() {
    let (oagw, authn) = (FakeOagw::new(), ok_authn());
    let upstream_id = oagw.seed_upstream("API.OpenAI.com.");

    let outcome = provisioner(&oagw, &authn)
        .provision(&single())
        .await
        .expect("a case-differing stored alias is still found");

    assert_eq!(outcome.targets[0].state, ProvisioningState::Provisioned);
    assert!(
        oagw.calls().contains(&OagwCall::ListRoutes { upstream_id }),
        "the reused upstream's routes are listed"
    );
    assert_eq!(oagw.upstream_count(), 1);
}

#[tokio::test]
async fn the_fake_gateway_enforces_oagw_alias_derivation() {
    use oagw_sdk::{CreateUpstreamRequest, Endpoint, Scheme, Server};

    let (oagw, _authn) = (FakeOagw::new(), ok_authn());
    let create = |host: &str, port: u16, alias: Option<&str>| {
        let mut builder = CreateUpstreamRequest::builder(
            Server {
                endpoints: vec![Endpoint {
                    scheme: Scheme::Http,
                    host: host.to_owned(),
                    port,
                }],
            },
            oagw_sdk::HTTP_PROTOCOL_ID,
        );
        if let Some(alias) = alias {
            builder = builder.alias(alias.to_owned());
        }
        builder.build()
    };

    // A hostname with a non-standard port rejects the bare-host alias.
    assert!(
        oagw.create_upstream(s2s_ctx(), create("localhost", 18418, Some("localhost")))
            .await
            .is_err()
    );
    // ... and accepts the derived one (any case) or none.
    assert!(
        oagw.create_upstream(
            s2s_ctx(),
            create("localhost", 18418, Some("LOCALHOST:18418"))
        )
        .await
        .is_ok()
    );
    assert!(
        oagw.create_upstream(s2s_ctx(), create("example.com", 80, None))
            .await
            .is_ok()
    );
    // An IP literal needs an explicit alias.
    assert!(
        oagw.create_upstream(s2s_ctx(), create("10.0.0.1", 80, None))
            .await
            .is_err()
    );
    assert!(
        oagw.create_upstream(s2s_ctx(), create("10.0.0.1", 80, Some("ip-alias")))
            .await
            .is_ok()
    );
}

//! OAGW provisioning at gear start (DESIGN 3.2 "OAGW provisioning", 3.5 "LLM
//! Provider" object table): one upstream and one chat route per provider entry
//! and tenant override, reuse when OAGW reports the alias as taken, fail fast
//! on a deterministic misconfiguration, defer transient failures to the
//! background reconcile.
//!
//! Public API under test (see `S3-test-interface.md` section 2.4):
//! `mini_chat::infra::llm::provisioning::{provision_providers, reconcile_delay,
//! ProvisionReport, ProvisionError}` against the recording [`FakeOagw`].

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

mod chat_support;
mod common;
mod stream_support;

use std::sync::atomic::Ordering;
use std::time::Duration;

use mini_chat::infra::llm::provisioning::{ProvisionError, provision_providers, reconcile_delay};
use oagw_sdk::{HTTP_PROTOCOL_ID, HttpMethod, PathSuffixMode, Route, Scheme, Upstream};
use serde_json::json;
use stream_support::{FakeOagw, Fault, PROVIDER_ALIAS, TENANT_A, ctx_a, stream_config};

async fn provision(
    oagw: &FakeOagw,
    patch: serde_json::Value,
) -> Result<mini_chat::infra::llm::provisioning::ProvisionReport, ProvisionError> {
    let config = stream_config(&patch);
    provision_providers(oagw, &ctx_a(), &config).await
}

fn http_match(route: &Route) -> &oagw_sdk::HttpMatch {
    route.match_rules.http.as_ref().expect("an HTTP route")
}

fn upstream_by_alias(oagw: &FakeOagw, alias: &str) -> Upstream {
    oagw.stored_upstreams()
        .into_iter()
        .find(|u| u.alias == alias)
        .unwrap_or_else(|| panic!("no upstream with alias `{alias}`"))
}

fn chat_route(oagw: &FakeOagw, upstream: &Upstream) -> Route {
    oagw.stored_routes()
        .into_iter()
        .find(|r| {
            r.upstream_id == upstream.id
                && http_match(r).methods.contains(&HttpMethod::Post)
                && !http_match(r).path.contains("files")
                && !http_match(r).path.contains("vector_stores")
        })
        .expect("the chat route of the upstream")
}

#[tokio::test]
async fn one_upstream_with_the_provider_endpoint_alias_and_auth_is_registered() {
    let oagw = FakeOagw::new();
    let report = provision(&oagw, json!({}))
        .await
        .expect("provisioning succeeds");
    assert_eq!(report.provisioned, vec!["openai".to_owned()]);
    assert!(report.pending.is_empty());

    let upstream = upstream_by_alias(&oagw, PROVIDER_ALIAS);
    assert_eq!(upstream.protocol, HTTP_PROTOCOL_ID);
    assert!(upstream.enabled);
    assert_eq!(upstream.server.endpoints.len(), 1);
    let endpoint = &upstream.server.endpoints[0];
    assert_eq!(endpoint.host, "provider.test");
    assert_eq!(endpoint.port, 443, "default HTTPS port");
    assert_eq!(endpoint.scheme, Scheme::Https);

    let auth = upstream
        .auth
        .as_ref()
        .expect("credentials are injected by OAGW");
    assert_eq!(
        auth.plugin_type,
        "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1"
    );
    let config = auth.config.as_ref().expect("auth config");
    assert_eq!(config["header"], "Authorization");
    assert_eq!(config["prefix"], "Bearer ");
    assert_eq!(config["secret_ref"], "cred://openai-key");
}

#[tokio::test]
async fn use_http_and_an_explicit_port_select_a_plain_http_endpoint() {
    let oagw = FakeOagw::new();
    provision(
        &oagw,
        json!({"providers": {"openai": {"use_http": true, "port": 8080}}}),
    )
    .await
    .unwrap();
    let endpoint = &upstream_by_alias(&oagw, PROVIDER_ALIAS).server.endpoints[0];
    assert_eq!(endpoint.scheme, Scheme::Http);
    assert_eq!(endpoint.port, 8080);

    let oagw = FakeOagw::new();
    provision(&oagw, json!({"providers": {"openai": {"use_http": true}}}))
        .await
        .unwrap();
    assert_eq!(
        upstream_by_alias(&oagw, PROVIDER_ALIAS).server.endpoints[0].port,
        80,
        "plain HTTP defaults to port 80"
    );
}

#[tokio::test]
async fn the_alias_defaults_to_the_host() {
    let oagw = FakeOagw::new();
    provision(
        &oagw,
        json!({"providers": {"openai": {"upstream_alias": null}}}),
    )
    .await
    .unwrap();
    let upstream = upstream_by_alias(&oagw, "provider.test");
    assert_eq!(upstream.server.endpoints[0].host, "provider.test");
}

#[tokio::test]
async fn the_chat_route_posts_on_the_api_path() {
    let oagw = FakeOagw::new();
    provision(&oagw, json!({})).await.unwrap();
    let upstream = upstream_by_alias(&oagw, PROVIDER_ALIAS);
    let route = chat_route(&oagw, &upstream);
    let m = http_match(&route);
    assert_eq!(m.methods, vec![HttpMethod::Post]);
    assert_eq!(m.path, "/v1/responses");
    assert!(m.query_allowlist.is_empty());
    assert!(route.enabled);
}

#[tokio::test]
async fn a_model_placeholder_in_the_api_path_becomes_a_path_suffix_and_the_query_is_allowed() {
    let oagw = FakeOagw::new();
    provision(
        &oagw,
        json!({"providers": {"openai": {
            "api_path": "/openai/deployments/{model}/responses?api-version=2024-10-21"
        }}}),
    )
    .await
    .unwrap();
    let upstream = upstream_by_alias(&oagw, PROVIDER_ALIAS);
    let route = chat_route(&oagw, &upstream);
    let m = http_match(&route);
    assert_eq!(m.path, "/openai/deployments");
    assert_eq!(m.path_suffix_mode, PathSuffixMode::Append);
    assert_eq!(m.query_allowlist, vec!["api-version".to_owned()]);
}

#[tokio::test]
async fn rag_routes_are_registered_for_entries_with_a_storage_kind() {
    let oagw = FakeOagw::new();
    provision(&oagw, json!({})).await.unwrap();
    let upstream = upstream_by_alias(&oagw, PROVIDER_ALIAS);
    let routes: Vec<Route> = oagw
        .stored_routes()
        .into_iter()
        .filter(|r| r.upstream_id == upstream.id)
        .collect();
    let paths: Vec<&str> = routes.iter().map(|r| http_match(r).path.as_str()).collect();
    assert!(
        paths.iter().any(|p| p.starts_with("/v1/files")),
        "files route: {paths:?}"
    );
    assert!(
        paths.iter().any(|p| p.starts_with("/v1/vector_stores")),
        "vector store route: {paths:?}"
    );
    assert!(
        routes
            .iter()
            .any(|r| http_match(r).methods.contains(&HttpMethod::Delete)),
        "file / vector store deletion is routable"
    );
}

#[tokio::test]
async fn a_failing_rag_route_does_not_fail_provisioning() {
    let oagw = FakeOagw::new();
    oagw.fail_routes_under("/v1/files", Fault::Unavailable);
    oagw.fail_routes_under("/v1/vector_stores", Fault::Unavailable);
    let report = provision(&oagw, json!({}))
        .await
        .expect("RAG routes are best effort");
    assert_eq!(report.provisioned, vec!["openai".to_owned()]);
    let upstream = upstream_by_alias(&oagw, PROVIDER_ALIAS);
    chat_route(&oagw, &upstream);
}

#[tokio::test]
async fn an_alias_that_already_exists_is_reused() {
    let oagw = FakeOagw::new();
    let existing = oagw.preload_upstream(PROVIDER_ALIAS);
    let report = provision(&oagw, json!({}))
        .await
        .expect("reuse, not an error");
    assert_eq!(report.provisioned, vec!["openai".to_owned()]);
    assert_eq!(oagw.stored_upstreams().len(), 1, "no second upstream");
    assert!(oagw.create_upstream_calls.load(Ordering::SeqCst) >= 1);
    let route = chat_route(&oagw, &existing);
    assert_eq!(
        route.upstream_id, existing.id,
        "the route binds to the existing upstream"
    );
}

#[tokio::test]
async fn provisioning_twice_registers_nothing_twice() {
    let oagw = FakeOagw::new();
    provision(&oagw, json!({})).await.unwrap();
    let upstreams = oagw.stored_upstreams().len();
    let routes = oagw.stored_routes().len();
    provision(&oagw, json!({})).await.unwrap();
    assert_eq!(oagw.stored_upstreams().len(), upstreams);
    assert_eq!(
        oagw.stored_routes().len(),
        routes,
        "an existing route is not created again"
    );
}

#[tokio::test]
async fn a_deterministic_misconfiguration_fails_provisioning() {
    let oagw = FakeOagw::new();
    oagw.fail_upstream(PROVIDER_ALIAS, Fault::InvalidArgument);
    let err = provision(&oagw, json!({}))
        .await
        .expect_err("startup must fail");
    let ProvisionError::Misconfigured { provider, .. } = err;
    assert_eq!(provider, "openai");
    assert!(oagw.stored_routes().is_empty());
}

#[tokio::test]
async fn a_transient_failure_defers_the_entry_to_the_background_reconcile() {
    let oagw = FakeOagw::new();
    oagw.fail_upstream(PROVIDER_ALIAS, Fault::Unavailable);
    let report = provision(&oagw, json!({}))
        .await
        .expect("transient is not fatal");
    assert!(report.provisioned.is_empty());
    assert_eq!(report.pending, vec!["openai".to_owned()]);
    assert!(oagw.stored_routes().is_empty());

    // the reconcile task calls the same function again once the secret is readable
    oagw.clear_upstream_faults();
    let report = provision(&oagw, json!({})).await.unwrap();
    assert_eq!(report.provisioned, vec!["openai".to_owned()]);
    assert!(report.pending.is_empty());
}

#[tokio::test]
async fn one_pending_entry_does_not_block_the_others() {
    let oagw = FakeOagw::new();
    oagw.fail_upstream("other-alias", Fault::Unavailable);
    let report = provision(
        &oagw,
        json!({"providers": {"other": {
            "kind": "openai_responses", "host": "other.test", "upstream_alias": "other-alias",
            "api_path": "/v1/responses", "storage_kind": "openai"
        }}}),
    )
    .await
    .unwrap();
    assert_eq!(report.provisioned, vec!["openai".to_owned()]);
    assert_eq!(report.pending, vec!["other".to_owned()]);
    upstream_by_alias(&oagw, PROVIDER_ALIAS);
}

#[tokio::test]
async fn a_tenant_override_gets_its_own_upstream_and_route() {
    let oagw = FakeOagw::new();
    let mut overrides = serde_json::Map::new();
    overrides.insert(TENANT_A.to_string(), json!({"host": "tenant-host.test"}));
    provision(
        &oagw,
        json!({"providers": {"openai": {"tenant_overrides": overrides}}}),
    )
    .await
    .unwrap();
    assert_eq!(oagw.stored_upstreams().len(), 2);
    let own = upstream_by_alias(&oagw, "tenant-host.test");
    assert_eq!(own.server.endpoints[0].host, "tenant-host.test");
    chat_route(&oagw, &own);
}

#[test]
fn the_reconcile_delay_starts_at_two_seconds_doubles_and_stays_at_sixty() {
    let delays: Vec<u64> = (0..9).map(|n| reconcile_delay(n).as_secs()).collect();
    assert_eq!(delays, vec![2, 4, 8, 16, 32, 60, 60, 60, 60]);
    assert_eq!(reconcile_delay(u32::MAX), Duration::from_secs(60));
}

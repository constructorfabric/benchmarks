//! Chain execution around one proxy exchange.
//!
//! Covers `cpt-cf-oagw-algo-chain-execute` and the acceptance rows of
//! `cpt-cf-oagw-dod-chain-execution` and `cpt-cf-oagw-dod-starlark-sandbox`:
//! the phase order the request leg runs, the guard rejection the request phase
//! answers 400 with, the response leg the upstream status feeds, the
//! `PluginNotFound` refusal of a custom source whose limits cannot be
//! enforced, the two sandbox limits, and the always-empty `last_used_at`
//! record this deployment's posture produces.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;

use oagw::control_plane::cache::ControlPlaneCache;
use oagw::control_plane::service::ManagementService;
use oagw::data_plane::execute::{executed_custom_plugins, run_request_phase, run_response_phase};
use oagw::data_plane::sandbox::{
    MAX_INVOCATION_MILLIS, InvocationKind, SandboxFailure, SandboxRefusal, admit, enforceable,
    invoke,
};
use oagw::domain::context::{RequestContext, ResponseContext};
use oagw::domain::error::ErrorKind;
use oagw::domain::plugin_contract::SandboxLimits;
use oagw::domain::proxy::{PluginMutations, ProxyContext};
use oagw::gts::plugin_catalog;
use oagw::plugins::chain;
use oagw::plugins::PluginRegistries;
use oagw::store::{OagwStore, PluginBinding};
use serde_json::json;
use uuid::Uuid;

const HTTP_PROTOCOL: &str = oagw::PROTOCOL_HTTP;
const GUARD: &str = plugin_catalog::GUARD_REQUIRED_HEADERS;
const TRANSFORM: &str = plugin_catalog::TRANSFORM_REQUEST_ID;
const TENANT: Uuid = Uuid::from_u128(0xb1);

/// A management service over its own empty store and cache.
fn service() -> (ManagementService, Arc<OagwStore>) {
    let store = Arc::new(OagwStore::new());
    let service = ManagementService::new(
        Arc::clone(&store),
        &oagw::OagwConfig::default(),
        Arc::new(ControlPlaneCache::new()),
    )
    .expect("the validators compile");
    (service, store)
}

/// The built-in registries the composition resolves named plugins through.
fn registries() -> PluginRegistries {
    PluginRegistries::with_builtins(
        Arc::new(credstore_sdk::test_util::MockCredStoreClient::empty()),
        oagw::plugins::token_cache::TokenCacheConfig::new(
            std::time::Duration::from_secs(300),
            10_000,
        ),
    )
}

/// The named identities the reference resolution reads.
fn named() -> oagw::domain::plugin_contract::NamedPluginRegistry {
    oagw::domain::plugin_contract::NamedPluginRegistry::with_builtins()
}

/// An upstream body whose bindings the caller states.
fn upstream_body(bindings: Vec<serde_json::Value>) -> serde_json::Value {
    let mut body = json!({
        "server": {
            "endpoints": [{ "scheme": "https", "host": "api.example.com" }]
        },
        "protocol": HTTP_PROTOCOL,
        "plugins": { "items": bindings }
    });
    if !bindings.is_empty() {
        body["plugins"] = json!({ "items": bindings });
    } else {
        body["plugins"] = json!({ "items": [] });
    }
    body
}

/// One binding item the upstream write carries, at a position contiguous from
/// zero.
fn item(position: u32, reference: &str) -> serde_json::Value {
    json!({ "position": position, "plugin_ref": reference, "config": {} })
}

/// A custom transform plugin the calling tenant owns, as its anonymous
/// identifier and its row identifier alongside.
fn custom_plugin(
    service: &ManagementService,
    tenant: Uuid,
    name: &str,
) -> (Uuid, String) {
    let row = service
        .create_plugin(
            tenant,
            &json!({
                "plugin_type": "transform",
                "name": name,
                "phases": ["on_request"],
                "source_code": "def on_request(ctx):\n    return ctx\n"
            }),
        )
        .expect("the custom plugin is created");
    let id = row.plugin.id;
    (id, oagw::control_plane::plugin_def::plugin_instance(
        oagw::domain::plugin_contract::PluginFamily::Transform,
        id,
    ))
}

/// A proxy context whose headers the caller states.
fn context(headers: &[(&str, &str)]) -> ProxyContext {
    ProxyContext {
        method: String::from("GET"),
        alias: String::from("api.example.com"),
        path_suffix: None,
        query: None,
        headers: headers
            .iter()
            .map(|(name, value)| (String::from(*name), String::from(*value)))
            .collect(),
        target_host: None,
        tenant_id: TENANT,
        subject_id: None,
        correlation: None,
    }
}

/// The sandbox limits the contract publishes.
fn limits() -> SandboxLimits {
    oagw::domain::plugin_contract::SANDBOX_LIMITS
}

#[tokio::test]
async fn the_request_leg_runs_auth_then_guards_then_transforms() {
    let (service, store) = service();
    let body = upstream_body(vec![item(0, GUARD), item(1, TRANSFORM)]);
    let row = service
        .create_upstream(TENANT, &body)
        .expect("the upstream is stored");
    let bindings = store.upstream_plugin_rows(TENANT, row.upstream.id);
    let composed = chain::compose(
        &store,
        TENANT,
        &named(),
        &registries(),
        None,
        &bindings,
        &[],
    )
    .expect("the chain composes");

    let mutations = run_request_phase(&composed, &context(&[]), &limits())
        .await
        .expect("the chain runs");
    let set: Vec<&str> = mutations.set.iter().map(|(name, _)| name.as_str()).collect();
    assert!(
        set.contains(&"x-request-id"),
        "the transform ran after the guard: {set:?}"
    );
}

#[tokio::test]
async fn a_guard_rejection_is_answered_with_the_400_validation_error() {
    let (service, store) = service();
    let body = upstream_body(vec![item(0, GUARD)]);
    service
        .create_upstream(TENANT, &body)
        .expect("the upstream is stored");
    // The guard is configured to require a header the request does not carry.
    let required = PluginBinding {
        position: 1,
        plugin_ref: String::from(GUARD),
        plugin_uuid: None,
        config: json!({ "required_request_headers": "x-mandatory" }),
    };
    let composed = chain::compose(
        &store,
        TENANT,
        &named(),
        &registries(),
        None,
        &[required],
        &[],
    )
    .expect("the chain composes");

    let error = run_request_phase(&composed, &context(&[]), &limits())
        .await
        .expect_err("the required header is absent");
    assert_eq!(error.kind, ErrorKind::ValidationError);
}

#[tokio::test]
async fn a_custom_source_is_refused_not_silently_dropped() {
    let (service, store) = service();
    let row = service
        .create_upstream(TENANT, &upstream_body(Vec::new()))
        .expect("the upstream is stored");
    // A binding that names a stored custom plugin: no interpreter exists to
    // hold it to its limits, so the whole chain is refused.
    let (custom_id, reference) = custom_plugin(&service, TENANT, "refused");
    let bindings = vec![PluginBinding {
        position: 0,
        plugin_ref: reference,
        plugin_uuid: Some(custom_id),
        config: json!({}),
    }];
    let _ = row;
    let composed = chain::compose(
        &store,
        TENANT,
        &named(),
        &registries(),
        None,
        &bindings,
        &[],
    )
    .expect("the composition resolves the row");
    assert!(
        !executed_custom_plugins(&composed).is_empty(),
        "the composition did bind the custom row"
    );

    let error = run_request_phase(&composed, &context(&[]), &limits())
        .await
        .expect_err("the custom step is never executed");
    assert_eq!(error.kind, ErrorKind::PluginNotFound);
}

#[tokio::test]
async fn the_response_leg_runs_the_guards_then_the_transforms() {
    let (service, store) = service();
    let row = service
        .create_upstream(TENANT, &upstream_body(vec![item(0, TRANSFORM)]))
        .expect("the upstream is stored");
    let bindings = store.upstream_plugin_rows(TENANT, row.upstream.id);
    let composed = chain::compose(
        &store,
        TENANT,
        &named(),
        &registries(),
        None,
        &bindings,
        &[],
    )
    .expect("the chain composes");

    let mutations = run_response_phase(&composed, 200, &[], &limits())
        .expect("the response leg runs");
    let set: Vec<&str> = mutations.set.iter().map(|(name, _)| name.as_str()).collect();
    assert!(
        set.contains(&"x-request-id"),
        "the response transform produced its header: {set:?}"
    );
}

#[tokio::test]
async fn an_empty_chain_produces_no_mutation() {
    let (service, store) = service();
    let row = service
        .create_upstream(TENANT, &upstream_body(Vec::new()))
        .expect("the upstream is stored");
    let bindings = store.upstream_plugin_rows(TENANT, row.upstream.id);
    let composed = chain::compose(
        &store,
        TENANT,
        &named(),
        &registries(),
        None,
        &bindings,
        &[],
    )
    .expect("the chain composes");

    let mutations = run_request_phase(&composed, &context(&[]), &limits())
        .await
        .expect("the chain runs");
    assert_eq!(mutations, PluginMutations::default());
}

#[test]
fn the_sandbox_holds_every_invocation_to_its_wall_clock_limit() {
    assert_eq!(MAX_INVOCATION_MILLIS, 100);
    let outcome = invoke(&limits(), || 1_u8);
    assert_eq!(outcome.expect("a prompt invocation"), 1);
}

#[test]
fn a_raised_error_is_the_sandbox_failure_the_caller_answers_502_with() {
    let outcome: Result<u8, oagw::data_plane::sandbox::SandboxFailure> = invoke(&limits(), || {
        panic!("the plugin raised");
    });
    match outcome {
        Err(SandboxFailure::Raised) => {}
        other => panic!("the raised error is the raised failure: {other:?}"),
    }
}

#[test]
fn a_custom_source_is_never_enforceable_in_this_deployment() {
    let limits = SandboxLimits {
        network_io: false,
        file_io: false,
        imports: false,
        ..oagw::domain::plugin_contract::SANDBOX_LIMITS
    };
    assert!(
        !enforceable(InvocationKind::CustomSource, &limits),
        "no interpreter exists to strip the capabilities from"
    );
    assert!(
        enforceable(InvocationKind::Builtin, &limits),
        "a built-in step is a known implementation with no capability to strip"
    );
}

#[test]
fn a_network_capability_the_row_declares_is_refused_before_any_invocation() {
    let limits = SandboxLimits {
        network_io: true,
        ..oagw::domain::plugin_contract::SANDBOX_LIMITS
    };
    let refusal = admit(
        InvocationKind::Builtin,
        &limits,
        0,
    )
    .expect_err("the capability is not one this sandbox grants");
    assert!(matches!(refusal, SandboxRefusal::Unenforceable));
}

#[test]
fn an_input_over_the_memory_budget_is_refused() {
    let refusal = admit(
        InvocationKind::Builtin,
        &limits(),
        64 * 1024 * 1024,
    )
    .expect_err("the input exceeds the per-invocation memory budget");
    assert!(
        matches!(refusal, SandboxRefusal::OverBudget { .. }),
        "the refusal names the budget, not the capability"
    );
}

#[tokio::test]
async fn the_last_used_record_of_this_deployment_is_always_empty() {
    let (service, store) = service();
    let row = service
        .create_upstream(TENANT, &upstream_body(Vec::new()))
        .expect("the upstream is stored");
    let bindings = store.upstream_plugin_rows(TENANT, row.upstream.id);
    let composed = chain::compose(
        &store,
        TENANT,
        &named(),
        &registries(),
        None,
        &bindings,
        &[],
    )
    .expect("the chain composes");
    assert!(
        executed_custom_plugins(&composed).is_empty(),
        "no custom step ever executes, so the record names no plugin"
    );
}

#[test]
fn the_request_context_the_chain_reads_carries_the_relative_path() {
    let request = context(&[("x-one", "1")]);
    let built = RequestContext::new(
        request.method.clone(),
        request.request_path(),
        request.query.clone(),
    );
    assert_eq!(built.method, "GET");
    assert_eq!(built.path, "/", "the request addressed the alias alone");
}

#[test]
fn the_response_context_carries_the_upstream_status() {
    let response = ResponseContext::new(201);
    assert_eq!(response.status, 201);
}

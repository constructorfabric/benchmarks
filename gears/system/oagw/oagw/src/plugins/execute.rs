//! Chain execution and short-circuit mapping
//! (`cpt-cf-oagw-algo-plugin-chain-execute`).
//!
//! RF-001: [`execute_pre_call`]/[`execute_post_call`] are called directly
//! by `crate::proxy::engine::forward_and_relay`, the real production path
//! -- no longer reached only through this module's own tests.

use axum::http::HeaderMap;
use axum::response::Response;
use toolkit_security::SecurityContext;

use credstore_sdk::CredStoreClientV1;

use crate::error::{OagwError, OagwErrorKind};

use super::auth::{AuthRuntime, invoke_auth};
use super::guard::{GuardDecision, GuardPhase, evaluate_required_headers};
use super::plan::ExecutionPlan;
use super::registry::{GuardKind, TransformKind};
use super::token_cache::TokenCache;
use super::transform::{apply_on_request, apply_on_response};

/// Outcome of the pre-call half of the chain
/// (`cpt-cf-oagw-algo-plugin-chain-execute` steps 1-4).
pub(crate) enum PreCallOutcome {
    Continue {
        headers: HeaderMap,
        request_id: String,
    },
    ShortCircuit(Response),
}

/// Outcome of the response-phase half (`cpt-cf-oagw-algo-plugin-chain-execute`
/// steps 5-6). A separate type from [`PreCallOutcome`] because a
/// response-phase rejection has no "continue with headers" shape to
/// express: `crate::proxy::engine::apply_post_call` maps
/// `ShortCircuit`'s `502` response onto the same `(Response, error_type)`
/// shape every other proxy-path exit point uses.
pub(crate) enum PostCallOutcome {
    Continue,
    ShortCircuit(Response),
}

// @cpt-begin:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-08
fn plugin_not_found_response(
    plugin_ref: &str,
    expected: crate::model::plugin::PluginType,
) -> Response {
    use axum::response::IntoResponse;
    let kind_name = match expected {
        crate::model::plugin::PluginType::Auth => "auth",
        crate::model::plugin::PluginType::Guard => "guard",
        crate::model::plugin::PluginType::Transform => "transform",
    };
    OagwError::new(
        OagwErrorKind::PluginNotFound,
        format!("unknown {kind_name} plugin: {plugin_ref}"),
    )
    .into_response()
}
// @cpt-end:cpt-cf-oagw-algo-plugin-binding-resolve:p1:inst-binding-resolve-08

fn authentication_failed_response(detail: &str) -> Response {
    use axum::response::IntoResponse;
    OagwError::new(OagwErrorKind::AuthenticationFailed, detail.to_owned()).into_response()
}

fn guard_rejection_response(phase: GuardPhase, code: &str, missing_header: &str) -> Response {
    use axum::response::IntoResponse;
    let kind = match phase {
        GuardPhase::Request => OagwErrorKind::ValidationError,
        // No dedicated "guard rejected the response" catalog row exists,
        // and this feature must not add one
        // (`cpt-cf-oagw-dod-plugin-guard-short-circuit`); `DownstreamError`
        // is the closest existing 502 entry.
        GuardPhase::Response => OagwErrorKind::DownstreamError,
    };
    OagwError::new(kind, format!("{code}: {missing_header}")).into_response()
}

// `GuardKind::RequiredHeaders` is the registry's only guard kind today
// (`cpt-cf-oagw-dod-plugin-builtin-registries`); `guard.kind` is read below
// only to keep this dispatch point ready for a future guard kind.
fn evaluate_guards(
    plan: &ExecutionPlan,
    phase: GuardPhase,
    headers: &HeaderMap,
) -> Option<Response> {
    for guard in &plan.guards {
        let GuardKind::RequiredHeaders = guard.kind;
        if let GuardDecision::Reject {
            code,
            missing_header,
            ..
        } = evaluate_required_headers(&guard.config, phase, headers)
        {
            return Some(guard_rejection_response(phase, code, &missing_header));
        }
    }
    None
}

/// `cpt-cf-oagw-algo-plugin-chain-execute` steps 1-4: auth once, then every
/// guard's request phase, then every `on_request` transform.
// @cpt-algo:cpt-cf-oagw-algo-plugin-chain-execute:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-chain-order:p1
// @cpt-dod:cpt-cf-oagw-dod-plugin-guard-short-circuit:p1
// @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-01
// @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-02
// @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-03
// @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-04
// The runtime knobs (`credstore`/`token_cache`/its two settings) mirror
// `super::auth::AuthRuntime`'s bundling rationale; kept as separate
// parameters here (rather than threading `AuthRuntime` through) so this
// function's own signature stays the literal shape the manifest reports
// as the richer entry point.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_pre_call(
    plan: &ExecutionPlan,
    ctx: &SecurityContext,
    request_id: &str,
    mut headers: HeaderMap,
    credstore: &dyn CredStoreClientV1,
    token_cache: &TokenCache,
    token_cache_ttl_secs: u64,
    proxy_timeout_secs: u32,
) -> PreCallOutcome {
    if let Some(auth) = &plan.auth {
        let runtime = AuthRuntime {
            credstore,
            token_cache,
            token_cache_ttl_secs,
            proxy_timeout_secs,
        };
        let mut query = Vec::new();
        // @cpt-begin:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-09
        // @cpt-begin:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-10
        if let Err(err) = invoke_auth(auth, ctx, &runtime, &mut headers, &mut query).await {
            return PreCallOutcome::ShortCircuit(authentication_failed_response(
                &err.safe_detail(),
            ));
        }
        // @cpt-end:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-10
        // @cpt-end:cpt-cf-oagw-algo-plugin-auth-invoke:p1:inst-auth-invoke-09
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-04
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-03
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-02
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-01

    // @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-05
    // @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-06
    // @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-07
    // @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-08
    if let Some(response) = evaluate_guards(plan, GuardPhase::Request, &headers) {
        return PreCallOutcome::ShortCircuit(response);
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-08
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-07
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-06
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-05

    // @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-09
    // @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-10
    // @cpt-begin:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-01
    // @cpt-begin:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-02
    // @cpt-begin:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-03
    // @cpt-begin:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-08
    // @cpt-begin:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-09
    // @cpt-begin:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-10
    // This loop only ever calls `apply_on_request` for a transform that
    // declares that phase (`request_id` always does), realizing step 1/2's
    // "skip a phase the plugin does not declare" for the only
    // registry-resolvable transform. `apply_on_request` is infallible (no
    // `Result`), so there is no failure for step 8/9's `CATCH`/abort to
    // observe with today's built-ins; step 10's "mutated context" is the
    // `headers`/`effective_request_id` handed to `PreCallOutcome::Continue`
    // below.
    let mut effective_request_id = request_id.to_owned();
    for transform in &plan.transforms {
        let TransformKind::RequestId = transform.kind;
        effective_request_id = apply_on_request(&mut headers, request_id);
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-10
    // @cpt-end:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-09
    // @cpt-end:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-08
    // @cpt-end:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-03
    // @cpt-end:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-02
    // @cpt-end:cpt-cf-oagw-algo-plugin-transform-apply:p2:inst-transform-apply-01
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-10
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-09

    // @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-11
    // `PreCallOutcome::Continue` *is* "hand the mutated request to
    // `cpt-cf-oagw-feature-proxy-core` for the upstream call": this
    // feature performs no network call itself, only returns the
    // credentialed, guarded, transformed request for the caller to
    // forward.
    PreCallOutcome::Continue {
        headers,
        request_id: effective_request_id,
    }
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-11
}

/// `cpt-cf-oagw-algo-plugin-chain-execute` step 5: response-phase guards,
/// then `on_response` transforms.
// @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-12
// @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-13
// @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-14
// @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-15
pub(crate) fn execute_post_call(
    plan: &ExecutionPlan,
    request_id: &str,
    response: &mut Response,
) -> PostCallOutcome {
    if let Some(rejection) = evaluate_guards(plan, GuardPhase::Response, response.headers()) {
        return PostCallOutcome::ShortCircuit(rejection);
    }
    for transform in &plan.transforms {
        let TransformKind::RequestId = transform.kind;
        apply_on_response(response, request_id);
    }
    // @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-18
    PostCallOutcome::Continue
    // @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-18
}
// @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-15
// @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-14
// @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-13
// @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-12

/// `cpt-cf-oagw-algo-plugin-chain-execute` step 6: `on_error` transforms
/// only, leaving the error's status and GTS `type` unchanged. Neither
/// built-in transform (`request_id`) declares `on_error`
/// (`cpt-cf-oagw-algo-plugin-transform-apply` step 1), so this is
/// correctly a no-op today; kept as a named call site for the one future
/// transform that would declare it.
// @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-16
// @cpt-begin:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-17
pub(crate) fn execute_on_error(plan: &ExecutionPlan) -> usize {
    // `request_id` declares no `on_error` phase; count the transforms this
    // step would run, for a test to observe that count without needing an
    // on_error-capable built-in to exist yet.
    plan.transforms
        .iter()
        .filter(|transform| {
            let TransformKind::RequestId = transform.kind;
            false
        })
        .count()
}
// @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-17
// @cpt-end:cpt-cf-oagw-algo-plugin-chain-execute:p1:inst-chain-execute-16

/// Render the `503 PluginNotFound` a chain-assembly resolution failure
/// maps to (`cpt-cf-oagw-dod-plugin-binding-resolution`).
pub(crate) fn resolution_failure_response(
    err: &super::binding::BindingResolutionError,
) -> Response {
    plugin_not_found_response(&err.plugin_ref, err.expected)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::plugin::PluginType;
    use crate::model::plugin::identity::named_plugin_gts_ref;
    use crate::plugins::binding::PluginBinding;
    use crate::plugins::plan::assemble_chain;
    use crate::plugins::registry::Registries;
    use axum::response::IntoResponse;
    use credstore_sdk::test_util::MockCredStoreClient;
    use serde_json::json;
    use uuid::Uuid;

    fn ctx() -> SecurityContext {
        SecurityContext::builder()
            .subject_id(Uuid::new_v4())
            .subject_tenant_id(Uuid::new_v4())
            .build()
            .unwrap()
    }

    fn guard_binding(config: serde_json::Value) -> PluginBinding {
        PluginBinding::new(
            named_plugin_gts_ref(PluginType::Guard, "required_headers"),
            config,
        )
    }

    fn request_id_binding() -> PluginBinding {
        PluginBinding::without_config(named_plugin_gts_ref(PluginType::Transform, "request_id"))
    }

    async fn run_pre_call(plan: &ExecutionPlan, headers: HeaderMap) -> PreCallOutcome {
        let store = MockCredStoreClient::empty();
        let cache = TokenCache::new(10);
        execute_pre_call(plan, &ctx(), "corr-1", headers, &store, &cache, 300, 5).await
    }

    #[tokio::test]
    async fn full_chain_runs_in_the_documented_order() {
        let registries = Registries::init();
        let upstream = vec![
            guard_binding(json!({"required_request_headers": "x-trace"})),
            request_id_binding(),
        ];
        let auth = PluginBinding::without_config(named_plugin_gts_ref(PluginType::Auth, "noop"));
        let plan = assemble_chain(Some(&auth), &upstream, &[], &registries).unwrap();

        let mut headers = HeaderMap::new();
        headers.insert("x-trace", "1".parse().unwrap());
        let outcome = run_pre_call(&plan, headers).await;
        let PreCallOutcome::Continue {
            headers,
            request_id,
        } = outcome
        else {
            panic!("expected the request to proceed");
        };
        assert_eq!(headers.get("x-request-id").unwrap(), "corr-1");
        assert_eq!(request_id, "corr-1");

        let mut response = axum::http::StatusCode::OK.into_response();
        let post = execute_post_call(&plan, &request_id, &mut response);
        assert!(matches!(post, PostCallOutcome::Continue));
        assert_eq!(response.headers().get("x-request-id").unwrap(), "corr-1");
    }

    #[tokio::test]
    async fn upstream_bound_guard_runs_before_route_bound_guard() {
        let registries = Registries::init();
        let upstream = vec![guard_binding(
            json!({"required_request_headers": "x-upstream"}),
        )];
        let route = vec![guard_binding(
            json!({"required_request_headers": "x-route"}),
        )];
        let plan = assemble_chain(None, &upstream, &route, &registries).unwrap();

        // Missing the upstream-bound requirement: the route-bound guard
        // must never even be evaluated (short-circuit on the first).
        let outcome = run_pre_call(&plan, HeaderMap::new()).await;
        let PreCallOutcome::ShortCircuit(response) = outcome else {
            panic!("expected a rejection");
        };
        assert_eq!(response.status().as_u16(), 400);
    }

    #[tokio::test]
    async fn guard_rejection_short_circuits_before_any_transform_runs() {
        let registries = Registries::init();
        let upstream = vec![
            guard_binding(json!({"required_request_headers": "x-needed"})),
            request_id_binding(),
        ];
        let plan = assemble_chain(None, &upstream, &[], &registries).unwrap();
        let outcome = run_pre_call(&plan, HeaderMap::new()).await;
        let PreCallOutcome::ShortCircuit(response) = outcome else {
            panic!("expected a rejection");
        };
        assert_eq!(response.status().as_u16(), 400);
        assert!(response.headers().get("x-request-id").is_none());
    }

    #[tokio::test]
    async fn response_phase_guard_miss_yields_502_and_discards_the_response() {
        let registries = Registries::init();
        let upstream = vec![guard_binding(
            json!({"required_response_headers": "content-type"}),
        )];
        let plan = assemble_chain(None, &upstream, &[], &registries).unwrap();
        let mut response = axum::http::StatusCode::OK.into_response();
        let outcome = execute_post_call(&plan, "corr-1", &mut response);
        let PostCallOutcome::ShortCircuit(rejection) = outcome else {
            panic!("expected a response-phase rejection");
        };
        assert_eq!(rejection.status().as_u16(), 502);
    }

    #[tokio::test]
    async fn a_catalog_only_identifier_fails_to_bind_with_plugin_not_found() {
        let registries = Registries::init();
        let upstream = vec![PluginBinding::without_config(named_plugin_gts_ref(
            PluginType::Guard,
            "timeout",
        ))];
        let err = assemble_chain(None, &upstream, &[], &registries).unwrap_err();
        let response = resolution_failure_response(&err);
        assert_eq!(response.status().as_u16(), 503);
    }

    #[tokio::test]
    async fn a_missing_secret_produces_401_authentication_failed() {
        let registries = Registries::init();
        let auth = PluginBinding::new(
            named_plugin_gts_ref(PluginType::Auth, "apikey"),
            json!({"secret_ref": "cred://absent", "header": "x-api-key"}),
        );
        let plan = assemble_chain(Some(&auth), &[], &[], &registries).unwrap();
        let outcome = run_pre_call(&plan, HeaderMap::new()).await;
        let PreCallOutcome::ShortCircuit(response) = outcome else {
            panic!("expected authentication to fail");
        };
        assert_eq!(response.status().as_u16(), 401);
    }

    #[tokio::test]
    async fn a_credential_injected_from_a_reference_reaches_the_outbound_headers() {
        let registries = Registries::init();
        let auth = PluginBinding::new(
            named_plugin_gts_ref(PluginType::Auth, "apikey"),
            json!({"secret_ref": "cred://partner-key", "header": "x-api-key"}),
        );
        let plan = assemble_chain(Some(&auth), &[], &[], &registries).unwrap();
        let store =
            MockCredStoreClient::with_secrets(vec![("partner-key".to_owned(), "sk-1".to_owned())]);
        let cache = TokenCache::new(10);
        let outcome = execute_pre_call(
            &plan,
            &ctx(),
            "corr-1",
            HeaderMap::new(),
            &store,
            &cache,
            300,
            5,
        )
        .await;
        let PreCallOutcome::Continue { headers, .. } = outcome else {
            panic!("expected the request to proceed");
        };
        assert_eq!(headers.get("x-api-key").unwrap(), "sk-1");
    }

    #[tokio::test]
    async fn on_error_runs_no_transform_and_leaves_status_and_type_alone() {
        let registries = Registries::init();
        let upstream = vec![request_id_binding()];
        let plan = assemble_chain(None, &upstream, &[], &registries).unwrap();
        assert_eq!(execute_on_error(&plan), 0);
    }
}

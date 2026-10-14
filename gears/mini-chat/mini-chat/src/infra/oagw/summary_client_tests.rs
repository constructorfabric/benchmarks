//! The OAGW adapter of the summary transport over the mock gateway: the `POST` the gateway sees
//! (URI verbatim with its query, JSON body, `Content-Type` and `Accept` `application/json`, no
//! credential header, the S2S security context), the answer of a 2xx and of an error status (a 429
//! and a provider 5xx stay answers), the bounded reads (16 KiB of an error body, 1 MiB of a success
//! body with the over-limit flag), the gateway failures (an `Err`, a gateway-marked 502, 503 or
//! 504 Problem) and the transport failures (an unbound S2S context, a body that breaks), and that
//! the adapter makes no retry.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-background-maintenance`: algorithm
//! `cpt-cf-mini-chat-algo-background-maintenance-summary-adapter-call` (`inst-ad-post`,
//! `inst-ad-transport-failed`, `inst-ad-return-transport`, `inst-ad-non-2xx`, `inst-ad-read-error`,
//! `inst-ad-parse`, `inst-ad-no-extras`, `inst-ad-no-content-logs`); definitions of done
//! `cpt-cf-mini-chat-dod-background-maintenance-summary-execution` and
//! `cpt-cf-mini-chat-dod-background-maintenance-handler-wiring` (every provider call under the S2S
//! context, no credential in the gear).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use serde_json::json;
use tracing_test::traced_test;

use super::OagwSummaryClient;
use crate::domain::maintenance::settings::{MAX_ERROR_BODY_BYTES, MAX_SUCCESS_BODY_BYTES};
use crate::domain::maintenance::summary_call::{SummaryTransport, TransportFailure};
use crate::infra::provider_task::S2sContext;
use crate::test_support::provider::{GatewayFailure, MockProvider, Reply, Step};
use crate::test_support::send::provider_s2s;

fn client(upstream: &Arc<MockProvider>) -> OagwSummaryClient {
    OagwSummaryClient::new(upstream.clone(), S2sContext::bound(provider_s2s()))
}

const URI: &str =
    "/azure.example.com/openai/deployments/m/responses?api-version=2025-03-01-preview";

#[tokio::test]
async fn the_request_is_a_post_of_the_json_body_to_the_uri_as_given() {
    // inst-ad-post: the URI with its query verbatim, the body as JSON
    let upstream = MockProvider::answering(Reply::http(200, &json!({ "ok": true })));
    let body = json!({ "model": "m", "input": [{ "role": "user", "content": "hi" }] });
    client(&upstream)
        .post_json(URI, &body)
        .await
        .expect("an answer");
    let call = upstream.only_call();
    assert_eq!(call.method, "POST");
    assert_eq!(call.uri, URI);
    assert_eq!(call.body, body);
}

#[tokio::test]
async fn the_headers_are_json_and_carry_no_credential() {
    // inst-ad-post, constraint no-credentials: OAGW injects the credentials
    let upstream = MockProvider::answering(Reply::http(200, &json!({})));
    client(&upstream).post_json(URI, &json!({})).await.unwrap();
    let call = upstream.only_call();
    assert_eq!(call.header("content-type"), Some("application/json"));
    assert_eq!(call.header("accept"), Some("application/json"));
    for name in [
        "authorization",
        "api-key",
        "x-api-key",
        "cookie",
        "proxy-authorization",
    ] {
        assert_eq!(call.header(name), None, "{name}");
    }
}

#[tokio::test]
async fn the_call_goes_out_under_the_s2s_security_context() {
    // inst-ad-post: not under the user's context
    let upstream = MockProvider::answering(Reply::http(200, &json!({})));
    client(&upstream).post_json(URI, &json!({})).await.unwrap();
    let call = upstream.only_call();
    assert_eq!(call.subject, provider_s2s().subject_id());
    assert_eq!(call.tenant, provider_s2s().subject_tenant_id());
}

#[tokio::test]
async fn a_2xx_answer_carries_its_status_and_body() {
    // inst-ad-parse: the complete JSON response
    let body = json!({ "status": "completed", "output": [] });
    let upstream = MockProvider::answering(Reply::http(200, &body));
    let answer = client(&upstream).post_json(URI, &json!({})).await.unwrap();
    assert_eq!(answer.status, 200);
    assert!(!answer.too_large);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&answer.body).unwrap(),
        body
    );
}

#[tokio::test]
async fn an_upstream_error_status_is_an_answer_not_a_failure() {
    // inst-ad-non-2xx: 400, 429 and a provider 5xx (marked as an upstream error) are answers
    for status in [400, 413, 429, 500, 502, 503, 504] {
        let upstream =
            MockProvider::answering(Reply::http(status, &json!({ "error": { "message": "x" } })));
        let answer = client(&upstream)
            .post_json(URI, &json!({}))
            .await
            .unwrap_or_else(|failure| {
                panic!("{status}: {failure:?}");
            });
        assert_eq!(answer.status, status);
        assert!(
            !answer.body.is_empty(),
            "{status}: the error body is kept for the classification"
        );
    }
}

#[tokio::test]
async fn an_error_body_is_read_up_to_sixteen_kib() {
    // inst-ad-read-error
    let huge = "x".repeat(200 * 1024);
    let upstream = MockProvider::answering(Reply::Http {
        status: 500,
        headers: Vec::new(),
        body: huge,
        from_gateway: false,
    });
    let answer = client(&upstream).post_json(URI, &json!({})).await.unwrap();
    assert_eq!(answer.status, 500);
    assert_eq!(
        answer.body.len(),
        MAX_ERROR_BODY_BYTES,
        "read bounded, not rejected"
    );
    assert!(!answer.too_large);
}

#[tokio::test]
async fn a_success_body_up_to_one_mib_is_read_whole() {
    // inst-ad-parse
    let exact = "y".repeat(MAX_SUCCESS_BODY_BYTES);
    let upstream = MockProvider::answering(Reply::Http {
        status: 200,
        headers: Vec::new(),
        body: exact,
        from_gateway: false,
    });
    let answer = client(&upstream).post_json(URI, &json!({})).await.unwrap();
    assert!(!answer.too_large);
    assert_eq!(answer.body.len(), MAX_SUCCESS_BODY_BYTES);
}

#[tokio::test]
async fn a_success_body_over_one_mib_is_flagged_and_not_returned() {
    // inst-ad-parse, inst-ad-malformed: "the body is larger than that"
    let over = "z".repeat(MAX_SUCCESS_BODY_BYTES + 1);
    let upstream = MockProvider::answering(Reply::Http {
        status: 200,
        headers: Vec::new(),
        body: over,
        from_gateway: false,
    });
    let answer = client(&upstream).post_json(URI, &json!({})).await.unwrap();
    assert_eq!(answer.status, 200);
    assert!(answer.too_large);
    assert!(answer.body.is_empty());
}

#[tokio::test]
async fn a_gateway_error_is_a_gateway_failure() {
    // inst-ad-transport-failed, inst-ad-return-transport: an `Err` from `proxy_request`
    for failure in [
        GatewayFailure::Timeout,
        GatewayFailure::Unavailable,
        GatewayFailure::Internal,
    ] {
        let upstream = MockProvider::answering(Reply::Fail(failure));
        let result = client(&upstream).post_json(URI, &json!({})).await;
        assert_eq!(
            result.unwrap_err(),
            TransportFailure::Gateway,
            "{failure:?}"
        );
    }
}

#[tokio::test]
async fn a_gateway_problem_response_is_a_gateway_failure() {
    // inst-ad-transport-failed: the gateway's own 502, 503 or 504 Problem
    for status in [502, 503, 504] {
        let upstream = MockProvider::answering(Reply::gateway_problem(
            status,
            &json!({ "type": "about:blank", "title": "gateway", "status": status }),
        ));
        let result = client(&upstream).post_json(URI, &json!({})).await;
        assert_eq!(result.unwrap_err(), TransportFailure::Gateway, "{status}");
    }
}

#[tokio::test]
async fn an_unbound_s2s_context_is_a_transport_failure_without_a_call() {
    // inst-ad-transport-failed: no context, no request
    let upstream = MockProvider::answering(Reply::http(200, &json!({})));
    let client = OagwSummaryClient::new(upstream.clone(), S2sContext::unbound());
    let result = client.post_json(URI, &json!({})).await;
    assert_eq!(result.unwrap_err(), TransportFailure::Transport);
    assert_eq!(upstream.call_count(), 0);
}

#[tokio::test]
async fn a_body_that_breaks_is_a_transport_failure() {
    // inst-ad-transport-failed: the connection broke while the body was read
    let upstream = MockProvider::answering(Reply::Sse(vec![
        Step::Raw("{\"partial\":".to_owned()),
        Step::Break,
    ]));
    let result = client(&upstream).post_json(URI, &json!({})).await;
    assert_eq!(result.unwrap_err(), TransportFailure::Transport);
}

#[tokio::test]
async fn the_adapter_makes_one_call_and_never_retries() {
    // inst-ad-post: "the call is made without retry by this algorithm"
    for reply in [
        Reply::http(500, &json!({})),
        Reply::http(429, &json!({})),
        Reply::Fail(GatewayFailure::Unavailable),
    ] {
        let upstream = MockProvider::answering(reply);
        drop(client(&upstream).post_json(URI, &json!({})).await);
        assert_eq!(upstream.call_count(), 1);
    }
}

#[traced_test]
#[tokio::test]
async fn nothing_of_the_request_or_the_answer_is_logged() {
    // inst-ad-no-content-logs: never the request body, the prompt, the answer or the error body
    let upstream = MockProvider::answering(Reply::http(500, &json!({ "error": "ANSWER-SECRET" })));
    drop(
        client(&upstream)
            .post_json(URI, &json!({ "input": "PROMPT-SECRET" }))
            .await,
    );
    assert!(!logs_contain("PROMPT-SECRET"));
    assert!(!logs_contain("ANSWER-SECRET"));
}

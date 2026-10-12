#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The fake OAGW and the fake provider behind it: upstream and route CRUD, recorded proxy
//! requests, scripted responses and the SSE parser of the harness.

use bytes::Bytes;
use http::Request;
use mini_chat::test_support::{
    FakeOagw, FakeProvider, ScriptedResponse, TestResponse, responses_failed, responses_text_stream,
};
use oagw_sdk::{
    Body, CreateRouteRequest, CreateUpstreamRequest, Endpoint, HttpMatch, HttpMethod, ListQuery,
    MatchRules, PathSuffixMode, Scheme, Server, ServiceGatewayClientV1,
};
use serde_json::json;
use toolkit_security::SecurityContext;
use uuid::Uuid;

fn context() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(Uuid::new_v4())
        .token_scopes(vec!["*".to_owned()])
        .build()
        .unwrap()
}

fn oagw() -> (FakeOagw, FakeProvider) {
    let provider = FakeProvider::default();
    (FakeOagw::new(provider.clone()), provider)
}

fn post(path: &str, body: &serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::Bytes(Bytes::from(body.to_string())))
        .unwrap()
}

async fn body_text(response: http::Response<Body>) -> (u16, String) {
    let status = response.status().as_u16();
    let bytes = response.into_body().into_bytes().await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn upstreams_and_routes_can_be_created_read_listed_and_deleted() {
    let (oagw, _) = oagw();
    let ctx = context();
    let server = Server {
        endpoints: vec![Endpoint {
            scheme: Scheme::Https,
            host: "llm.test".to_owned(),
            port: 443,
        }],
    };

    let upstream = oagw
        .create_upstream(
            ctx.clone(),
            CreateUpstreamRequest::builder(server, "gts.protocol").build(),
        )
        .await
        .unwrap();
    assert_eq!(upstream.alias, "llm.test", "the alias defaults to the host");
    assert_eq!(upstream.tenant_id, ctx.subject_tenant_id());

    let route = oagw
        .create_route(
            ctx.clone(),
            CreateRouteRequest::builder(
                upstream.id,
                MatchRules {
                    http: Some(HttpMatch {
                        methods: vec![HttpMethod::Post],
                        path: "/v1/responses".to_owned(),
                        query_allowlist: vec![],
                        path_suffix_mode: PathSuffixMode::default(),
                    }),
                    grpc: None,
                },
            )
            .build(),
        )
        .await
        .unwrap();
    assert_eq!(route.upstream_id, upstream.id);

    assert_eq!(
        oagw.get_upstream(ctx.clone(), upstream.id)
            .await
            .unwrap()
            .id,
        upstream.id
    );
    assert_eq!(
        oagw.list_upstreams(ctx.clone(), &ListQuery::default())
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        oagw.list_routes(ctx.clone(), Some(upstream.id), &ListQuery::default())
            .await
            .unwrap()
            .len(),
        1
    );
    let (resolved, resolved_route) = oagw
        .resolve_proxy_target(ctx.clone(), "llm.test", "POST", "/v1/responses")
        .await
        .unwrap();
    assert_eq!((resolved.id, resolved_route.id), (upstream.id, route.id));

    oagw.delete_route(ctx.clone(), route.id).await.unwrap();
    oagw.delete_upstream(ctx.clone(), upstream.id)
        .await
        .unwrap();
    assert!(oagw.get_upstream(ctx.clone(), upstream.id).await.is_err());
    assert!(oagw.delete_upstream(ctx, upstream.id).await.is_err());
}

#[tokio::test]
async fn proxied_requests_are_recorded_and_chat_requests_are_told_apart() {
    let (oagw, provider) = oagw();

    oagw.proxy_request(
        context(),
        post("/llm.test/v1/responses", &json!({"model": "m"})),
    )
    .await
    .unwrap();
    oagw.proxy_request(context(), post("/llm.test/v1/files", &json!({})))
        .await
        .unwrap();

    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/llm.test/v1/responses");
    assert_eq!(requests[0].body_json, Some(json!({"model": "m"})));
    assert!(
        requests[0]
            .headers
            .iter()
            .any(|(name, _)| name == "content-type")
    );
    assert_eq!(provider.chat_requests().len(), 1);
}

#[tokio::test]
async fn scripted_responses_play_first_in_first_out_per_pattern() {
    let (oagw, provider) = oagw();
    provider.enqueue(
        "/files",
        ScriptedResponse::Json {
            status: 201,
            body: json!({"id": "file-1"}),
        },
    );
    provider.enqueue(
        "/files",
        ScriptedResponse::Error {
            status: 429,
            body: json!({"error": "slow down"}),
            headers: vec![("retry-after".to_owned(), "7".to_owned())],
        },
    );

    let first = oagw
        .proxy_request(context(), post("/h/v1/files", &json!({})))
        .await
        .unwrap();
    let second = oagw
        .proxy_request(context(), post("/h/v1/files", &json!({})))
        .await
        .unwrap();
    let default = oagw
        .proxy_request(context(), post("/h/v1/files", &json!({})))
        .await
        .unwrap();

    assert_eq!(second.headers()["retry-after"], "7");
    assert_eq!(
        body_text(first).await,
        (201, r#"{"id":"file-1"}"#.to_owned())
    );
    assert_eq!(body_text(second).await.0, 429);
    assert_eq!(
        body_text(default).await,
        (200, "{}".to_owned()),
        "nothing queued: empty JSON"
    );
}

#[tokio::test]
async fn an_unscripted_chat_request_gets_a_short_completed_stream() {
    let (oagw, _) = oagw();

    let response = oagw
        .proxy_request(context(), post("/h/v1/responses", &json!({})))
        .await
        .unwrap();

    let (status, text) = body_text(response).await;
    assert_eq!(status, 200);
    let events = TestResponse {
        status: http::StatusCode::OK,
        headers: http::HeaderMap::new(),
        body: Bytes::from(text),
    }
    .sse_events();
    let names: Vec<&str> = events.iter().map(|event| event.event.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "response.created",
            "response.output_text.delta",
            "response.completed"
        ]
    );
    assert_eq!(events[1].data["delta"], json!("Hello"));
    assert_eq!(
        events[2].data["response"]["usage"]["input_tokens"],
        json!(10)
    );
    assert_eq!(
        events[2].data["response"]["usage"]["output_tokens"],
        json!(5)
    );
}

#[tokio::test]
async fn the_response_helpers_build_completed_and_failed_streams() {
    let ScriptedResponse::Sse(frames) = responses_text_stream(&["a", "b"], 3, 4) else {
        panic!("a text stream is an SSE response");
    };
    let names: Vec<&str> = frames.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "response.created",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.completed"
        ]
    );

    let ScriptedResponse::Sse(failed) = responses_failed("boom") else {
        panic!("a failed stream is an SSE response");
    };
    assert_eq!(failed.last().unwrap().0, "response.failed");
    assert_eq!(
        failed.last().unwrap().1["response"]["error"]["message"],
        json!("boom")
    );
}

#[test]
fn the_sse_parser_reads_events_ignores_comments_and_keeps_non_json_data_as_text() {
    let response = TestResponse {
        status: http::StatusCode::OK,
        headers: http::HeaderMap::new(),
        body: Bytes::from(
            ": keep-alive\n\nevent: delta\ndata: {\"text\":\"hi\"}\n\nevent: ping\ndata: plain\n\ndata: {\"x\":1}\n\n",
        ),
    };

    let events = response.sse_events();

    assert_eq!(events.len(), 3);
    assert_eq!(
        (events[0].event.as_str(), &events[0].data),
        ("delta", &json!({"text": "hi"}))
    );
    assert_eq!(
        (events[1].event.as_str(), &events[1].data),
        ("ping", &json!("plain"))
    );
    assert_eq!(
        (events[2].event.as_str(), &events[2].data),
        ("message", &json!({"x": 1}))
    );
}

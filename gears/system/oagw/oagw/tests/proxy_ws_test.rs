// Created: 2026-08-29 by Constructor Tech
//! WebSocket upgrades relayed in both directions, and upstream refusal.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{json_body, post, tenant};
use futures_util::{SinkExt, StreamExt};
use serde_json::json;
use tokio_tungstenite::tungstenite;

const PROTOCOL: &str = "gts.cf.core.oagw.protocol.v1~cf.core.oagw.http.v1";

/// Echo WebSocket server: every text/binary frame is returned to the sender.
async fn echo_app() -> axum::Router {
    use axum::extract::ws::{WebSocket, WebSocketUpgrade};
    use axum::routing::any;

    async fn echo(socket: WebSocket) {
        let (mut sink, mut stream) = socket.split();
        while let Some(Ok(message)) = stream.next().await {
            let keep_serving = match &message {
                axum::extract::ws::Message::Text(text) => sink
                    .send(axum::extract::ws::Message::Text(
                        format!("echo:{text}").into(),
                    ))
                    .await
                    .is_ok(),
                axum::extract::ws::Message::Binary(data) => {
                    let mut echoed = data.to_vec();
                    echoed.reverse();
                    sink.send(axum::extract::ws::Message::Binary(echoed.into()))
                        .await
                        .is_ok()
                }
                axum::extract::ws::Message::Close(_) => false,
                _ => true,
            };
            if !keep_serving {
                break;
            }
        }
    }

    let upgrade = |ws: WebSocketUpgrade| async move { ws.on_upgrade(echo) };
    axum::Router::new()
        .route("/", any(upgrade))
        .route("/ws", any(upgrade))
}

/// Serve `app` on an ephemeral loopback port.
async fn serve(app: axum::Router) -> u16 {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    port
}

/// The gear's router behind a middleware that stands in for host authn.
async fn serve_gateway(harness: &common::Harness) -> u16 {
    use axum::extract::Request;
    use axum::middleware::{Next, from_fn};
    use axum::response::Response;

    async fn inject(mut request: Request, next: Next) -> Response {
        request
            .extensions_mut()
            .insert(common::security_for(tenant()));
        next.run(request).await
    }

    let app = harness.router().clone().layer(from_fn(inject));
    serve(app).await
}

async fn register(harness: &common::Harness, alias: &str, port: u16) -> String {
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": alias,
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": port } ] },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": id, "match": { "http": { "methods": ["GET"], "path": "/" } } }),
        tenant(),
    )
    .await;
    id
}

#[tokio::test]
async fn websocket_frames_are_relayed_both_ways() {
    let upstream_port = serve(echo_app().await).await;
    let harness = common::Harness::new(common::test_config(), None);
    register(&harness, "ws.example.com", upstream_port).await;
    let gateway_port = serve_gateway(&harness).await;

    let (mut client, _response) = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{gateway_port}/oagw/v1/proxy/ws.example.com/ws"
    ))
    .await
    .expect("upgrade through the gateway");
    assert_eq!(
        _response.status(),
        tungstenite::http::StatusCode::SWITCHING_PROTOCOLS
    );

    // Downstream (client → upstream → client).
    client
        .send(tungstenite::Message::text("ping-1"))
        .await
        .expect("send text");
    let echoed = tokio::time::timeout(std::time::Duration::from_secs(5), client.next())
        .await
        .expect("echo within the deadline")
        .expect("stream open")
        .expect("frame");
    assert_eq!(echoed, tungstenite::Message::text("echo:ping-1"));

    // Binary payloads survive the tunnel.
    let payload = vec![7u8, 3, 9, 42];
    client
        .send(tungstenite::Message::binary(payload.clone()))
        .await
        .expect("send binary");
    let echoed = tokio::time::timeout(std::time::Duration::from_secs(5), client.next())
        .await
        .expect("echo within the deadline")
        .expect("stream open")
        .expect("frame");
    assert_eq!(
        echoed.into_data(),
        [42u8, 9, 3, 7].to_vec(),
        "reversed by the echo upstream"
    );

    // Closing the client side ends the upstream leg too.
    client.close(None).await.expect("close");
}

#[tokio::test]
async fn a_refused_upstream_handshake_is_a_502() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);

    let harness = common::Harness::new(common::test_config(), None);
    register(&harness, "refused.example.com", port).await;
    let gateway_port = serve_gateway(&harness).await;

    let error = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{gateway_port}/oagw/v1/proxy/refused.example.com/ws"
    ))
    .await
    .expect_err("the upstream is not there");
    match error {
        tungstenite::Error::Http(response) => {
            assert_eq!(
                response.status(),
                tungstenite::http::StatusCode::BAD_GATEWAY
            );
        }
        other => panic!("expected an HTTP rejection, got {other:?}"),
    }
}

#[tokio::test]
async fn a_plain_get_to_the_proxy_path_still_relays_http() {
    let upstream_port = serve(echo_app().await).await;
    let harness = common::Harness::new(common::test_config(), None);
    register(&harness, "plain.example.com", upstream_port).await;
    let gateway_port = serve_gateway(&harness).await;

    // A plain HTTP request on the same alias is not an upgrade: the data plane
    // proxies it as HTTP, and the echo server (a WebSocket endpoint) answers
    // with its own handshake rejection.
    let mut request = axum::http::Request::builder()
        .method("GET")
        .uri(format!(
            "http://127.0.0.1:{gateway_port}/oagw/v1/proxy/plain.example.com/ws"
        ))
        .body(axum::body::Body::empty())
        .unwrap();
    request
        .extensions_mut()
        .insert(common::security_for(tenant()));
    let response =
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build_http()
            .request(request)
            .await
            .expect("response");
    assert!(
        response.status().is_client_error() || response.status().is_server_error(),
        "a plain GET is forwarded upstream, which refuses it: {}",
        response.status()
    );
}

/// A request guard that always refuses, with the status it demands.
struct RejectingHandshake;

#[async_trait::async_trait]
impl oagw::domain::plugin::GuardPlugin for RejectingHandshake {
    fn id(&self) -> &str {
        "test.ws-reject.v1"
    }

    fn plugin_type(&self) -> &str {
        "guard_plugin"
    }

    async fn guard_request(
        &self,
        _ctx: &oagw::domain::plugin::RequestContext,
    ) -> Result<oagw::domain::plugin::GuardDecision, oagw::domain::plugin::PluginError> {
        Ok(oagw::domain::plugin::GuardDecision::Reject {
            status: axum::http::StatusCode::FORBIDDEN,
            error_code: "WS_HANDSHAKE_REFUSED".to_owned(),
            message: "the handshake is not permitted".to_owned(),
        })
    }

    async fn guard_response(
        &self,
        _ctx: &oagw::domain::plugin::ResponseContext,
    ) -> Result<oagw::domain::plugin::GuardDecision, oagw::domain::plugin::PluginError> {
        Ok(oagw::domain::plugin::GuardDecision::Allow)
    }
}

#[tokio::test]
async fn a_request_guard_rejects_the_handshake_before_the_upgrade() {
    let upstream_port = serve(echo_app().await).await;
    let harness = common::Harness::with_plugins(common::test_config(), None, |registry| {
        registry.register_guard(std::sync::Arc::new(RejectingHandshake));
    });
    let created = json_body(
        post(
            harness.router(),
            "/oagw/v1/upstreams",
            json!({
                "alias": "guarded-ws.example.com",
                "protocol": PROTOCOL,
                "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": upstream_port } ] },
                "plugins": { "items": ["gts.cf.core.oagw.guard_plugin.v1~test.ws-reject.v1"] },
            }),
            tenant(),
        )
        .await,
    )
    .await;
    let id = created["id"].as_str().unwrap().to_owned();
    post(
        harness.router(),
        "/oagw/v1/routes",
        json!({ "upstream_id": id, "match": { "http": { "methods": ["GET"], "path": "/" } } }),
        tenant(),
    )
    .await;
    let gateway_port = serve_gateway(&harness).await;

    let error = tokio_tungstenite::connect_async(format!(
        "ws://127.0.0.1:{gateway_port}/oagw/v1/proxy/guarded-ws.example.com/ws"
    ))
    .await
    .expect_err("the guard refuses the handshake");
    match error {
        tungstenite::Error::Http(response) => {
            assert_eq!(
                response.status(),
                tungstenite::http::StatusCode::FORBIDDEN,
                "the guard's status is what the caller sees"
            );
        }
        other => panic!("expected an HTTP rejection, got {other:?}"),
    }
}

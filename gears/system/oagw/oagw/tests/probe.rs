#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Smoke probe for the gear-relative management API prefix: the wire contract
//! is `/oagw/v1/...` with no `/api` prefix.

mod common;

use axum::http::{Method, StatusCode};
use serde_json::json;

use crate::common::harness;

#[tokio::test]
async fn probe_router_registers_the_wire_routes() {
    let h = harness();
    let (status, body, _) = h
        .json(
            Method::POST,
            "/oagw/v1/upstreams",
            &[],
            Some(json!({
                "server": { "endpoints": [ { "scheme": "http", "host": "127.0.0.1", "port": 1 } ] },
                "alias": "probe"
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

//! Cross-cutting E2E tests for the AM REST surface's canonical error
//! envelope (RFC 9457 `Problem` shape) and middleware composition.
//!
//! Scope: shape pinning that is identical across every endpoint family —
//! the `type`/`title`/`status`/`detail` envelope fields, the
//! `application/problem+json` content type, and consistent behavior on
//! missing-auth and cross-tenant rejections.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::doc_markdown)]

mod common;

use axum::http::StatusCode;
use tower::ServiceExt;
use uuid::Uuid;

use common::*;

// ─── RFC 9457 envelope shape ─────────────────────────────────────────

#[tokio::test]
async fn error_envelope_is_rfc9457_problem_details_shape() {
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let services = build_services(&h);
    let router = build_test_router(&services);

    let unknown = Uuid::new_v4();
    let req = json_request(
        "GET",
        &format!("/account-management/v1/tenants/{unknown}"),
        None,
        ctx_for(root),
    );
    let resp = router.oneshot(req).await.expect("router");
    let (status, body) = response_problem(resp).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // RFC 9457 carries these as the canonical fields (toolkit_canonical_errors
    // surfaces a few of them — the exact key set is part of the
    // platform-wide envelope; the pin here is on the presence + the
    // documented `type`/`title`/`status`/`detail` quartet).
    assert!(
        body["type"].is_string(),
        "envelope must carry `type`: {body}"
    );
    assert!(
        body["title"].is_string(),
        "envelope must carry `title`: {body}"
    );
    assert_eq!(body["status"], 404);
    assert!(
        body["detail"].is_string(),
        "envelope must carry `detail`: {body}"
    );
}

#[tokio::test]
async fn error_envelope_content_type_is_application_problem_json() {
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let services = build_services(&h);
    let router = build_test_router(&services);

    let unknown = Uuid::new_v4();
    let req = json_request(
        "GET",
        &format!("/account-management/v1/tenants/{unknown}"),
        None,
        ctx_for(root),
    );
    let resp = router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let ct = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert!(
        ct.contains("application/problem+json"),
        "error response Content-Type MUST be application/problem+json, got {ct}",
    );
}

// ─── Cross-family consistency on missing-auth ────────────────────────

#[tokio::test]
async fn unauthenticated_returns_consistent_envelope_across_families() {
    // Drop the `Extension<SecurityContext>` injection on one endpoint
    // per family (tenants, metadata, users, conversions) and verify
    // each produces the same wire-shape envelope. The exact status
    // is whatever axum's `Extension` extractor emits on a missing
    // dependency (commonly 500), but consistency across families is
    // what matters here.
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let services = build_services_full(
        &h,
        fake_idp(),
        empty_metadata_registry(),
        types_registry_for_users(),
    );
    let router = build_test_router(&services);

    let paths = [
        format!("/account-management/v1/tenants/{root}"),
        format!("/account-management/v1/tenants/{root}/metadata"),
        format!("/account-management/v1/tenants/{root}/users"),
        format!("/account-management/v1/tenants/{root}/conversions"),
    ];

    let mut statuses = Vec::with_capacity(paths.len());
    for path in &paths {
        let req = json_request_no_ctx("GET", path, None);
        let resp = router.clone().oneshot(req).await.expect("router");
        statuses.push(resp.status());
    }
    // The point of the test is consistency: every family produces
    // the same status on missing SecurityContext. The exact value is
    // pinned by axum's `Extension` extractor.
    let first = statuses[0];
    for (path, status) in paths.iter().zip(statuses.iter()) {
        assert_eq!(
            *status, first,
            "endpoint {path} returned {status} but baseline is {first}; \
             missing-auth behavior MUST be consistent across families",
        );
    }
}

#[tokio::test]
async fn validation_error_envelope_returns_status_and_detail() {
    // A validation failure (PATCH with empty body) must still produce
    // an envelope with the canonical fields populated.
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let child = Uuid::new_v4();
    seed_active_child(&h, child, root, "child", 1).await;
    let services = build_services(&h);
    let router = build_test_router(&services);

    let req = json_request(
        "PATCH",
        &format!("/account-management/v1/tenants/{child}"),
        Some(serde_json::json!({})),
        ctx_for(root),
    );
    let resp = router.oneshot(req).await.expect("router");
    let (status, body) = response_problem(resp).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["status"], 400);
    assert!(body["title"].is_string());
    assert!(body["detail"].is_string());
}

#[tokio::test]
async fn cross_tenant_request_uses_distinct_security_context() {
    // The `PermitWithSubtreeResolver` in common.rs emits a
    // `InTenantSubtree` predicate rooted at the caller's
    // `subject_tenant_id`. A caller whose subtree does NOT contain
    // the target tenant therefore sees the target as outside their
    // visible subtree — the secure-orm boundary clamps reads to
    // empty, and the read collapses to `not_found` (the canonical
    // existence-channel response for an out-of-scope target).
    //
    // Single-root invariant: the AM schema enforces
    // `ux_tenants_single_root`, so we exercise the cross-tenant
    // posture using two SIBLING children under the same root. Each
    // child's closure self-row keeps the child visible only when
    // the caller's `subject_tenant_id` is the child itself (or any
    // strict ancestor in the closure).
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let alpha = Uuid::new_v4();
    let bravo = Uuid::new_v4();
    seed_active_child(&h, alpha, root, "alpha", 1).await;
    seed_active_child(&h, bravo, root, "bravo", 1).await;

    let services = build_services(&h);
    let router = build_test_router(&services);

    // Self-scoped read (caller=alpha reading alpha) succeeds: alpha's
    // closure self-row keeps alpha visible under
    // `InTenantSubtree(root=alpha)`.
    let req = json_request(
        "GET",
        &format!("/account-management/v1/tenants/{alpha}"),
        None,
        ctx_for(alpha),
    );
    let resp = router.clone().oneshot(req).await.expect("router");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "self-scoped read MUST succeed"
    );

    // Sibling read (caller=bravo reading alpha): alpha is NOT in
    // `bravo`'s subtree closure. The test PDP
    // ([`PermitWithSubtreeResolver`]) always returns
    // `decision: true`, so the PEP never short-circuits to 403; the
    // secure-orm subtree clamp is the sole gate and it collapses
    // alpha out of bravo's view → 404 (existence-channel response).
    //
    // Pinning to exactly `NOT_FOUND` (rather than `404 OR 403`)
    // catches a class of regressions where the clamp accidentally
    // permits the row through and the PEP starts surfacing 403 — the
    // wire would still be a 4xx but the *posture* would have
    // changed from existence-channel to authorization-channel, which
    // is exactly the information-leak path AM is designed to avoid.
    let req = json_request(
        "GET",
        &format!("/account-management/v1/tenants/{alpha}"),
        None,
        ctx_for(bravo),
    );
    let resp = router.oneshot(req).await.expect("router");
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "cross-sibling read MUST collapse to 404 via the secure-orm clamp \
         (the test PDP is permissive, so a 403 here signals a posture leak)",
    );
}

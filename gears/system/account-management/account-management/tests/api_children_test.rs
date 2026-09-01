//! HTTP-level E2E tests for the
//! `/account-management/v1/tenants/{tenant_id}/children` sub-resource.
//!
//! Pins the OData parsing seam (filter, orderby, limit clamp) flowing
//! through the real router into the service-side `list_children`
//! repository call.

#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
#![cfg_attr(coverage_nightly, coverage(off))]
#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::doc_markdown
)]

mod common;

use axum::http::StatusCode;
use tower::ServiceExt;
use uuid::Uuid;

use common::*;

// ─── Happy path ──────────────────────────────────────────────────────

#[tokio::test]
async fn list_children_returns_200_with_page() {
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let c1 = Uuid::new_v4();
    let c2 = Uuid::new_v4();
    let c3 = Uuid::new_v4();
    seed_active_child(&h, c1, root, "alpha", 1).await;
    seed_active_child(&h, c2, root, "bravo", 1).await;
    seed_active_child(&h, c3, root, "charlie", 1).await;

    let services = build_services(&h);
    let router = build_test_router(&services);

    let req = json_request(
        "GET",
        &format!("/account-management/v1/tenants/{root}/children"),
        None,
        ctx_for(root),
    );
    let resp = router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_body(resp).await;
    let items = body["items"].as_array().expect("items array");
    assert_eq!(items.len(), 3, "expected 3 children, got body={body}");
    let names: Vec<&str> = items
        .iter()
        .map(|m| m["name"].as_str().expect("name"))
        .collect();
    assert!(names.contains(&"alpha"));
    assert!(names.contains(&"bravo"));
    assert!(names.contains(&"charlie"));
}

// ─── Clamp / pagination ──────────────────────────────────────────────

#[tokio::test]
async fn list_children_clamps_top_to_service_max() {
    // The handler runs `clamp_listing_top(query, max_list_children_top())`
    // before the service call. With the default config (`max_top=200`)
    // a request that asks for half a million rows must be silently
    // clamped to 200, surfaced through the `page_info.limit` field on
    // the response.
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let services = build_services(&h);
    let max_top = services.tenant_service.max_list_children_top();
    let router = build_test_router(&services);

    let req = json_request(
        "GET",
        &format!("/account-management/v1/tenants/{root}/children?$top=999999"),
        None,
        ctx_for(root),
    );
    let resp = router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_body(resp).await;
    let limit = body["page_info"]["limit"]
        .as_u64()
        .expect("page_info.limit must be present and numeric");
    assert_eq!(
        u32::try_from(limit).expect("limit fits in u32"),
        max_top,
        "handler-side clamp must rewrite oversize $top to the service max",
    );
}

// ─── Filter / orderby ────────────────────────────────────────────────

#[tokio::test]
async fn list_children_filter_by_status_active() {
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let active = Uuid::new_v4();
    let suspended = Uuid::new_v4();
    seed_active_child(&h, active, root, "active", 1).await;
    insert_tenant(
        &h.provider,
        suspended,
        Some(root),
        "suspended",
        SUSPENDED,
        false,
        1,
    )
    .await
    .expect("seed suspended child");
    insert_closure(&h.provider, suspended, suspended, 0, SUSPENDED)
        .await
        .expect("seed suspended self-row");
    insert_closure(&h.provider, root, suspended, 0, SUSPENDED)
        .await
        .expect("seed (root, suspended) closure");

    let services = build_services(&h);
    let router = build_test_router(&services);

    let req = json_request(
        "GET",
        &format!(
            "/account-management/v1/tenants/{root}/children?%24filter=status%20eq%20%27active%27"
        ),
        None,
        ctx_for(root),
    );
    let resp = router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_body(resp).await;
    let items = body["items"].as_array().expect("items array");
    // The filter narrows to active only; soft-deleted are excluded by
    // default anyway, suspended is filtered out by the explicit
    // `eq 'active'` (string contract — storage SMALLINT is impl-side).
    let statuses: Vec<&str> = items
        .iter()
        .map(|m| m["status"].as_str().expect("status"))
        .collect();
    assert!(
        statuses.iter().all(|s| *s == "active"),
        "filter=status eq 'active' must only surface active rows, got {statuses:?}",
    );
}

#[tokio::test]
async fn list_children_filter_by_status_deleted_surfaces_soft_deleted() {
    // The default list_children call hides `status=deleted` rows; the
    // `?$filter=status eq 'deleted'` opt-in surfaces them.
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let deleted = Uuid::new_v4();
    insert_tenant(&h.provider, deleted, Some(root), "gone", DELETED, false, 1)
        .await
        .expect("seed deleted child");
    insert_closure(&h.provider, deleted, deleted, 0, DELETED)
        .await
        .expect("seed deleted self-row");
    insert_closure(&h.provider, root, deleted, 0, DELETED)
        .await
        .expect("seed (root, deleted) closure");

    let services = build_services(&h);
    let router = build_test_router(&services);

    // Default list (no filter): deleted row is hidden.
    let req = json_request(
        "GET",
        &format!("/account-management/v1/tenants/{root}/children"),
        None,
        ctx_for(root),
    );
    let resp = router.clone().oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_body(resp).await;
    let items = body["items"].as_array().expect("items");
    assert!(
        items.is_empty(),
        "default listing hides soft-deleted rows: {body}",
    );

    // Opt-in surface for deleted rows: `?$filter=status eq 'deleted'`.
    let req = json_request(
        "GET",
        &format!(
            "/account-management/v1/tenants/{root}/children?%24filter=status%20eq%20%27deleted%27"
        ),
        None,
        ctx_for(root),
    );
    let resp = router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_body(resp).await;
    let items = body["items"].as_array().expect("items");
    assert_eq!(
        items.len(),
        1,
        "deleted filter must surface the row: {body}"
    );
    assert_eq!(items[0]["status"], "deleted");
}

#[tokio::test]
async fn list_children_orderby_created_at_descending() {
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let c1 = Uuid::new_v4();
    let c2 = Uuid::new_v4();
    seed_active_child(&h, c1, root, "first", 1).await;
    // Force a non-trivial gap between created_at stamps by sleeping
    // briefly. `OffsetDateTime::now_utc()` is high-resolution on
    // modern systems but two same-microsecond inserts would defeat
    // the orderby assertion. Sleep is bounded; no risk of flakiness.
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    seed_active_child(&h, c2, root, "second", 1).await;

    let services = build_services(&h);
    let router = build_test_router(&services);

    let req = json_request(
        "GET",
        &format!("/account-management/v1/tenants/{root}/children?%24orderby=created_at%20desc"),
        None,
        ctx_for(root),
    );
    let resp = router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = response_body(resp).await;
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    // Descending: latest first.
    assert_eq!(
        items[0]["id"],
        c2.to_string(),
        "orderby=created_at desc must surface the newest row first: {body}"
    );
    assert_eq!(items[1]["id"], c1.to_string());
}

// ─── Validation ──────────────────────────────────────────────────────

#[tokio::test]
async fn list_children_invalid_filter_syntax_returns_400() {
    let h = setup_sqlite().await.expect("sqlite");
    let root = Uuid::new_v4();
    seed_root(&h, root).await;
    let services = build_services(&h);
    let router = build_test_router(&services);

    let req = json_request(
        "GET",
        &format!("/account-management/v1/tenants/{root}/children?%24filter=garbage"),
        None,
        ctx_for(root),
    );
    let resp = router.oneshot(req).await.expect("router");
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

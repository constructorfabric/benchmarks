//! Unit tests for [`FakeConversionRepo`].
//!
//! These tests pin the production semantics the fake mirrors:
//!
//! * `insert_pending` happy path.
//! * `insert_pending` rejects a second pending row per tenant
//!   ([`DomainError::PendingExists`] with the existing request id).
//! * `insert_pending` succeeds when the prior row is resolved
//!   (approved / cancelled / rejected / expired).
//! * `transition_pending_to_*` flips column values on `pending` rows
//!   and returns [`DomainError::AlreadyResolved`] on already-resolved
//!   rows. Threads the optional `*_comment` audit payload.
//! * `query_expired` filters by `pending`, `expires_at <= cutoff`, and
//!   excludes soft-deleted rows.
//! * `soft_delete_resolved_older_than` stamps `deleted_at` on resolved
//!   rows only.
//! * Listings: `(requested_at DESC, id ASC)` ordering, `OData` `$top`
//!   page cap, best-effort `status eq <i16>` filter.

use time::{Duration, OffsetDateTime};
use toolkit_odata::ODataQuery;
use toolkit_odata::ast::{CompareOperator, Expr, Value as OdataValue};
use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::conversion::model::{
    ConversionRequest, ConversionSide, ConversionStatus, NewConversionRequest, TargetMode,
};
use crate::domain::conversion::repo::ConversionRepo;
use crate::domain::conversion::test_support::repo::FakeConversionRepo;
use crate::domain::error::DomainError;

const APPROVER: u128 = 0xA1;
const CANCELLER: u128 = 0xC1;
const REJECTOR: u128 = 0xE1;
const REQUESTER: u128 = 0xF1;

fn scope() -> AccessScope {
    AccessScope::allow_all()
}

/// Build an [`ODataQuery`] with the given `$top` page cap and no filter
/// / cursor / orderby. Mirrors the production call shape — REST handlers
/// hand the service an `ODataQuery` parsed off `?$top` / `?$filter`
/// query-string params.
fn default_query(limit: u64) -> ODataQuery {
    ODataQuery::default().with_limit(limit)
}

/// Build an [`ODataQuery`] with a flat `status eq <code>` predicate and
/// the given `$top` page cap. Constructed via the AST hand-builder so
/// the test stays insulated from grammar changes in the `OData` parser
/// (mirrors the pattern used in tenant service tests).
fn status_eq_query(code: i64, limit: u64) -> ODataQuery {
    let expr = Expr::Compare(
        Box::new(Expr::Identifier("status".to_owned())),
        CompareOperator::Eq,
        Box::new(Expr::Value(OdataValue::Number(code.into()))),
    );
    ODataQuery::default().with_filter(expr).with_limit(limit)
}

fn fixed_now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("epoch")
}

/// Build a `NewConversionRequest` with deterministic ids so tests can
/// assert on the surfaced `existing_request_id` without grabbing a live
/// timestamp.
fn new_pending(
    request_id_marker: u128,
    tenant_id_marker: u128,
    parent_id_marker: u128,
) -> NewConversionRequest {
    NewConversionRequest {
        id: Uuid::from_u128(request_id_marker),
        tenant_id: Uuid::from_u128(tenant_id_marker),
        parent_id: Some(Uuid::from_u128(parent_id_marker)),
        child_tenant_name: format!("child-{tenant_id_marker}"),
        initiator_side: ConversionSide::Child,
        target_mode: TargetMode::SelfManaged,
        requested_by: Uuid::from_u128(REQUESTER),
        requested_at: fixed_now(),
        expires_at: fixed_now() + Duration::days(7),
        requested_comment: None,
    }
}

/// Build a fully-resolved `ConversionRequest` row for seeding the fake
/// — matches the column shape produced by the SQL impl after a
/// terminal transition. Audit-comment columns default to `None`; tests
/// that need to assert the per-transition stamp construct rows via the
/// repo API directly.
fn seeded_resolved(
    request_id: Uuid,
    tenant_id: Uuid,
    parent_id: Option<Uuid>,
    status: ConversionStatus,
    requested_at: OffsetDateTime,
    resolved_at: OffsetDateTime,
) -> ConversionRequest {
    let approved_by =
        matches!(status, ConversionStatus::Approved).then_some(Uuid::from_u128(APPROVER));
    let cancelled_by =
        matches!(status, ConversionStatus::Cancelled).then_some(Uuid::from_u128(CANCELLER));
    let rejected_by =
        matches!(status, ConversionStatus::Rejected).then_some(Uuid::from_u128(REJECTOR));
    ConversionRequest {
        id: request_id,
        tenant_id,
        parent_id,
        child_tenant_name: format!("child-{tenant_id}"),
        initiator_side: ConversionSide::Child,
        target_mode: TargetMode::SelfManaged,
        status,
        requested_by: Uuid::from_u128(REQUESTER),
        approved_by,
        cancelled_by,
        rejected_by,
        requested_at,
        resolved_at: Some(resolved_at),
        expires_at: requested_at + Duration::days(7),
        deleted_at: None,
        requested_comment: None,
        approved_comment: None,
        cancelled_comment: None,
        rejected_comment: None,
    }
}

/// Build a `Pending` row directly (skip the repo round-trip) for
/// listing / expiry / retention fixtures that need a row with a
/// specific `expires_at` / `requested_at` shape.
fn seeded_pending(
    request_id: Uuid,
    tenant_id: Uuid,
    parent_id: Option<Uuid>,
    requested_at: OffsetDateTime,
    expires_at: OffsetDateTime,
) -> ConversionRequest {
    ConversionRequest {
        id: request_id,
        tenant_id,
        parent_id,
        child_tenant_name: format!("child-{tenant_id}"),
        initiator_side: ConversionSide::Child,
        target_mode: TargetMode::SelfManaged,
        status: ConversionStatus::Pending,
        requested_by: Uuid::from_u128(REQUESTER),
        approved_by: None,
        cancelled_by: None,
        rejected_by: None,
        requested_at,
        resolved_at: None,
        expires_at,
        deleted_at: None,
        requested_comment: None,
        approved_comment: None,
        cancelled_comment: None,
        rejected_comment: None,
    }
}

#[tokio::test]
async fn insert_pending_succeeds_on_clean_tenant() {
    let repo = FakeConversionRepo::new();
    let new = new_pending(0xAA, 0x11, 0x22);
    let row = repo.insert_pending(&scope(), &new).await.expect("insert");
    assert_eq!(row.id, new.id);
    assert_eq!(row.tenant_id, new.tenant_id);
    assert_eq!(row.parent_id, new.parent_id);
    assert!(matches!(row.status, ConversionStatus::Pending));
    assert!(row.approved_by.is_none());
    assert!(row.cancelled_by.is_none());
    assert!(row.rejected_by.is_none());
    assert!(row.resolved_at.is_none());
    assert!(row.deleted_at.is_none());
    assert!(row.requested_comment.is_none());
}

#[tokio::test]
async fn insert_pending_persists_requested_comment_when_supplied() {
    let repo = FakeConversionRepo::new();
    let mut new = new_pending(0xAA, 0x11, 0x22);
    new.requested_comment = Some("audit rationale".to_owned());
    let row = repo.insert_pending(&scope(), &new).await.expect("insert");
    assert_eq!(row.requested_comment.as_deref(), Some("audit rationale"));
}

#[tokio::test]
async fn insert_pending_returns_pending_exists_when_pending_row_exists() {
    let repo = FakeConversionRepo::new();
    let first = new_pending(0xAA, 0x11, 0x22);
    repo.insert_pending(&scope(), &first)
        .await
        .expect("first insert");
    // Same tenant, fresh request_id — should collide on the partial-
    // unique-index mirror and surface the existing pending request id.
    let second = new_pending(0xBB, 0x11, 0x22);
    let err = repo
        .insert_pending(&scope(), &second)
        .await
        .expect_err("second insert must fail");
    match err {
        DomainError::PendingExists { request_id } => {
            assert_eq!(request_id, first.id.to_string());
        }
        other => panic!("expected PendingExists, got {other:?}"),
    }
}

#[tokio::test]
async fn insert_pending_succeeds_when_prior_row_is_approved() {
    let repo = FakeConversionRepo::with_seed(vec![seeded_resolved(
        Uuid::from_u128(0xAA),
        Uuid::from_u128(0x11),
        Some(Uuid::from_u128(0x22)),
        ConversionStatus::Approved,
        fixed_now(),
        fixed_now() + Duration::minutes(1),
    )]);
    let next = new_pending(0xBB, 0x11, 0x22);
    let row = repo.insert_pending(&scope(), &next).await.expect("insert");
    assert!(matches!(row.status, ConversionStatus::Pending));
}

#[tokio::test]
async fn insert_pending_succeeds_when_prior_row_is_cancelled() {
    let repo = FakeConversionRepo::with_seed(vec![seeded_resolved(
        Uuid::from_u128(0xAA),
        Uuid::from_u128(0x11),
        Some(Uuid::from_u128(0x22)),
        ConversionStatus::Cancelled,
        fixed_now(),
        fixed_now() + Duration::minutes(1),
    )]);
    let next = new_pending(0xBB, 0x11, 0x22);
    let row = repo.insert_pending(&scope(), &next).await.expect("insert");
    assert!(matches!(row.status, ConversionStatus::Pending));
}

#[tokio::test]
async fn insert_pending_succeeds_when_prior_row_is_rejected() {
    let repo = FakeConversionRepo::with_seed(vec![seeded_resolved(
        Uuid::from_u128(0xAA),
        Uuid::from_u128(0x11),
        Some(Uuid::from_u128(0x22)),
        ConversionStatus::Rejected,
        fixed_now(),
        fixed_now() + Duration::minutes(1),
    )]);
    let next = new_pending(0xBB, 0x11, 0x22);
    let row = repo.insert_pending(&scope(), &next).await.expect("insert");
    assert!(matches!(row.status, ConversionStatus::Pending));
}

#[tokio::test]
async fn insert_pending_succeeds_when_prior_row_is_expired() {
    let repo = FakeConversionRepo::with_seed(vec![seeded_resolved(
        Uuid::from_u128(0xAA),
        Uuid::from_u128(0x11),
        Some(Uuid::from_u128(0x22)),
        ConversionStatus::Expired,
        fixed_now(),
        fixed_now() + Duration::minutes(1),
    )]);
    let next = new_pending(0xBB, 0x11, 0x22);
    let row = repo.insert_pending(&scope(), &next).await.expect("insert");
    assert!(matches!(row.status, ConversionStatus::Pending));
}

#[tokio::test]
async fn __transition_pending_to_approved_test_only_sets_approver_and_resolved_at() {
    let repo = FakeConversionRepo::new();
    let new = new_pending(0xAA, 0x11, 0x22);
    let inserted = repo.insert_pending(&scope(), &new).await.expect("insert");
    let approver = Uuid::from_u128(APPROVER);
    let when = fixed_now() + Duration::minutes(5);
    let updated = repo
        .__transition_pending_to_approved_test_only(&scope(), inserted.id, approver, when, None)
        .await
        .expect("approve");
    assert!(matches!(updated.status, ConversionStatus::Approved));
    assert_eq!(updated.approved_by, Some(approver));
    assert_eq!(updated.resolved_at, Some(when));
    assert!(updated.approved_comment.is_none());
    // The derived pending index must release the slot so a new
    // pending request can be inserted for the same tenant afterwards.
    assert_eq!(repo.pending_request_id_for(new.tenant_id), None);
}

#[tokio::test]
async fn __transition_pending_to_approved_test_only_stamps_approved_comment_when_supplied() {
    let repo = FakeConversionRepo::new();
    let new = new_pending(0xAA, 0x11, 0x22);
    let inserted = repo.insert_pending(&scope(), &new).await.expect("insert");
    let updated = repo
        .__transition_pending_to_approved_test_only(
            &scope(),
            inserted.id,
            Uuid::from_u128(APPROVER),
            fixed_now() + Duration::minutes(5),
            Some("looks good".to_owned()),
        )
        .await
        .expect("approve");
    assert_eq!(updated.approved_comment.as_deref(), Some("looks good"));
}

#[tokio::test]
async fn transition_pending_to_cancelled_stamps_cancelled_comment_when_supplied() {
    let repo = FakeConversionRepo::new();
    let new = new_pending(0xAA, 0x11, 0x22);
    let inserted = repo.insert_pending(&scope(), &new).await.expect("insert");
    let updated = repo
        .transition_pending_to_cancelled(
            &scope(),
            inserted.id,
            Uuid::from_u128(CANCELLER),
            fixed_now() + Duration::minutes(5),
            Some("changed mind".to_owned()),
        )
        .await
        .expect("cancel");
    assert_eq!(updated.cancelled_comment.as_deref(), Some("changed mind"));
}

#[tokio::test]
async fn transition_pending_to_rejected_stamps_rejected_comment_when_supplied() {
    let repo = FakeConversionRepo::new();
    let new = new_pending(0xAA, 0x11, 0x22);
    let inserted = repo.insert_pending(&scope(), &new).await.expect("insert");
    let updated = repo
        .transition_pending_to_rejected(
            &scope(),
            inserted.id,
            Uuid::from_u128(REJECTOR),
            fixed_now() + Duration::minutes(5),
            Some("not approved".to_owned()),
        )
        .await
        .expect("reject");
    assert_eq!(updated.rejected_comment.as_deref(), Some("not approved"));
}

#[tokio::test]
async fn transition_pending_to_cancelled_on_approved_returns_already_resolved() {
    let repo = FakeConversionRepo::new();
    let new = new_pending(0xAA, 0x11, 0x22);
    let inserted = repo.insert_pending(&scope(), &new).await.expect("insert");
    let approver = Uuid::from_u128(APPROVER);
    let when = fixed_now() + Duration::minutes(5);
    repo.__transition_pending_to_approved_test_only(&scope(), inserted.id, approver, when, None)
        .await
        .expect("approve");
    let err = repo
        .transition_pending_to_cancelled(
            &scope(),
            inserted.id,
            Uuid::from_u128(CANCELLER),
            when + Duration::minutes(1),
            None,
        )
        .await
        .expect_err("cancel-after-approve must fail");
    assert!(matches!(err, DomainError::AlreadyResolved), "got {err:?}");
}

#[tokio::test]
async fn transition_pending_to_rejected_on_approved_returns_already_resolved() {
    let repo = FakeConversionRepo::new();
    let new = new_pending(0xAA, 0x11, 0x22);
    let inserted = repo.insert_pending(&scope(), &new).await.expect("insert");
    let approver = Uuid::from_u128(APPROVER);
    let when = fixed_now() + Duration::minutes(5);
    repo.__transition_pending_to_approved_test_only(&scope(), inserted.id, approver, when, None)
        .await
        .expect("approve");
    let err = repo
        .transition_pending_to_rejected(
            &scope(),
            inserted.id,
            Uuid::from_u128(REJECTOR),
            when + Duration::minutes(1),
            None,
        )
        .await
        .expect_err("reject-after-approve must fail");
    assert!(matches!(err, DomainError::AlreadyResolved), "got {err:?}");
}

#[tokio::test]
async fn transition_pending_to_expired_on_cancelled_returns_already_resolved() {
    let repo = FakeConversionRepo::new();
    let new = new_pending(0xAA, 0x11, 0x22);
    let inserted = repo.insert_pending(&scope(), &new).await.expect("insert");
    let when = fixed_now() + Duration::minutes(5);
    repo.transition_pending_to_cancelled(
        &scope(),
        inserted.id,
        Uuid::from_u128(CANCELLER),
        when,
        None,
    )
    .await
    .expect("cancel");
    let err = repo
        .transition_pending_to_expired(&scope(), inserted.id, when + Duration::minutes(1))
        .await
        .expect_err("expire-after-cancel must fail");
    assert!(matches!(err, DomainError::AlreadyResolved), "got {err:?}");
}

#[tokio::test]
async fn query_expired_returns_only_pending_past_cutoff() {
    let now = fixed_now();
    let cutoff = now + Duration::days(1);
    // Pending row with `expires_at` BEFORE cutoff -> due.
    let due_id = Uuid::from_u128(0xAA);
    let due_row = seeded_pending(
        due_id,
        Uuid::from_u128(0x11),
        Some(Uuid::from_u128(0x22)),
        now,
        cutoff - Duration::seconds(1),
    );
    // Pending row with `expires_at` AFTER cutoff -> not yet due.
    let not_yet_id = Uuid::from_u128(0xBB);
    let not_yet_row = seeded_pending(
        not_yet_id,
        Uuid::from_u128(0x33),
        Some(Uuid::from_u128(0x22)),
        now,
        cutoff + Duration::days(1),
    );
    // Approved row past cutoff: must be excluded (status != pending).
    let approved_id = Uuid::from_u128(0xCC);
    let approved_row = seeded_resolved(
        approved_id,
        Uuid::from_u128(0x44),
        Some(Uuid::from_u128(0x22)),
        ConversionStatus::Approved,
        now,
        now + Duration::minutes(1),
    );
    let repo = FakeConversionRepo::with_seed(vec![due_row, not_yet_row, approved_row]);
    let rows = repo
        .query_expired(&scope(), cutoff, 10)
        .await
        .expect("query_expired");
    let returned: Vec<Uuid> = rows.into_iter().map(|r| r.id).collect();
    assert_eq!(returned, vec![due_id]);
}

#[tokio::test]
async fn query_expired_excludes_soft_deleted() {
    let now = fixed_now();
    let cutoff = now + Duration::days(1);
    let pending_id = Uuid::from_u128(0xAA);
    let mut soft_deleted = seeded_pending(
        pending_id,
        Uuid::from_u128(0x11),
        Some(Uuid::from_u128(0x22)),
        now,
        cutoff - Duration::seconds(1),
    );
    soft_deleted.deleted_at = Some(now);
    let repo = FakeConversionRepo::with_seed(vec![soft_deleted]);
    let rows = repo
        .query_expired(&scope(), cutoff, 10)
        .await
        .expect("query_expired");
    assert!(rows.is_empty(), "soft-deleted row must not surface");
}

#[tokio::test]
async fn soft_delete_resolved_older_than_only_touches_resolved() {
    let now = fixed_now();
    let cutoff = now + Duration::days(7);
    let resolved_long_ago = seeded_resolved(
        Uuid::from_u128(0xAA),
        Uuid::from_u128(0x11),
        Some(Uuid::from_u128(0x22)),
        ConversionStatus::Approved,
        now,
        now + Duration::minutes(1),
    );
    let pending_old = seeded_pending(
        Uuid::from_u128(0xBB),
        Uuid::from_u128(0x33),
        Some(Uuid::from_u128(0x22)),
        now,
        now + Duration::days(30),
    );
    let repo = FakeConversionRepo::with_seed(vec![resolved_long_ago.clone(), pending_old.clone()]);
    let touched = repo
        .soft_delete_resolved_older_than(&scope(), cutoff, now + Duration::days(8), 10)
        .await
        .expect("delete_tenant");
    assert_eq!(touched, 1, "only the resolved row should be touched");
    let snapshot = repo.snapshot_all();
    let resolved_after = snapshot
        .iter()
        .find(|r| r.id == resolved_long_ago.id)
        .expect("resolved row present");
    assert!(resolved_after.deleted_at.is_some());
    let pending_after = snapshot
        .iter()
        .find(|r| r.id == pending_old.id)
        .expect("pending row present");
    assert!(pending_after.deleted_at.is_none());
}

#[tokio::test]
async fn list_own_for_tenant_filters_status_eq_predicate() {
    let now = fixed_now();
    let tenant = Uuid::from_u128(0x11);
    let parent = Uuid::from_u128(0x22);
    // Three resolved rows + one pending row, all on the same tenant.
    // Status filter narrows to the resolved ones; the `$top` clamp
    // then takes the two most-recent per the
    // `(requested_at DESC, id ASC)` contract.
    let pending = seeded_pending(
        Uuid::from_u128(0xA1),
        tenant,
        Some(parent),
        now,
        now + Duration::days(7),
    );
    let approved_old = seeded_resolved(
        Uuid::from_u128(0xA2),
        tenant,
        Some(parent),
        ConversionStatus::Approved,
        now - Duration::days(2),
        now - Duration::days(2) + Duration::minutes(1),
    );
    let approved_mid = seeded_resolved(
        Uuid::from_u128(0xA3),
        tenant,
        Some(parent),
        ConversionStatus::Approved,
        now - Duration::days(1),
        now - Duration::days(1) + Duration::minutes(1),
    );
    let approved_new = seeded_resolved(
        Uuid::from_u128(0xA4),
        tenant,
        Some(parent),
        ConversionStatus::Approved,
        now,
        now + Duration::minutes(1),
    );
    let repo = FakeConversionRepo::with_seed(vec![
        pending,
        approved_old,
        approved_mid,
        approved_new.clone(),
    ]);
    // `status eq 1` -> Approved storage code. Page cap 2.
    let page = repo
        .list_own_for_tenant(
            &scope(),
            tenant,
            &status_eq_query(i64::from(ConversionStatus::Approved.as_smallint()), 2),
        )
        .await
        .expect("list");
    let ids: Vec<Uuid> = page.items.into_iter().map(|r| r.id).collect();
    // Newest-first: approved_new then approved_mid.
    assert_eq!(ids, vec![approved_new.id, Uuid::from_u128(0xA3)]);
    assert_eq!(page.page_info.limit, 2);
}

#[tokio::test]
async fn list_inbound_for_parent_orders_requested_at_desc() {
    let now = fixed_now();
    let parent = Uuid::from_u128(0x22);
    let other_parent = Uuid::from_u128(0x99);
    let row_old = seeded_pending(
        Uuid::from_u128(0xB1),
        Uuid::from_u128(0x10),
        Some(parent),
        now - Duration::hours(2),
        now + Duration::days(7),
    );
    let row_mid = ConversionRequest {
        id: Uuid::from_u128(0xB2),
        tenant_id: Uuid::from_u128(0x20),
        requested_at: now - Duration::hours(1),
        ..row_old.clone()
    };
    let row_new = ConversionRequest {
        id: Uuid::from_u128(0xB3),
        tenant_id: Uuid::from_u128(0x30),
        requested_at: now,
        ..row_old.clone()
    };
    // Row under a different parent — must be filtered out.
    let row_other = ConversionRequest {
        id: Uuid::from_u128(0xB4),
        tenant_id: Uuid::from_u128(0x40),
        parent_id: Some(other_parent),
        ..row_old.clone()
    };
    let repo = FakeConversionRepo::with_seed(vec![row_old, row_mid, row_new, row_other]);
    let page = repo
        .list_inbound_for_parent(&scope(), parent, &default_query(10))
        .await
        .expect("list");
    let ids: Vec<Uuid> = page.items.into_iter().map(|r| r.id).collect();
    // Newest-first: B3 (now), B2 (1h ago), B1 (2h ago).
    assert_eq!(
        ids,
        vec![
            Uuid::from_u128(0xB3),
            Uuid::from_u128(0xB2),
            Uuid::from_u128(0xB1),
        ],
    );
}

#[tokio::test]
async fn get_own_for_tenant_returns_row_when_tenant_matches() {
    let now = fixed_now();
    let tenant = Uuid::from_u128(0x11);
    let parent = Uuid::from_u128(0x22);
    let row = seeded_pending(
        Uuid::from_u128(0xC1),
        tenant,
        Some(parent),
        now,
        now + Duration::days(7),
    );
    let repo = FakeConversionRepo::with_seed(vec![row.clone()]);
    let found = repo
        .get_own_for_tenant(&scope(), tenant, row.id)
        .await
        .expect("get");
    assert!(found.is_some());
    assert_eq!(found.unwrap().id, row.id);
}

#[tokio::test]
async fn get_own_for_tenant_returns_none_when_tenant_mismatches() {
    let now = fixed_now();
    let row = seeded_pending(
        Uuid::from_u128(0xC1),
        Uuid::from_u128(0x11),
        Some(Uuid::from_u128(0x22)),
        now,
        now + Duration::days(7),
    );
    let repo = FakeConversionRepo::with_seed(vec![row.clone()]);
    let found = repo
        .get_own_for_tenant(&scope(), Uuid::from_u128(0x12), row.id)
        .await
        .expect("get");
    assert!(
        found.is_none(),
        "row exists but its tenant_id does not match the URL-bound tenant -- must collapse to None"
    );
}

#[tokio::test]
async fn get_inbound_for_parent_returns_row_when_parent_matches() {
    let now = fixed_now();
    let tenant = Uuid::from_u128(0x11);
    let parent = Uuid::from_u128(0x22);
    let row = seeded_pending(
        Uuid::from_u128(0xC2),
        tenant,
        Some(parent),
        now,
        now + Duration::days(7),
    );
    let repo = FakeConversionRepo::with_seed(vec![row.clone()]);
    let found = repo
        .get_inbound_for_parent(&scope(), parent, row.id)
        .await
        .expect("get");
    assert!(found.is_some());
    assert_eq!(found.unwrap().tenant_id, tenant);
}

#[tokio::test]
async fn get_inbound_for_parent_returns_none_when_parent_mismatches() {
    let now = fixed_now();
    let row = seeded_pending(
        Uuid::from_u128(0xC2),
        Uuid::from_u128(0x11),
        Some(Uuid::from_u128(0x22)),
        now,
        now + Duration::days(7),
    );
    let repo = FakeConversionRepo::with_seed(vec![row.clone()]);
    let found = repo
        .get_inbound_for_parent(&scope(), Uuid::from_u128(0x99), row.id)
        .await
        .expect("get");
    assert!(found.is_none(), "wrong-parent URL must collapse to None");
}

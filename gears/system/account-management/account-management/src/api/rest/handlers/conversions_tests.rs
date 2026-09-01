//! Unit tests for the PATCH dispatcher in
//! [`super::dispatch_patch`].
//!
//! Handler-level integration tests (axum router + service fake) are
//! out of scope here per the gear-level convention; the goal is to
//! pin the pure status-to-method routing the PATCH endpoints share
//! between own-side and parent-side. A regression that re-wires one
//! PATCH branch to the wrong service method would surface here as a
//! mismatched `status` on the returned row.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use time::OffsetDateTime;
use toolkit_security::SecurityContext;
use uuid::Uuid;

use crate::api::rest::dto::{ConversionPatchDto, ConversionPatchStatusDto};
use crate::api::rest::handlers::conversions::dispatch_patch;
use crate::domain::conversion::model::{ConversionSide, ConversionStatus, TargetMode};
use crate::domain::conversion::service::{
    ConversionCaller, ConversionService, RequestConversionInput,
};
use crate::domain::conversion::test_support::FakeConversionRepo;
use crate::domain::tenant::model::{TenantModel, TenantStatus};
use crate::domain::tenant::test_support::{FakeTenantRepo, mock_enforcer};
use crate::domain::tenant_type::inert_tenant_type_checker;

const APPROVAL_TTL_SECS: u64 = 7 * 24 * 60 * 60;
const RETENTION_SECS: u64 = 7 * 24 * 60 * 60;
const CTX_SUBJECT: Uuid = Uuid::from_u128(0xCAFE);

fn fixed_now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("epoch")
}

fn ctx_subject_root() -> Uuid {
    Uuid::from_u128(0xCAFE_BABE)
}

fn ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(CTX_SUBJECT)
        .subject_tenant_id(ctx_subject_root())
        .build()
        .expect("ctx")
}

fn seed_tenant(fake: &FakeTenantRepo, id: Uuid, parent_id: Option<Uuid>, self_managed: bool) {
    let now = fixed_now();
    fake.insert_tenant_raw(TenantModel {
        id,
        parent_id,
        name: format!("t-{id}"),
        status: TenantStatus::Active,
        self_managed,
        tenant_type_uuid: Uuid::from_u128(0xAA),
        depth: u32::from(parent_id.is_some()),
        created_at: now,
        updated_at: now,
        deleted_at: None,
    });
    fake.seed_closure(ctx_subject_root(), id, 0, TenantStatus::Active);
    if ctx_subject_root() != id {
        fake.seed_closure(id, id, 0, TenantStatus::Active);
    }
}

fn build_service(conv: Arc<FakeConversionRepo>, tenants: Arc<FakeTenantRepo>) -> ConversionService {
    let now = fixed_now();
    let now_fn: Arc<dyn Fn() -> OffsetDateTime + Send + Sync> = Arc::new(move || now);
    ConversionService::new(
        conv,
        tenants,
        inert_tenant_type_checker(),
        mock_enforcer(),
        StdDuration::from_secs(APPROVAL_TTL_SECS),
        StdDuration::from_secs(RETENTION_SECS),
    )
    .with_now_fn(now_fn)
}

/// Spin up a pending child-initiated conversion request via the
/// service so the PATCH dispatch tests can run against a real row.
async fn seed_pending_child_initiated(
    conv: Arc<FakeConversionRepo>,
    tenants: Arc<FakeTenantRepo>,
    child: Uuid,
) -> (ConversionService, Uuid) {
    let svc = build_service(conv.clone(), tenants);
    let inserted = svc
        .request_conversion(
            &ctx(),
            RequestConversionInput {
                tenant_id: child,
                caller: ConversionCaller::child(child),
                target_mode: TargetMode::SelfManaged,
                comment: None,
            },
        )
        .await
        .expect("request happy path");
    (svc, inserted.id)
}

#[tokio::test]
async fn patch_dispatch_routes_approved_to_service_approve() {
    let parent = Uuid::from_u128(0x100);
    let child = Uuid::from_u128(0x101);
    let tenants = Arc::new(FakeTenantRepo::new());
    seed_tenant(&tenants, parent, None, false);
    seed_tenant(&tenants, child, Some(parent), false);
    let conv = Arc::new(FakeConversionRepo::new().with_tenant_repo(Arc::clone(&tenants)));
    let (svc, request_id) =
        seed_pending_child_initiated(Arc::clone(&conv), Arc::clone(&tenants), child).await;

    let body = ConversionPatchDto {
        status: ConversionPatchStatusDto::Approved,
        comment: None,
    };
    let row = dispatch_patch(
        &svc,
        &ctx(),
        request_id,
        ConversionCaller::parent(parent),
        body,
    )
    .await
    .expect("approve dispatch");
    assert_eq!(row.status, ConversionStatus::Approved);
}

#[tokio::test]
async fn patch_dispatch_routes_cancelled_to_service_cancel() {
    let parent = Uuid::from_u128(0x200);
    let child = Uuid::from_u128(0x201);
    let tenants = Arc::new(FakeTenantRepo::new());
    seed_tenant(&tenants, parent, None, false);
    seed_tenant(&tenants, child, Some(parent), false);
    let conv = Arc::new(FakeConversionRepo::new());
    let (svc, request_id) =
        seed_pending_child_initiated(Arc::clone(&conv), Arc::clone(&tenants), child).await;

    let body = ConversionPatchDto {
        status: ConversionPatchStatusDto::Cancelled,
        comment: None,
    };
    // Initiator-only: child caller cancels.
    let row = dispatch_patch(
        &svc,
        &ctx(),
        request_id,
        ConversionCaller::child(child),
        body,
    )
    .await
    .expect("cancel dispatch");
    assert_eq!(row.status, ConversionStatus::Cancelled);
}

#[tokio::test]
async fn patch_dispatch_routes_rejected_to_service_reject() {
    let parent = Uuid::from_u128(0x300);
    let child = Uuid::from_u128(0x301);
    let tenants = Arc::new(FakeTenantRepo::new());
    seed_tenant(&tenants, parent, None, false);
    seed_tenant(&tenants, child, Some(parent), false);
    let conv = Arc::new(FakeConversionRepo::new());
    let (svc, request_id) =
        seed_pending_child_initiated(Arc::clone(&conv), Arc::clone(&tenants), child).await;

    let body = ConversionPatchDto {
        status: ConversionPatchStatusDto::Rejected,
        comment: None,
    };
    // Counterparty-only: parent caller rejects.
    let row = dispatch_patch(
        &svc,
        &ctx(),
        request_id,
        ConversionCaller::parent(parent),
        body,
    )
    .await
    .expect("reject dispatch");
    assert_eq!(row.status, ConversionStatus::Rejected);
}

#[tokio::test]
async fn patch_dispatch_threads_comment_through_to_service() {
    let parent = Uuid::from_u128(0x400);
    let child = Uuid::from_u128(0x401);
    let tenants = Arc::new(FakeTenantRepo::new());
    seed_tenant(&tenants, parent, None, false);
    seed_tenant(&tenants, child, Some(parent), false);
    let conv = Arc::new(FakeConversionRepo::new());
    let (svc, request_id) =
        seed_pending_child_initiated(Arc::clone(&conv), Arc::clone(&tenants), child).await;

    let body = ConversionPatchDto {
        status: ConversionPatchStatusDto::Rejected,
        comment: Some("not approved".to_owned()),
    };
    let row = dispatch_patch(
        &svc,
        &ctx(),
        request_id,
        ConversionCaller::parent(parent),
        body,
    )
    .await
    .expect("reject dispatch with comment");
    assert_eq!(row.rejected_comment.as_deref(), Some("not approved"));
}

#[tokio::test]
async fn patch_dispatch_propagates_invalid_actor_for_transition() {
    // Counterparty trying to cancel (initiator-only): the dispatch
    // routes to `svc.cancel(...)`, which surfaces
    // `InvalidActorForTransition`. The dispatcher MUST forward that
    // verbatim so the canonical envelope reaches the caller.
    let parent = Uuid::from_u128(0x500);
    let child = Uuid::from_u128(0x501);
    let tenants = Arc::new(FakeTenantRepo::new());
    seed_tenant(&tenants, parent, None, false);
    seed_tenant(&tenants, child, Some(parent), false);
    let conv = Arc::new(FakeConversionRepo::new());
    let (svc, request_id) =
        seed_pending_child_initiated(Arc::clone(&conv), Arc::clone(&tenants), child).await;

    let body = ConversionPatchDto {
        status: ConversionPatchStatusDto::Cancelled,
        comment: None,
    };
    let err = dispatch_patch(
        &svc,
        &ctx(),
        request_id,
        ConversionCaller::parent(parent),
        body,
    )
    .await
    .expect_err("wrong-side cancel must surface InvalidActorForTransition");
    let crate::domain::error::DomainError::InvalidActorForTransition {
        attempted_status,
        caller_side,
    } = err
    else {
        panic!("expected InvalidActorForTransition, got {err:?}");
    };
    // Pin the seeded scenario by name rather than asserting against
    // an enum's string label (which would tautologically test
    // `ConversionSide::Child.as_str()`): the seed was initiator =
    // child, the dispatcher was called with the parent caller, so
    // the rejected attempted status is cancel-from-parent.
    assert_eq!(attempted_status, "cancelled");
    assert_eq!(caller_side, ConversionSide::Parent.as_str());
}

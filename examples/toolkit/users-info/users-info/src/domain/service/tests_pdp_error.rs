#![allow(clippy::unwrap_used, clippy::expect_used)]

use uuid::Uuid;

use std::sync::Arc;

use crate::domain::error::DomainError;
use crate::domain::service::ServiceConfig;
use crate::test_support::{
    FailingAuthZResolver, build_services, build_services_with_authz, ctx_allow_tenants, inmem_db,
    seed_user,
};
use users_info_sdk::{NewAddress, NewCity};

// ---------------------------------------------------------------------------
// PDP evaluation failure tests (AuthZResolverError::Internal → DomainError::InternalError)
// ---------------------------------------------------------------------------

/// When the PDP returns an internal error (e.g., unreachable), the service
/// must propagate it as `DomainError::InternalError`, not `Forbidden`.
#[tokio::test]
async fn pdp_internal_error_returns_internal_for_list_users() {
    let db = inmem_db().await;
    let tenant_id = Uuid::new_v4();

    let services = build_services_with_authz(
        db.clone(),
        ServiceConfig::default(),
        Arc::new(FailingAuthZResolver),
    );
    let ctx = ctx_allow_tenants(&[tenant_id]);

    let err = services
        .users
        .list_users_page(&ctx, &toolkit_odata::ODataQuery::default())
        .await
        .unwrap_err();

    assert!(
        matches!(err, DomainError::InternalError),
        "Expected DomainError::InternalError from PDP failure, got: {err:?}"
    );
}

/// PDP internal error on `create_address` → `DomainError::InternalError`.
#[tokio::test]
async fn pdp_internal_error_returns_internal_for_create_address() {
    let db = inmem_db().await;
    let tenant_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let conn = db.conn().unwrap();
    seed_user(&conn, user_id, tenant_id, "fail@example.com", "Fail User").await;

    // Create a city with a permissive resolver first
    let permissive = build_services(db.clone(), ServiceConfig::default());
    let pctx = ctx_allow_tenants(&[tenant_id]);

    let city = permissive
        .cities
        .create_city(
            &pctx,
            NewCity {
                id: None,
                tenant_id,
                name: "Fail City".to_owned(),
                country: "FL".to_owned(),
            },
        )
        .await
        .unwrap();

    // Now use the failing resolver
    let services = build_services_with_authz(
        db.clone(),
        ServiceConfig::default(),
        Arc::new(FailingAuthZResolver),
    );
    let ctx = ctx_allow_tenants(&[tenant_id]);

    let err = services
        .addresses
        .create_address(
            &ctx,
            NewAddress {
                id: None,
                tenant_id,
                user_id,
                city_id: city.id,
                street: "Fail St".to_owned(),
                postal_code: "99999".to_owned(),
            },
        )
        .await
        .unwrap_err();

    assert!(
        matches!(err, DomainError::InternalError),
        "Expected DomainError::InternalError from PDP failure, got: {err:?}"
    );
}

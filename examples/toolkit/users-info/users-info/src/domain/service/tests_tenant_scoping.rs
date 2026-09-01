#![allow(clippy::unwrap_used, clippy::expect_used)]

use uuid::Uuid;

use crate::domain::error::DomainError;
use crate::domain::service::ServiceConfig;
use crate::test_support::{build_services, ctx_allow_tenants, ctx_deny_all, inmem_db, seed_user};
use toolkit_db::DBProvider;
use users_info_sdk::{NewAddress, NewCity, NewUser};

#[tokio::test]
async fn tenant_scope_only_sees_its_tenant() {
    let db = inmem_db().await;
    let tenant1 = Uuid::new_v4();
    let tenant2 = Uuid::new_v4();

    let user1 = Uuid::new_v4();
    let user2 = Uuid::new_v4();
    let conn = db.conn().unwrap();
    seed_user(&conn, user1, tenant1, "u1@example.com", "U1").await;
    seed_user(&conn, user2, tenant2, "u2@example.com", "U2").await;

    let services = build_services(db.clone(), ServiceConfig::default());
    let ctx_t1 = ctx_allow_tenants(&[tenant1]);

    let page = services
        .users
        .list_users_page(&ctx_t1, &toolkit_odata::ODataQuery::default())
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].tenant_id, tenant1);
}

#[tokio::test]
async fn deny_all_returns_forbidden() {
    let db = inmem_db().await;
    let tenant = Uuid::new_v4();
    let conn = db.conn().unwrap();
    seed_user(&conn, Uuid::new_v4(), tenant, "u@example.com", "U").await;

    let services = build_services(db.clone(), ServiceConfig::default());
    let ctx = ctx_deny_all();

    // Anonymous context has no tenant → mock returns empty constraints
    // → Decision Matrix: require_constraints=true + empty → ConstraintsRequiredButAbsent → Forbidden
    let result = services
        .users
        .list_users_page(&ctx, &toolkit_odata::ODataQuery::default())
        .await;
    let err = result.unwrap_err();
    assert!(
        matches!(err, DomainError::Forbidden),
        "Expected DomainError::Forbidden for anonymous context, got: {err:?}"
    );
}

#[tokio::test]
async fn create_user_with_transaction() {
    let db = inmem_db().await;
    let tenant_id = Uuid::new_v4();

    let services = build_services(db.clone(), ServiceConfig::default());
    // Use a context with tenants, not root, because insert requires tenant scope
    let ctx = ctx_allow_tenants(&[tenant_id]);

    let new_user = NewUser {
        id: None,
        tenant_id,
        email: "test@example.com".to_owned(),
        display_name: "Test User".to_owned(),
    };

    let result = services.users.create_user(&ctx, new_user).await;
    assert!(result.is_ok(), "create_user failed: {:?}", result.err());

    let created = result.unwrap();
    assert_eq!(created.email, "test@example.com");
    assert_eq!(created.display_name, "Test User");
    assert_eq!(created.tenant_id, tenant_id);
}

#[tokio::test]
async fn dbprovider_transaction_smoke() {
    use crate::infra::storage::entity::user::{ActiveModel, Entity as UserEntity};
    use sea_orm::Set;
    use time::OffsetDateTime;
    use toolkit_db::secure::{AccessScope, secure_insert};

    let db = inmem_db().await;
    let provider: DBProvider<toolkit_db::DbError> = DBProvider::new(db.clone());

    let tenant_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let now = OffsetDateTime::now_utc();
    let scope = AccessScope::for_tenants(vec![tenant_id]);

    provider
        .transaction(|tx| {
            Box::pin(async move {
                let user = ActiveModel {
                    id: Set(user_id),
                    tenant_id: Set(tenant_id),
                    email: Set("tx@example.com".to_owned()),
                    display_name: Set("Tx User".to_owned()),
                    created_at: Set(now),
                    updated_at: Set(now),
                };
                let _ = secure_insert::<UserEntity>(user, &scope, tx).await?;
                Ok(())
            })
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn create_address_validates_user_exists() {
    let db = inmem_db().await;
    let tenant_id = Uuid::new_v4();

    let services = build_services(db.clone(), ServiceConfig::default());
    let ctx = ctx_allow_tenants(&[tenant_id]);

    let bogus_user_id = Uuid::new_v4();
    let result = services
        .addresses
        .create_address(
            &ctx,
            NewAddress {
                id: None,
                tenant_id,
                user_id: bogus_user_id,
                city_id: Uuid::new_v4(),
                street: "Nowhere St".to_owned(),
                postal_code: "00000".to_owned(),
            },
        )
        .await;

    assert!(result.is_err(), "Expected error for non-existent user");
}

#[tokio::test]
async fn create_address_forces_user_tenant() {
    let db = inmem_db().await;
    let tenant_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let conn = db.conn().unwrap();
    seed_user(&conn, user_id, tenant_id, "addr@example.com", "Addr User").await;

    let services = build_services(db.clone(), ServiceConfig::default());
    let ctx = ctx_allow_tenants(&[tenant_id]);

    // Create a city in the same tenant
    let city = services
        .cities
        .create_city(
            &ctx,
            NewCity {
                id: None,
                tenant_id,
                name: "Test City".to_owned(),
                country: "TC".to_owned(),
            },
        )
        .await
        .unwrap();

    // Pass a different tenant_id in NewAddress — the service must override it
    let different_tenant = Uuid::new_v4();
    let created = services
        .addresses
        .create_address(
            &ctx,
            NewAddress {
                id: None,
                tenant_id: different_tenant,
                user_id,
                city_id: city.id,
                street: "123 Main St".to_owned(),
                postal_code: "12345".to_owned(),
            },
        )
        .await
        .unwrap();

    assert_eq!(
        created.tenant_id, tenant_id,
        "Address tenant must match user's tenant, not the request"
    );
}

#[tokio::test]
async fn put_user_address_creates_then_updates() {
    let db = inmem_db().await;
    let tenant_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    let conn = db.conn().unwrap();
    seed_user(&conn, user_id, tenant_id, "put@example.com", "Put User").await;

    let services = build_services(db.clone(), ServiceConfig::default());
    let ctx = ctx_allow_tenants(&[tenant_id]);

    let city = services
        .cities
        .create_city(
            &ctx,
            NewCity {
                id: None,
                tenant_id,
                name: "City A".to_owned(),
                country: "CA".to_owned(),
            },
        )
        .await
        .unwrap();

    // First PUT — should create
    let created = services
        .addresses
        .put_user_address(
            &ctx,
            user_id,
            NewAddress {
                id: None,
                tenant_id,
                user_id,
                city_id: city.id,
                street: "First St".to_owned(),
                postal_code: "11111".to_owned(),
            },
        )
        .await
        .unwrap();

    assert_eq!(created.street, "First St");
    assert_eq!(created.tenant_id, tenant_id);

    // Second PUT — should update
    let updated = services
        .addresses
        .put_user_address(
            &ctx,
            user_id,
            NewAddress {
                id: None,
                tenant_id,
                user_id,
                city_id: city.id,
                street: "Second St".to_owned(),
                postal_code: "22222".to_owned(),
            },
        )
        .await
        .unwrap();

    assert_eq!(updated.id, created.id, "Should update the same address");
    assert_eq!(updated.street, "Second St");
    assert_eq!(updated.postal_code, "22222");
}

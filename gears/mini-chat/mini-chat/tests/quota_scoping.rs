//! Secure ORM scoping of `quota_usage` (DESIGN 3.7 "Secure ORM": tenant column
//! `tenant_id`, owner column `user_id`) through the quota repository: a row is
//! visible and writable only under a scope whose tenant and owner match, a
//! deny-all scope sees nothing, and the reserve / settlement writes are atomic
//! upserts on the unique bucket key (never a unique violation).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

use mini_chat::domain::ports::{QuotaOwner, QuotaRepository, QuotaSettlement};
use mini_chat::domain::quota::{Bucket, PeriodStarts};
use mini_chat::infra::db::migrations::gear_migrations;
use time::OffsetDateTime;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::{ConnectOpts, DBProvider, DbError, connect_db};
use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
use uuid::Uuid;

const TENANT_A: Uuid = Uuid::from_u128(0xA);
const TENANT_B: Uuid = Uuid::from_u128(0xB);
const USER_1: Uuid = Uuid::from_u128(0x1);
const USER_2: Uuid = Uuid::from_u128(0x2);

async fn provider() -> DBProvider<DbError> {
    let db = connect_db(
        "sqlite::memory:",
        ConnectOpts {
            max_conns: Some(1),
            min_conns: Some(1),
            ..Default::default()
        },
    )
    .await
    .expect("connect in-memory sqlite");
    run_migrations_for_testing(&db, gear_migrations())
        .await
        .expect("gear migrations apply");
    DBProvider::new(db)
}

fn scope(tenant: Uuid, owner: Uuid) -> AccessScope {
    AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
        ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant),
        ScopeFilter::eq(pep_properties::OWNER_ID, owner),
    ])])
}

fn owner(tenant: Uuid, user: Uuid) -> QuotaOwner {
    QuotaOwner {
        tenant_id: tenant,
        user_id: user,
    }
}

async fn reserve(db: &DBProvider<DbError>, scope: &AccessScope, who: QuotaOwner, credits: i64) {
    let conn = db.conn().unwrap();
    conn.reserve_quota(
        scope,
        who,
        Bucket::of_tier(mini_chat_sdk::ModelTier::Premium),
        PeriodStarts::at(OffsetDateTime::now_utc()),
        credits,
        OffsetDateTime::now_utc(),
    )
    .await
    .unwrap();
}

async fn reserved_of(db: &DBProvider<DbError>, scope: &AccessScope, who: QuotaOwner) -> Vec<i64> {
    let conn = db.conn().unwrap();
    conn.quota_rows(scope, who, PeriodStarts::at(OffsetDateTime::now_utc()))
        .await
        .unwrap()
        .into_iter()
        .map(|row| row.reserved_credits_micro)
        .collect()
}

#[tokio::test]
async fn rows_are_visible_only_under_the_matching_tenant_and_owner_scope() {
    let db = provider().await;
    let a1 = owner(TENANT_A, USER_1);
    reserve(&db, &scope(TENANT_A, USER_1), a1, 700).await;

    assert_eq!(
        reserved_of(&db, &scope(TENANT_A, USER_1), a1).await,
        vec![700; 4],
        "premium: total and tier:premium, daily and monthly"
    );
    // another user of the tenant, another tenant, and a scope that does not match the
    // requested owner see nothing, even when the query names the owner explicitly
    assert!(
        reserved_of(&db, &scope(TENANT_A, USER_2), a1)
            .await
            .is_empty()
    );
    assert!(
        reserved_of(&db, &scope(TENANT_B, USER_1), a1)
            .await
            .is_empty()
    );
    assert!(
        reserved_of(&db, &AccessScope::default(), a1)
            .await
            .is_empty()
    );
    assert!(
        reserved_of(&db, &scope(TENANT_A, USER_1), owner(TENANT_A, USER_2))
            .await
            .is_empty(),
        "the owner filter of the scope and the named owner must both match"
    );
}

#[tokio::test]
async fn a_write_under_a_foreign_scope_is_denied() {
    let db = provider().await;
    let conn = db.conn().unwrap();
    let starts = PeriodStarts::at(OffsetDateTime::now_utc());
    for foreign in [
        scope(TENANT_A, USER_2),
        scope(TENANT_B, USER_1),
        AccessScope::default(),
    ] {
        let denied = conn
            .reserve_quota(
                &foreign,
                owner(TENANT_A, USER_1),
                &[Bucket::Total],
                starts,
                10,
                OffsetDateTime::now_utc(),
            )
            .await;
        assert!(denied.is_err(), "a foreign scope must not write the row");
    }
    assert!(
        reserved_of(&db, &scope(TENANT_A, USER_1), owner(TENANT_A, USER_1))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn reserve_and_settlement_are_upserts_on_the_bucket_key() {
    let db = provider().await;
    let a1 = owner(TENANT_A, USER_1);
    let own = scope(TENANT_A, USER_1);
    reserve(&db, &own, a1, 100).await;
    reserve(&db, &own, a1, 50).await; // the same keys again: incremented, never a conflict
    assert_eq!(reserved_of(&db, &own, a1).await, vec![150; 4]);

    let conn = db.conn().unwrap();
    let settlement = QuotaSettlement {
        release_credits_micro: 100,
        spent_credits_micro: 80,
        input_tokens: 7,
        output_tokens: 3,
        web_search_calls: 1,
        code_interpreter_calls: 0,
    };
    conn.settle_quota(
        &own,
        a1,
        Bucket::of_tier(mini_chat_sdk::ModelTier::Premium),
        PeriodStarts::at(OffsetDateTime::now_utc()),
        &settlement,
        OffsetDateTime::now_utc(),
    )
    .await
    .unwrap();
    let rows = conn
        .quota_rows(&own, a1, PeriodStarts::at(OffsetDateTime::now_utc()))
        .await
        .unwrap();
    assert_eq!(rows.len(), 4, "no duplicate rows");
    for row in rows {
        assert_eq!(
            (row.spent_credits_micro, row.reserved_credits_micro),
            (80, 50),
            "{row:?}"
        );
        let total = row.bucket == Bucket::Total;
        assert_eq!(
            row.web_search_calls,
            i64::from(total),
            "tool counters: total only"
        );
    }
}

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use time::macros::date;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::*;
use crate::domain::ports::{BucketSettlement, QuotaBucketKey, QuotaOwner};
use crate::domain::quota::{QuotaBucket, QuotaPeriod};
use crate::infra::db::repo::tests::{
    TENANT_A, TENANT_B, TestStore, USER_A1, USER_A2, at, subject_scope,
};

const OWNER: QuotaOwner = QuotaOwner {
    tenant_id: TENANT_A,
    user_id: USER_A1,
};

fn key(period: QuotaPeriod, bucket: QuotaBucket) -> QuotaBucketKey {
    QuotaBucketKey {
        period,
        period_start: match period {
            QuotaPeriod::Daily => date!(2026 - 10 - 11),
            QuotaPeriod::Monthly => date!(2026 - 10 - 01),
        },
        bucket,
    }
}

fn all_keys() -> Vec<QuotaBucketKey> {
    vec![
        key(QuotaPeriod::Daily, QuotaBucket::Total),
        key(QuotaPeriod::Monthly, QuotaBucket::Total),
        key(QuotaPeriod::Daily, QuotaBucket::Premium),
        key(QuotaPeriod::Monthly, QuotaBucket::Premium),
    ]
}

fn scope(owner: QuotaOwner) -> AccessScope {
    subject_scope(owner.tenant_id, owner.user_id)
}

async fn rows(store: &TestStore, owner: QuotaOwner) -> Vec<QuotaUsageRecord> {
    read_buckets(&store.conn(), &scope(owner), owner, &all_keys(), false)
        .await
        .expect("read buckets")
}

fn find(rows: &[QuotaUsageRecord], key: QuotaBucketKey) -> QuotaUsageRecord {
    *rows.iter().find(|row| row.key == key).expect("row exists")
}

#[tokio::test]
async fn reserve_creates_missing_rows_and_increments_existing_ones() {
    let store = TestStore::without_outbox().await;
    let keys = &all_keys()[..2];

    add_reserve(
        &store.conn(),
        store.ts(),
        &scope(OWNER),
        OWNER,
        keys,
        100,
        at(0),
    )
    .await
    .expect("first reserve");
    add_reserve(
        &store.conn(),
        store.ts(),
        &scope(OWNER),
        OWNER,
        keys,
        50,
        at(1),
    )
    .await
    .expect("second reserve");

    let stored = rows(&store, OWNER).await;
    assert_eq!(stored.len(), 2, "only the reserved buckets exist");
    for key in keys {
        let row = find(&stored, *key);
        assert_eq!(row.reserved_credits_micro, 150);
        assert_eq!(row.spent_credits_micro, 0);
        assert_eq!(row.calls, 0);
    }
}

#[tokio::test]
async fn reads_are_confined_to_the_owner() {
    let store = TestStore::without_outbox().await;
    let others = [
        QuotaOwner {
            tenant_id: TENANT_A,
            user_id: USER_A2,
        },
        QuotaOwner {
            tenant_id: TENANT_B,
            user_id: USER_A1,
        },
    ];
    for owner in others {
        add_reserve(
            &store.conn(),
            store.ts(),
            &scope(owner),
            owner,
            &all_keys(),
            7,
            at(0),
        )
        .await
        .expect("reserve of another owner");
    }
    add_reserve(
        &store.conn(),
        store.ts(),
        &scope(OWNER),
        OWNER,
        &all_keys(),
        1,
        at(0),
    )
    .await
    .expect("reserve");

    let stored = rows(&store, OWNER).await;
    assert_eq!(stored.len(), 4);
    assert!(stored.iter().all(|row| row.reserved_credits_micro == 1));

    // An unconstrained scope still gets the explicit owner predicates.
    let unscoped = read_buckets(
        &store.conn(),
        &AccessScope::allow_all(),
        OWNER,
        &all_keys(),
        true,
    )
    .await
    .expect("read");
    assert_eq!(unscoped.len(), 4);
    assert!(unscoped.iter().all(|row| row.reserved_credits_micro == 1));
}

#[tokio::test]
async fn reads_of_other_periods_return_nothing() {
    let store = TestStore::without_outbox().await;
    add_reserve(
        &store.conn(),
        store.ts(),
        &scope(OWNER),
        OWNER,
        &all_keys(),
        5,
        at(0),
    )
    .await
    .expect("reserve");
    let next_day = QuotaBucketKey {
        period_start: date!(2026 - 10 - 12),
        ..key(QuotaPeriod::Daily, QuotaBucket::Total)
    };
    let stored = read_buckets(&store.conn(), &scope(OWNER), OWNER, &[next_day], false)
        .await
        .expect("read");
    assert!(stored.is_empty());
    assert!(
        read_buckets(&store.conn(), &scope(OWNER), OWNER, &[], false)
            .await
            .expect("read")
            .is_empty()
    );
}

#[tokio::test]
async fn settlement_moves_reserve_to_spent_and_counts_telemetry() {
    let store = TestStore::without_outbox().await;
    let total = key(QuotaPeriod::Daily, QuotaBucket::Total);
    add_reserve(
        &store.conn(),
        store.ts(),
        &scope(OWNER),
        OWNER,
        &[total],
        300,
        at(0),
    )
    .await
    .expect("reserve");

    let settlement = BucketSettlement {
        key: total,
        release_reserved_credits_micro: 200,
        spent_credits_micro: 120,
        input_tokens: 40,
        output_tokens: 60,
        web_search_calls: 2,
        code_interpreter_calls: 1,
    };
    settle(
        &store.conn(),
        store.ts(),
        &scope(OWNER),
        OWNER,
        &[settlement],
        at(1),
    )
    .await
    .expect("settle");

    let row = find(&rows(&store, OWNER).await, total);
    assert_eq!(row.reserved_credits_micro, 100);
    assert_eq!(row.spent_credits_micro, 120);
    assert_eq!(row.calls, 1);
    assert_eq!((row.input_tokens, row.output_tokens), (40, 60));
    assert_eq!((row.web_search_calls, row.code_interpreter_calls), (2, 1));
}

#[tokio::test]
async fn settlement_clamps_the_release_and_creates_missing_rows() {
    let store = TestStore::without_outbox().await;
    let premium = key(QuotaPeriod::Monthly, QuotaBucket::Premium);
    add_reserve(
        &store.conn(),
        store.ts(),
        &scope(OWNER),
        OWNER,
        &[premium],
        10,
        at(0),
    )
    .await
    .expect("reserve");
    let missing = key(QuotaPeriod::Daily, QuotaBucket::Premium);
    let settlement = |key| BucketSettlement {
        key,
        release_reserved_credits_micro: 50,
        spent_credits_micro: 30,
        input_tokens: 0,
        output_tokens: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
    };
    settle(
        &store.conn(),
        store.ts(),
        &scope(OWNER),
        OWNER,
        &[settlement(premium), settlement(missing)],
        at(1),
    )
    .await
    .expect("settle");

    let stored = rows(&store, OWNER).await;
    for key in [premium, missing] {
        let row = find(&stored, key);
        assert_eq!(row.reserved_credits_micro, 0, "{key:?}");
        assert_eq!(row.spent_credits_micro, 30, "{key:?}");
        assert_eq!(row.calls, 1, "{key:?}");
    }
}

#[tokio::test]
async fn writes_outside_the_scope_are_rejected() {
    let store = TestStore::without_outbox().await;
    let foreign = QuotaOwner {
        tenant_id: TENANT_B,
        user_id: USER_A1,
    };
    let result = add_reserve(
        &store.conn(),
        store.ts(),
        &scope(OWNER),
        foreign,
        &all_keys(),
        1,
        at(0),
    )
    .await;
    assert!(result.is_err());
    assert!(rows(&store, foreign).await.is_empty());
}

fn stored_row(store: &TestStore, period_type: &str, bucket: &str) -> quota_usage::Model {
    quota_usage::Model {
        id: Uuid::new_v4(),
        tenant_id: OWNER.tenant_id,
        user_id: OWNER.user_id,
        period_type: period_type.to_owned(),
        period_start: date!(2026 - 10 - 11),
        bucket: bucket.to_owned(),
        spent_credits_micro: 1,
        reserved_credits_micro: 2,
        calls: 3,
        input_tokens: 4,
        output_tokens: 5,
        file_search_calls: 0,
        web_search_calls: 6,
        code_interpreter_calls: 7,
        rag_retrieval_calls: 0,
        image_inputs: 0,
        image_upload_bytes: 0,
        updated_at: store.stamp(at(0)),
    }
}

#[tokio::test]
async fn stored_rows_map_to_records_and_unknown_values_are_internal_errors() {
    let store = TestStore::without_outbox().await;
    let record = to_record(&stored_row(&store, "daily", "total")).expect("known values");
    assert_eq!(record.key, key(QuotaPeriod::Daily, QuotaBucket::Total));
    assert_eq!(
        (
            record.spent_credits_micro,
            record.reserved_credits_micro,
            record.calls,
            record.input_tokens,
            record.output_tokens,
            record.web_search_calls,
            record.code_interpreter_calls
        ),
        (1, 2, 3, 4, 5, 6, 7)
    );
    assert!(matches!(
        to_record(&stored_row(&store, "weekly", "total")),
        Err(DomainError::Internal(_))
    ));
    assert!(matches!(
        to_record(&stored_row(&store, "daily", "tier:standard")),
        Err(DomainError::Internal(_))
    ));
}

#[tokio::test]
async fn port_methods_run_inside_the_callers_transaction() {
    let store = TestStore::without_outbox().await;
    let repo = std::sync::Arc::clone(&store.repo);
    let keys = all_keys();

    let read_back = store
        .db
        .transaction_ref_mapped(move |tx| {
            Box::pin(async move {
                let scope = scope(OWNER);
                repo.add_reserve_in_tx(tx, &scope, OWNER, &keys, 9, at(0))
                    .await?;
                repo.read_buckets_in_tx(tx, &scope, OWNER, &keys).await
            })
        })
        .await
        .expect("transaction");
    assert_eq!(read_back.len(), 4);

    let committed = store
        .repo
        .read_buckets(&scope(OWNER), OWNER, &all_keys(), true)
        .await
        .expect("read");
    assert!(committed.iter().all(|row| row.reserved_credits_micro == 9));
}

#[tokio::test]
async fn a_failed_transaction_leaves_no_reserve() {
    let store = TestStore::without_outbox().await;
    let repo = std::sync::Arc::clone(&store.repo);
    let keys = all_keys();

    let result: Result<(), DomainError> = store
        .db
        .transaction_ref_mapped(move |tx| {
            Box::pin(async move {
                repo.add_reserve_in_tx(tx, &scope(OWNER), OWNER, &keys, 9, at(0))
                    .await?;
                Err(DomainError::internal("abort"))
            })
        })
        .await;
    assert!(result.is_err());
    assert!(rows(&store, OWNER).await.is_empty());
}

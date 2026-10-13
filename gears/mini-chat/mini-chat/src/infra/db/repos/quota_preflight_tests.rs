#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 2: the preflight read of `SeaQuotaRepository` on `SQLite`
//! (`cpt-cf-mini-chat-algo-quota-billing-preflight`, steps 4 and 5) and the
//! scoped bucket read behind the status and the warnings.

use toolkit_security::AccessScope;
use uuid::Uuid;

use crate::domain::service::quota::periods::PeriodKeys;
use crate::domain::service::quota::ports::{PreflightRead, QuotaRepository};
use crate::domain::service::quota::test_support::{date, noon_keys};
use crate::domain::service::quota::types::QuotaBucket::{TierPremium, Total};
use crate::domain::service::quota::types::QuotaPeriod::{Daily, Monthly};
use crate::quota_test_env::{BucketSeed, MessageSeed, QuotaEnv};
use crate::test_doubles::{OTHER_TENANT, OTHER_USER, USER, USER_TENANT};

fn read(chat: Uuid) -> PreflightRead {
    PreflightRead {
        tenant_id: USER_TENANT,
        user_id: USER,
        chat_id: chat,
        keys: noon_keys(),
    }
}

// ---- bucket rows -------------------------------------------------------------------------------------

#[tokio::test]
async fn the_four_rows_come_back_in_the_canonical_order_whatever_the_insertion_order() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    env.seed_bucket(&BucketSeed::premium("monthly", 4, 40).tools(0, 0))
        .await;
    env.seed_bucket(&BucketSeed::premium("daily", 2, 20)).await;
    env.seed_bucket(&BucketSeed::total("monthly", 3, 30).tools(7, 8))
        .await;
    env.seed_bucket(&BucketSeed::total("daily", 1, 10).tools(5, 6))
        .await;

    let rows = env.repo.read_preflight(&read(chat)).await.unwrap();

    let order: Vec<_> = rows
        .buckets
        .iter()
        .map(|row| {
            (
                row.period,
                row.bucket,
                row.spent_credits_micro,
                row.reserved_credits_micro,
            )
        })
        .collect();
    assert_eq!(
        order,
        vec![
            (Daily, Total, 1, 10),
            (Daily, TierPremium, 2, 20),
            (Monthly, Total, 3, 30),
            (Monthly, TierPremium, 4, 40),
        ]
    );
    assert_eq!(
        (
            rows.buckets[0].web_search_calls,
            rows.buckets[0].code_interpreter_calls
        ),
        (5, 6)
    );
    assert_eq!(
        (
            rows.buckets[2].web_search_calls,
            rows.buckets[2].code_interpreter_calls
        ),
        (7, 8)
    );
}

#[tokio::test]
async fn a_missing_bucket_row_is_absent_not_zero_filled() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    env.seed_bucket(&BucketSeed::total("daily", 5, 0)).await;

    let rows = env.repo.read_preflight(&read(chat)).await.unwrap();

    assert_eq!(rows.buckets.len(), 1);
    assert_eq!(
        (rows.buckets[0].period, rows.buckets[0].bucket),
        (Daily, Total)
    );
}

#[tokio::test]
async fn a_user_without_rows_reads_nothing_and_no_prior_context() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;

    let rows = env.repo.read_preflight(&read(chat)).await.unwrap();

    assert!(rows.buckets.is_empty());
    assert_eq!(rows.prior_context_tokens, 0);
}

#[tokio::test]
async fn only_the_rows_of_the_requested_period_starts_are_read() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    env.seed_bucket(&BucketSeed::total("daily", 111, 0).at("2026-03-09"))
        .await;
    env.seed_bucket(&BucketSeed::total("daily", 1, 0)).await;
    env.seed_bucket(&BucketSeed::total("monthly", 222, 0).at("2026-02-01"))
        .await;
    env.seed_bucket(&BucketSeed::total("monthly", 2, 0)).await;

    let rows = env.repo.read_preflight(&read(chat)).await.unwrap();

    let spent: Vec<i64> = rows.buckets.iter().map(|r| r.spent_credits_micro).collect();
    assert_eq!(spent, vec![1, 2]);
}

#[tokio::test]
async fn the_period_starts_of_the_request_decide_not_the_clock() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    env.seed_bucket(&BucketSeed::total("daily", 111, 0).at("2026-03-09"))
        .await;
    let mut request = read(chat);
    request.keys = PeriodKeys {
        daily_start: date(2026, 3, 9),
        monthly_start: date(2026, 3, 1),
    };

    let rows = env.repo.read_preflight(&request).await.unwrap();

    assert_eq!(rows.buckets.len(), 1);
    assert_eq!(rows.buckets[0].spent_credits_micro, 111);
}

#[tokio::test]
async fn rows_of_other_users_and_other_tenants_are_never_read() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    env.seed_bucket(&BucketSeed::total("daily", 1, 0)).await;
    env.seed_bucket(&BucketSeed::total("daily", 900, 0).of(USER_TENANT, OTHER_USER))
        .await;
    env.seed_bucket(&BucketSeed::total("daily", 901, 0).of(OTHER_TENANT, USER))
        .await;
    env.seed_bucket(&BucketSeed::total("daily", 902, 0).of(OTHER_TENANT, OTHER_USER))
        .await;

    let rows = env.repo.read_preflight(&read(chat)).await.unwrap();

    assert_eq!(rows.buckets.len(), 1);
    assert_eq!(rows.buckets[0].spent_credits_micro, 1);
}

#[tokio::test]
async fn only_the_total_and_premium_buckets_are_read() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let mut analytics = BucketSeed::total("daily", 999, 0);
    analytics.bucket = "tier:standard";
    env.seed_bucket(&analytics).await;
    env.seed_bucket(&BucketSeed::total("daily", 1, 0)).await;

    let rows = env.repo.read_preflight(&read(chat)).await.unwrap();

    assert_eq!(rows.buckets.len(), 1);
    assert_eq!(rows.buckets[0].bucket, Total);
}

#[tokio::test]
async fn the_preflight_read_writes_nothing() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    env.seed_bucket(&BucketSeed::total("daily", 1, 2)).await;
    env.seed_message(chat, &MessageSeed::assistant(1, 1, 10, 20))
        .await;
    let quota_before = env.quota_dump().await;
    let messages_before = env.dump("messages").await;
    let chats_before = env.dump("chats").await;

    let rows = env.repo.read_preflight(&read(chat)).await.unwrap();

    assert_eq!(rows.prior_context_tokens, 30, "the read ran");
    assert_eq!(env.quota_dump().await, quota_before);
    assert_eq!(env.dump("messages").await, messages_before);
    assert_eq!(env.dump("chats").await, chats_before);
    assert_eq!(env.outbox_message_count().await, 0);
}

// ---- prior context ------------------------------------------------------------------------------------

#[tokio::test]
async fn the_prior_context_is_input_plus_output_of_the_latest_assistant_message() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    env.seed_message(chat, &MessageSeed::assistant(1, 1, 10, 20))
        .await;
    env.seed_message(chat, &MessageSeed::assistant(2, 5, 100, 200))
        .await;

    let rows = env.repo.read_preflight(&read(chat)).await.unwrap();

    assert_eq!(rows.prior_context_tokens, 300);
}

#[tokio::test]
async fn an_assistant_message_with_zero_token_counts_is_skipped() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    env.seed_message(chat, &MessageSeed::assistant(1, 1, 10, 20))
        .await;
    env.seed_message(chat, &MessageSeed::assistant(2, 9, 0, 0))
        .await;

    let rows = env.repo.read_preflight(&read(chat)).await.unwrap();

    assert_eq!(rows.prior_context_tokens, 30);
}

#[tokio::test]
async fn one_nonzero_count_is_enough() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    env.seed_message(chat, &MessageSeed::assistant(1, 1, 0, 7))
        .await;
    assert_eq!(
        env.repo
            .read_preflight(&read(chat))
            .await
            .unwrap()
            .prior_context_tokens,
        7
    );

    env.seed_message(chat, &MessageSeed::assistant(2, 2, 9, 0))
        .await;
    assert_eq!(
        env.repo
            .read_preflight(&read(chat))
            .await
            .unwrap()
            .prior_context_tokens,
        9
    );
}

#[tokio::test]
async fn a_soft_deleted_assistant_message_is_skipped() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    env.seed_message(chat, &MessageSeed::assistant(1, 1, 10, 20))
        .await;
    env.seed_message(chat, &MessageSeed::assistant(2, 9, 1000, 1000).deleted())
        .await;

    let rows = env.repo.read_preflight(&read(chat)).await.unwrap();

    assert_eq!(rows.prior_context_tokens, 30);
}

#[tokio::test]
async fn only_assistant_messages_count() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    env.seed_message(chat, &MessageSeed::assistant(1, 1, 10, 20))
        .await;
    env.seed_message(chat, &MessageSeed::assistant(2, 9, 5000, 5000).role("user"))
        .await;
    env.seed_message(
        chat,
        &MessageSeed::assistant(3, 10, 6000, 6000).role("system"),
    )
    .await;

    let rows = env.repo.read_preflight(&read(chat)).await.unwrap();

    assert_eq!(rows.prior_context_tokens, 30);
}

#[tokio::test]
async fn only_messages_of_the_chat_count() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    let other = env.seed_chat(2).await;
    env.seed_message(chat, &MessageSeed::assistant(1, 1, 10, 20))
        .await;
    env.seed_message(other, &MessageSeed::assistant(2, 9, 4000, 4000))
        .await;

    let rows = env.repo.read_preflight(&read(chat)).await.unwrap();

    assert_eq!(rows.prior_context_tokens, 30);
}

#[tokio::test]
async fn a_tie_on_created_at_is_broken_by_the_greater_id() {
    let env = QuotaEnv::new().await;
    let chat = env.seed_chat(1).await;
    env.seed_message(
        chat,
        &MessageSeed::assistant(1, 5, 1, 1).id(Uuid::from_u128(0xD000_0002)),
    )
    .await;
    env.seed_message(
        chat,
        &MessageSeed::assistant(2, 5, 50, 50).id(Uuid::from_u128(0xD000_0009)),
    )
    .await;
    env.seed_message(
        chat,
        &MessageSeed::assistant(3, 5, 7, 7).id(Uuid::from_u128(0xD000_0003)),
    )
    .await;

    let rows = env.repo.read_preflight(&read(chat)).await.unwrap();

    assert_eq!(rows.prior_context_tokens, 100);
}

// ---- the scoped read of the status -----------------------------------------------------------------

#[tokio::test]
async fn the_scoped_read_returns_the_rows_of_the_scope_owner_only() {
    let env = QuotaEnv::new().await;
    env.seed_bucket(&BucketSeed::total("daily", 1, 0)).await;
    env.seed_bucket(&BucketSeed::premium("monthly", 2, 0)).await;
    env.seed_bucket(&BucketSeed::total("daily", 900, 0).of(USER_TENANT, OTHER_USER))
        .await;
    env.seed_bucket(&BucketSeed::total("daily", 901, 0).of(OTHER_TENANT, USER))
        .await;
    let scope = AccessScope::for_tenant(USER_TENANT).ensure_owner(USER);

    let rows = env.repo.read_buckets(&scope, noon_keys()).await.unwrap();

    let found: Vec<_> = rows
        .iter()
        .map(|r| (r.period, r.bucket, r.spent_credits_micro))
        .collect();
    assert_eq!(found, vec![(Daily, Total, 1), (Monthly, TierPremium, 2)]);
}

#[tokio::test]
async fn the_scoped_read_follows_the_scope_not_the_caller_supplied_ids() {
    let env = QuotaEnv::new().await;
    env.seed_bucket(&BucketSeed::total("daily", 1, 0)).await;
    env.seed_bucket(&BucketSeed::total("daily", 900, 0).of(USER_TENANT, OTHER_USER))
        .await;

    let other_user = AccessScope::for_tenant(USER_TENANT).ensure_owner(OTHER_USER);
    let other_tenant = AccessScope::for_tenant(OTHER_TENANT).ensure_owner(USER);

    let theirs = env
        .repo
        .read_buckets(&other_user, noon_keys())
        .await
        .unwrap();
    let foreign = env
        .repo
        .read_buckets(&other_tenant, noon_keys())
        .await
        .unwrap();

    assert_eq!(theirs.len(), 1);
    assert_eq!(theirs[0].spent_credits_micro, 900);
    assert!(foreign.is_empty());
}

#[tokio::test]
async fn the_scoped_read_returns_only_the_requested_periods_in_canonical_order() {
    let env = QuotaEnv::new().await;
    env.seed_bucket(&BucketSeed::premium("monthly", 4, 0)).await;
    env.seed_bucket(&BucketSeed::total("daily", 1, 0).at("2026-03-09"))
        .await;
    env.seed_bucket(&BucketSeed::total("monthly", 3, 0)).await;
    env.seed_bucket(&BucketSeed::total("daily", 1, 0)).await;
    let scope = AccessScope::for_tenant(USER_TENANT).ensure_owner(USER);

    let rows = env.repo.read_buckets(&scope, noon_keys()).await.unwrap();

    let order: Vec<_> = rows.iter().map(|r| (r.period, r.bucket)).collect();
    assert_eq!(
        order,
        vec![(Daily, Total), (Monthly, Total), (Monthly, TierPremium)]
    );
}

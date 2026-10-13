//! Quota service tests.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::missing_panics_doc, clippy::many_single_char_names)]

mod arith;
mod preflight;
mod settle;
mod status;

use std::sync::Arc;

use sea_orm::{ActiveModelTrait, ColumnTrait, Condition, EntityTrait, QueryFilter};
use time::{Date, OffsetDateTime};
use toolkit_db::secure::{SecureEntityExt, SecureInsertExt};
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::{PreflightDecision, PreflightRequest, ToolInputs};
use crate::domain::error::DomainError;
use crate::domain::services::AppServices;
use crate::infra::db::entities::{chat_turn, quota_usage};
use crate::testing::{TENANT_A, USER_A1};

/// Preflight request of `USER_A1` with a 400-byte message (220 estimated text tokens).
pub fn req(selected: &str) -> PreflightRequest {
    PreflightRequest {
        tenant_id: TENANT_A,
        user_id: USER_A1,
        selected_model: selected.to_owned(),
        message_bytes: 400,
        image_count: 0,
        prior_context_tokens: 0,
        tools: ToolInputs::default(),
        now: crate::clock::now(),
    }
}

/// Inserts (or replaces) a bucket row of `USER_A1`.
pub async fn put_row(
    app: &AppServices,
    period_type: &str,
    start: Date,
    bucket: &str,
    f: impl FnOnce(&mut quota_usage::Model),
) {
    let mut m = quota_usage::Model {
        id: Uuid::new_v4(),
        tenant_id: TENANT_A,
        user_id: USER_A1,
        period_type: period_type.to_owned(),
        period_start: start,
        bucket: bucket.to_owned(),
        spent_credits_micro: 0,
        reserved_credits_micro: 0,
        calls: 0,
        input_tokens: 0,
        output_tokens: 0,
        file_search_calls: 0,
        web_search_calls: 0,
        code_interpreter_calls: 0,
        rag_retrieval_calls: 0,
        image_inputs: 0,
        image_upload_bytes: 0,
        updated_at: crate::clock::now(),
    };
    f(&mut m);
    let am = quota_usage::ActiveModel::from(m).reset_all();
    let conn = app.db.conn().unwrap();
    quota_usage::Entity::insert(am).secure().scope_unchecked(&AccessScope::allow_all()).unwrap().exec(&conn).await.unwrap();
}

/// Reads a bucket row of `USER_A1`.
pub async fn get_row(app: &AppServices, period_type: &str, start: Date, bucket: &str) -> Option<quota_usage::Model> {
    let conn = app.db.conn().unwrap();
    quota_usage::Entity::find()
        .filter(
            Condition::all()
                .add(quota_usage::Column::TenantId.eq(TENANT_A))
                .add(quota_usage::Column::UserId.eq(USER_A1))
                .add(quota_usage::Column::PeriodType.eq(period_type))
                .add(quota_usage::Column::PeriodStart.eq(start))
                .add(quota_usage::Column::Bucket.eq(bucket)),
        )
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
}

/// Number of quota rows in the database.
pub async fn row_count(app: &AppServices) -> usize {
    let conn = app.db.conn().unwrap();
    quota_usage::Entity::find().secure().scope_with(&AccessScope::allow_all()).all(&conn).await.unwrap().len()
}

/// Books the decision's reserve in its own transaction.
pub async fn book(app: &Arc<AppServices>, decision: &PreflightDecision) -> Result<(), DomainError> {
    let d = decision.clone();
    app.db
        .transaction(move |tx| Box::pin(async move { super::reserve(tx, TENANT_A, USER_A1, &d).await }))
        .await
}

/// An in-memory turn with the reserve columns of a decision (not persisted).
pub fn turn_of(decision: Option<&PreflightDecision>, started_at: OffsetDateTime) -> chat_turn::Model {
    chat_turn::Model {
        id: Uuid::new_v4(),
        tenant_id: TENANT_A,
        chat_id: Uuid::new_v4(),
        request_id: Uuid::new_v4(),
        requester_type: "user".to_owned(),
        requester_user_id: Some(USER_A1),
        state: "running".to_owned(),
        provider_name: None,
        provider_response_id: None,
        assistant_message_id: None,
        error_code: None,
        reserve_tokens: decision.map(|d| d.reserve_tokens),
        max_output_tokens_applied: decision.map(|d| i32::try_from(d.max_output_tokens_applied).unwrap()),
        reserved_credits_micro: decision.map(|d| d.reserved_credits_micro),
        policy_version_applied: decision.map(|d| d.policy_version),
        effective_model: decision.map(|d| d.effective_model.id.clone()),
        minimal_generation_floor_applied: decision.map(|d| i32::try_from(d.minimal_generation_floor_applied).unwrap()),
        error_detail: None,
        deleted_at: None,
        replaced_by_request_id: None,
        started_at,
        last_progress_at: Some(started_at),
        web_search_enabled: false,
        web_search_completed_count: 0,
        code_interpreter_completed_count: 0,
        file_search_completed_count: 0,
        completed_at: None,
        updated_at: started_at,
    }
}

/// Asserts a 429 `quota_exceeded` with the given subject.
pub fn assert_quota_exceeded(err: &DomainError, scope: &str) {
    match err {
        DomainError::ResourceExhausted { subject, description, .. } => {
            assert_eq!(subject, scope, "{err:?}");
            assert_eq!(description, "quota_exceeded");
        }
        other => panic!("expected quota_exceeded({scope}), got {other:?}"),
    }
}

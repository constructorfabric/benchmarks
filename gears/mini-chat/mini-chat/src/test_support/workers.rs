//! Fixtures of the worker tests (orphan watchdog, upload reaper): rows as a crashed process
//! leaves them, written straight through the entities.

use sea_orm::ActiveValue::NotSet;
use sea_orm::Set;
use time::OffsetDateTime;
use toolkit_db::Db;
use toolkit_db::secure::{AccessScope, secure_insert};
use uuid::Uuid;

use crate::domain::quota::periods::period_starts;
use crate::domain::quota::{Bucket, Period};
use crate::infra::db::entity::{attachments, chat_turns};
use crate::infra::db::repo::quota_usage::{self as quota_repo, BucketDelta, BucketKey};
use crate::infra::db::ts::normalize;

/// The reserve columns the preflight writes on a `running` turn.
#[derive(Debug, Clone)]
pub struct SeedReserve {
    pub reserve_tokens: i64,
    pub max_output_tokens_applied: i32,
    pub reserved_credits_micro: i64,
    pub policy_version_applied: i64,
    pub effective_model: &'static str,
    pub minimal_generation_floor_applied: i32,
}

/// A `running` turn to seed.
#[derive(Debug, Clone)]
pub struct SeedTurn {
    pub tenant: Uuid,
    pub chat: Uuid,
    pub user: Option<Uuid>,
    pub reserve: Option<SeedReserve>,
    pub started_at: OffsetDateTime,
    pub last_progress_at: Option<OffsetDateTime>,
    pub web_search_completed_count: i32,
}

impl SeedTurn {
    /// A `running` turn of `user` with the reserve columns, started and last seen at `at`.
    pub fn reserved(
        tenant: Uuid,
        chat: Uuid,
        user: Uuid,
        reserve: SeedReserve,
        at: OffsetDateTime,
    ) -> Self {
        Self {
            tenant,
            chat,
            user: Some(user),
            reserve: Some(reserve),
            started_at: at,
            last_progress_at: Some(at),
            web_search_completed_count: 0,
        }
    }

    /// A `running` turn of `user` without reserve fields (a retry/edit turn the process left
    /// before its preflight values were written), started and last seen at `at`.
    pub fn unreserved(tenant: Uuid, chat: Uuid, user: Uuid, at: OffsetDateTime) -> Self {
        Self {
            tenant,
            chat,
            user: Some(user),
            reserve: None,
            started_at: at,
            last_progress_at: Some(at),
            web_search_completed_count: 0,
        }
    }
}

/// Inserts the turn and returns `(turn id, request id)`.
pub async fn seed_running_turn(db: &Db, t: &SeedTurn) -> (Uuid, Uuid) {
    let (id, request_id) = (Uuid::new_v4(), Uuid::new_v4());
    let r = t.reserve.as_ref();
    let row = chat_turns::ActiveModel {
        id: Set(id),
        tenant_id: Set(t.tenant),
        chat_id: Set(t.chat),
        request_id: Set(request_id),
        requester_type: Set("user".to_owned()),
        requester_user_id: Set(t.user),
        state: Set("running".to_owned()),
        provider_name: NotSet,
        provider_response_id: NotSet,
        assistant_message_id: NotSet,
        error_code: NotSet,
        reserve_tokens: Set(r.map(|r| r.reserve_tokens)),
        max_output_tokens_applied: Set(r.map(|r| r.max_output_tokens_applied)),
        reserved_credits_micro: Set(r.map(|r| r.reserved_credits_micro)),
        policy_version_applied: Set(r.map(|r| r.policy_version_applied)),
        effective_model: Set(r.map(|r| r.effective_model.to_owned())),
        minimal_generation_floor_applied: Set(r.map(|r| r.minimal_generation_floor_applied)),
        error_detail: NotSet,
        deleted_at: NotSet,
        replaced_by_request_id: NotSet,
        started_at: Set(normalize(t.started_at)),
        last_progress_at: Set(t.last_progress_at.map(normalize)),
        web_search_enabled: Set(false),
        web_search_completed_count: Set(t.web_search_completed_count),
        code_interpreter_completed_count: NotSet,
        file_search_completed_count: NotSet,
        completed_at: NotSet,
        updated_at: Set(normalize(t.started_at)),
    };
    let conn = db.conn().expect("conn");
    secure_insert::<chat_turns::Entity>(row, &AccessScope::allow_all(), &conn)
        .await
        .expect("seed running turn");
    (id, request_id)
}

/// Books `credits` of reserve on the `total` and `tier:premium` rows of both periods of `at`
/// (what a premium turn's reserve transaction leaves behind).
pub async fn seed_premium_reserve(
    db: &Db,
    tenant: Uuid,
    user: Uuid,
    at: OffsetDateTime,
    credits: i64,
) {
    let starts = period_starts(at);
    let conn = db.conn().expect("conn");
    for period in Period::ALL {
        for bucket in [Bucket::Total, Bucket::Premium] {
            let key = BucketKey {
                tenant_id: tenant,
                user_id: user,
                period,
                period_start: starts.start(period),
                bucket,
            };
            let delta = BucketDelta {
                reserved_credits_micro: credits,
                ..BucketDelta::default()
            };
            quota_repo::add(&conn, &key, &delta)
                .await
                .expect("seed reserve");
        }
    }
}

/// An upload row to seed.
#[derive(Debug, Clone)]
pub struct SeedUpload {
    pub tenant: Uuid,
    pub chat: Uuid,
    pub uploader: Uuid,
    /// `pending` or `uploaded`.
    pub status: &'static str,
    /// Provider file id (`None` before the provider upload finished).
    pub provider_file_id: Option<String>,
    pub updated_at: OffsetDateTime,
    pub cleanup_status: Option<&'static str>,
}

/// Inserts the attachment row and returns its id.
pub async fn seed_upload(db: &Db, u: &SeedUpload) -> Uuid {
    let id = Uuid::new_v4();
    let at = normalize(u.updated_at);
    let row = attachments::ActiveModel {
        id: Set(id),
        tenant_id: Set(u.tenant),
        chat_id: Set(u.chat),
        uploaded_by_user_id: Set(u.uploader),
        filename: Set("report.pdf".to_owned()),
        content_type: Set("application/pdf".to_owned()),
        size_bytes: Set(10),
        storage_backend: Set("openai".to_owned()),
        provider_file_id: Set(u.provider_file_id.clone()),
        status: Set(u.status.to_owned()),
        error_code: NotSet,
        attachment_kind: Set("document".to_owned()),
        for_file_search: Set(true),
        for_code_interpreter: Set(false),
        doc_summary: NotSet,
        img_thumbnail: NotSet,
        img_thumbnail_width: NotSet,
        img_thumbnail_height: NotSet,
        summary_model: NotSet,
        summary_updated_at: NotSet,
        cleanup_status: Set(u.cleanup_status.map(str::to_owned)),
        cleanup_attempts: NotSet,
        last_cleanup_error: NotSet,
        cleanup_updated_at: NotSet,
        created_at: Set(at),
        updated_at: Set(at),
        deleted_at: NotSet,
        secondary_file_id: NotSet,
        secondary_status: NotSet,
        secondary_provider_kind: NotSet,
    };
    let conn = db.conn().expect("conn");
    secure_insert::<attachments::Entity>(row, &AccessScope::allow_all(), &conn)
        .await
        .expect("seed upload");
    id
}

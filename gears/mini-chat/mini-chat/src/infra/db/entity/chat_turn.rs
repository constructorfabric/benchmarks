//! `chat_turns` table.
use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db::secure::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "chat_turns")]
#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub requester_type: String,
    pub requester_user_id: Option<Uuid>,
    pub state: String,
    pub provider_name: Option<String>,
    pub provider_response_id: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub error_code: Option<String>,
    pub reserve_tokens: Option<i64>,
    pub max_output_tokens_applied: Option<i32>,
    pub reserved_credits_micro: Option<i64>,
    pub policy_version_applied: Option<i64>,
    pub effective_model: Option<String>,
    pub minimal_generation_floor_applied: Option<i32>,
    pub error_detail: Option<String>,
    pub deleted_at: Option<OffsetDateTime>,
    pub replaced_by_request_id: Option<Uuid>,
    pub started_at: OffsetDateTime,
    pub last_progress_at: Option<OffsetDateTime>,
    pub web_search_enabled: bool,
    pub web_search_completed_count: i32,
    pub code_interpreter_completed_count: i32,
    pub file_search_completed_count: i32,
    pub completed_at: Option<OffsetDateTime>,
    pub updated_at: OffsetDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}


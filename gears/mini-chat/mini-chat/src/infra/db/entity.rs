//! SeaORM entities with Secure ORM scoping.
//!
//! Every table is tenant-scoped (`tenant_id`). `chats`, `quota_usage` and
//! `message_reactions` are also owner-scoped (`user_id`); child tables of a
//! chat are reached through an owner-scoped chat query first.

pub mod chats {
    use sea_orm::entity::prelude::*;
    use time::OffsetDateTime;
    use toolkit_db::secure::Scopable;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "chats")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", owner_col = "user_id", no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub user_id: Uuid,
        pub model: String,
        pub title: Option<String>,
        pub is_temporary: bool,
        pub created_at: OffsetDateTime,
        pub updated_at: OffsetDateTime,
        pub deleted_at: Option<OffsetDateTime>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod messages {
    use sea_orm::entity::prelude::*;
    use time::OffsetDateTime;
    use toolkit_db::secure::Scopable;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "messages")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub chat_id: Uuid,
        pub request_id: Option<Uuid>,
        pub role: String,
        pub content: String,
        pub content_type: String,
        pub token_estimate: i32,
        pub provider_response_id: Option<String>,
        pub request_kind: String,
        pub features_used: Json,
        pub input_tokens: i64,
        pub output_tokens: i64,
        pub cache_read_input_tokens: i64,
        pub cache_write_input_tokens: i64,
        pub reasoning_tokens: i64,
        pub model: Option<String>,
        pub is_compressed: bool,
        pub created_at: OffsetDateTime,
        pub deleted_at: Option<OffsetDateTime>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod chat_turns {
    use sea_orm::entity::prelude::*;
    use time::OffsetDateTime;
    use toolkit_db::secure::Scopable;

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
}

pub mod attachments {
    use sea_orm::entity::prelude::*;
    use time::OffsetDateTime;
    use toolkit_db::secure::Scopable;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "attachments")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub chat_id: Uuid,
        pub uploaded_by_user_id: Uuid,
        pub filename: String,
        pub content_type: String,
        pub size_bytes: i64,
        pub storage_backend: String,
        pub provider_file_id: Option<String>,
        pub status: String,
        pub error_code: Option<String>,
        pub attachment_kind: String,
        pub for_file_search: bool,
        pub for_code_interpreter: bool,
        pub doc_summary: Option<String>,
        pub img_thumbnail: Option<Vec<u8>>,
        pub img_thumbnail_width: Option<i32>,
        pub img_thumbnail_height: Option<i32>,
        pub summary_model: Option<String>,
        pub summary_updated_at: Option<OffsetDateTime>,
        pub cleanup_status: Option<String>,
        pub cleanup_attempts: i32,
        pub last_cleanup_error: Option<String>,
        pub cleanup_updated_at: Option<OffsetDateTime>,
        pub created_at: OffsetDateTime,
        pub updated_at: OffsetDateTime,
        pub deleted_at: Option<OffsetDateTime>,
        pub secondary_file_id: Option<String>,
        pub secondary_status: String,
        pub secondary_provider_kind: Option<String>,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod message_attachments {
    use sea_orm::entity::prelude::*;
    use time::OffsetDateTime;
    use toolkit_db::secure::Scopable;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "message_attachments")]
    #[secure(tenant_col = "tenant_id", no_resource, no_owner, no_type)]
    pub struct Model {
        pub tenant_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub chat_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub message_id: Uuid,
        #[sea_orm(primary_key, auto_increment = false)]
        pub attachment_id: Uuid,
        pub created_at: OffsetDateTime,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod thread_summaries {
    use sea_orm::entity::prelude::*;
    use time::OffsetDateTime;
    use toolkit_db::secure::Scopable;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "thread_summaries")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub chat_id: Uuid,
        pub summary_text: String,
        pub summarized_up_to_created_at: OffsetDateTime,
        pub summarized_up_to_message_id: Uuid,
        pub token_estimate: i32,
        pub created_at: OffsetDateTime,
        pub updated_at: OffsetDateTime,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod chat_vector_stores {
    use sea_orm::entity::prelude::*;
    use time::OffsetDateTime;
    use toolkit_db::secure::Scopable;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "chat_vector_stores")]
    #[secure(tenant_col = "tenant_id", no_resource, no_owner, no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub chat_id: Uuid,
        pub vector_store_id: Option<String>,
        pub provider: String,
        pub file_count: i32,
        pub created_at: OffsetDateTime,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod quota_usage {
    use sea_orm::entity::prelude::*;
    use time::{Date, OffsetDateTime};
    use toolkit_db::secure::Scopable;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "quota_usage")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", owner_col = "user_id", no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub tenant_id: Uuid,
        pub user_id: Uuid,
        pub period_type: String,
        pub period_start: Date,
        pub bucket: String,
        pub spent_credits_micro: i64,
        pub reserved_credits_micro: i64,
        pub calls: i32,
        pub input_tokens: i64,
        pub output_tokens: i64,
        pub file_search_calls: i32,
        pub web_search_calls: i32,
        pub code_interpreter_calls: i32,
        pub rag_retrieval_calls: i32,
        pub image_inputs: i32,
        pub image_upload_bytes: i64,
        pub updated_at: OffsetDateTime,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

pub mod message_reactions {
    use sea_orm::entity::prelude::*;
    use time::OffsetDateTime;
    use toolkit_db::secure::Scopable;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
    #[sea_orm(table_name = "message_reactions")]
    #[secure(tenant_col = "tenant_id", resource_col = "id", owner_col = "user_id", no_type)]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub id: Uuid,
        pub message_id: Uuid,
        pub user_id: Uuid,
        pub tenant_id: Uuid,
        pub reaction: String,
        pub created_at: OffsetDateTime,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}

    impl ActiveModelBehavior for ActiveModel {}
}

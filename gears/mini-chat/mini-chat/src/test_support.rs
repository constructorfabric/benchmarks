//! Test helpers (tests only).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use mini_chat_sdk::{
    KillSwitches, ModelCatalogEntry, PolicySnapshot, PublishError, TierLimits, UsageEvent,
    UserLimits,
};
use sea_orm::ActiveValue::Set;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use toolkit_db::migration_runner::run_migrations_for_testing;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};
use toolkit_db::secure::secure_insert;
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::DomainError;
use crate::domain::ports::{AuthzPort, ChatAction, PolicyProvider};
use crate::infra::db::all_migrations;
use crate::infra::db::entities::{attachment, chat, chat_turn, message, message_attachment};
use crate::infra::outbox::{MiniChatOutbox, OutboxHandlers};

/// Fresh, uniquely named shared-cache in-memory `SQLite` database with every
/// gear and outbox migration applied.
pub async fn test_db() -> toolkit_db::Db {
    let dsn = format!(
        "sqlite:file:mini-chat-{}?mode=memory&cache=shared",
        Uuid::new_v4()
    );
    let opts = ConnectOpts {
        max_conns: Some(4),
        min_conns: Some(1),
        ..Default::default()
    };
    let db = connect_db(&dsn, opts)
        .await
        .expect("connect in-memory sqlite test database");
    run_migrations_for_testing(&db, all_migrations())
        .await
        .expect("apply mini-chat migrations");
    db
}

/// A [`DBProvider`] over a fresh [`test_db`].
pub async fn test_provider() -> Arc<DBProvider<DomainError>> {
    Arc::new(DBProvider::new(test_db().await))
}

// ── Domain fakes ─────────────────────────────────────────────────────────────

/// Caller context with fresh subject and tenant ids.
pub fn test_ctx() -> SecurityContext {
    SecurityContext::builder()
        .subject_id(Uuid::new_v4())
        .subject_tenant_id(Uuid::new_v4())
        .build()
        .expect("security context")
}

/// Minimal valid catalog entry (`provider_id` / `provider_model_id` are
/// deliberately distinct from `id` so leaks are detectable).
pub fn catalog_entry(id: &str, enabled: bool) -> ModelCatalogEntry {
    serde_json::from_value(serde_json::json!({
        "id": id,
        "provider_model_id": format!("prov-model-{id}"),
        "display_name": format!("Model {id}"),
        "provider_id": "secret-provider",
        "provider_display_name": "Secret Provider",
        "tier": "premium",
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 100_000,
        "input_tokens_credit_multiplier_micro": 1_500_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "max_num_results": 5,
        "general_config": {
            "type": "",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {"stop": []},
            "features": {"streaming": true, "structured_output": false},
            "tool_support": {
                "web_search": true, "file_search": true, "image_generation": false,
                "code_interpreter": false, "mcp": false
            },
            "supported_endpoints": {
                "chat_completions": true, "responses": true, "embeddings": false,
                "image_generation": false, "audio_speech_generation": false,
                "audio_transcription": false, "audio_translation": false
            }
        },
        "description": format!("About {id}"),
        "multiplier_display": "2x",
        "enabled": enabled,
        "multimodal_capabilities": ["VISION_INPUT", "RAG"],
        "preference": {"is_default": true, "sort_order": 1}
    }))
    .expect("catalog entry fixture")
}

/// Snapshot (version 1, no kill switch) over `catalog`.
pub fn snapshot(catalog: Vec<ModelCatalogEntry>) -> PolicySnapshot {
    PolicySnapshot {
        policy_version: 1,
        model_catalog: catalog,
        kill_switches: KillSwitches {
            disable_premium_tier: false,
            force_standard_tier: false,
            disable_web_search: false,
            disable_file_search: false,
            disable_images: false,
            disable_code_interpreter: false,
        },
    }
}

/// [`AuthzPort`] fake: tenant + owner scope for the caller; records model and
/// chat actions; denies everything when `deny`.
#[derive(Default)]
pub struct FakeAuthz {
    pub deny: bool,
    pub model_actions: Mutex<Vec<String>>,
    pub chat_actions: Mutex<Vec<ChatAction>>,
}

impl FakeAuthz {
    pub fn denying() -> Self {
        Self {
            deny: true,
            ..Self::default()
        }
    }

    pub fn model_actions(&self) -> Vec<String> {
        self.model_actions.lock().unwrap().clone()
    }

    pub fn chat_actions(&self) -> Vec<ChatAction> {
        self.chat_actions.lock().unwrap().clone()
    }

    fn check(&self) -> Result<(), DomainError> {
        if self.deny {
            Err(DomainError::AuthzDenied)
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl AuthzPort for FakeAuthz {
    async fn chat_scope(
        &self,
        ctx: &SecurityContext,
        action: ChatAction,
        _chat_id: Option<Uuid>,
    ) -> Result<AccessScope, DomainError> {
        self.chat_actions.lock().unwrap().push(action);
        self.check()?;
        Ok(AccessScope::for_tenant(ctx.subject_tenant_id()).ensure_owner(ctx.subject_id()))
    }

    async fn model_access(&self, _ctx: &SecurityContext, action: &str) -> Result<(), DomainError> {
        self.model_actions.lock().unwrap().push(action.to_owned());
        self.check()
    }

    async fn quota_scope(&self, ctx: &SecurityContext) -> Result<AccessScope, DomainError> {
        self.check()?;
        Ok(AccessScope::for_tenant(ctx.subject_tenant_id()).ensure_owner(ctx.subject_id()))
    }
}

/// [`PolicyProvider`] fake serving one fixed snapshot (and optional limits,
/// returned for every user with that user's id); counts `current` calls.
pub struct FakePolicy {
    pub snapshot: Arc<PolicySnapshot>,
    pub limits: Option<(TierLimits, TierLimits)>,
    pub current_calls: Mutex<u32>,
}

impl FakePolicy {
    pub fn new(snapshot: PolicySnapshot) -> Self {
        Self {
            snapshot: Arc::new(snapshot),
            limits: None,
            current_calls: Mutex::new(0),
        }
    }

    /// Serves `standard` / `premium` limits at the snapshot's version.
    pub fn with_limits(
        snapshot: PolicySnapshot,
        standard: TierLimits,
        premium: TierLimits,
    ) -> Self {
        Self {
            limits: Some((standard, premium)),
            ..Self::new(snapshot)
        }
    }

    pub fn current_calls(&self) -> u32 {
        *self.current_calls.lock().unwrap()
    }
}

#[async_trait]
impl PolicyProvider for FakePolicy {
    async fn current(&self, _user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError> {
        *self.current_calls.lock().unwrap() += 1;
        Ok(Arc::clone(&self.snapshot))
    }

    async fn snapshot(
        &self,
        _user_id: Uuid,
        _version: u64,
    ) -> Result<Arc<PolicySnapshot>, DomainError> {
        Ok(Arc::clone(&self.snapshot))
    }

    async fn user_limits(&self, user_id: Uuid, version: u64) -> Result<UserLimits, DomainError> {
        let (standard, premium) = self
            .limits
            .ok_or_else(|| DomainError::Internal("FakePolicy has no limits".to_owned()))?;
        Ok(UserLimits {
            user_id,
            policy_version: version,
            standard,
            premium,
        })
    }

    async fn publish_usage(&self, _ev: UsageEvent) -> Result<(), PublishError> {
        Ok(())
    }
}

/// [`PolicyProvider`] that panics on every call (proves a code path never
/// reaches the policy plugin).
pub struct PanicPolicy;

#[async_trait]
impl PolicyProvider for PanicPolicy {
    async fn current(&self, _user_id: Uuid) -> Result<Arc<PolicySnapshot>, DomainError> {
        panic!("PanicPolicy::current called");
    }

    async fn snapshot(
        &self,
        _user_id: Uuid,
        _version: u64,
    ) -> Result<Arc<PolicySnapshot>, DomainError> {
        panic!("PanicPolicy::snapshot called");
    }

    async fn user_limits(&self, _user_id: Uuid, _version: u64) -> Result<UserLimits, DomainError> {
        panic!("PanicPolicy::user_limits called");
    }

    async fn publish_usage(&self, _ev: UsageEvent) -> Result<(), PublishError> {
        panic!("PanicPolicy::publish_usage called");
    }
}

// ── Outbox and seeding helpers ───────────────────────────────────────────────

/// Context for an explicit tenant and user.
pub fn ctx_for(tenant_id: Uuid, user_id: Uuid) -> SecurityContext {
    SecurityContext::builder()
        .subject_id(user_id)
        .subject_tenant_id(tenant_id)
        .build()
        .expect("security context")
}

/// File-backed WAL database with every migration applied. Use it when the
/// outbox pipeline runs: shared-cache in-memory `SQLite` uses table locks and
/// deadlocks the concurrent outbox workers.
pub async fn test_file_db() -> (tempfile::TempDir, toolkit_db::Db) {
    let dir = tempfile::tempdir().expect("temp dir");
    let dsn = format!(
        "sqlite://{}?mode=rwc&journal_mode=wal",
        dir.path().join("mini-chat.db").display()
    );
    let db = connect_db(
        &dsn,
        ConnectOpts {
            max_conns: Some(5),
            min_conns: Some(1),
            ..Default::default()
        },
    )
    .await
    .expect("connect file sqlite test database");
    run_migrations_for_testing(&db, all_migrations())
        .await
        .expect("apply mini-chat migrations");
    (dir, db)
}

/// Forwards `(payload_type, payload)` of every delivered message to a channel.
struct Recorder(mpsc::UnboundedSender<(String, serde_json::Value)>);

#[async_trait]
impl LeasedMessageHandler for Recorder {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let body = serde_json::from_slice(&msg.payload).expect("JSON outbox payload");
        self.0.send((msg.payload_type.clone(), body)).ok();
        MessageResult::Ok
    }
}

/// Started outbox whose every queue records delivered messages.
pub async fn test_outbox(
    db: toolkit_db::Db,
) -> (
    Arc<MiniChatOutbox>,
    mpsc::UnboundedReceiver<(String, serde_json::Value)>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let rec = || -> Arc<dyn LeasedMessageHandler> { Arc::new(Recorder(tx.clone())) };
    let handlers = OutboxHandlers {
        usage: rec(),
        audit: rec(),
        attachment_cleanup: rec(),
        chat_cleanup: rec(),
        thread_summary: rec(),
    };
    let outbox = MiniChatOutbox::start(
        db,
        &OutboxConfig::default(),
        std::time::Duration::from_secs(300),
        handlers,
    )
    .await
    .expect("outbox starts");
    (Arc::new(outbox), rx)
}

/// Inserts a live chat owned by `ctx` with explicit timestamps.
pub async fn seed_chat(
    db: &DBProvider<DomainError>,
    ctx: &SecurityContext,
    title: Option<&str>,
    updated_at: OffsetDateTime,
) -> chat::Model {
    let conn = db.conn().expect("conn");
    secure_insert::<chat::Entity>(
        chat::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(ctx.subject_tenant_id()),
            user_id: Set(ctx.subject_id()),
            model: Set("b".to_owned()),
            title: Set(title.map(str::to_owned)),
            is_temporary: Set(false),
            created_at: Set(updated_at),
            updated_at: Set(updated_at),
            deleted_at: Set(None),
        },
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .expect("seed chat")
}

/// Inserts a message (`request_id` random unless given).
pub async fn seed_message(
    db: &DBProvider<DomainError>,
    chat: &chat::Model,
    role: &str,
    created_at: OffsetDateTime,
) -> message::Model {
    let conn = db.conn().expect("conn");
    secure_insert::<message::Entity>(
        message::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(chat.tenant_id),
            chat_id: Set(chat.id),
            request_id: Set(Some(Uuid::new_v4())),
            role: Set(role.to_owned()),
            content: Set(format!("{role} message")),
            content_type: Set("text".to_owned()),
            token_estimate: Set(1),
            provider_response_id: Set(None),
            request_kind: Set("chat".to_owned()),
            features_used: Set(serde_json::json!([])),
            input_tokens: Set(0),
            output_tokens: Set(0),
            cache_read_input_tokens: Set(0),
            cache_write_input_tokens: Set(0),
            reasoning_tokens: Set(0),
            model: Set(None),
            is_compressed: Set(false),
            created_at: Set(created_at),
            deleted_at: Set(None),
        },
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .expect("seed message")
}

/// Inserts a message from a full active model (fields the caller wants to
/// control), defaulting nothing.
pub async fn insert_message(
    db: &DBProvider<DomainError>,
    am: message::ActiveModel,
) -> message::Model {
    let conn = db.conn().expect("conn");
    secure_insert::<message::Entity>(am, &AccessScope::allow_all(), &conn)
        .await
        .expect("insert message")
}

/// Inserts a turn in `state`.
pub async fn seed_turn(
    db: &DBProvider<DomainError>,
    chat: &chat::Model,
    request_id: Uuid,
    state: &str,
    started_at: OffsetDateTime,
) -> chat_turn::Model {
    let conn = db.conn().expect("conn");
    secure_insert::<chat_turn::Entity>(
        chat_turn::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(chat.tenant_id),
            chat_id: Set(chat.id),
            request_id: Set(request_id),
            requester_type: Set("user".to_owned()),
            requester_user_id: Set(Some(chat.user_id)),
            state: Set(state.to_owned()),
            provider_name: Set(None),
            provider_response_id: Set(None),
            assistant_message_id: Set(None),
            error_code: Set(None),
            reserve_tokens: Set(None),
            max_output_tokens_applied: Set(None),
            reserved_credits_micro: Set(None),
            policy_version_applied: Set(None),
            effective_model: Set(None),
            minimal_generation_floor_applied: Set(None),
            error_detail: Set(None),
            deleted_at: Set(None),
            replaced_by_request_id: Set(None),
            started_at: Set(started_at),
            last_progress_at: Set(None),
            web_search_enabled: Set(false),
            web_search_completed_count: Set(0),
            code_interpreter_completed_count: Set(0),
            file_search_completed_count: Set(0),
            completed_at: Set(None),
            updated_at: Set(started_at),
        },
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .expect("seed turn")
}

/// Inserts a turn from a full active model.
pub async fn insert_turn(
    db: &DBProvider<DomainError>,
    am: chat_turn::ActiveModel,
) -> chat_turn::Model {
    let conn = db.conn().expect("conn");
    secure_insert::<chat_turn::Entity>(am, &AccessScope::allow_all(), &conn)
        .await
        .expect("insert turn")
}

/// Inserts an attachment of `kind` in `status` (thumbnail bytes optional).
pub async fn seed_attachment(
    db: &DBProvider<DomainError>,
    chat: &chat::Model,
    kind: &str,
    status: &str,
    thumbnail: Option<Vec<u8>>,
) -> attachment::Model {
    let conn = db.conn().expect("conn");
    let now = crate::domain::time::db_now();
    let has_thumb = thumbnail.is_some();
    secure_insert::<attachment::Entity>(
        attachment::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(chat.tenant_id),
            chat_id: Set(chat.id),
            uploaded_by_user_id: Set(chat.user_id),
            filename: Set(format!("{kind}.bin")),
            content_type: Set("application/octet-stream".to_owned()),
            size_bytes: Set(10),
            storage_backend: Set("openai".to_owned()),
            provider_file_id: Set(Some("file-secret".to_owned())),
            status: Set(status.to_owned()),
            error_code: Set(None),
            attachment_kind: Set(kind.to_owned()),
            for_file_search: Set(kind == "document"),
            for_code_interpreter: Set(false),
            doc_summary: Set(None),
            img_thumbnail: Set(thumbnail),
            img_thumbnail_width: Set(has_thumb.then_some(64)),
            img_thumbnail_height: Set(has_thumb.then_some(32)),
            summary_model: Set(None),
            summary_updated_at: Set(None),
            cleanup_status: Set(None),
            cleanup_attempts: Set(0),
            last_cleanup_error: Set(None),
            cleanup_updated_at: Set(None),
            created_at: Set(now),
            updated_at: Set(now),
            deleted_at: Set(None),
            secondary_file_id: Set(None),
            secondary_status: Set("not_attempted".to_owned()),
            secondary_provider_kind: Set(None),
        },
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .expect("seed attachment")
}

/// Links `attachment_id` to `message`.
pub async fn link_attachment(
    db: &DBProvider<DomainError>,
    message: &message::Model,
    attachment_id: Uuid,
) {
    let conn = db.conn().expect("conn");
    secure_insert::<message_attachment::Entity>(
        message_attachment::ActiveModel {
            tenant_id: Set(message.tenant_id),
            chat_id: Set(message.chat_id),
            message_id: Set(message.id),
            attachment_id: Set(attachment_id),
            created_at: Set(crate::domain::time::db_now()),
        },
        &AccessScope::allow_all(),
        &conn,
    )
    .await
    .expect("link attachment");
}

//! Chats, messages, reactions, turn status, models and quota status.

use std::collections::HashMap;

use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait, QueryFilter, QueryOrder, Set};
use toolkit_db::odata::{FieldMap, LimitCfg, paginate_with_odata};
use toolkit_db::secure::{DBRunner, SecureDeleteExt, SecureEntityExt, SecureInsertExt, SecureUpdateExt};
use toolkit_odata::filter::FieldKind;
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz::actions;
use crate::domain::error::DomainError;
use crate::domain::odata_compat;
use crate::domain::quota::{PeriodStarts, PeriodStatus, read_usage, status_entries};
use crate::domain::service::{MiniChat, owned_chat_cond};
use crate::domain::views::{
    AttachmentSummaryView, ChatView, MessageView, ModelView, ReactionView, TurnStatusView, thumbnail_of,
};
use crate::infra::db::entity::{
    attachments, chat_turns, chats, message_attachments, message_reactions, messages,
};
use crate::infra::db::now;
use crate::infra::outbox::{ChatCleanupEvent, PayloadTooLarge, Queue};

const LIMITS: LimitCfg = LimitCfg { default: 20, max: 100 };
const MAX_TITLE: usize = 255;

/// Validate and trim a title (1..=255 chars after trim).
///
/// # Errors
/// `InvalidTitle`.
pub fn validate_title(raw: &str) -> Result<String, DomainError> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(DomainError::InvalidTitle("title must not be empty".into()));
    }
    if t.chars().count() > MAX_TITLE {
        return Err(DomainError::InvalidTitle("title must be at most 255 characters".into()));
    }
    Ok(t.to_owned())
}

fn rfc3339(t: &time::OffsetDateTime) -> String {
    t.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

fn chat_fields() -> FieldMap<chats::Entity> {
    FieldMap::new()
        .insert_with_extractor("updated_at", chats::Column::UpdatedAt, FieldKind::DateTimeUtc, |m: &chats::Model| {
            rfc3339(&m.updated_at)
        })
        .insert_with_extractor("id", chats::Column::Id, FieldKind::Uuid, |m: &chats::Model| m.id.to_string())
        .insert_with_extractor("title", chats::Column::Title, FieldKind::String, |m: &chats::Model| {
            m.title.clone().unwrap_or_default()
        })
}

fn message_fields() -> FieldMap<messages::Entity> {
    FieldMap::new()
        .insert_with_extractor("created_at", messages::Column::CreatedAt, FieldKind::DateTimeUtc, |m: &messages::Model| {
            rfc3339(&m.created_at)
        })
        .insert_with_extractor("id", messages::Column::Id, FieldKind::Uuid, |m: &messages::Model| m.id.to_string())
        .insert_with_extractor("role", messages::Column::Role, FieldKind::String, |m: &messages::Model| m.role.clone())
}

fn with_default_order(q: &ODataQuery, field: &str, dir: SortDir) -> ODataQuery {
    let mut q = q.clone();
    if q.cursor.is_none() && q.order.is_empty() {
        q.order = ODataOrderBy(vec![OrderKey {
            field: field.to_owned(),
            dir,
        }]);
    }
    if let Some(f) = q.filter.take() {
        q.filter = Some(Box::new(odata_compat::normalize_literals(*f)));
    }
    q
}


/// Quota status response (domain form).
#[derive(Debug, Clone)]
pub struct QuotaStatusView {
    pub entries: Vec<PeriodStatus>,
    pub warning_threshold_pct: u8,
}

impl MiniChat {
    /// `POST /v1/chats`.
    ///
    /// # Errors
    /// `INVALID_TITLE`, `INVALID_MODEL`, authorization, database.
    pub async fn create_chat(
        &self,
        ctx: &SecurityContext,
        title: Option<String>,
        model: Option<String>,
    ) -> Result<ChatView, DomainError> {
        let title = title.as_deref().map(validate_title).transpose()?;
        let scope = self.authz.chat_scope(ctx, actions::CREATE, None).await?;
        let snap = self.snapshot(ctx).await?;
        let model_id = match model {
            Some(m) => snap
                .find_enabled(&m)
                .map(|e| e.id.clone())
                .ok_or_else(|| DomainError::InvalidModel(format!("model '{m}' is not available")))?,
            None => snap
                .default_model()
                .map(|e| e.id.clone())
                .ok_or_else(|| DomainError::InvalidModel("no enabled model in the catalog".into()))?,
        };
        let ts = now();
        let am = chats::ActiveModel {
            id: Set(Uuid::new_v4()),
            tenant_id: Set(ctx.subject_tenant_id()),
            user_id: Set(ctx.subject_id()),
            model: Set(model_id),
            title: Set(title),
            is_temporary: Set(false),
            created_at: Set(ts),
            updated_at: Set(ts),
            deleted_at: Set(None),
        };
        let conn = self.db.conn()?;
        let row = chats::Entity::insert(am)
            .secure()
            .scope_unchecked(&scope)?
            .exec_with_returning(&conn)
            .await?;
        Ok(ChatView::from_model(row, 0))
    }

    /// `GET /v1/chats/{id}`.
    ///
    /// # Errors
    /// 404 / authorization / database.
    pub async fn get_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<ChatView, DomainError> {
        let (_, chat) = self.authorize_chat(ctx, actions::READ, chat_id).await?;
        let conn = self.db.conn()?;
        let n = self.message_count(&conn, chat.tenant_id, chat.id).await?;
        Ok(ChatView::from_model(chat, n))
    }

    /// `GET /v1/chats`.
    ///
    /// # Errors
    /// `OData` validation, authorization, database.
    pub async fn list_chats(&self, ctx: &SecurityContext, q: &ODataQuery) -> Result<Page<ChatView>, DomainError> {
        let scope = self.authz.chat_scope(ctx, actions::LIST, None).await?;
        let conn = self.db.conn()?;
        let base = chats::Entity::find()
            .filter(
                Condition::all()
                    .add(chats::Column::TenantId.eq(ctx.subject_tenant_id()))
                    .add(chats::Column::UserId.eq(ctx.subject_id()))
                    .add(chats::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope)
            .into_inner();
        let q = odata_compat::sqlite_cursor_fix(with_default_order(q, "updated_at", SortDir::Desc), self.db.db().backend());
        let page = paginate_with_odata(base, &conn, &q, &chat_fields(), ("id", SortDir::Desc), LIMITS, |m| m)
            .await
            .map_err(OdataFailure)?;
        let mut items = Vec::with_capacity(page.items.len());
        for m in page.items {
            let n = self.message_count(&conn, m.tenant_id, m.id).await?;
            items.push(ChatView::from_model(m, n));
        }
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    /// `PATCH /v1/chats/{id}`.
    ///
    /// # Errors
    /// `INVALID_TITLE`, 404, authorization, database.
    pub async fn update_chat(&self, ctx: &SecurityContext, chat_id: Uuid, title: &str) -> Result<ChatView, DomainError> {
        let title = validate_title(title)?;
        let scope = self.authz.chat_scope(ctx, actions::UPDATE, Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let res = chats::Entity::update_many()
            .col_expr(chats::Column::Title, Expr::value(title))
            .col_expr(chats::Column::UpdatedAt, Expr::value(now()))
            .filter(owned_chat_cond(ctx, chat_id))
            .secure()
            .scope_with(&scope)
            .exec(&conn)
            .await?;
        if res.rows_affected == 0 {
            return Err(DomainError::ChatNotFound(chat_id.to_string()));
        }
        let chat = self.load_chat(&conn, &scope, ctx, chat_id).await?;
        let n = self.message_count(&conn, chat.tenant_id, chat.id).await?;
        Ok(ChatView::from_model(chat, n))
    }

    /// `DELETE /v1/chats/{id}`: soft delete, mark attachments for cleanup and
    /// enqueue the chat cleanup in one transaction.
    ///
    /// # Errors
    /// 404, authorization, outbox payload too large (400), database.
    pub async fn delete_chat(&self, ctx: &SecurityContext, chat_id: Uuid) -> Result<(), DomainError> {
        let scope = self.authz.chat_scope(ctx, actions::DELETE, Some(chat_id)).await?;
        let ctx2 = ctx.clone();
        let outbox = self.outbox.clone();
        let wake = crate::infra::db::tx_retry(&self.db, move |tx| {
                let ctx2 = ctx2.clone();
                let outbox = outbox.clone();
                let scope = scope.clone();
                Box::pin(async move {
                    let ts = now();
                    let res = chats::Entity::update_many()
                        .col_expr(chats::Column::DeletedAt, Expr::value(Some(ts)))
                        .col_expr(chats::Column::UpdatedAt, Expr::value(ts))
                        .filter(owned_chat_cond(&ctx2, chat_id))
                        .secure()
                        .scope_with(&scope)
                        .exec(tx)
                        .await?;
                    if res.rows_affected == 0 {
                        return Err(DomainError::ChatNotFound(chat_id.to_string()));
                    }
                    let tenant = ctx2.subject_tenant_id();
                    attachments::Entity::update_many()
                        .col_expr(attachments::Column::CleanupStatus, Expr::value(Some("pending".to_owned())))
                        .col_expr(attachments::Column::CleanupUpdatedAt, Expr::value(Some(ts)))
                        .filter(
                            Condition::all()
                                .add(attachments::Column::ChatId.eq(chat_id))
                                .add(attachments::Column::CleanupStatus.is_null()),
                        )
                        .secure()
                        .scope_with(&AccessScope::for_tenant(tenant))
                        .exec(tx)
                        .await?;
                    let event = ChatCleanupEvent {
                        tenant_id: tenant,
                        chat_id,
                        system_request_id: Uuid::new_v4(),
                        reason: "chat_soft_delete".into(),
                        chat_deleted_at: rfc3339(&ts),
                    };
                    outbox
                        .enqueue_checked(tx, Queue::ChatCleanup, chat_id, &event)
                        .await
                        .map_err(|e| match e {
                            Ok(PayloadTooLarge(msg)) => DomainError::ChatCleanupPayloadTooLarge(msg),
                            Err(d) => d,
                        })
                })
            })
            .await?;
        wake.fire();
        Ok(())
    }

    /// `GET /v1/chats/{id}/messages`.
    ///
    /// # Errors
    /// `OData` validation, 404, authorization, database.
    pub async fn list_messages(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        q: &ODataQuery,
    ) -> Result<Page<MessageView>, DomainError> {
        let (_, chat) = self.authorize_chat(ctx, actions::LIST_MESSAGES, chat_id).await?;
        let conn = self.db.conn()?;
        let tenant_scope = AccessScope::for_tenant(chat.tenant_id);
        let base = messages::Entity::find()
            .filter(
                Condition::all()
                    .add(messages::Column::ChatId.eq(chat_id))
                    .add(messages::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&tenant_scope)
            .into_inner();
        let q = odata_compat::sqlite_cursor_fix(with_default_order(q, "created_at", SortDir::Asc), self.db.db().backend());
        let page = paginate_with_odata(base, &conn, &q, &message_fields(), ("id", SortDir::Asc), LIMITS, |m| m)
            .await
            .map_err(OdataFailure)?;
        let ids: Vec<Uuid> = page.items.iter().map(|m| m.id).collect();
        let mut items = Vec::with_capacity(page.items.len());
        for m in page.items {
            let id = m.id;
            let v = MessageView::from_model(m)
                .ok_or_else(|| DomainError::Internal(format!("message {id} has no request_id")))?;
            items.push(v);
        }
        let atts = self.attachments_for_messages(&conn, chat.tenant_id, chat_id, &ids).await?;
        let reactions = self.reactions_for(&conn, ctx, &ids).await?;
        for v in &mut items {
            if let Some(a) = atts.get(&v.id) {
                v.attachments.clone_from(a);
            }
            if v.role == "assistant" {
                v.my_reaction = reactions.get(&v.id).cloned();
            }
        }
        Ok(Page {
            items,
            page_info: page.page_info,
        })
    }

    async fn attachments_for_messages(
        &self,
        runner: &impl DBRunner,
        tenant: Uuid,
        chat_id: Uuid,
        message_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<AttachmentSummaryView>>, DomainError> {
        let mut out: HashMap<Uuid, Vec<AttachmentSummaryView>> = HashMap::new();
        if message_ids.is_empty() {
            return Ok(out);
        }
        let scope = AccessScope::for_tenant(tenant);
        let links = message_attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(message_attachments::Column::ChatId.eq(chat_id))
                    .add(message_attachments::Column::MessageId.is_in(message_ids.to_vec())),
            )
            .order_by_asc(message_attachments::Column::CreatedAt)
            .secure()
            .scope_with(&scope)
            .all(runner)
            .await?;
        if links.is_empty() {
            return Ok(out);
        }
        let att_ids: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
        let rows = attachments::Entity::find()
            .filter(
                Condition::all()
                    .add(attachments::Column::ChatId.eq(chat_id))
                    .add(attachments::Column::Id.is_in(att_ids))
                    .add(attachments::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&scope)
            .all(runner)
            .await?;
        let by_id: HashMap<Uuid, attachments::Model> = rows.into_iter().map(|r| (r.id, r)).collect();
        for l in links {
            if let Some(a) = by_id.get(&l.attachment_id) {
                out.entry(l.message_id).or_default().push(AttachmentSummaryView {
                    attachment_id: a.id,
                    kind: a.attachment_kind.clone(),
                    filename: a.filename.clone(),
                    status: a.status.clone(),
                    img_thumbnail: thumbnail_of(a),
                });
            }
        }
        Ok(out)
    }

    async fn reactions_for(
        &self,
        runner: &impl DBRunner,
        ctx: &SecurityContext,
        message_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, String>, DomainError> {
        if message_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let rows = message_reactions::Entity::find()
            .filter(
                Condition::all()
                    .add(message_reactions::Column::MessageId.is_in(message_ids.to_vec()))
                    .add(message_reactions::Column::UserId.eq(ctx.subject_id())),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(ctx.subject_tenant_id()))
            .all(runner)
            .await?;
        Ok(rows.into_iter().map(|r| (r.message_id, r.reaction)).collect())
    }

    async fn reaction_target(
        &self,
        ctx: &SecurityContext,
        action: &str,
        chat_id: Uuid,
        msg_id: Uuid,
    ) -> Result<messages::Model, DomainError> {
        let (_, chat) = self.authorize_chat(ctx, action, chat_id).await?;
        let conn = self.db.conn()?;
        let msg = messages::Entity::find()
            .filter(
                Condition::all()
                    .add(messages::Column::Id.eq(msg_id))
                    .add(messages::Column::ChatId.eq(chat.id))
                    .add(messages::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(chat.tenant_id))
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::MessageNotFound(msg_id.to_string()))?;
        if msg.role != "assistant" {
            return Err(DomainError::ReactionTarget);
        }
        Ok(msg)
    }

    /// `PUT .../reaction` (upsert).
    ///
    /// # Errors
    /// `INVALID_REACTION` (before authorization), 404, `reaction_target`.
    pub async fn set_reaction(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        msg_id: Uuid,
        reaction: &str,
    ) -> Result<ReactionView, DomainError> {
        if reaction != "like" && reaction != "dislike" {
            return Err(DomainError::InvalidReaction);
        }
        let msg = self.reaction_target(ctx, actions::SET_REACTION, chat_id, msg_id).await?;
        let conn = self.db.conn()?;
        let scope = AccessScope::for_tenant(ctx.subject_tenant_id());
        let ts = now();
        let own_reaction = Condition::all()
            .add(message_reactions::Column::MessageId.eq(msg.id))
            .add(message_reactions::Column::UserId.eq(ctx.subject_id()));
        let updated = message_reactions::Entity::update_many()
            .col_expr(message_reactions::Column::Reaction, Expr::value(reaction.to_owned()))
            .col_expr(message_reactions::Column::CreatedAt, Expr::value(ts))
            .filter(own_reaction.clone())
            .secure()
            .scope_with(&scope)
            .exec(&conn)
            .await?;
        if updated.rows_affected == 0 {
            let am = message_reactions::ActiveModel {
                id: Set(Uuid::new_v4()),
                message_id: Set(msg.id),
                user_id: Set(ctx.subject_id()),
                tenant_id: Set(ctx.subject_tenant_id()),
                reaction: Set(reaction.to_owned()),
                created_at: Set(ts),
            };
            let ins = message_reactions::Entity::insert(am)
                .secure()
                .scope_unchecked(&scope)?
                .exec(&conn)
                .await;
            if let Err(e) = ins {
                let e: DomainError = e.into();
                if !e.is_unique_violation() {
                    return Err(e);
                }
                message_reactions::Entity::update_many()
                    .col_expr(message_reactions::Column::Reaction, Expr::value(reaction.to_owned()))
                    .col_expr(message_reactions::Column::CreatedAt, Expr::value(ts))
                    .filter(own_reaction)
                    .secure()
                    .scope_with(&scope)
                    .exec(&conn)
                    .await?;
            }
        }
        Ok(ReactionView {
            message_id: msg.id,
            reaction: reaction.to_owned(),
            created_at: ts,
        })
    }

    /// `DELETE .../reaction` (idempotent).
    ///
    /// # Errors
    /// 404, `reaction_target`.
    pub async fn delete_reaction(&self, ctx: &SecurityContext, chat_id: Uuid, msg_id: Uuid) -> Result<(), DomainError> {
        let msg = self.reaction_target(ctx, actions::DELETE_REACTION, chat_id, msg_id).await?;
        let conn = self.db.conn()?;
        message_reactions::Entity::delete_many()
            .filter(
                Condition::all()
                    .add(message_reactions::Column::MessageId.eq(msg.id))
                    .add(message_reactions::Column::UserId.eq(ctx.subject_id())),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(ctx.subject_tenant_id()))
            .exec(&conn)
            .await?;
        Ok(())
    }

    /// `GET /v1/chats/{id}/turns/{request_id}`.
    ///
    /// # Errors
    /// 404 (chat or turn), authorization.
    pub async fn get_turn(&self, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> Result<TurnStatusView, DomainError> {
        let (_, chat) = self.authorize_chat(ctx, actions::READ_TURN, chat_id).await?;
        let conn = self.db.conn()?;
        let t = chat_turns::Entity::find()
            .filter(
                Condition::all()
                    .add(chat_turns::Column::ChatId.eq(chat.id))
                    .add(chat_turns::Column::RequestId.eq(request_id))
                    .add(chat_turns::Column::DeletedAt.is_null()),
            )
            .secure()
            .scope_with(&AccessScope::for_tenant(chat.tenant_id))
            .one(&conn)
            .await?
            .ok_or_else(|| DomainError::TurnNotFound(request_id.to_string()))?;
        let state = match t.state.as_str() {
            "completed" => "done",
            "failed" => "error",
            other => other,
        }
        .to_owned();
        Ok(TurnStatusView {
            request_id: t.request_id,
            state,
            error_code: if t.state == "failed" { t.error_code } else { None },
            assistant_message_id: if t.state == "completed" || t.state == "cancelled" {
                t.assistant_message_id
            } else {
                None
            },
            updated_at: t.updated_at,
        })
    }

    /// `GET /v1/models`.
    ///
    /// # Errors
    /// Authorization, policy plugin.
    pub async fn list_models(&self, ctx: &SecurityContext) -> Result<Vec<ModelView>, DomainError> {
        self.authz.model_access(ctx, "list").await?;
        let snap = self.snapshot(ctx).await?;
        Ok(snap
            .model_catalog
            .iter()
            .filter(|m| m.enabled)
            .map(model_view)
            .collect())
    }

    /// `GET /v1/models/{id}`.
    ///
    /// # Errors
    /// 404 when disabled or unknown.
    pub async fn get_model(&self, ctx: &SecurityContext, model_id: &str) -> Result<ModelView, DomainError> {
        self.authz.model_access(ctx, "read").await?;
        let snap = self.snapshot(ctx).await?;
        snap.find_enabled(model_id)
            .map(model_view)
            .ok_or_else(|| DomainError::ModelNotFound(model_id.to_owned()))
    }

    /// `GET /v1/quota/status`.
    ///
    /// # Errors
    /// Authorization, policy plugin, database.
    pub async fn quota_status(&self, ctx: &SecurityContext) -> Result<QuotaStatusView, DomainError> {
        self.authz.quota_scope(ctx).await?;
        let entries = self.quota_entries(ctx.subject_tenant_id(), ctx.subject_id()).await?;
        Ok(QuotaStatusView {
            entries,
            warning_threshold_pct: self.cfg.quota.warning_threshold_pct,
        })
    }

    /// Current quota entries of a user.
    ///
    /// # Errors
    /// Policy plugin, database.
    pub async fn quota_entries(&self, tenant: Uuid, user: Uuid) -> Result<Vec<PeriodStatus>, DomainError> {
        let client = self.policy.client().await?;
        let v = client
            .get_current_policy_version(user)
            .await
            .map_err(|e| DomainError::Internal(format!("policy plugin: {e}")))?;
        let limits = self.policy.user_limits(user, v.policy_version).await?;
        let t = now();
        let conn = self.db.conn()?;
        let usage = read_usage(&conn, tenant, user, PeriodStarts::of(t)).await?;
        Ok(status_entries(&usage, &limits, self.cfg.quota.warning_threshold_pct, t))
    }
}

fn model_view(m: &mini_chat_sdk::ModelCatalogEntry) -> ModelView {
    ModelView {
        model_id: m.id.clone(),
        display_name: m.display_name.clone(),
        tier: m.tier.as_str().to_owned(),
        multiplier_display: m.multiplier_display.clone(),
        description: (!m.description.is_empty()).then(|| m.description.clone()),
        multimodal_capabilities: m.multimodal_capabilities.clone(),
        context_window: m.context_window,
    }
}

#[allow(non_snake_case)]
fn OdataFailure(e: toolkit_odata::Error) -> DomainError {
    DomainError::OData(e)
}

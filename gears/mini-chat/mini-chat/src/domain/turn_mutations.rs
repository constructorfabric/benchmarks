//! Turn mutations of the latest turn (DESIGN 3.9 "Turn Mutation Rules",
//! 3.6 "Retry / edit variant"): retry, edit and delete.
//!
//! Order of one call: PDP (once, `retry_turn` / `edit_turn` / `delete_turn` on
//! the chat), owner-scoped chat lookup, turn lookup, then the read-only
//! mutation preview (requester 403, running target 400 before the latest-turn
//! check, latest turn 409). Retry and edit then run the stream service's quota
//! preflight, so a rejection leaves the previous answer in place, and commit
//! the mutation in one transaction: the latest-turn CAS (soft delete of the
//! turn and its messages), the new `running` turn and user message, the
//! carried-forward attachment links, the summary invalidation, the
//! `chats.updated_at` bump and the audit event. After the commit the stream
//! service assembles the context, resolves the provider, takes the reserve and
//! streams exactly like a send: settlement, usage and audit events are the
//! stream service's, there is no second path.
//!
//! The first statement of the commit transaction is a write (the chat lock, a
//! no-op update of the chat row that orders the mutation against the other
//! mutations and the summary worker commit of the chat), so a `SQLite`
//! transaction never upgrades from a read; a lost insert race on the
//! running-turn index rolls the whole transaction back as
//! `GENERATION_IN_PROGRESS`.

use std::sync::Arc;

use authz_resolver_sdk::PolicyEnforcer;
use mini_chat_sdk::AuditEvent;
use time::OffsetDateTime;
use toolkit_db::outbox::Wake;
use toolkit_db::secure::TxConfig;
use toolkit_db::{DBProvider, DbTx};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::authz::{actions, chat_scope};
use crate::domain::chats::{ChatRow, chat_not_found};
use crate::domain::clock::now_utc;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::messages::{MessageRole, MessageRow, NewMessage};
use crate::domain::ports::{
    AttachmentRepository, AuditPort, ChatRepository, MessageMutationRepository, MessageRepository,
    NewMessageAttachment, ThreadSummaryRepository, TurnLifecycleRepository, TurnRepository,
};
use crate::domain::provider::TokenBreakdown;
use crate::domain::streaming::{
    MutationRequest, StartedTurn, StreamService, TurnEvents, TurnScopes,
};
use crate::domain::turns::{NewUnstartedTurn, TurnRow, TurnState};

/// Retries, edits and deletes the latest turn of a chat.
pub struct TurnMutationService {
    db: Arc<DBProvider<DomainError>>,
    stream: Arc<StreamService>,
    enforcer: PolicyEnforcer,
    audit: Option<Arc<dyn AuditPort>>,
}

/// What a retry / edit puts in place of the replaced turn.
#[derive(Clone)]
struct Replacement {
    turn_id: Uuid,
    request_id: Uuid,
    user_message_id: Uuid,
    content: String,
}

/// The kind of mutation and its audit event.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Retry,
    Edit,
    Delete,
}

/// Everything the mutation transaction writes (cloned per attempt).
#[derive(Clone)]
struct Commit {
    kind: Kind,
    scopes: TurnScopes,
    chat: ChatRow,
    target: TurnRow,
    /// The user message of the replaced turn.
    user_message: Option<MessageRow>,
    replacement: Option<Replacement>,
    actor: Uuid,
    now: OffsetDateTime,
}

/// A target turn that passed the mutation preview.
struct Located {
    scopes: TurnScopes,
    chat: ChatRow,
    turn: TurnRow,
    user_message: Option<MessageRow>,
}

impl TurnMutationService {
    /// Creates the service over the stream service whose pipeline a retry /
    /// edit reuses. Without [`Self::with_audit`] no audit event is enqueued.
    #[must_use]
    pub fn new(
        db: Arc<DBProvider<DomainError>>,
        stream: Arc<StreamService>,
        enforcer: PolicyEnforcer,
    ) -> Self {
        Self {
            db,
            stream,
            enforcer,
            audit: None,
        }
    }

    /// Wires the audit outbox port: every committed mutation enqueues its
    /// `turn_retry` / `turn_edit` / `turn_delete` event in its transaction.
    #[must_use]
    pub fn with_audit<A: AuditPort + 'static>(mut self, audit: Arc<A>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// Replaces the latest turn by a new turn that answers its original user
    /// message again, and streams the answer.
    ///
    /// The whole call runs in a spawned task, so a client that disconnects
    /// after the commit cannot strand the new turn in `running`.
    ///
    /// # Errors
    /// Every error is raised before any event exists: `AccessDenied` /
    /// `ServiceUnavailable` (PDP), `NotFound` (chat, turn), `AccessDenied`
    /// (another requester), `InvalidTurnState` (running target),
    /// `NotLatestTurn`, `GenerationInProgress`, the preflight rejections of a
    /// send (`QuotaExceeded`, `FeatureDisabled`, `InvalidAttachment`,
    /// `InvalidInput`, ...) and, after the commit, `OutOfRange`
    /// (`CONTEXT_BUDGET_EXCEEDED`), `QuotaExceeded`, `Internal`.
    pub async fn retry(
        self: &Arc<Self>,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<TurnEvents, DomainError> {
        let service = Arc::clone(self);
        tokio::spawn(async move { service.replace(&ctx, chat_id, request_id, None).await })
            .await
            .map_err(|error| DomainError::internal(format!("retry task failed: {error}")))?
    }

    /// Like [`Self::retry`], with `content` (stored verbatim) in place of the
    /// original user message.
    ///
    /// # Errors
    /// As [`Self::retry`], plus `EmptyContent` for a blank `content`.
    pub async fn edit(
        self: &Arc<Self>,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        content: String,
    ) -> Result<TurnEvents, DomainError> {
        let service = Arc::clone(self);
        tokio::spawn(async move {
            service
                .replace(&ctx, chat_id, request_id, Some(content))
                .await
        })
        .await
        .map_err(|error| DomainError::internal(format!("edit task failed: {error}")))?
    }

    /// Soft-deletes the latest turn and its messages. No turn is created and
    /// nothing is settled.
    ///
    /// # Errors
    /// `AccessDenied` / `ServiceUnavailable` (PDP), `NotFound` (chat, turn),
    /// `AccessDenied` (another requester), `InvalidTurnState`, `NotLatestTurn`,
    /// `Internal` (the whole transaction rolls back).
    pub async fn delete(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<(), DomainError> {
        let located = self
            .locate(ctx, chat_id, request_id, actions::DELETE_TURN)
            .await?;
        let commit = Commit {
            kind: Kind::Delete,
            scopes: located.scopes,
            chat: located.chat,
            target: located.turn,
            user_message: located.user_message,
            replacement: None,
            actor: ctx.subject_id(),
            now: now_utc(),
        };
        self.commit(commit).await?.fire();
        Ok(())
    }

    async fn replace(
        self: &Arc<Self>,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        edited: Option<String>,
    ) -> Result<TurnEvents, DomainError> {
        let (action, kind) = if edited.is_some() {
            (actions::EDIT_TURN, Kind::Edit)
        } else {
            (actions::RETRY_TURN, Kind::Retry)
        };
        let located = self.locate(ctx, chat_id, request_id, action).await?;
        // `locate` reads without a transaction: a racing mutation that committed
        // in between has soft-deleted the turn and its messages, so a missing live
        // user message means the turn is no longer the latest (rule 7), not a
        // server fault.
        let user_message = located.user_message.ok_or(DomainError::NotLatestTurn)?;
        if edited.as_ref().is_some_and(|text| text.trim().is_empty()) {
            return Err(DomainError::EmptyContent);
        }
        let content = edited.unwrap_or_else(|| user_message.content.clone());

        let conn = self.db.conn()?;
        let attachment_ids = conn
            .live_linked_attachment_ids(&located.scopes.tenant, chat_id, user_message.id)
            .await?;
        // Preflight before the mutation commits: a rejection changes nothing.
        let plan = self
            .stream
            .mutation_preflight(
                ctx,
                &located.scopes,
                &located.chat,
                MutationRequest {
                    content: content.clone(),
                    web_search: located.turn.web_search_enabled,
                    attachment_ids,
                    replaced_request: request_id,
                },
            )
            .await?;

        let now = now_utc();
        let replacement = Replacement {
            turn_id: Uuid::new_v4(),
            request_id: Uuid::new_v4(),
            user_message_id: Uuid::new_v4(),
            content,
        };
        let started = StartedTurn {
            turn_id: replacement.turn_id,
            request_id: replacement.request_id,
            user_created_at: now,
        };
        let commit = Commit {
            kind,
            scopes: located.scopes.clone(),
            chat: located.chat.clone(),
            target: located.turn,
            user_message: Some(user_message),
            replacement: Some(replacement),
            actor: ctx.subject_id(),
            now,
        };
        self.commit(commit).await?.fire();
        self.stream
            .resume_mutation(ctx, located.scopes, located.chat, started, plan)
            .await
    }

    /// PDP, owner-scoped chat, turn, and the read-only mutation preview
    /// (DESIGN 3.9 rules 1 - 3).
    async fn locate(
        &self,
        ctx: &SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        action: &str,
    ) -> Result<Located, DomainError> {
        let scope = chat_scope(&self.enforcer, ctx, action, Some(chat_id)).await?;
        let scopes = TurnScopes::of(&scope);
        let conn = self.db.conn()?;
        let chat = conn
            .find_live_chat(&scope, chat_id)
            .await?
            .ok_or_else(|| chat_not_found(chat_id))?;
        let turn = conn
            .find_turn(&scopes.tenant, chat_id, request_id)
            .await?
            .ok_or_else(|| DomainError::NotFound {
                resource: ResourceKind::Turn,
                id: request_id.to_string(),
            })?;
        // Rule 2: the turn must be the caller's own.
        if turn.requester_user_id != Some(ctx.subject_id()) {
            return Err(DomainError::AccessDenied { cause: None });
        }
        // Rule 1 / 4: only non-deleted turns are mutable.
        if turn.deleted_at.is_some() {
            return Err(DomainError::NotLatestTurn);
        }
        // Rule 3, checked before the latest-turn check.
        if TurnState::parse(&turn.state)? == TurnState::Running {
            return Err(DomainError::InvalidTurnState);
        }
        let latest = conn.find_latest_turn(&scopes.tenant, chat_id).await?;
        if latest.is_none_or(|latest| latest.id != turn.id) {
            return Err(DomainError::NotLatestTurn);
        }
        let user_message = conn
            .find_turn_user_message(&scopes.tenant, chat_id, request_id)
            .await?;
        Ok(Located {
            scopes,
            chat,
            turn,
            user_message,
        })
    }

    /// The mutation transaction; the returned wake is fired once it is durable.
    async fn commit(&self, commit: Commit) -> Result<Wake, DomainError> {
        let audit = self.audit.clone();
        self.db
            .db()
            .transaction_with_retry(TxConfig::default(), DomainError::db_cause, |tx| {
                let commit = commit.clone();
                let audit = audit.clone();
                Box::pin(async move { commit_tx(tx, &commit, audit.as_deref()).await })
            })
            .await
    }
}

/// The body of the mutation transaction. The first statement is the chat lock
/// (a no-op write on the chat row), so every mutation and the summary worker
/// commit are ordered per chat and a `SQLite` transaction never upgrades from a
/// read; then the CAS decides the winner among concurrent mutations of the same
/// turn.
async fn commit_tx(
    tx: &DbTx<'_>,
    commit: &Commit,
    audit: Option<&dyn AuditPort>,
) -> Result<Wake, DomainError> {
    let tenant = &commit.scopes.tenant;
    let (chat, target, now) = (&commit.chat, &commit.target, commit.now);
    let replaced_by = commit.replacement.as_ref().map(|r| r.request_id);

    // Orders this mutation against every other mutation and against the summary
    // worker commit of the chat; a chat deleted meanwhile is gone (404).
    if !tx.lock_live_chat(&commit.scopes.owner, chat.id).await? {
        return Err(chat_not_found(chat.id));
    }
    // Rules 1, 3, 7: the target is still the live, terminal turn ...
    if !tx
        .soft_delete_turn(tenant, target.id, replaced_by, now)
        .await?
    {
        return Err(DomainError::NotLatestTurn);
    }
    tx.soft_delete_turn_messages(tenant, chat.id, target.request_id, now)
        .await?;
    // ... and no newer live turn appeared since the preview.
    if tx.has_newer_live_turn(tenant, chat.id, target).await? {
        return Err(DomainError::NotLatestTurn);
    }

    if let Some(replacement) = &commit.replacement {
        insert_replacement(tx, commit, replacement).await?;
    }
    invalidate_summary(tx, tenant, chat.id, commit.user_message.as_ref()).await?;

    let mut wake = Wake::empty();
    if let Some(audit) = audit {
        wake += audit.enqueue_audit(tx, &audit_event(commit)).await?;
    }
    Ok(wake)
}

/// Inserts the new running turn, the new user message and the copied
/// attachment links, and bumps the chat.
async fn insert_replacement(
    tx: &DbTx<'_>,
    commit: &Commit,
    replacement: &Replacement,
) -> Result<(), DomainError> {
    let (tenant, chat, now) = (&commit.scopes.tenant, &commit.chat, commit.now);
    // The index violation is the lost race of rule 7, not a conflict the
    // client could act on: 409 `GENERATION_IN_PROGRESS`, never `already_exists`.
    tx.insert_unstarted_turn(
        tenant,
        NewUnstartedTurn {
            id: replacement.turn_id,
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            request_id: replacement.request_id,
            requester_user_id: commit.actor,
            web_search_enabled: commit.target.web_search_enabled,
            now,
        },
    )
    .await
    .map_err(|error| match error {
        DomainError::Conflict { .. } => DomainError::GenerationInProgress,
        other => other,
    })?;
    tx.insert_message(
        tenant,
        NewMessage {
            id: replacement.user_message_id,
            tenant_id: chat.tenant_id,
            chat_id: chat.id,
            request_id: replacement.request_id,
            role: MessageRole::User,
            content: replacement.content.clone(),
            model: None,
            provider_response_id: None,
            input_tokens: 0,
            output_tokens: 0,
            breakdown: TokenBreakdown::default(),
            created_at: now,
        },
    )
    .await?;
    if let Some(old) = &commit.user_message {
        // Deleted attachments are silently left out; the old rows stay.
        let links: Vec<NewMessageAttachment> = tx
            .live_linked_attachment_ids(tenant, chat.id, old.id)
            .await?
            .into_iter()
            .map(|attachment_id| NewMessageAttachment {
                tenant_id: chat.tenant_id,
                chat_id: chat.id,
                message_id: replacement.user_message_id,
                attachment_id,
                now,
            })
            .collect();
        tx.link_attachments(tenant, &links).await?;
    }
    if !tx.touch_chat(&commit.scopes.owner, chat.id, now).await? {
        return Err(chat_not_found(chat.id));
    }
    Ok(())
}

/// DESIGN 3.9 "Summary Interaction on Turn Mutation": a summary whose frontier
/// `(created_at, id)` is at or after the mutated turn's user message no longer
/// describes the conversation; its row is deleted and `is_compressed` cleared
/// on every message of the chat. A summary that stops before the turn stays.
async fn invalidate_summary(
    tx: &DbTx<'_>,
    tenant: &AccessScope,
    chat_id: Uuid,
    user_message: Option<&MessageRow>,
) -> Result<(), DomainError> {
    let Some(message) = user_message else {
        return Ok(());
    };
    let Some(frontier) = tx.find_summary_frontier(tenant, chat_id).await? else {
        return Ok(());
    };
    if (frontier.created_at, frontier.message_id) >= (message.created_at, message.id) {
        tx.delete_summary(tenant, chat_id).await?;
        tx.clear_compression(tenant, chat_id).await?;
    }
    Ok(())
}

/// The audit event of a mutation: identities only, never content.
fn audit_event(commit: &Commit) -> AuditEvent {
    let tenant_id = commit.chat.tenant_id;
    let chat_id = commit.chat.id;
    let actor_user_id = commit.actor;
    let timestamp = commit.now;
    let original_request_id = commit.target.request_id;
    let new_request_id = commit
        .replacement
        .as_ref()
        .map_or(original_request_id, |replacement| replacement.request_id);
    match commit.kind {
        Kind::Retry => AuditEvent::TurnRetry {
            tenant_id,
            actor_user_id,
            chat_id,
            original_request_id,
            new_request_id,
            timestamp,
        },
        Kind::Edit => AuditEvent::TurnEdit {
            tenant_id,
            actor_user_id,
            chat_id,
            original_request_id,
            new_request_id,
            timestamp,
        },
        Kind::Delete => AuditEvent::TurnDelete {
            tenant_id,
            actor_user_id,
            chat_id,
            request_id: original_request_id,
            timestamp,
        },
    }
}

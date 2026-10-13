//! Turn status and tail-only turn mutations (DESIGN §3.9).

use std::sync::Arc;

use mini_chat_sdk::{AuditEvent, TurnMutationAuditEvent};
use time::OffsetDateTime;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use crate::domain::errors::{DomainError, DomainResult, Res};
use crate::domain::finalize::book_reserve;
use crate::domain::state::{AppState, ChatScopes};
use crate::domain::stream::{StreamStart, validate_content};
use crate::infra::db::entities::{chat_turns, chats, messages};
use crate::infra::db::repo::{self, NewMessage, NewTurn, Preflight as PreflightCols, TerminalUpdate};
use crate::infra::outbox::{Queue, Wakes};

#[derive(Debug, Clone)]
pub struct TurnStatusView {
    pub request_id: Uuid,
    pub state: &'static str,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub updated_at: OffsetDateTime,
}

#[must_use]
pub fn api_state(internal: &str) -> &'static str {
    match internal {
        repo::STATE_RUNNING => "running",
        repo::STATE_COMPLETED => "done",
        repo::STATE_CANCELLED => "cancelled",
        _ => "error",
    }
}

fn turn_not_found(request_id: Uuid) -> DomainError {
    DomainError::not_found(Res::Turn, request_id.to_string())
}

fn not_latest() -> DomainError {
    DomainError::aborted(Res::Turn, "NOT_LATEST_TURN", "Only the latest turn can be modified")
}

fn generation_in_progress() -> DomainError {
    DomainError::aborted(Res::Turn, "GENERATION_IN_PROGRESS", "Another generation is in progress")
}

fn turn_running() -> DomainError {
    DomainError::precondition(Res::Turn, "turn_state", "STATE", "The turn is still running")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mutation {
    Retry,
    Edit,
    Delete,
}

impl Mutation {
    fn action(self) -> &'static str {
        match self {
            Self::Retry => "retry_turn",
            Self::Edit => "edit_turn",
            Self::Delete => "delete_turn",
        }
    }
    fn audit_type(self) -> &'static str {
        match self {
            Self::Retry => "turn_retry",
            Self::Edit => "turn_edit",
            Self::Delete => "turn_delete",
        }
    }
}

/// Delete the thread summary when its frontier covers `(created_at, id)`.
async fn drop_covering_summary(
    tx: &impl toolkit_db::secure::DBRunner,
    tenant_scope: &AccessScope,
    chat_id: Uuid,
    user_msg: Option<&messages::Model>,
) -> DomainResult<()> {
    let Some(m) = user_msg else { return Ok(()) };
    if let Some(s) = repo::find_summary(tx, tenant_scope, chat_id).await?
        && (s.summarized_up_to_created_at, s.summarized_up_to_message_id) >= (m.created_at, m.id)
    {
        repo::delete_summary(tx, tenant_scope, chat_id).await?;
        repo::set_compressed_all(tx, tenant_scope, chat_id, false).await?;
    }
    Ok(())
}

impl AppState {
    pub async fn turn_status(&self, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> DomainResult<TurnStatusView> {
        let scopes = self.chat_scope(ctx, "read_turn", Some(chat_id)).await?;
        let conn = self.db.conn()?;
        repo::require_chat(&conn, &scopes.chat, chat_id).await?;
        let t = repo::find_turn_by_request(&conn, &scopes.tenant, chat_id, request_id)
            .await?
            .filter(|t| t.deleted_at.is_none())
            .ok_or_else(|| turn_not_found(request_id))?;
        let state = api_state(&t.state);
        Ok(TurnStatusView {
            request_id: t.request_id,
            state,
            error_code: if state == "error" { t.error_code.clone() } else { None },
            assistant_message_id: if matches!(state, "done" | "cancelled") { t.assistant_message_id } else { None },
            updated_at: t.updated_at,
        })
    }

    /// Read-only mutation preview: latest, terminal, ownership.
    async fn mutation_target(
        &self,
        ctx: &SecurityContext,
        op: Mutation,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> DomainResult<(ChatScopes, chats::Model, chat_turns::Model)> {
        let scopes = self.chat_scope(ctx, op.action(), Some(chat_id)).await?;
        let conn = self.db.conn()?;
        let chat = repo::require_chat(&conn, &scopes.chat, chat_id).await?;
        let t = repo::find_turn_by_request(&conn, &scopes.tenant, chat_id, request_id)
            .await?
            .ok_or_else(|| turn_not_found(request_id))?;
        if t.deleted_at.is_some() {
            return Err(not_latest());
        }
        if t.state == repo::STATE_RUNNING {
            return Err(turn_running());
        }
        let latest = repo::latest_turn(&conn, &scopes.tenant, chat_id).await?;
        if latest.as_ref().is_none_or(|l| l.id != t.id) {
            return Err(not_latest());
        }
        if t.requester_user_id != Some(ctx.subject_id()) {
            return Err(DomainError::denied());
        }
        Ok((scopes, chat, t))
    }

    pub async fn delete_turn(&self, ctx: &SecurityContext, chat_id: Uuid, request_id: Uuid) -> DomainResult<()> {
        let (scopes, _chat, t) = self.mutation_target(ctx, Mutation::Delete, chat_id, request_id).await?;
        let outbox = self.outbox.clone();
        let tenant_id = ctx.subject_tenant_id();
        let actor = ctx.subject_id();
        let wakes = self
            .write_tx(move |tx| {
                let scopes = scopes.clone();
                let outbox = outbox.clone();
                let t = t.clone();
                Box::pin(async move {
                    let latest = repo::latest_turn(tx, &scopes.tenant, chat_id).await?;
                    if latest.as_ref().is_none_or(|l| l.id != t.id) {
                        return Err(not_latest());
                    }
                    let now = repo::now();
                    if !repo::soft_delete_turn(tx, &scopes.tenant, t.id, None, now).await? {
                        return Err(not_latest());
                    }
                    let msgs = repo::messages_of_request(tx, &scopes.tenant, chat_id, t.request_id).await?;
                    let user_msg = msgs.iter().find(|m| m.role == "user" && m.deleted_at.is_none()).cloned();
                    repo::soft_delete_request_messages(tx, &scopes.tenant, chat_id, t.request_id, now).await?;
                    drop_covering_summary(tx, &scopes.tenant, chat_id, user_msg.as_ref()).await?;
                    let ev = AuditEvent::Mutation(TurnMutationAuditEvent {
                        event_type: Mutation::Delete.audit_type().to_owned(),
                        timestamp: now,
                        tenant_id,
                        actor_user_id: actor,
                        chat_id,
                        request_id: Some(t.request_id),
                        original_request_id: None,
                        new_request_id: None,
                    });
                    let mut wakes = Wakes::default();
                    wakes.push(outbox.enqueue(tx, Queue::Audit, tenant_id, &ev).await.map_err(as_internal)?);
                    Ok(wakes)
                })
            })
            .await?;
        wakes.fire();
        Ok(())
    }

    pub async fn retry_turn(self: &Arc<Self>, ctx: SecurityContext, chat_id: Uuid, request_id: Uuid) -> DomainResult<StreamStart> {
        self.mutate_and_stream(ctx, Mutation::Retry, chat_id, request_id, None).await
    }

    pub async fn edit_turn(
        self: &Arc<Self>,
        ctx: SecurityContext,
        chat_id: Uuid,
        request_id: Uuid,
        content: String,
    ) -> DomainResult<StreamStart> {
        validate_content(&content)?;
        self.mutate_and_stream(ctx, Mutation::Edit, chat_id, request_id, Some(content)).await
    }

    #[allow(clippy::too_many_lines)]
    async fn mutate_and_stream(
        self: &Arc<Self>,
        ctx: SecurityContext,
        op: Mutation,
        chat_id: Uuid,
        request_id: Uuid,
        new_content: Option<String>,
    ) -> DomainResult<StreamStart> {
        let (scopes, chat, old) = self.mutation_target(&ctx, op, chat_id, request_id).await?;
        let conn = self.db.conn()?;
        let old_msgs = repo::messages_of_request(&conn, &scopes.tenant, chat_id, old.request_id).await?;
        let old_user = old_msgs
            .iter()
            .find(|m| m.role == "user" && m.deleted_at.is_none())
            .cloned()
            .ok_or_else(|| DomainError::internal("turn without user message"))?;
        let links = repo::message_attachment_links(&conn, &scopes.tenant, chat_id, &[old_user.id]).await?;
        let linked: Vec<Uuid> = links.iter().map(|l| l.attachment_id).collect();
        let live_atts: Vec<Uuid> = repo::attachments_by_ids(&conn, &scopes.tenant, &linked)
            .await?
            .into_iter()
            .filter(|a| a.deleted_at.is_none() && a.chat_id == chat_id)
            .map(|a| a.id)
            .collect();
        let carried: Vec<Uuid> = linked.into_iter().filter(|id| live_atts.contains(id)).collect();
        drop(conn);
        let text = new_content.unwrap_or_else(|| old_user.content.clone());

        // Preflight before the mutation commits: a rejection changes nothing.
        let pre = self
            .preflight(&ctx, &chat, &scopes.tenant, &text, &carried, old.web_search_enabled)
            .await?;

        // Mutation commit.
        let new_request_id = Uuid::new_v4();
        let new_turn_id = Uuid::new_v4();
        let new_user_msg_id = Uuid::new_v4();
        let outbox = self.outbox.clone();
        let tenant_id = ctx.subject_tenant_id();
        let actor = ctx.subject_id();
        let txt = text.clone();
        let sc = scopes.clone();
        let old_c = old.clone();
        let old_user_c = old_user.clone();
        let carried_c = carried.clone();
        let res = self
            .write_tx(move |tx| {
                let scopes = sc.clone();
                let outbox = outbox.clone();
                let old = old_c.clone();
                let old_user = old_user_c.clone();
                let carried = carried_c.clone();
                let txt = txt.clone();
                Box::pin(async move {
                    let latest = repo::latest_turn(tx, &scopes.tenant, chat_id).await?;
                    match latest {
                        Some(l) if l.id == old.id => {}
                        Some(l) if l.state == repo::STATE_RUNNING => return Err(generation_in_progress()),
                        _ => return Err(not_latest()),
                    }
                    let now = repo::now();
                    if !repo::soft_delete_turn(tx, &scopes.tenant, old.id, Some(new_request_id), now).await? {
                        return Err(not_latest());
                    }
                    repo::soft_delete_request_messages(tx, &scopes.tenant, chat_id, old.request_id, now).await?;
                    repo::insert_message(
                        tx,
                        &scopes.tenant,
                        NewMessage {
                            id: new_user_msg_id,
                            tenant_id,
                            chat_id,
                            request_id: new_request_id,
                            role: "user",
                            content: txt,
                            model: None,
                            input_tokens: 0,
                            output_tokens: 0,
                            cache_read_input_tokens: 0,
                            cache_write_input_tokens: 0,
                            reasoning_tokens: 0,
                            provider_response_id: None,
                            created_at: now,
                        },
                    )
                    .await?;
                    repo::insert_message_attachments(tx, &scopes.tenant, tenant_id, chat_id, new_user_msg_id, &carried, now)
                        .await?;
                    repo::insert_turn(
                        tx,
                        &scopes.tenant,
                        &NewTurn {
                            id: new_turn_id,
                            tenant_id,
                            chat_id,
                            request_id: new_request_id,
                            requester_user_id: actor,
                            web_search_enabled: old.web_search_enabled,
                            started_at: now,
                        },
                        None,
                    )
                    .await
                    .map_err(|e| if e.is_unique_violation() { generation_in_progress() } else { e })?;
                    drop_covering_summary(tx, &scopes.tenant, chat_id, Some(&old_user)).await?;
                    repo::touch_chat(tx, &scopes.tenant, chat_id, now).await?;
                    let ev = AuditEvent::Mutation(TurnMutationAuditEvent {
                        event_type: op.audit_type().to_owned(),
                        timestamp: now,
                        tenant_id,
                        actor_user_id: actor,
                        chat_id,
                        request_id: None,
                        original_request_id: Some(old.request_id),
                        new_request_id: Some(new_request_id),
                    });
                    let mut wakes = Wakes::default();
                    wakes.push(outbox.enqueue(tx, Queue::Audit, tenant_id, &ev).await.map_err(as_internal)?);
                    Ok(wakes)
                })
            })
            .await;
        let wakes = match res {
            Ok(w) => w,
            Err(e) if e.is_unique_violation() => return Err(generation_in_progress()),
            Err(e) => return Err(e),
        };
        wakes.fire();

        // Post-commit setup: context assembly and the reserve (last step).
        let assembled = match self
            .assemble_request(&ctx, &chat, &scopes.tenant, &pre, &text, Some(new_user_msg_id))
            .await
        {
            Ok(a) => a,
            Err(e) => {
                let code = match &e {
                    DomainError::OutOfRange { reason, .. } if reason == "CONTEXT_BUDGET_EXCEEDED" => {
                        "context_length_exceeded"
                    }
                    _ => "turn_setup_failed",
                };
                self.fail_unstarted(new_turn_id, tenant_id, code).await;
                return Err(e);
            }
        };
        let tier = pre.decision.effective.tier;
        let periods = pre.periods;
        let reserve = pre.decision.reserve;
        let limits = pre.limits.clone();
        let eff_model = pre.decision.effective.id.clone();
        let floor = pre.floor;
        let policy_version = i64::try_from(pre.snap.policy_version).unwrap_or(i64::MAX);
        let tenant_scope = scopes.tenant.clone();
        let booked = self
            .write_tx(move |tx| {
                let limits = limits.clone();
                let eff_model = eff_model.clone();
                let tenant_scope = tenant_scope.clone();
                Box::pin(async move {
                    if !book_reserve(tx, tenant_id, actor, tier, periods, reserve.reserved_credits_micro, &limits).await? {
                        return Err(DomainError::quota_exceeded("tokens"));
                    }
                    repo::set_turn_preflight(
                        tx,
                        &tenant_scope,
                        new_turn_id,
                        PreflightCols {
                            reserve_tokens: reserve.reserve_tokens,
                            max_output_tokens_applied: reserve.max_output_tokens_applied,
                            reserved_credits_micro: reserve.reserved_credits_micro,
                            policy_version_applied: policy_version,
                            effective_model: &eff_model,
                            minimal_generation_floor_applied: floor,
                        },
                    )
                    .await?;
                    Ok(())
                })
            })
            .await;
        if let Err(e) = booked {
            let code = if matches!(e, DomainError::ResourceExhausted { .. }) { "quota_exceeded" } else { "turn_setup_failed" };
            self.fail_unstarted(new_turn_id, tenant_id, code).await;
            return Err(e);
        }
        let run = self.turn_run(&ctx, &chat, &pre, assembled, new_turn_id, new_request_id, true);
        Ok(StreamStart::Live(self.spawn_run(run)))
    }

    /// Plain CAS to `failed` for a retry/edit turn whose setup failed before
    /// the reserve (no settlement, no outbox).
    async fn fail_unstarted(&self, turn_id: Uuid, tenant_id: Uuid, code: &str) {
        let Ok(conn) = self.db.conn() else { return };
        let _ = repo::cas_finalize_turn(
            &conn,
            &AccessScope::for_tenant(tenant_id),
            turn_id,
            &TerminalUpdate {
                state: repo::STATE_FAILED,
                error_code: Some(code.to_owned()),
                ..TerminalUpdate::default()
            },
            repo::now(),
        )
        .await;
    }
}

fn as_internal(e: DomainError) -> DomainError {
    match e {
        DomainError::InvalidFormat { message, .. } => DomainError::internal(message),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::api_state;

    #[test]
    fn state_mapping() {
        assert_eq!(api_state("running"), "running");
        assert_eq!(api_state("completed"), "done");
        assert_eq!(api_state("failed"), "error");
        assert_eq!(api_state("cancelled"), "cancelled");
    }
}

//! Retry / edit of the last turn (DESIGN section 3.6 retry/edit variant,
//! section 3.9). Child of `stream_service_tests` (shares its fixture).

use super::*;
use crate::domain::enums::MessageRole;
use crate::domain::services::quota_service::{PeriodStarts, ReserveRequest};
use crate::domain::services::turn_service::{MutationKind, Replacement};
use crate::infra::db::repos::message_repo::NewMessage;
use crate::infra::db::repos::turn_repo::{NewRunningTurn, TurnPreflight};
use crate::infra::outbox::AUDIT_PAYLOAD_TYPE;
use crate::test_support::link_attachment;

impl Fx {
    async fn retry(&self, request_id: Uuid) -> Result<StreamStart, DomainError> {
        self.svc
            .retry(self.ctx.clone(), self.chat.id, request_id)
            .await
    }

    async fn edit(&self, request_id: Uuid, content: &str) -> Result<StreamStart, DomainError> {
        self.svc
            .edit(
                self.ctx.clone(),
                self.chat.id,
                request_id,
                content.to_owned(),
            )
            .await
    }

    /// Every outbox message delivered within `window`.
    async fn drain_outbox(&mut self, window: Duration) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + window;
        while let Ok(Some(msg)) = tokio::time::timeout_at(deadline, self.outbox_rx.recv()).await {
            out.push(msg);
        }
        out
    }

    /// Mutation audit events delivered within 1.5 s.
    async fn mutation_audits(&mut self) -> Vec<Value> {
        self.drain_outbox(Duration::from_millis(1500))
            .await
            .into_iter()
            .filter(|(ty, body)| ty == AUDIT_PAYLOAD_TYPE && body["kind"] == "mutation")
            .map(|(_, body)| body)
            .collect()
    }

    async fn links_of(&self, message_id: Uuid) -> Vec<Uuid> {
        let conn = self.db.conn().unwrap();
        let mut ids: Vec<Uuid> = message_attachment::Entity::find()
            .filter(message_attachment::Column::MessageId.eq(message_id))
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .unwrap()
            .into_iter()
            .map(|l| l.attachment_id)
            .collect();
        ids.sort();
        ids
    }

    /// A completed turn whose user message has `content` and links
    /// `attachments`; returns `(request_id, user message)`.
    async fn seed_turn_with(&self, content: &str, attachments: &[Uuid]) -> (Uuid, message::Model) {
        let rid = Uuid::new_v4();
        let user =
            insert_message(&self.db, message_am(&self.chat, rid, "user", content, None)).await;
        insert_message(
            &self.db,
            message_am(&self.chat, rid, "assistant", "old answer", Some(MODEL)),
        )
        .await;
        seed_turn(&self.db, &self.chat, rid, "completed", db_now()).await;
        for a in attachments {
            link_attachment(&self.db, &user, *a).await;
        }
        (rid, user)
    }

    async fn sent_turn(&self, content: &str, attachment_ids: Vec<Uuid>) -> Uuid {
        let mut req = self.req(content);
        req.attachment_ids = attachment_ids;
        let rid = req.request_id.unwrap();
        let evs = collect(live(self.send(req).await)).await;
        assert_eq!(evs.last().unwrap().name(), "done");
        rid
    }

    async fn live_turns(&self) -> Vec<chat_turn::Model> {
        self.turns()
            .await
            .into_iter()
            .filter(|t| t.deleted_at.is_none())
            .collect()
    }
}

fn user_input(r: &LlmRequest) -> Vec<ContentPart> {
    match r.input.last().unwrap() {
        InputItem::Message {
            role: "user",
            content,
        } => content.clone(),
        other => panic!("last input item is {other:?}"),
    }
}

/// The previous turn and the chat are untouched after a rejection.
async fn assert_unchanged(fx: &mut Fx, rid: Uuid) {
    let t = fx.turn(rid).await;
    assert_eq!(t.deleted_at, None);
    assert_eq!(t.replaced_by_request_id, None);
    assert_eq!(fx.turns().await.len(), 1);
    assert!(fx.messages().await.iter().all(|m| m.deleted_at.is_none()));
    assert!(fx.mutation_audits().await.is_empty());
}

// ── Retry / edit happy paths ─────────────────────────────────────────────────

#[tokio::test]
async fn retry_replaces_turn_and_streams_new_request_id() {
    let mut fx = fx().await;
    let old_rid = fx.sent_turn("hi", vec![]).await;
    let before = fx.drain_outbox(Duration::from_millis(500)).await;
    drop(before);

    let evs = collect(live(fx.retry(old_rid).await)).await;

    assert_eq!(names(&evs), ["stream_started", "delta", "delta", "done"]);
    let s = started(&evs);
    assert_ne!(s.request_id, old_rid);
    assert_eq!(s.request_id.get_version_num(), 4);
    assert!(s.is_new_turn);
    let new_rid = s.request_id;

    let old = fx.turn(old_rid).await;
    assert!(old.deleted_at.is_some());
    assert_eq!(old.replaced_by_request_id, Some(new_rid));
    assert_eq!(old.state, "completed");
    let new = wait_turn_state(&fx, new_rid, "completed").await;
    assert_eq!(new.deleted_at, None);
    assert_eq!(new.requester_user_id, Some(fx.ctx.subject_id()));
    assert!(new.reserve_tokens.is_some());
    assert_eq!(new.effective_model.as_deref(), Some(MODEL));
    assert!(new.policy_version_applied.is_some());
    assert!(new.max_output_tokens_applied.is_some());
    assert!(new.reserved_credits_micro.is_some());
    assert!(new.minimal_generation_floor_applied.is_some());
    assert_eq!(new.assistant_message_id, Some(s.message_id));

    let msgs = fx.messages().await;
    let live_msgs: Vec<_> = msgs.iter().filter(|m| m.deleted_at.is_none()).collect();
    assert_eq!(live_msgs.len(), 2);
    assert_eq!(
        (live_msgs[0].role.as_str(), live_msgs[0].content.as_str()),
        ("user", "hi")
    );
    assert_eq!(live_msgs[0].request_id, Some(new_rid));
    assert_eq!(live_msgs[0].token_estimate, 0);
    assert_eq!(live_msgs[1].id, s.message_id);
    assert!(
        msgs.iter()
            .filter(|m| m.request_id == Some(old_rid))
            .all(|m| m.deleted_at.is_some())
    );

    // The replaced turn is not in the context: only the re-sent message.
    let r = fx.llm.last_request();
    assert_eq!(r.input.len(), 1, "{:?}", r.input);
    assert_eq!(
        user_input(&r),
        vec![ContentPart::InputText("hi".to_owned())]
    );

    // The mutation audit event plus the new turn's usage and audit events.
    let msgs = fx.drain_outbox(Duration::from_millis(1500)).await;
    let mutation: Vec<_> = msgs
        .iter()
        .filter(|(ty, b)| ty == AUDIT_PAYLOAD_TYPE && b["kind"] == "mutation")
        .collect();
    assert_eq!(mutation.len(), 1, "{msgs:?}");
    let ev = &mutation[0].1;
    assert_eq!(ev["event_type"], "turn_retry");
    assert_eq!(ev["tenant_id"], json!(fx.chat.tenant_id));
    assert_eq!(ev["chat_id"], json!(fx.chat.id));
    assert_eq!(ev["actor_user_id"], json!(fx.ctx.subject_id()));
    assert_eq!(ev["original_request_id"], json!(old_rid));
    assert_eq!(ev["new_request_id"], json!(new_rid));
    assert!(ev["request_id"].is_null());
    assert_eq!(
        msgs.iter()
            .filter(|(ty, b)| ty == USAGE_PAYLOAD_TYPE && b["request_id"] == json!(new_rid))
            .count(),
        1
    );

    // The old request id is no longer replayable.
    let mut replay = fx.req("hi");
    replay.request_id = Some(old_rid);
    assert_eq!(
        fx.send(replay).await.unwrap_err(),
        DomainError::RequestIdConflict
    );
    assert_eq!(
        fx.authz
            .chat_actions()
            .iter()
            .filter(|a| **a == ChatAction::RetryTurn)
            .count(),
        1
    );
}

#[tokio::test]
async fn retry_copies_web_search_flag_and_bumps_chat() {
    let fx = fx().await;
    let mut req = fx.req("search");
    req.web_search_enabled = true;
    let old_rid = req.request_id.unwrap();
    let _ = collect(live(fx.send(req).await)).await;
    let chat_before = chat_updated_at(&fx).await;

    let evs = collect(live(fx.retry(old_rid).await)).await;
    let new = fx.turn(started(&evs).request_id).await;

    assert!(new.web_search_enabled);
    assert!(
        fx.llm
            .last_request()
            .tools
            .iter()
            .any(|t| matches!(t, ToolSpec::WebSearch { .. }))
    );
    assert!(chat_updated_at(&fx).await > chat_before);
}

async fn chat_updated_at(fx: &Fx) -> OffsetDateTime {
    let conn = fx.db.conn().unwrap();
    chat::Entity::find()
        .filter(chat::Column::Id.eq(fx.chat.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .one(&conn)
        .await
        .unwrap()
        .unwrap()
        .updated_at
}

#[tokio::test]
async fn retry_preflight_ignores_the_replaced_turn_usage() {
    // The replaced turn's assistant usage (12/5) is not prior context of
    // the replacement: same content, same reserve as the original send.
    let fx = fx().await;
    let old_rid = fx.sent_turn("hi", vec![]).await;
    let evs = collect(live(fx.retry(old_rid).await)).await;
    let (old, new) = (
        fx.turn(old_rid).await,
        fx.turn(started(&evs).request_id).await,
    );
    assert_eq!(new.reserve_tokens, old.reserve_tokens);
    assert_eq!(new.reserved_credits_micro, old.reserved_credits_micro);
}

#[tokio::test]
async fn edit_uses_new_content_and_copies_attachments() {
    let mut fx = fx().await;
    let doc = fx
        .attachment("document", "ready", "file-doc10000000000")
        .await;
    let img = fx.attachment("image", "ready", "file-img10000000000").await;
    let old_rid = fx.sent_turn("old text", vec![doc.id, img.id]).await;
    let _ = fx.drain_outbox(Duration::from_millis(300)).await;

    let evs = collect(live(fx.edit(old_rid, "new text").await)).await;
    assert_eq!(evs.last().unwrap().name(), "done");
    let new_rid = started(&evs).request_id;

    let user = fx
        .messages()
        .await
        .into_iter()
        .find(|m| m.request_id == Some(new_rid) && m.role == "user")
        .unwrap();
    assert_eq!(user.content, "new text");
    assert_eq!(user.token_estimate, 0);
    let mut want = vec![doc.id, img.id];
    want.sort();
    assert_eq!(fx.links_of(user.id).await, want);
    // The old links stay on the soft-deleted message.
    let old_user = fx
        .messages()
        .await
        .into_iter()
        .find(|m| m.request_id == Some(old_rid) && m.role == "user")
        .unwrap();
    assert_eq!(fx.links_of(old_user.id).await, want);
    assert_eq!(
        user_input(&fx.llm.last_request()),
        vec![
            ContentPart::InputText("new text".to_owned()),
            ContentPart::InputImage {
                file_id: "file-img10000000000".to_owned()
            }
        ]
    );
    assert_eq!(fx.turn(old_rid).await.replaced_by_request_id, Some(new_rid));
    let audits = fx.mutation_audits().await;
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0]["event_type"], "turn_edit");
    assert_eq!(audits[0]["original_request_id"], json!(old_rid));
    assert_eq!(audits[0]["new_request_id"], json!(new_rid));
}

#[tokio::test]
async fn deleted_attachment_not_copied() {
    let fx = fx().await;
    let keep = fx
        .attachment("document", "ready", "file-doc10000000000")
        .await;
    let gone = fx
        .attachment("document", "ready", "file-doc20000000000")
        .await;
    let old_rid = fx.sent_turn("q", vec![keep.id, gone.id]).await;
    fx.update_attachment(gone.id, attachment::Column::DeletedAt, Some(db_now()))
        .await;

    let evs = collect(live(fx.retry(old_rid).await)).await;
    let new_rid = started(&evs).request_id;
    let user = fx
        .messages()
        .await
        .into_iter()
        .find(|m| m.request_id == Some(new_rid) && m.role == "user")
        .unwrap();
    assert_eq!(fx.links_of(user.id).await, vec![keep.id]);
}

#[tokio::test]
async fn edit_empty_content_rejected_after_preview() {
    let mut fx = fx().await;
    let (rid, _) = fx.seed_turn_with("q", &[]).await;
    assert_eq!(
        fx.edit(rid, "  \n ").await.unwrap_err(),
        DomainError::EmptyContent
    );
    // Preview checks come first.
    assert_eq!(
        fx.edit(Uuid::new_v4(), "").await.unwrap_err(),
        DomainError::NotFound {
            resource: ResourceKind::Turn
        }
    );
    assert_unchanged(&mut fx, rid).await;
}

// ── Preview rejections ───────────────────────────────────────────────────────

#[tokio::test]
async fn mutation_of_non_latest_is_409() {
    let mut fx = fx().await;
    let first = fx.sent_turn("one", vec![]).await;
    let second = fx.sent_turn("two", vec![]).await;
    let calls = fx.llm.calls();
    let _ = fx.drain_outbox(Duration::from_millis(300)).await;

    assert_eq!(
        fx.retry(first).await.unwrap_err(),
        DomainError::NotLatestTurn
    );
    assert_eq!(
        fx.edit(first, "x").await.unwrap_err(),
        DomainError::NotLatestTurn
    );
    assert_eq!(fx.llm.calls(), calls);
    assert!(fx.turns().await.iter().all(|t| t.deleted_at.is_none()));
    assert!(fx.mutation_audits().await.is_empty());
    let _ = second;
}

#[tokio::test]
async fn mutation_of_deleted_turn_is_409_not_latest() {
    let fx = fx().await;
    let (rid, _) = fx.seed_turn_with("q", &[]).await;
    let t = fx.turn(rid).await;
    let conn = fx.db.conn().unwrap();
    chat_turn::Entity::update_many()
        .col_expr(chat_turn::Column::DeletedAt, Expr::value(Some(db_now())))
        .filter(chat_turn::Column::Id.eq(t.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    assert_eq!(fx.retry(rid).await.unwrap_err(), DomainError::NotLatestTurn);
    assert_eq!(
        fx.edit(rid, "x").await.unwrap_err(),
        DomainError::NotLatestTurn
    );
    assert_eq!(
        fx.turns_svc
            .delete(&fx.ctx, fx.chat.id, rid)
            .await
            .unwrap_err(),
        DomainError::NotLatestTurn
    );
    assert_eq!(fx.llm.calls(), 0);
}

#[tokio::test]
async fn running_turn_is_400_turn_state() {
    let fx = fx().await;
    let rid = Uuid::new_v4();
    seed_turn(&fx.db, &fx.chat, rid, "running", db_now()).await;
    assert_eq!(
        fx.retry(rid).await.unwrap_err(),
        DomainError::TurnNotTerminal
    );
    assert_eq!(
        fx.edit(rid, "x").await.unwrap_err(),
        DomainError::TurnNotTerminal
    );
    assert_eq!(fx.llm.calls(), 0);
    assert_eq!(fx.turn(rid).await.deleted_at, None);
}

#[tokio::test]
async fn other_requester_403() {
    let mut fx = fx().await;
    let (rid, _) = fx.seed_turn_with("q", &[]).await;
    let t = fx.turn(rid).await;
    let conn = fx.db.conn().unwrap();
    chat_turn::Entity::update_many()
        .col_expr(
            chat_turn::Column::RequesterUserId,
            Expr::value(Some(Uuid::new_v4())),
        )
        .filter(chat_turn::Column::Id.eq(t.id))
        .secure()
        .scope_with(&AccessScope::allow_all())
        .exec(&conn)
        .await
        .unwrap();
    assert_eq!(fx.retry(rid).await.unwrap_err(), DomainError::NotRequester);
    assert_eq!(
        fx.edit(rid, "x").await.unwrap_err(),
        DomainError::NotRequester
    );
    assert_unchanged(&mut fx, rid).await;
}

#[tokio::test]
async fn unknown_turn_and_foreign_chat_are_404() {
    let fx = fx().await;
    assert_eq!(
        fx.retry(Uuid::new_v4()).await.unwrap_err(),
        DomainError::NotFound {
            resource: ResourceKind::Turn
        }
    );
    let (rid, _) = fx.seed_turn_with("q", &[]).await;
    let stranger = ctx_for(fx.ctx.subject_tenant_id(), Uuid::new_v4());
    assert_eq!(
        fx.svc.retry(stranger, fx.chat.id, rid).await.unwrap_err(),
        DomainError::NotFound {
            resource: ResourceKind::Chat
        }
    );
}

// ── Preflight rejections (before the mutation) ───────────────────────────────

#[tokio::test]
async fn quota_rejection_leaves_previous_turn_intact() {
    let mut fx = fx_with(Opts {
        limits: TierLimits {
            limit_daily_credits_micro: 1,
            limit_monthly_credits_micro: 1,
        },
        ..Opts::default()
    })
    .await;
    let (rid, _) = fx.seed_turn_with("q", &[]).await;
    assert_eq!(
        fx.retry(rid).await.unwrap_err(),
        DomainError::QuotaExceeded {
            scope: QuotaScope::Tokens
        }
    );
    assert_eq!(fx.llm.calls(), 0);
    assert_eq!(fx.reserved_total().await, 0);
    assert_unchanged(&mut fx, rid).await;
}

#[tokio::test]
async fn retry_resends_images_with_guards() {
    // Re-sent as input_image on retry.
    let fx1 = fx().await;
    let img = fx1
        .attachment("image", "ready", "file-img10000000000")
        .await;
    let rid = fx1.sent_turn("look", vec![img.id]).await;
    let _ = collect(live(fx1.retry(rid).await)).await;
    assert_eq!(
        user_input(&fx1.llm.last_request()),
        vec![
            ContentPart::InputText("look".to_owned()),
            ContentPart::InputImage {
                file_id: "file-img10000000000".to_owned()
            }
        ]
    );

    // disable_images: 400 before the mutation.
    let mut kill = snapshot(vec![]).kill_switches;
    kill.disable_images = true;
    let mut fx2 = fx_with(Opts {
        kill,
        ..Opts::default()
    })
    .await;
    let img = fx2
        .attachment("image", "ready", "file-img10000000000")
        .await;
    let (rid, _) = fx2.seed_turn_with("look", &[img.id]).await;
    assert_eq!(
        fx2.retry(rid).await.unwrap_err(),
        DomainError::FeatureDisabled { subject: "images" }
    );
    assert_eq!(
        fx2.edit(rid, "again").await.unwrap_err(),
        DomainError::FeatureDisabled { subject: "images" }
    );
    assert_unchanged(&mut fx2, rid).await;
    // A deleted image is not re-sent, so the guard no longer applies.
    fx2.update_attachment(img.id, attachment::Column::DeletedAt, Some(db_now()))
        .await;
    let _ = collect(live(fx2.retry(rid).await)).await;
    assert_eq!(
        user_input(&fx2.llm.last_request()),
        vec![ContentPart::InputText("look".to_owned())]
    );

    // Image count against max_images_per_message.
    let mut o = Opts::default();
    o.cfg.rag.max_images_per_message = 1;
    let mut fx3 = fx_with(o).await;
    let a = fx3
        .attachment("image", "ready", "file-img10000000000")
        .await;
    let b = fx3
        .attachment("image", "ready", "file-img20000000000")
        .await;
    let (rid, _) = fx3.seed_turn_with("look", &[a.id, b.id]).await;
    assert_eq!(
        fx3.retry(rid).await.unwrap_err(),
        DomainError::TooManyImages
    );
    assert_unchanged(&mut fx3, rid).await;

    // Vision on the effective model.
    let mut no_vision = catalog_entry(MODEL, true);
    no_vision.multimodal_capabilities = vec![];
    let mut fx4 = fx_with(Opts {
        catalog: vec![no_vision],
        ..Opts::default()
    })
    .await;
    let img = fx4
        .attachment("image", "ready", "file-img10000000000")
        .await;
    let (rid, _) = fx4.seed_turn_with("look", &[img.id]).await;
    assert_eq!(
        fx4.retry(rid).await.unwrap_err(),
        DomainError::VisionNotSupported
    );
    assert_unchanged(&mut fx4, rid).await;
}

#[tokio::test]
async fn edit_input_too_long_rejected_before_mutation() {
    let mut m = catalog_entry(MODEL, true);
    m.max_input_tokens = 50;
    let mut fx = fx_with(Opts {
        catalog: vec![m],
        ..Opts::default()
    })
    .await;
    let (rid, _) = fx.seed_turn_with("q", &[]).await;
    assert_eq!(
        fx.edit(rid, &"x".repeat(400)).await.unwrap_err(),
        DomainError::InputTooLong
    );
    assert_unchanged(&mut fx, rid).await;
}

// ── Failures after the mutation commit ───────────────────────────────────────

#[tokio::test]
async fn setup_failure_marks_new_turn_failed() {
    let mut m = catalog_entry(MODEL, true);
    m.context_window = 5000;
    m.max_input_tokens = 0;
    let mut fx = fx_with(Opts {
        catalog: vec![m],
        ..Opts::default()
    })
    .await;
    let (old_rid, _) = fx.seed_turn_with(&"x".repeat(3000), &[]).await;

    assert_eq!(
        fx.retry(old_rid).await.unwrap_err(),
        DomainError::ContextBudgetExceeded
    );

    let old = fx.turn(old_rid).await;
    assert!(old.deleted_at.is_some());
    let new_rid = old.replaced_by_request_id.expect("replaced");
    let new = fx.turn(new_rid).await;
    assert_eq!(new.state, "failed");
    assert_eq!(new.error_code.as_deref(), Some("context_length_exceeded"));
    assert!(new.completed_at.is_some());
    assert_eq!(new.reserve_tokens, None);
    assert_eq!(new.effective_model, None);
    assert_eq!(fx.reserved_total().await, 0);
    assert_eq!(fx.llm.calls(), 0);
    // Only the mutation audit: no usage event, no turn audit event.
    let msgs = fx.drain_outbox(Duration::from_millis(1500)).await;
    assert_eq!(msgs.len(), 1, "{msgs:?}");
    assert_eq!(msgs[0].1["event_type"], "turn_retry");
    // The chat is not blocked: a new send works.
    let _ = collect(live(fx.send(fx.req("short")).await)).await;
}

#[tokio::test]
async fn unstarted_error_codes() {
    use super::super::setup::unstarted_error_code;
    assert_eq!(
        unstarted_error_code(&DomainError::ContextBudgetExceeded),
        "context_length_exceeded"
    );
    assert_eq!(
        unstarted_error_code(&DomainError::QuotaExceeded {
            scope: QuotaScope::Tokens
        }),
        "quota_exceeded"
    );
    assert_eq!(
        unstarted_error_code(&DomainError::ProviderResolution("x".to_owned())),
        "turn_setup_failed"
    );
    assert_eq!(
        unstarted_error_code(&DomainError::Internal("x".to_owned())),
        "turn_setup_failed"
    );
}

fn replacement(fx: &Fx, web_search_enabled: bool) -> Replacement {
    let (now, rid) = (db_now(), Uuid::new_v4());
    Replacement {
        message: NewMessage {
            id: Uuid::new_v4(),
            tenant_id: fx.chat.tenant_id,
            chat_id: fx.chat.id,
            request_id: rid,
            role: MessageRole::User,
            content: "q".to_owned(),
            request_kind: "chat".to_owned(),
            features_used: json!([]),
            provider_response_id: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_input_tokens: 0,
            cache_write_input_tokens: 0,
            reasoning_tokens: 0,
            model: None,
            created_at: now,
        },
        attachment_ids: vec![],
        turn: NewRunningTurn {
            id: Uuid::new_v4(),
            tenant_id: fx.chat.tenant_id,
            chat_id: fx.chat.id,
            request_id: rid,
            requester_user_id: fx.ctx.subject_id(),
            preflight: None,
            web_search_enabled,
            now,
        },
    }
}

#[tokio::test]
async fn concurrent_retries_one_wins_generation_in_progress() {
    // Both mutations passed the preview before either committed; the second
    // commit loses the race for the one running turn.
    let mut fx = fx().await;
    let (rid, _) = fx.seed_turn_with("q", &[]).await;
    let target = fx
        .turns_svc
        .preview(&fx.ctx, fx.chat.id, rid, MutationKind::Retry)
        .await
        .unwrap();
    let (a, b) = (replacement(&fx, false), replacement(&fx, false));
    let (a_rid, b_rid) = (a.turn.request_id, b.turn.request_id);

    fx.turns_svc.commit_replacement(&target, a).await.unwrap();
    assert_eq!(
        fx.turns_svc
            .commit_replacement(&target, b)
            .await
            .unwrap_err(),
        DomainError::GenerationInProgress
    );

    assert_eq!(fx.turn(rid).await.replaced_by_request_id, Some(a_rid));
    let new = fx.turn(a_rid).await;
    assert_eq!((new.state.as_str(), new.reserve_tokens), ("running", None));
    assert!(new.last_progress_at.is_some());
    assert!(fx.turns().await.iter().all(|t| t.request_id != b_rid));
    assert!(
        fx.messages()
            .await
            .iter()
            .all(|m| m.request_id != Some(b_rid))
    );
    assert_eq!(fx.mutation_audits().await.len(), 1);
}

#[tokio::test]
async fn running_turn_insert_race_is_generation_in_progress() {
    // A running turn appeared after the preview (an older start time keeps
    // the target the latest turn): the new turn's insert hits the
    // one-running-turn index.
    let mut fx = fx().await;
    let (rid, _) = fx.seed_turn_with("q", &[]).await;
    let target = fx
        .turns_svc
        .preview(&fx.ctx, fx.chat.id, rid, MutationKind::Edit)
        .await
        .unwrap();
    let older = db_now() - time::Duration::hours(1);
    seed_turn(&fx.db, &fx.chat, Uuid::new_v4(), "running", older).await;
    assert_eq!(
        fx.turns_svc
            .commit_replacement(&target, replacement(&fx, false))
            .await
            .unwrap_err(),
        DomainError::GenerationInProgress
    );
    assert_eq!(fx.turn(rid).await.deleted_at, None);
    assert!(fx.mutation_audits().await.is_empty());
}

#[tokio::test]
async fn newer_turn_after_preview_is_not_latest() {
    let fx = fx().await;
    let (rid, _) = fx.seed_turn_with("q", &[]).await;
    let target = fx
        .turns_svc
        .preview(&fx.ctx, fx.chat.id, rid, MutationKind::Retry)
        .await
        .unwrap();
    let newer = db_now() + time::Duration::seconds(5);
    seed_turn(&fx.db, &fx.chat, Uuid::new_v4(), "completed", newer).await;
    assert_eq!(
        fx.turns_svc
            .commit_replacement(&target, replacement(&fx, false))
            .await
            .unwrap_err(),
        DomainError::NotLatestTurn
    );
    assert_eq!(fx.turn(rid).await.deleted_at, None);
}

#[tokio::test]
async fn concurrent_retry_race_has_one_winner() {
    let fx = fx().await;
    let rid = fx.sent_turn("hi", vec![]).await;
    let (a, b) = tokio::join!(fx.retry(rid), fx.retry(rid));
    let (ok, err): (Vec<_>, Vec<_>) = [a, b].into_iter().partition(Result::is_ok);
    assert_eq!(ok.len(), 1);
    let e = err.into_iter().next().unwrap().unwrap_err();
    assert!(
        matches!(
            e,
            DomainError::GenerationInProgress | DomainError::NotLatestTurn
        ),
        "{e:?}"
    );
    let start = ok.into_iter().next().unwrap().unwrap();
    let _ = collect(start).await;
    assert_eq!(fx.live_turns().await.len(), 1);
}

fn reserve(fx: &Fx, limit: i64) -> ReserveRequest {
    let user_id = fx.ctx.subject_id();
    let limits = TierLimits {
        limit_daily_credits_micro: limit,
        limit_monthly_credits_micro: limit,
    };
    ReserveRequest {
        tenant_id: fx.chat.tenant_id,
        user_id,
        premium: true,
        reserved_credits_micro: 1000,
        periods: PeriodStarts::at(db_now()),
        limits: UserLimits {
            user_id,
            policy_version: 1,
            standard: limits,
            premium: limits,
        },
    }
}

fn preflight() -> TurnPreflight {
    TurnPreflight {
        reserve_tokens: 10,
        max_output_tokens_applied: 5,
        reserved_credits_micro: 1000,
        policy_version_applied: 1,
        effective_model: MODEL.to_owned(),
        minimal_generation_floor_applied: 1,
    }
}

#[tokio::test]
async fn reserve_fills_preflight_once_and_rechecks_quota() {
    let fx = fx().await;
    let (rid, _) = fx.seed_turn_with("q", &[]).await;
    let target = fx
        .turns_svc
        .preview(&fx.ctx, fx.chat.id, rid, MutationKind::Retry)
        .await
        .unwrap();
    let r = replacement(&fx, false);
    let (turn_id, new_rid) = (r.turn.id, r.turn.request_id);
    fx.turns_svc.commit_replacement(&target, r).await.unwrap();
    let quota = Arc::clone(&fx.svc.quota);

    // Limit re-check fails: nothing written.
    let res =
        super::super::setup::commit_reserve(&fx.db, &quota, &reserve(&fx, 1), turn_id, preflight())
            .await;
    assert_eq!(
        res.unwrap_err(),
        DomainError::QuotaExceeded {
            scope: QuotaScope::Tokens
        }
    );
    assert_eq!(fx.turn(new_rid).await.reserve_tokens, None);
    assert_eq!(fx.reserved_total().await, 0);

    super::super::setup::commit_reserve(
        &fx.db,
        &quota,
        &reserve(&fx, 1_000_000_000),
        turn_id,
        preflight(),
    )
    .await
    .unwrap();
    let t = fx.turn(new_rid).await;
    assert_eq!(t.reserve_tokens, Some(10));
    assert_eq!(t.max_output_tokens_applied, Some(5));
    assert_eq!(t.reserved_credits_micro, Some(1000));
    assert_eq!(t.policy_version_applied, Some(1));
    assert_eq!(t.effective_model.as_deref(), Some(MODEL));
    assert_eq!(t.minimal_generation_floor_applied, Some(1));
    let reserved = fx.reserved_total().await;
    assert!(reserved > 0);

    // Written once: a second fill fails and rolls its reserve back.
    let mut again = preflight();
    again.reserve_tokens = 99;
    assert!(
        super::super::setup::commit_reserve(
            &fx.db,
            &quota,
            &reserve(&fx, 1_000_000_000),
            turn_id,
            again
        )
        .await
        .is_err()
    );
    assert_eq!(fx.turn(new_rid).await.reserve_tokens, Some(10));
    assert_eq!(fx.reserved_total().await, reserved);
}

#[tokio::test]
async fn attachment_deleted_after_preview_is_not_copied() {
    let fx = fx().await;
    let keep = fx
        .attachment("document", "ready", "file-doc10000000000")
        .await;
    let gone = fx
        .attachment("document", "ready", "file-doc20000000000")
        .await;
    let (rid, _) = fx.seed_turn_with("q", &[keep.id, gone.id]).await;
    let target = fx
        .turns_svc
        .preview(&fx.ctx, fx.chat.id, rid, MutationKind::Retry)
        .await
        .unwrap();
    assert_eq!(target.attachments.len(), 2);
    fx.update_attachment(gone.id, attachment::Column::DeletedAt, Some(db_now()))
        .await;
    let mut r = replacement(&fx, false);
    r.attachment_ids = vec![keep.id, gone.id];
    let message_id = r.message.id;
    fx.turns_svc.commit_replacement(&target, r).await.unwrap();
    assert_eq!(fx.links_of(message_id).await, vec![keep.id]);
}

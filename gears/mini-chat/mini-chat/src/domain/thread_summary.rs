//! Thread summary: prompt building, response parsing and the outbox-driven task
//! (DESIGN §3.6 "Thread Summary Update", B.5.5).

use std::sync::Arc;
use std::time::Duration;

use mini_chat_sdk::{ModelCatalogEntry, UsageEvent, UsageTokens};
use serde_json::json;
use toolkit_db::outbox::MessageResult;
use toolkit_security::constants::DEFAULT_SUBJECT_ID;
use uuid::Uuid;

use crate::config::DEFAULT_SUMMARY_SYSTEM_PROMPT;
use crate::domain::app::AppServices;
use crate::domain::error::DomainResult;
use crate::domain::time::now;
use crate::infra::db::entities::{message, thread_summary};
use crate::infra::db::repo;
use crate::infra::llm::ProviderCallError;
use crate::infra::llm::responses::{
    ChatRequest, InputMessage, Role, build_body, parse_usage, response_text,
};
use crate::infra::outbox::{ThreadSummaryPayload, Wakes};

const OPENING_NEW: &str = "Summarize the following conversation:";
const OPENING_MERGE: &str = "The existing summary below covers the earlier conversation. Incorporate it with the new messages into a single updated summary.\n\nIMPORTANT: Keep the summary concise. If the combined information is too large, prioritize: current topic and recent decisions > user preferences and corrections > older facts. Compress or drop the least relevant older details rather than letting the summary grow unboundedly.";
const NEW_MESSAGES: &str = "New messages to incorporate:";
const ANALYSIS_INSTRUCTION: &str = "Before providing your final summary, wrap your analysis in <analysis> tags. In your analysis:\n1. Chronologically review each exchange, identifying:\n   - The user's requests and questions\n   - Key decisions, answers, and information shared\n   - Any follow-up actions or commitments\n   - Specific names, dates, numbers, URLs, or references mentioned\n2. Verify accuracy and completeness.\n\nYour summary MUST include these sections:\n\n1. Conversation Purpose: The user's primary goals and recurring themes\n2. Key Information Exchanged: Important facts, decisions, recommendations, and answers\n3. User Requests and Preferences: All explicit user requests, stated preferences, and corrections\n4. Open Items: Any unresolved questions, wake actions, or things the user asked to revisit\n5. Current Topic: What was being discussed most recently, with enough detail to continue naturally\n\nRespond with an <analysis> block followed by a <summary> block.";

/// Logs an error when the summary model is missing or disabled (startup continues).
pub async fn check_summary_model(svc: &AppServices) {
    if !svc.cfg.thread_summary_worker.enabled {
        return;
    }
    let model_id = svc.cfg.thread_summary_worker.effective_summary_model_id();
    match svc.policy.current_snapshot(DEFAULT_SUBJECT_ID).await {
        Ok(s) => {
            if !s.find(model_id).is_some_and(|m| m.enabled) {
                tracing::error!(model = %model_id, "thread summary model is missing from the catalog or disabled");
            }
        }
        Err(e) => tracing::warn!(error = %e, "could not check the thread summary model"),
    }
}

/// System prompt: catalog `thread_summary_prompt`, else the configured prompt, else the default.
#[must_use]
pub fn system_prompt(model: &ModelCatalogEntry, configured: &str) -> String {
    if !model.thread_summary_prompt.trim().is_empty() {
        model.thread_summary_prompt.clone()
    } else if !configured.trim().is_empty() {
        configured.to_owned()
    } else {
        DEFAULT_SUMMARY_SYSTEM_PROMPT.to_owned()
    }
}

fn cut(content: &str, limit: usize) -> String {
    if limit == 0 || content.chars().count() <= limit {
        content.to_owned()
    } else {
        let s: String = content.chars().take(limit).collect();
        format!("{s}...")
    }
}

/// User prompt of the summary request.
#[must_use]
pub fn user_prompt(existing: Option<&str>, messages: &[(String, String)], limit: usize) -> String {
    let mut out = String::new();
    match existing {
        Some(s) => {
            out.push_str(OPENING_MERGE);
            out.push_str("\n\n<existing_summary>\n");
            out.push_str(s);
            out.push_str("\n</existing_summary>\n\n");
            out.push_str(NEW_MESSAGES);
        }
        None => out.push_str(OPENING_NEW),
    }
    out.push_str("\n\n");
    let entries: Vec<String> = messages
        .iter()
        .map(|(role, content)| {
            let label = if role == "assistant" {
                "Assistant"
            } else {
                "User"
            };
            format!("{label}: {}", cut(content, limit))
        })
        .collect();
    out.push_str(&entries.join("\n\n"));
    out.push_str("\n\n");
    out.push_str(ANALYSIS_INSTRUCTION);
    out
}

fn strip_block(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut s = text.to_owned();
    while let Some(start) = s.find(&open) {
        match s[start..].find(&close) {
            Some(rel) => {
                let end = start + rel + close.len();
                s.replace_range(start..end, "");
            }
            None => break,
        }
    }
    s
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::new();
    let mut blank = 0;
    for line in s.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out.trim().to_owned()
}

/// Extracts the stored summary from the model response.
#[must_use]
pub fn parse_summary(response: &str) -> String {
    let without_analysis = strip_block(response, "analysis");
    if let Some(start) = without_analysis.find("<summary>") {
        let rest = &without_analysis[start + "<summary>".len()..];
        let inner = rest.find("</summary>").map_or(rest, |end| &rest[..end]);
        return collapse_blank_lines(inner);
    }
    if without_analysis.contains("<analysis") || without_analysis.contains("<summary") {
        return String::new();
    }
    collapse_blank_lines(&without_analysis)
}

/// Prompt token estimate at `bytes_per_token_conservative`.
#[allow(clippy::integer_division)] // floor of bytes / bptc is the intended estimate
fn prompt_tokens(system: &str, user: &str, bptc: u32) -> i64 {
    let bytes = i64::try_from(system.len() + user.len()).unwrap_or(i64::MAX);
    bytes / i64::from(bptc.max(1)) + 1
}

/// Drops the oldest `ceil(n/5)` messages while the prompt is over budget (keeps at least two).
#[must_use]
pub fn fit_messages(
    model: &ModelCatalogEntry,
    system: &str,
    existing: Option<&str>,
    mut msgs: Vec<(String, String)>,
    limit: usize,
) -> Vec<(String, String)> {
    if model.context_window == 0 {
        return msgs;
    }
    let mut budget = i64::from(model.context_window) - i64::from(model.max_output_tokens);
    if model.max_input_tokens > 0 {
        budget = budget.min(i64::from(model.max_input_tokens));
    }
    let bptc = model.estimation_budgets.bytes_per_token_conservative;
    while msgs.len() > 2
        && prompt_tokens(system, &user_prompt(existing, &msgs, limit), bptc) > budget
    {
        let drop_n = msgs.len().div_ceil(5).min(msgs.len() - 2).max(1);
        msgs.drain(..drop_n);
    }
    msgs
}

fn summary_result(svc: &AppServices, result: &'static str) {
    svc.metrics
        .inc("thread_summary_execution", &[("result", result)]);
}

/// Runs one thread-summary work item.
#[allow(clippy::too_many_lines)]
pub async fn run_summary(svc: &Arc<AppServices>, p: &ThreadSummaryPayload) -> MessageResult {
    match run_summary_inner(svc, p).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, chat_id = %p.chat_id, "thread summary attempt failed");
            summary_result(svc, "retry");
            MessageResult::Retry
        }
    }
}

// Linear sequence of guarded steps of one summary attempt.
#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
async fn run_summary_inner(
    svc: &Arc<AppServices>,
    p: &ThreadSummaryPayload,
) -> DomainResult<MessageResult> {
    let base = match (p.base_frontier_created_at, p.base_frontier_message_id) {
        (Some(t), Some(id)) => Some((t, id)),
        _ => None,
    };
    let target = (p.frozen_target_created_at, p.frozen_target_message_id);
    let conn = svc.db.conn()?;
    let Some(chat) = repo::find_chat_any(&conn, p.tenant_id, p.chat_id).await? else {
        return Ok(MessageResult::Ok);
    };
    if chat.deleted_at.is_some() {
        return Ok(MessageResult::Ok);
    }
    let current = repo::find_summary(&conn, p.tenant_id, p.chat_id).await?;
    let current_frontier = current
        .as_ref()
        .map(|s| (s.summarized_up_to_created_at, s.summarized_up_to_message_id));
    match (base, current_frontier) {
        (Some(_), None) => {
            summary_result(svc, "base_missing");
            return Ok(MessageResult::Ok);
        }
        (b, c) if b != c => {
            svc.metrics.inc("thread_summary_cas_conflicts", &[]);
            return Ok(MessageResult::Ok);
        }
        _ => {}
    }
    let range: Vec<message::Model> =
        repo::summary_range(&conn, p.tenant_id, p.chat_id, base, target).await?;
    if range.is_empty() {
        return Ok(MessageResult::Ok);
    }

    let cfg = &svc.cfg.thread_summary_worker;
    let model_id = cfg.effective_summary_model_id().to_owned();
    let snapshot = svc.policy.current_snapshot(DEFAULT_SUBJECT_ID).await?;
    let Some(model) = snapshot.find(&model_id).filter(|m| m.enabled).cloned() else {
        tracing::error!(model = %model_id, "thread summary model is missing or disabled");
        summary_result(svc, "model_unavailable");
        return Ok(MessageResult::Reject(format!(
            "summary model '{model_id}' is unavailable"
        )));
    };
    let provider = svc.llm.resolve(&model.provider_id, p.tenant_id)?;
    let system = system_prompt(&model, &cfg.summary_system_prompt);
    let existing = current.as_ref().map(|s| s.summary_text.clone());
    let msgs: Vec<(String, String)> = range
        .iter()
        .filter(|m| m.role != "system")
        .map(|m| (m.role.clone(), m.content.clone()))
        .collect();
    let mut msgs = fit_messages(
        &model,
        &system,
        existing.as_deref(),
        msgs,
        cfg.message_content_limit,
    );

    let mut attempt = 0;
    let (text, usage) = loop {
        let req = ChatRequest {
            model: model.provider_model_id.clone(),
            instructions: system.clone(),
            input: vec![InputMessage {
                role: Role::User,
                text: user_prompt(existing.as_deref(), &msgs, cfg.message_content_limit),
                image_file_ids: Vec::new(),
            }],
            max_output_tokens: model.max_output_tokens,
            tools: Vec::new(),
            include_code_interpreter_outputs: false,
            max_tool_calls: None,
            user: format!(
                "{}{}",
                p.tenant_id.as_simple(),
                DEFAULT_SUBJECT_ID.as_simple()
            ),
            metadata: Some(json!({
                "tenant_id": p.tenant_id.to_string(),
                "user_id": DEFAULT_SUBJECT_ID.to_string(),
                "chat_id": p.chat_id.to_string(),
                "request_type": "summary",
                "feature": "none",
            })),
            api_params: model.general_config.api_params.clone(),
            stream: false,
            extra_input: Vec::new(),
        };
        let body = build_body(&req);
        let res = svc
            .llm
            .send_json(
                http::Method::POST,
                &provider.chat_uri(&model.provider_model_id),
                Some(&body),
                Duration::from_secs(cfg.claim_timeout_secs.saturating_sub(10).max(20)),
            )
            .await;
        match res {
            Ok(v) => {
                let usage = v.get("usage").and_then(parse_usage).unwrap_or_default();
                break (response_text(&v), usage);
            }
            Err(ProviderCallError::ContextLength(_)) if attempt < 2 && msgs.len() > 2 => {
                attempt += 1;
                let drop_n = msgs.len().div_ceil(5).min(msgs.len() - 2).max(1);
                msgs.drain(..drop_n);
            }
            Err(e) => {
                tracing::warn!(error = %e, chat_id = %p.chat_id, "thread summary provider call failed");
                summary_result(svc, "provider_error");
                return Ok(MessageResult::Retry);
            }
        }
    };
    let summary_text = parse_summary(&text);
    if summary_text.is_empty() {
        summary_result(svc, "empty_summary");
        return Ok(MessageResult::Retry);
    }
    let diff = usage.output_tokens - usage.reasoning_tokens;
    let token_estimate = if diff > 0 {
        diff
    } else {
        i64::try_from(summary_text.len().div_ceil(4)).unwrap_or(i64::MAX)
    };
    let ids: Vec<Uuid> = range.iter().map(|m| m.id).collect();
    let outbox = Arc::clone(&svc.outbox);
    let payload = p.clone();
    let model_id2 = model_id.clone();
    let res = svc
        .db
        .transaction(move |tx| {
            Box::pin(async move {
                let ts = now();
                let target_msg = repo::find_message(
                    tx,
                    payload.tenant_id,
                    payload.chat_id,
                    payload.frozen_target_message_id,
                )
                .await?;
                if target_msg.is_none() {
                    return Ok(("frontier_deleted", None));
                }
                let est = i32::try_from(token_estimate).unwrap_or(i32::MAX);
                match base {
                    None => {
                        let insert = repo::insert_summary(
                            tx,
                            thread_summary::Model {
                                id: Uuid::new_v4(),
                                tenant_id: payload.tenant_id,
                                chat_id: payload.chat_id,
                                summary_text,
                                summarized_up_to_created_at: target.0,
                                summarized_up_to_message_id: target.1,
                                token_estimate: est,
                                created_at: ts,
                                updated_at: ts,
                            },
                        )
                        .await;
                        match insert {
                            Ok(()) => {}
                            Err(e) if e.is_unique_violation() => return Ok(("cas_conflict", None)),
                            Err(e) => return Err(e),
                        }
                    }
                    Some(b) => {
                        let n = repo::cas_update_summary(
                            tx,
                            payload.tenant_id,
                            payload.chat_id,
                            b,
                            summary_text,
                            target,
                            est,
                            ts,
                        )
                        .await?;
                        if n == 0 {
                            return Ok(("cas_conflict", None));
                        }
                    }
                }
                repo::mark_compressed(tx, payload.tenant_id, payload.chat_id, ids).await?;
                let ev = UsageEvent {
                    tenant_id: payload.tenant_id,
                    user_id: None,
                    chat_id: payload.chat_id,
                    turn_id: None,
                    request_id: payload.system_request_id,
                    effective_model: model_id2.clone(),
                    selected_model: model_id2,
                    terminal_state: "completed".to_owned(),
                    billing_outcome: "system_task".to_owned(),
                    usage: Some(UsageTokens { ..usage }),
                    actual_credits_micro: 0,
                    settlement_method: "none".to_owned(),
                    policy_version_applied: 0,
                    web_search_calls: 0,
                    code_interpreter_calls: 0,
                    file_search_calls: 0,
                    timestamp: ts,
                    requester_type: "system".to_owned(),
                    dedupe_key: format!(
                        "{}/thread_summary_update/{}",
                        payload.tenant_id.as_simple(),
                        payload.system_request_id.as_simple()
                    ),
                    system_task_type: Some("thread_summary_update".to_owned()),
                };
                let mut wakes = Wakes::default();
                wakes.push(
                    outbox
                        .usage(tx, &ev)
                        .await
                        .map_err(crate::domain::turns::internal_payload)?,
                );
                Ok(("success", Some(wakes)))
            })
        })
        .await?;
    match res {
        ("success", Some(w)) => {
            w.fire();
            summary_result(svc, "success");
        }
        ("cas_conflict", _) => svc.metrics.inc("thread_summary_cas_conflicts", &[]),
        (label, _) => summary_result(svc, label),
    }
    Ok(MessageResult::Ok)
}

#[cfg(test)]
#[path = "thread_summary_tests.rs"]
mod thread_summary_tests;

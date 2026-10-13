//! S3 (streaming core) harness on top of the S1 fakes and the S2 chat harness.
//!
//! Kept apart from `common` and `chat_support` so the S1 / S2 test binaries
//! (which never declare `mod stream_support;`) compile independently of the
//! S3 production items. Everything here relies only on the items listed in
//! `S3-test-interface.md`. Test files declare `mod common; mod chat_support;
//! mod stream_support;`.
//!
//! * [`FakeOagw`] is an in-process `oagw_sdk::ServiceGatewayClientV1`: it
//!   scripts the provider answers of `proxy_request` (OpenAI Responses SSE
//!   streams, HTTP errors, gateway errors), records every request and keeps
//!   an in-memory upstream / route store for the provisioning tests.
//! * [`StreamHarness`] wires the real `StreamService` / `TurnsService` over the
//!   S2 chat harness database and PDP and registers the S3 routes.
//! * [`LiveSse`] / [`Sent`] read the SSE response incrementally / completely.

#![allow(
    dead_code,
    unused_imports,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::similar_names,
    clippy::use_debug,
    clippy::unreadable_literal,
    clippy::needless_pass_by_value
)]

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body as AxumBody, BodyDataStream, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use bytes::Bytes;
use futures::StreamExt;
use mini_chat::api::rest::register_stream_routes;
use mini_chat::config::MiniChatConfig;
use mini_chat::domain::error::DomainError;
use mini_chat::domain::streaming::StreamService;
use mini_chat::domain::turns::TurnsService;
use mini_chat::infra::db::entity::{chat_turns, chat_vector_stores, message_attachments, messages};
use mini_chat::infra::db::timestamp::{to_db, to_db_opt};
use mini_chat::infra::llm::LlmProviders;
use mini_chat::infra::model_policy::ModelPolicyGateway;
use mini_chat_sdk::{
    KillSwitches, MiniChatModelPolicyPluginClientV1, ModelCatalogEntry, ModelPolicyPluginError,
    PolicySnapshot, PolicyVersionInfo, PublishError, UsageEvent, UserLimits,
};
use oagw_sdk::api::{ErrorSource, ServiceGatewayClientV1};
use oagw_sdk::body::{Body, BodyStream, BoxError};
use oagw_sdk::{
    CreateRouteRequest, CreateUpstreamRequest, ListQuery, Route, UpdateRouteRequest,
    UpdateUpstreamRequest, Upstream,
};
use sea_orm::ActiveValue::Set;
use sea_orm::EntityTrait;
use serde_json::{Value, json};
use time::OffsetDateTime;
use toolkit_canonical_errors::{CanonicalError, resource_error};
use toolkit_db::secure::{SecureEntityExt, secure_insert};
use toolkit_security::{AccessScope, SecurityContext};
use tower::ServiceExt;
use uuid::Uuid;

pub use crate::chat_support::{
    AttachmentSeed, ChatHarness, ChatHarnessOptions, ChatSeed, MessageSeed, ids_of, page_items,
    parse_ts, ts, uid,
};
pub use crate::common::{
    FakePdp, PdpMode, Resp, TENANT_A, TENANT_B, USER_A, USER_B, assert_problem, ctx, ctx_a,
};

// ── constants ────────────────────────────────────────────────────────────────

/// `providers.<id>` entry every stream-test catalog model points at.
pub const PROVIDER_ID: &str = "openai";
/// `upstream_alias` of that entry: the first path segment of the proxied URI.
pub const PROVIDER_ALIAS: &str = "provider-alias";
/// `api_path` of that entry.
pub const API_PATH: &str = "/v1/responses";
/// System prompt of every stream-test catalog model.
pub const SYSTEM_PROMPT: &str = "You are the stream test assistant.";
pub const WEB_GUARD: &str = "TEST-WEB-SEARCH-GUARD";
pub const FILE_GUARD: &str = "TEST-FILE-SEARCH-GUARD";

/// Provider-side model name of a catalog id (never equal to the catalog id).
pub fn provider_model(id: &str) -> String {
    format!("provider-model-{id}")
}

pub fn second_user() -> SecurityContext {
    crate::chat_support::ctx_a2()
}

pub fn tenant_b_user() -> SecurityContext {
    crate::chat_support::ctx_b()
}

// ── catalog ──────────────────────────────────────────────────────────────────

/// JSON of one catalog entry with every field the streaming code reads; tests
/// tweak the returned value (`m["context_window"] = json!(...)`).
pub fn stream_model_json(id: &str, tier: &str) -> Value {
    let premium = tier.eq_ignore_ascii_case("premium");
    json!({
        "id": id,
        "provider_model_id": provider_model(id),
        "display_name": format!("Display {id}"),
        "description": "",
        "provider_id": PROVIDER_ID,
        "provider_display_name": "Provider Inc.",
        "icon": "",
        "tier": tier,
        "enabled": true,
        "system_prompt": SYSTEM_PROMPT,
        "thread_summary_prompt": "",
        "multimodal_capabilities": ["VISION_INPUT", "RAG"],
        "context_window": 128_000,
        "max_output_tokens": 4096,
        "max_input_tokens": 100_000,
        "input_tokens_credit_multiplier_micro": 1_000_000,
        "output_tokens_credit_multiplier_micro": 3_000_000,
        "multiplier_display": if premium { "3x" } else { "1x" },
        "estimation_budgets": {
            "bytes_per_token_conservative": 4,
            "fixed_overhead_tokens": 100,
            "safety_margin_pct": 10,
            "image_token_budget": 1000,
            "tool_surcharge_tokens": 500,
            "web_search_surcharge_tokens": 500,
            "code_interpreter_surcharge_tokens": 1000,
            "minimal_generation_floor": 50
        },
        "max_num_results": 5,
        "web_search_context_size": "medium",
        "max_tool_calls": 3,
        "general_config": {
            "type": "model.general.v1",
            "available_from": "1970-01-01T00:00:00Z",
            "max_file_size_mb": 25,
            "api_params": {"temperature": 0.7, "top_p": 0.9, "frequency_penalty": 0.1,
                           "presence_penalty": 0.2, "stop": []},
            "features": {"streaming": true, "structured_output": false},
            "tool_support": {"web_search": true, "file_search": true,
                             "image_generation": false, "code_interpreter": true, "mcp": false},
            "supported_endpoints": {"chat_completions": false, "responses": true,
                                    "embeddings": false, "image_generation": false,
                                    "audio_speech_generation": false,
                                    "audio_transcription": false, "audio_translation": false}
        },
        "preference": {"is_default": premium, "sort_order": 0}
    })
}

/// `gpt-premium` (default), `gpt-standard` (no web search), `gpt-off` (disabled).
pub fn default_stream_models() -> Vec<Value> {
    let mut standard = stream_model_json("gpt-standard", "Standard");
    standard["general_config"]["tool_support"]["web_search"] = json!(false);
    let mut off = stream_model_json("gpt-off", "Standard");
    off["enabled"] = json!(false);
    vec![stream_model_json("gpt-premium", "Premium"), standard, off]
}

/// A catalog of the single premium model `gpt-premium` with `patch` merged in.
pub fn premium_with(patch: Value) -> Vec<Value> {
    let mut model = stream_model_json("gpt-premium", "Premium");
    merge(&mut model, &patch);
    vec![model]
}

pub fn model_entries(models: &[Value]) -> Vec<ModelCatalogEntry> {
    models
        .iter()
        .map(|m| serde_json::from_value(m.clone()).expect("catalog entry"))
        .collect()
}

/// Recursive JSON merge (`patch` wins, objects merge).
pub fn merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                merge(b.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (b, p) => *b = p.clone(),
    }
}

/// Gear configuration with one provider entry (`openai`, alias
/// [`PROVIDER_ALIAS`]) and the test guards; `patch` is merged on top.
pub fn stream_config(patch: &Value) -> Arc<MiniChatConfig> {
    let mut cfg = json!({
        "client_credentials": {"client_id": "mini-chat", "client_secret": "s3cr3t-value"},
        "providers": {
            PROVIDER_ID: {
                "kind": "openai_responses",
                "host": "provider.test",
                "upstream_alias": PROVIDER_ALIAS,
                "api_path": API_PATH,
                "storage_kind": "openai",
                "auth_plugin_type": "gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.apikey.v1",
                "auth_config": {"header": "Authorization", "prefix": "Bearer ",
                                "secret_ref": "cred://openai-key"}
            }
        },
        "context": {"web_search_guard": WEB_GUARD, "file_search_guard": FILE_GUARD}
    });
    merge(&mut cfg, patch);
    let cfg: MiniChatConfig = serde_json::from_value(cfg).expect("gear configuration");
    cfg.validate().expect("valid gear configuration");
    Arc::new(cfg)
}

// ── scriptable model-policy plugin (catalog + kill switches) ─────────────────

pub struct StreamPolicy {
    catalog: Mutex<Vec<ModelCatalogEntry>>,
    switches: Mutex<KillSwitches>,
    pub snapshot_calls: AtomicUsize,
}

impl StreamPolicy {
    pub fn new(models: &[Value]) -> Arc<Self> {
        Arc::new(Self {
            catalog: Mutex::new(model_entries(models)),
            switches: Mutex::new(no_kill_switches()),
            snapshot_calls: AtomicUsize::new(0),
        })
    }

    pub fn set_catalog(&self, models: &[Value]) {
        *self.catalog.lock().unwrap() = model_entries(models);
    }

    /// Replace the kill switches; `patch` lists the switches to turn on.
    pub fn set_kill_switches(&self, patch: Value) {
        let mut all = serde_json::to_value(no_kill_switches()).unwrap();
        merge(&mut all, &patch);
        *self.switches.lock().unwrap() = serde_json::from_value(all).expect("kill switches");
    }
}

pub fn no_kill_switches() -> KillSwitches {
    serde_json::from_value(json!({
        "disable_premium_tier": false, "force_standard_tier": false,
        "disable_web_search": false, "disable_file_search": false,
        "disable_images": false, "disable_code_interpreter": false,
    }))
    .expect("kill switches")
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for StreamPolicy {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<PolicyVersionInfo, ModelPolicyPluginError> {
        Ok(serde_json::from_value(
            json!({"policy_version": 1, "generated_at": "2026-02-27T13:00:00Z"}),
        )
        .expect("policy version"))
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, ModelPolicyPluginError> {
        self.snapshot_calls.fetch_add(1, Ordering::SeqCst);
        Ok(PolicySnapshot {
            policy_version: 1,
            model_catalog: self.catalog.lock().unwrap().clone(),
            kill_switches: self.switches.lock().unwrap().clone(),
        })
    }

    async fn get_user_limits(
        &self,
        user_id: Uuid,
        policy_version: u64,
    ) -> Result<UserLimits, ModelPolicyPluginError> {
        Ok(serde_json::from_value(json!({
            "user_id": user_id,
            "policy_version": policy_version,
            "standard": {"limit_daily_credits_micro": 100_000_000_000_i64,
                         "limit_monthly_credits_micro": 1_000_000_000_000_i64},
            "premium": {"limit_daily_credits_micro": 100_000_000_000_i64,
                        "limit_monthly_credits_micro": 1_000_000_000_000_i64},
        }))
        .expect("user limits"))
    }

    async fn publish_usage(&self, _payload: UsageEvent) -> Result<(), PublishError> {
        Ok(())
    }
}

// ── fake OAGW ────────────────────────────────────────────────────────────────

#[resource_error(gts_id!("cf.core.oagw.proxy.v1~"))]
struct ProxyScope;

/// One piece of a scripted provider SSE body.
#[derive(Clone, Debug)]
pub enum Piece {
    /// `event: <name>\ndata: <json>\n\n`
    Event(String, Value),
    /// Bytes written as is (for frames without an `event:` line, junk data, ...).
    Raw(String),
    /// Wait until `FakeOagw::release()` is called.
    Pause,
    /// Wait this long.
    Sleep(Duration),
    /// The body stream yields an error (provider connection drop).
    Fail(String),
}

pub fn ev(name: &str, data: Value) -> Piece {
    Piece::Event(name.to_owned(), data)
}

/// What the fake answers to one `proxy_request`.
pub enum ProviderReply {
    /// 200 `text/event-stream`.
    Sse(Vec<Piece>),
    /// 200 `application/json`: the body of a non-streaming Responses API call.
    Json(Value),
    /// Non-2xx answer of the provider (upstream error source).
    Http {
        status: u16,
        body: Value,
        headers: Vec<(&'static str, String)>,
    },
    /// The proxy call itself fails (gateway-originated error).
    Gateway(CanonicalError),
    /// The gateway answers with its own RFC 9457 problem document
    /// (`x-oagw-error-source: gateway`), e.g. 502 when the upstream is down.
    GatewayProblem { status: u16, body: Value },
}

impl ProviderReply {
    pub fn sse(pieces: Vec<Piece>) -> Self {
        Self::Sse(pieces)
    }

    /// A normal completed answer streaming `chunks` (default [`Opts`]).
    pub fn text(chunks: &[&str]) -> Self {
        Self::Sse(responses_stream(chunks, &Opts::default()))
    }

    pub fn text_with(chunks: &[&str], opts: &Opts) -> Self {
        Self::Sse(responses_stream(chunks, opts))
    }

    /// A non-streaming answer: the JSON Responses body holding `text`, with the usage and
    /// the status of `opts` (pieces of `opts` that only make sense for a stream are ignored).
    pub fn json_text(text: &str, opts: &Opts) -> Self {
        Self::Json(json!({
            "id": opts.response_id,
            "object": "response",
            "model": opts.model,
            "status": if opts.incomplete { "incomplete" } else { "completed" },
            "error": null,
            "output": [{
                "type": "message",
                "role": "assistant",
                "status": "completed",
                "content": [{
                    "type": "output_text",
                    "text": text,
                    "annotations": opts.annotations,
                }],
            }],
            "usage": {
                "input_tokens": opts.input_tokens,
                "output_tokens": opts.output_tokens,
                "input_tokens_details": {"cached_tokens": opts.cached_tokens},
                "output_tokens_details": {"reasoning_tokens": opts.reasoning_tokens},
            },
        }))
    }

    pub fn http_error(status: u16, message: &str) -> Self {
        Self::Http {
            status,
            body: json!({"error": {"message": message, "type": "invalid_request_error",
                                   "code": "mock_error"}}),
            headers: vec![],
        }
    }

    pub fn http_error_with(
        status: u16,
        message: &str,
        headers: Vec<(&'static str, String)>,
    ) -> Self {
        Self::Http {
            status,
            body: json!({"error": {"message": message, "type": "server_error", "code": "x"}}),
            headers,
        }
    }

    /// A non-2xx answer of the provider with an arbitrary JSON body.
    pub fn http_body(status: u16, body: Value) -> Self {
        Self::Http {
            status,
            body,
            headers: vec![],
        }
    }

    /// OAGW's own problem document for an unreachable upstream: it names the
    /// upstream host, the proxied path and internal type URIs.
    pub fn gateway_problem(status: u16) -> Self {
        Self::GatewayProblem {
            status,
            body: json!({
                "type": "gts://gts.cf.core.errors.err.v1~cf.core.err.service_unavailable.v1~",
                "title": "Service Unavailable",
                "status": status,
                "detail": "Service temporarily unavailable",
                "instance": "/api.openai-internal.example/v1/responses",
                "context": {"retry_after_seconds": 5},
            }),
        }
    }

    /// The gateway's own `deadline_exceeded` problem (HTTP 504 of OAGW).
    pub fn gateway_timeout() -> Self {
        Self::Gateway(ProxyScope::deadline_exceeded("upstream did not respond in time").create())
    }

    /// Any other gateway failure.
    pub fn gateway_unavailable() -> Self {
        Self::Gateway(CanonicalError::service_unavailable().create())
    }
}

/// Knobs of [`responses_stream`].
#[derive(Clone, Debug)]
pub struct Opts {
    pub response_id: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// `usage.input_tokens_details.cached_tokens`.
    pub cached_tokens: u64,
    /// `usage.output_tokens_details.reasoning_tokens`.
    pub reasoning_tokens: u64,
    /// `response.completed` (false) or `response.incomplete` (true).
    pub incomplete: bool,
    /// Annotations placed on the `output_text` part of the final response.
    pub annotations: Vec<Value>,
    /// Pieces inserted after the n-th delta (`0` = before the first one).
    pub inserts: Vec<(usize, Vec<Piece>)>,
    /// Do not emit the terminal event (stream just ends).
    pub no_terminal: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            response_id: "resp_fakeDefault0001".to_owned(),
            model: "provider-model".to_owned(),
            input_tokens: 12,
            output_tokens: 5,
            cached_tokens: 0,
            reasoning_tokens: 0,
            incomplete: false,
            annotations: vec![],
            inserts: vec![],
            no_terminal: false,
        }
    }
}

impl Opts {
    pub fn usage(mut self, input: u64, output: u64) -> Self {
        self.input_tokens = input;
        self.output_tokens = output;
        self
    }

    /// The cache / reasoning breakdown the provider reports with its usage.
    pub fn details(mut self, cached_tokens: u64, reasoning_tokens: u64) -> Self {
        self.cached_tokens = cached_tokens;
        self.reasoning_tokens = reasoning_tokens;
        self
    }

    pub fn response_id(mut self, id: &str) -> Self {
        id.clone_into(&mut self.response_id);
        self
    }

    pub fn incomplete(mut self) -> Self {
        self.incomplete = true;
        self
    }

    pub fn annotated(mut self, annotations: Vec<Value>) -> Self {
        self.annotations = annotations;
        self
    }

    /// Pause (until `FakeOagw::release()`) after the n-th delta.
    pub fn pause_after(mut self, deltas: usize) -> Self {
        self.inserts.push((deltas, vec![Piece::Pause]));
        self
    }

    pub fn insert_after(mut self, deltas: usize, pieces: Vec<Piece>) -> Self {
        self.inserts.push((deltas, pieces));
        self
    }

    pub fn no_terminal(mut self) -> Self {
        self.no_terminal = true;
        self
    }
}

fn response_object(opts: &Opts, status: &str, text: &str, with_usage: bool) -> Value {
    let mut response = json!({
        "id": opts.response_id,
        "object": "response",
        "status": status,
        "model": opts.model,
        "output": [],
    });
    if status != "in_progress" {
        response["output"] = json!([{
            "id": "msg_item_1", "type": "message", "status": "completed", "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": opts.annotations}]
        }]);
    }
    if with_usage {
        response["usage"] = json!({
            "input_tokens": opts.input_tokens,
            "input_tokens_details": {"cached_tokens": opts.cached_tokens},
            "output_tokens": opts.output_tokens,
            "output_tokens_details": {"reasoning_tokens": opts.reasoning_tokens},
            "total_tokens": opts.input_tokens + opts.output_tokens,
        });
    }
    response
}

/// The OpenAI Responses SSE sequence of a plain text answer: `response.created`,
/// `response.in_progress`, one `response.output_text.delta` per chunk,
/// `response.output_text.done`, then `response.completed` / `response.incomplete`.
pub fn responses_stream(chunks: &[&str], opts: &Opts) -> Vec<Piece> {
    let text: String = chunks.concat();
    let mut out = vec![
        ev(
            "response.created",
            json!({"type": "response.created", "sequence_number": 0,
                   "response": response_object(opts, "in_progress", "", false)}),
        ),
        ev(
            "response.in_progress",
            json!({"type": "response.in_progress", "sequence_number": 1,
                   "response": response_object(opts, "in_progress", "", false)}),
        ),
    ];
    let inserts_at = |n: usize| -> Vec<Piece> {
        opts.inserts
            .iter()
            .filter(|(at, _)| *at == n)
            .flat_map(|(_, pieces)| pieces.clone())
            .collect()
    };
    out.extend(inserts_at(0));
    for (i, chunk) in chunks.iter().enumerate() {
        out.push(ev(
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "sequence_number": i + 2,
                   "item_id": "msg_item_1", "output_index": 0, "content_index": 0,
                   "delta": chunk}),
        ));
        out.extend(inserts_at(i + 1));
    }
    out.push(ev(
        "response.output_text.done",
        json!({"type": "response.output_text.done", "item_id": "msg_item_1",
               "output_index": 0, "content_index": 0, "text": text}),
    ));
    if !opts.no_terminal {
        let (name, status) = if opts.incomplete {
            ("response.incomplete", "incomplete")
        } else {
            ("response.completed", "completed")
        };
        out.push(ev(
            name,
            json!({"type": name, "response": response_object(opts, status, &text, true)}),
        ));
    }
    out
}

/// A stream that emits `partial` deltas and ends with `response.failed`.
pub fn failed_stream(
    code: &str,
    message: &str,
    partial: &[&str],
    usage: Option<(u64, u64)>,
) -> Vec<Piece> {
    failed_stream_with_details(code, message, partial, usage, (0, 0))
}

/// [`failed_stream`] whose usage also carries `(cached_tokens, reasoning_tokens)`.
pub fn failed_stream_with_details(
    code: &str,
    message: &str,
    partial: &[&str],
    usage: Option<(u64, u64)>,
    (cached, reasoning): (u64, u64),
) -> Vec<Piece> {
    let opts = Opts::default().no_terminal();
    let mut out = responses_stream(partial, &opts);
    let mut failed = json!({
        "id": "resp_failedRun000001", "object": "response", "status": "failed", "output": [],
        "error": {"code": code, "message": message},
    });
    if let Some((input, output)) = usage {
        failed["usage"] = json!({
            "input_tokens": input,
            "input_tokens_details": {"cached_tokens": cached},
            "output_tokens": output,
            "output_tokens_details": {"reasoning_tokens": reasoning},
            "total_tokens": input + output,
        });
    }
    out.push(ev(
        "response.failed",
        json!({"type": "response.failed", "response": failed}),
    ));
    out
}

pub fn file_search_pieces(results: Option<usize>) -> Vec<Piece> {
    let mut done = json!({"type": "response.file_search_call.completed", "item_id": "fs_1",
                          "output_index": 0});
    if let Some(n) = results {
        done["results"] = json!(
            (0..n)
                .map(|i| json!({"file_id": format!("f{i}")}))
                .collect::<Vec<_>>()
        );
    }
    vec![
        ev(
            "response.file_search_call.in_progress",
            json!({"type": "response.file_search_call.in_progress", "item_id": "fs_1"}),
        ),
        ev(
            "response.file_search_call.searching",
            json!({"type": "response.file_search_call.searching", "item_id": "fs_1"}),
        ),
        ev("response.file_search_call.completed", done),
    ]
}

pub fn web_search_pieces() -> Vec<Piece> {
    vec![
        ev(
            "response.web_search_call.in_progress",
            json!({"type": "response.web_search_call.in_progress", "item_id": "ws_1"}),
        ),
        ev(
            "response.web_search_call.searching",
            json!({"type": "response.web_search_call.searching", "item_id": "ws_1"}),
        ),
        ev(
            "response.web_search_call.completed",
            json!({"type": "response.web_search_call.completed", "item_id": "ws_1"}),
        ),
    ]
}

/// `in_progress`, `interpreting`, `completed` (all three are provider noise
/// except `in_progress`) and the `output_item.done` that carries the logs.
pub fn code_interpreter_pieces(logs: &str) -> Vec<Piece> {
    vec![
        ev(
            "response.code_interpreter_call.in_progress",
            json!({"type": "response.code_interpreter_call.in_progress", "item_id": "ci_1"}),
        ),
        ev(
            "response.code_interpreter_call.interpreting",
            json!({"type": "response.code_interpreter_call.interpreting", "item_id": "ci_1"}),
        ),
        ev(
            "response.code_interpreter_call.completed",
            json!({"type": "response.code_interpreter_call.completed", "item_id": "ci_1"}),
        ),
        ev(
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 1,
                   "item": {"id": "ci_1", "type": "code_interpreter_call", "status": "completed",
                            "code": "print(1)",
                            "outputs": [{"type": "logs", "logs": logs}]}}),
        ),
    ]
}

pub fn web_annotation(url: &str, title: &str, start: u64, end: u64) -> Value {
    json!({"type": "url_citation", "url": url, "title": title,
           "start_index": start, "end_index": end})
}

pub fn file_annotation(file_id: &str, filename: &str) -> Value {
    json!({"type": "file_citation", "file_id": file_id, "filename": filename, "index": 0})
}

#[derive(Default)]
struct Shared {
    released: AtomicBool,
    paused: AtomicUsize,
    pulled: AtomicUsize,
    finished: AtomicUsize,
    dropped_early: AtomicUsize,
}

struct LiveBody {
    pieces: VecDeque<Piece>,
    shared: Arc<Shared>,
    finished: std::cell::Cell<bool>,
}

impl Drop for LiveBody {
    fn drop(&mut self) {
        if self.finished.get() {
            self.shared.finished.fetch_add(1, Ordering::SeqCst);
        } else {
            self.shared.dropped_early.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn live_body(pieces: Vec<Piece>, shared: Arc<Shared>) -> BodyStream {
    let state = LiveBody {
        pieces: pieces.into(),
        shared,
        finished: std::cell::Cell::new(false),
    };
    Box::pin(futures::stream::unfold(state, |mut live| async move {
        loop {
            let Some(piece) = live.pieces.pop_front() else {
                live.finished.set(true);
                return None;
            };
            match piece {
                Piece::Event(name, data) => {
                    live.shared.pulled.fetch_add(1, Ordering::SeqCst);
                    // handing out the last frame completes the stream: dropping the body
                    // afterwards is not an early drop
                    live.finished.set(live.pieces.is_empty());
                    let frame = format!("event: {name}\ndata: {data}\n\n");
                    return Some((Ok::<Bytes, BoxError>(Bytes::from(frame)), live));
                }
                Piece::Raw(frame) => {
                    live.shared.pulled.fetch_add(1, Ordering::SeqCst);
                    live.finished.set(live.pieces.is_empty());
                    return Some((Ok(Bytes::from(frame)), live));
                }
                Piece::Sleep(d) => tokio::time::sleep(d).await,
                Piece::Pause => {
                    live.shared.paused.fetch_add(1, Ordering::SeqCst);
                    while !live.shared.released.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    live.shared.paused.fetch_sub(1, Ordering::SeqCst);
                }
                Piece::Fail(message) => {
                    live.pieces.clear();
                    live.finished.set(true);
                    return Some((Err(message.into()), live));
                }
            }
        }
    }))
}

/// One request that reached `proxy_request` with a chat-style JSON body.
#[derive(Clone, Debug)]
pub struct RecordedProviderRequest {
    pub method: http::Method,
    pub uri: http::Uri,
    pub headers: HeaderMap,
    pub body: Value,
    pub raw: Bytes,
    pub subject_id: Uuid,
    pub tenant_id: Uuid,
}

impl RecordedProviderRequest {
    /// Path of the proxied URI: `/{alias}{api_path}`.
    pub fn path(&self) -> &str {
        self.uri.path()
    }

    pub fn model(&self) -> &str {
        self.body["model"].as_str().unwrap_or_default()
    }

    fn item_text(item: &Value) -> String {
        match &item["content"] {
            Value::String(s) => s.clone(),
            Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        }
    }

    fn input_items(&self) -> Vec<Value> {
        match &self.body["input"] {
            Value::Array(items) => items.clone(),
            Value::String(s) => vec![json!({"role": "user", "content": s})],
            _ => vec![],
        }
    }

    /// System prompt text: the `instructions` field plus every `system` /
    /// `developer` item of `input` (the adapter may use either representation).
    pub fn system_text(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(s) = self.body["instructions"].as_str() {
            parts.push(s.to_owned());
        }
        for item in self.input_items() {
            if matches!(item["role"].as_str(), Some("system" | "developer")) {
                parts.push(Self::item_text(&item));
            }
        }
        parts.join("\n")
    }

    /// `(role, text)` of every `user` / `assistant` item of `input`, in order.
    pub fn conversation(&self) -> Vec<(String, String)> {
        self.input_items()
            .iter()
            .filter_map(|item| {
                let role = item["role"].as_str()?;
                matches!(role, "user" | "assistant")
                    .then(|| (role.to_owned(), Self::item_text(item)))
            })
            .collect()
    }

    /// The conversation as `"role:text"` strings (compact assertions).
    pub fn conversation_texts(&self) -> Vec<String> {
        self.conversation()
            .into_iter()
            .map(|(r, t)| format!("{r}:{t}"))
            .collect()
    }

    pub fn tools(&self) -> Vec<Value> {
        self.body["tools"].as_array().cloned().unwrap_or_default()
    }

    pub fn tool_types(&self) -> Vec<String> {
        self.tools()
            .iter()
            .filter_map(|t| t["type"].as_str().map(str::to_owned))
            .collect()
    }

    pub fn tool(&self, ty: &str) -> Option<Value> {
        self.tools().into_iter().find(|t| t["type"] == json!(ty))
    }
}

/// Which failure the fake injects for an upstream alias / route path.
#[derive(Clone, Copy, Debug)]
pub enum Fault {
    /// `invalid_argument`: deterministic misconfiguration.
    InvalidArgument,
    /// `service_unavailable`: transient (for example the secret is not readable yet).
    Unavailable,
}

impl Fault {
    fn error(self) -> CanonicalError {
        match self {
            Self::InvalidArgument => ProxyScope::invalid_argument()
                .with_field_violation("server", "bad upstream", "INVALID_UPSTREAM")
                .create(),
            Self::Unavailable => CanonicalError::service_unavailable().create(),
        }
    }
}

type DefaultReply = Box<dyn Fn() -> ProviderReply + Send + Sync>;

pub struct FakeOagw {
    replies: Mutex<VecDeque<ProviderReply>>,
    default_reply: Mutex<DefaultReply>,
    recorded: Mutex<Vec<RecordedProviderRequest>>,
    shared: Arc<Shared>,
    upstreams: Mutex<Vec<Upstream>>,
    routes: Mutex<Vec<Route>>,
    upstream_faults: Mutex<HashMap<String, Fault>>,
    route_faults: Mutex<Vec<(String, Fault)>>,
    pub create_upstream_calls: AtomicUsize,
    pub create_route_calls: AtomicUsize,
}

impl FakeOagw {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(VecDeque::new()),
            default_reply: Mutex::new(Box::new(|| {
                ProviderReply::text(&["Hello", " from the fake provider."])
            })),
            recorded: Mutex::new(Vec::new()),
            shared: Arc::new(Shared::default()),
            upstreams: Mutex::new(Vec::new()),
            routes: Mutex::new(Vec::new()),
            upstream_faults: Mutex::new(HashMap::new()),
            route_faults: Mutex::new(Vec::new()),
            create_upstream_calls: AtomicUsize::new(0),
            create_route_calls: AtomicUsize::new(0),
        })
    }

    // ── scripting ────────────────────────────────────────────────────────────

    /// Queue one answer (served FIFO; the default answer applies when empty).
    pub fn push(&self, reply: ProviderReply) {
        self.replies.lock().unwrap().push_back(reply);
    }

    pub fn set_default(&self, factory: impl Fn() -> ProviderReply + Send + Sync + 'static) {
        *self.default_reply.lock().unwrap() = Box::new(factory);
    }

    /// Let every paused stream (and every later `Piece::Pause`) continue.
    pub fn release(&self) {
        self.shared.released.store(true, Ordering::SeqCst);
    }

    /// Wait until `n` streams sit on a `Piece::Pause`.
    pub async fn wait_paused(&self, n: usize) {
        for _ in 0..500 {
            if self.shared.paused.load(Ordering::SeqCst) >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "expected {n} paused provider stream(s), got {}",
            self.shared.paused.load(Ordering::SeqCst)
        );
    }

    /// Wait until `n` provider streams were dropped before they ended (the
    /// hard cancel of DESIGN 3.6 "Streaming Cancellation").
    pub async fn wait_dropped_early(&self, n: usize) {
        for _ in 0..500 {
            if self.dropped_early() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "expected {n} provider stream(s) cancelled, got {}",
            self.dropped_early()
        );
    }

    pub fn dropped_early(&self) -> usize {
        self.shared.dropped_early.load(Ordering::SeqCst)
    }

    pub fn finished_streams(&self) -> usize {
        self.shared.finished.load(Ordering::SeqCst)
    }

    /// Provider SSE frames pulled by the reader so far (backpressure probe).
    pub fn pulled_frames(&self) -> usize {
        self.shared.pulled.load(Ordering::SeqCst)
    }

    // ── recording ────────────────────────────────────────────────────────────

    pub fn requests(&self) -> Vec<RecordedProviderRequest> {
        self.recorded.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.recorded.lock().unwrap().len()
    }

    pub fn last_request(&self) -> RecordedProviderRequest {
        self.requests()
            .pop()
            .expect("the provider was never called")
    }

    pub fn assert_no_provider_call(&self) {
        assert_eq!(
            self.request_count(),
            0,
            "the provider must not be called, got {:?}",
            self.requests()
                .iter()
                .map(|r| r.uri.to_string())
                .collect::<Vec<_>>()
        );
    }

    // ── provisioning store ───────────────────────────────────────────────────

    pub fn fail_upstream(&self, alias: &str, fault: Fault) {
        self.upstream_faults
            .lock()
            .unwrap()
            .insert(alias.to_owned(), fault);
    }

    pub fn clear_upstream_faults(&self) {
        self.upstream_faults.lock().unwrap().clear();
    }

    /// Route creation fails for routes whose path starts with `prefix`.
    pub fn fail_routes_under(&self, prefix: &str, fault: Fault) {
        self.route_faults
            .lock()
            .unwrap()
            .push((prefix.to_owned(), fault));
    }

    pub fn stored_upstreams(&self) -> Vec<Upstream> {
        self.upstreams.lock().unwrap().clone()
    }

    pub fn stored_routes(&self) -> Vec<Route> {
        self.routes.lock().unwrap().clone()
    }

    /// Pre-existing upstream (the alias is "already taken" for `create_upstream`).
    pub fn preload_upstream(&self, alias: &str) -> Upstream {
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id: TENANT_A,
            alias: alias.to_owned(),
            server: oagw_sdk::Server { endpoints: vec![] },
            protocol: oagw_sdk::HTTP_PROTOCOL_ID.to_owned(),
            enabled: true,
            auth: None,
            headers: None,
            plugins: None,
            rate_limit: None,
            cors: None,
            tags: vec![],
        };
        self.upstreams.lock().unwrap().push(upstream.clone());
        upstream
    }
}

fn unsupported(what: &str) -> CanonicalError {
    CanonicalError::internal(format!("fake oagw: {what} is not scripted")).create()
}

#[async_trait]
impl ServiceGatewayClientV1 for FakeOagw {
    async fn create_upstream(
        &self,
        ctx: SecurityContext,
        req: CreateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        self.create_upstream_calls.fetch_add(1, Ordering::SeqCst);
        let alias = req
            .alias()
            .map(str::to_owned)
            .or_else(|| req.server().endpoints.first().map(|e| e.host.clone()))
            .unwrap_or_default();
        if let Some(fault) = self.upstream_faults.lock().unwrap().get(&alias).copied() {
            return Err(fault.error());
        }
        let mut store = self.upstreams.lock().unwrap();
        if store.iter().any(|u| u.alias == alias) {
            return Err(ProxyScope::already_exists("alias already exists")
                .with_resource(alias)
                .create());
        }
        let upstream = Upstream {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            alias,
            server: req.server().clone(),
            protocol: req.protocol().to_owned(),
            enabled: req.enabled(),
            auth: req.auth().cloned(),
            headers: req.headers().cloned(),
            plugins: req.plugins().cloned(),
            rate_limit: req.rate_limit().cloned(),
            cors: req.cors().cloned(),
            tags: req.tags().to_vec(),
        };
        store.push(upstream.clone());
        Ok(upstream)
    }

    async fn get_upstream(
        &self,
        _ctx: SecurityContext,
        id: Uuid,
    ) -> Result<Upstream, CanonicalError> {
        self.upstreams
            .lock()
            .unwrap()
            .iter()
            .find(|u| u.id == id)
            .cloned()
            .ok_or_else(|| {
                ProxyScope::not_found("upstream not found")
                    .with_resource(id.to_string())
                    .create()
            })
    }

    async fn list_upstreams(
        &self,
        _ctx: SecurityContext,
        query: &ListQuery,
    ) -> Result<Vec<Upstream>, CanonicalError> {
        Ok(self
            .upstreams
            .lock()
            .unwrap()
            .iter()
            .skip(query.skip as usize)
            .take(query.top as usize)
            .cloned()
            .collect())
    }

    async fn update_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateUpstreamRequest,
    ) -> Result<Upstream, CanonicalError> {
        Err(unsupported("update_upstream"))
    }

    async fn delete_upstream(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
    ) -> Result<(), CanonicalError> {
        Err(unsupported("delete_upstream"))
    }

    async fn create_route(
        &self,
        ctx: SecurityContext,
        req: CreateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        self.create_route_calls.fetch_add(1, Ordering::SeqCst);
        let path = req
            .match_rules()
            .http
            .as_ref()
            .map(|h| h.path.clone())
            .unwrap_or_default();
        for (prefix, fault) in self.route_faults.lock().unwrap().iter() {
            if path.starts_with(prefix) {
                return Err(fault.error());
            }
        }
        let route = Route {
            id: Uuid::new_v4(),
            tenant_id: ctx.subject_tenant_id(),
            upstream_id: req.upstream_id(),
            match_rules: req.match_rules().clone(),
            plugins: req.plugins().cloned(),
            rate_limit: req.rate_limit().cloned(),
            cors: req.cors().cloned(),
            tags: req.tags().to_vec(),
            priority: req.priority(),
            enabled: req.enabled(),
        };
        self.routes.lock().unwrap().push(route.clone());
        Ok(route)
    }

    async fn get_route(&self, _ctx: SecurityContext, id: Uuid) -> Result<Route, CanonicalError> {
        self.routes
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| {
                ProxyScope::not_found("route not found")
                    .with_resource(id.to_string())
                    .create()
            })
    }

    async fn list_routes(
        &self,
        _ctx: SecurityContext,
        upstream_id: Option<Uuid>,
        query: &ListQuery,
    ) -> Result<Vec<Route>, CanonicalError> {
        Ok(self
            .routes
            .lock()
            .unwrap()
            .iter()
            .filter(|r| upstream_id.is_none_or(|id| r.upstream_id == id))
            .skip(query.skip as usize)
            .take(query.top as usize)
            .cloned()
            .collect())
    }

    async fn update_route(
        &self,
        _ctx: SecurityContext,
        _id: Uuid,
        _req: UpdateRouteRequest,
    ) -> Result<Route, CanonicalError> {
        Err(unsupported("update_route"))
    }

    async fn delete_route(&self, _ctx: SecurityContext, _id: Uuid) -> Result<(), CanonicalError> {
        Err(unsupported("delete_route"))
    }

    async fn resolve_proxy_target(
        &self,
        _ctx: SecurityContext,
        _alias: &str,
        _method: &str,
        _path: &str,
    ) -> Result<(Upstream, Route), CanonicalError> {
        Err(unsupported("resolve_proxy_target"))
    }

    async fn proxy_request(
        &self,
        ctx: SecurityContext,
        req: http::Request<Body>,
    ) -> Result<http::Response<Body>, CanonicalError> {
        let (parts, body) = req.into_parts();
        let raw = body
            .into_bytes()
            .await
            .map_err(|e| CanonicalError::internal(format!("fake oagw body: {e}")).create())?;
        self.recorded.lock().unwrap().push(RecordedProviderRequest {
            method: parts.method.clone(),
            uri: parts.uri.clone(),
            headers: parts.headers.clone(),
            body: serde_json::from_slice(&raw).unwrap_or(Value::Null),
            raw,
            subject_id: ctx.subject_id(),
            tenant_id: ctx.subject_tenant_id(),
        });
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| (self.default_reply.lock().unwrap())());
        match reply {
            ProviderReply::Sse(pieces) => Ok(http::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(Body::Stream(live_body(pieces, self.shared.clone())))
                .expect("response")),
            ProviderReply::Json(body) => Ok(http::Response::builder()
                .status(200)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("response")),
            ProviderReply::Http {
                status,
                body,
                headers,
            } => {
                let mut builder = http::Response::builder()
                    .status(status)
                    .header("content-type", "application/json");
                for (name, value) in headers {
                    builder = builder.header(name, value);
                }
                let mut resp = builder
                    .body(Body::from(body.to_string()))
                    .expect("response");
                resp.extensions_mut().insert(ErrorSource::Upstream);
                Ok(resp)
            }
            ProviderReply::Gateway(err) => Err(err),
            ProviderReply::GatewayProblem { status, body } => {
                let mut resp = http::Response::builder()
                    .status(status)
                    .header("content-type", "application/problem+json")
                    .body(Body::from(body.to_string()))
                    .expect("response");
                resp.extensions_mut().insert(ErrorSource::Gateway);
                Ok(resp)
            }
        }
    }
}

// ── SSE reading ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct SseEvent {
    pub event: String,
    pub data: Value,
    pub raw: String,
}

fn parse_frame(frame: &str) -> Option<SseEvent> {
    let mut name = "message".to_owned();
    let mut data = Vec::new();
    for line in frame.lines() {
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "event" => value.clone_into(&mut name),
            "data" => data.push(value.to_owned()),
            _ => {}
        }
    }
    if data.is_empty() {
        return None;
    }
    let raw = data.join("\n");
    Some(SseEvent {
        event: name,
        data: serde_json::from_str(&raw).unwrap_or(Value::String(raw.clone())),
        raw,
    })
}

/// Incremental SSE reader over a live response body. Dropping it drops the
/// response body, i.e. disconnects the client.
pub struct LiveSse {
    stream: BodyDataStream,
    buf: String,
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub eof: bool,
}

impl LiveSse {
    /// Next event; `None` at end of stream. Panics when nothing arrives in 10 s.
    pub async fn next_event(&mut self) -> Option<SseEvent> {
        self.next_event_within(Duration::from_secs(10))
            .await
            .expect("timed out waiting for the next SSE event")
    }

    /// `Err(())` on timeout, `Ok(None)` at end of stream.
    pub async fn next_event_within(&mut self, within: Duration) -> Result<Option<SseEvent>, ()> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let normalized = self.buf.replace("\r\n", "\n");
            if let Some(pos) = normalized.find("\n\n") {
                let frame = normalized[..pos].to_owned();
                normalized[pos + 2..].clone_into(&mut self.buf);
                if let Some(event) = parse_frame(&frame) {
                    return Ok(Some(event));
                }
                continue;
            }
            if self.eof {
                return Ok(parse_frame(&normalized).inspect(|_| self.buf.clear()));
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, self.stream.next()).await {
                Err(_) => return Err(()),
                Ok(None | Some(Err(_))) => self.eof = true,
                Ok(Some(Ok(chunk))) => self.buf.push_str(&String::from_utf8_lossy(&chunk)),
            }
        }
    }

    /// The next event, which must be named `name`.
    pub async fn expect(&mut self, name: &str) -> SseEvent {
        let event = self
            .next_event()
            .await
            .unwrap_or_else(|| panic!("stream ended while waiting for `{name}`"));
        assert_eq!(event.event, name, "unexpected event: {event:?}");
        event
    }

    /// Next event that is not a `ping`.
    pub async fn next_non_ping(&mut self) -> Option<SseEvent> {
        loop {
            let event = self.next_event().await?;
            if event.event != "ping" {
                return Some(event);
            }
        }
    }

    /// Every remaining event until the connection closes (30 s budget).
    pub async fn drain(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self.next_event_within(remaining).await {
                Ok(Some(event)) => out.push(event),
                Ok(None) => return out,
                Err(()) => panic!("the SSE stream did not close within 30 s, got {out:?}"),
            }
        }
    }
}

/// Response head of a `messages:stream` call; the body is not read yet.
pub struct RawStream {
    pub status: StatusCode,
    pub headers: HeaderMap,
    body: AxumBody,
}

impl RawStream {
    pub fn from_response(resp: axum::response::Response) -> Self {
        let (parts, body) = resp.into_parts();
        Self {
            status: parts.status,
            headers: parts.headers,
            body,
        }
    }

    pub fn content_type(&self) -> String {
        self.headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    }

    pub fn is_sse(&self) -> bool {
        self.status == StatusCode::OK && self.content_type().starts_with("text/event-stream")
    }

    /// Switch to incremental reading.
    pub fn live(self) -> LiveSse {
        LiveSse {
            stream: self.body.into_data_stream(),
            buf: String::new(),
            status: self.status,
            headers: self.headers,
            eof: false,
        }
    }

    /// Read the whole body as a JSON / text response (rejections).
    pub async fn into_resp(self) -> Resp {
        let bytes = to_bytes(self.body, 8 * 1024 * 1024).await.expect("body");
        Resp {
            status: self.status,
            headers: self.headers,
            raw: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    /// Read the whole SSE body (the stream must close within 30 s).
    pub async fn collect(self) -> Sent {
        assert!(
            self.is_sse(),
            "expected a text/event-stream response, got {} {}",
            self.status,
            self.content_type()
        );
        let status = self.status;
        let headers = self.headers.clone();
        let mut live = self.live();
        let events = live.drain().await;
        Sent {
            status,
            headers,
            events,
        }
    }
}

/// A fully read SSE response.
#[derive(Clone, Debug)]
pub struct Sent {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub events: Vec<SseEvent>,
}

impl Sent {
    pub fn names(&self) -> Vec<String> {
        self.events.iter().map(|e| e.event.clone()).collect()
    }

    /// Event names without keep-alive pings.
    pub fn names_no_ping(&self) -> Vec<String> {
        self.events
            .iter()
            .filter(|e| e.event != "ping")
            .map(|e| e.event.clone())
            .collect()
    }

    pub fn of(&self, name: &str) -> Vec<&SseEvent> {
        self.events.iter().filter(|e| e.event == name).collect()
    }

    pub fn started(&self) -> &Value {
        &self.events.first().expect("no events").data
    }

    pub fn request_id(&self) -> Uuid {
        Uuid::parse_str(self.started()["request_id"].as_str().expect("request_id")).unwrap()
    }

    pub fn message_id(&self) -> Uuid {
        Uuid::parse_str(self.started()["message_id"].as_str().expect("message_id")).unwrap()
    }

    pub fn done(&self) -> &Value {
        &self
            .of("done")
            .first()
            .unwrap_or_else(|| panic!("no done event in {:?}", self.names()))
            .data
    }

    pub fn error(&self) -> &Value {
        &self
            .of("error")
            .first()
            .unwrap_or_else(|| panic!("no error event in {:?}", self.names()))
            .data
    }

    /// Concatenation of every delta's `content`.
    pub fn text(&self) -> String {
        self.of("delta")
            .iter()
            .map(|e| e.data["content"].as_str().unwrap_or_default())
            .collect()
    }

    pub fn terminal(&self) -> &SseEvent {
        self.events.last().expect("no events")
    }

    /// Whole body re-serialised (for "no provider identifier anywhere" checks).
    pub fn blob(&self) -> String {
        self.events
            .iter()
            .map(|e| format!("{}:{}", e.event, e.raw))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The generic shape every stream must have: `stream_started` first,
    /// exactly one terminal event, last; no ping after content started.
    pub fn assert_well_formed(&self) {
        let names = self.names();
        assert_eq!(
            names.first().map(String::as_str),
            Some("stream_started"),
            "{names:?}"
        );
        let terminals = names
            .iter()
            .filter(|n| *n == "done" || *n == "error")
            .count();
        assert_eq!(terminals, 1, "exactly one terminal event: {names:?}");
        assert!(
            matches!(names.last().map(String::as_str), Some("done" | "error")),
            "the terminal event is last: {names:?}"
        );
        assert!(
            names.iter().filter(|n| *n == "citations").count() <= 1,
            "at most one citations event: {names:?}"
        );
        if let Some(c) = names.iter().position(|n| n == "citations") {
            assert!(
                names[..c].iter().all(|n| n != "done") && c + 2 == names.len(),
                "citations come right before the terminal event: {names:?}"
            );
        }
        let first_content = names.iter().position(|n| n == "delta" || n == "tool");
        if let Some(first) = first_content {
            assert!(
                names[first..].iter().all(|n| n != "ping"),
                "no ping after the first delta/tool event: {names:?}"
            );
        }
    }
}

// ── the harness ──────────────────────────────────────────────────────────────

pub struct StreamOptions {
    pub models: Vec<Value>,
    pub pdp: Arc<FakePdp>,
    pub config: Value,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            models: default_stream_models(),
            pdp: FakePdp::allow(),
            config: json!({}),
        }
    }
}

pub struct StreamHarness {
    /// The S2 harness: database, message list / chat routes, seeds.
    pub chat: ChatHarness,
    pub router: Router,
    pub oagw: Arc<FakeOagw>,
    pub policy: Arc<StreamPolicy>,
    pub config: Arc<MiniChatConfig>,
    pub pdp: Arc<FakePdp>,
}

impl StreamHarness {
    pub async fn new() -> Self {
        Self::with(StreamOptions::default()).await
    }

    pub async fn with(opts: StreamOptions) -> Self {
        let chat = crate::chat_support::chat_harness_with(ChatHarnessOptions {
            models: model_entries(&opts.models),
            pdp: opts.pdp.clone(),
            ..Default::default()
        })
        .await;
        let oagw = FakeOagw::new();
        let policy = StreamPolicy::new(&opts.models);
        let config = stream_config(&opts.config);
        let gateway = Arc::new(ModelPolicyGateway::from_client(policy.clone()));
        let oagw_client: Arc<dyn ServiceGatewayClientV1> = oagw.clone();
        let llm = Arc::new(LlmProviders::new(oagw_client, &config));
        let enforcer = opts.pdp.enforcer();
        let stream = Arc::new(StreamService::new(
            chat.db.clone(),
            gateway,
            llm,
            config.clone(),
            enforcer.clone(),
        ));
        let turns = Arc::new(TurnsService::new(chat.db.clone(), enforcer));
        let router = register_stream_routes(
            chat.router.clone(),
            chat.openapi.as_ref(),
            stream,
            turns,
            &chat.prefix,
        );
        Self {
            chat,
            router,
            oagw,
            policy,
            config,
            pdp: opts.pdp,
        }
    }

    // ── URIs ─────────────────────────────────────────────────────────────────

    pub fn stream_uri(&self, chat: Uuid) -> String {
        format!("{}/v1/chats/{chat}/messages:stream", self.chat.prefix)
    }

    pub fn turn_uri(&self, chat: Uuid, request_id: Uuid) -> String {
        format!("{}/v1/chats/{chat}/turns/{request_id}", self.chat.prefix)
    }

    // ── seeds ────────────────────────────────────────────────────────────────

    /// A chat of `USER_A` on `gpt-premium`.
    pub async fn seed_chat(&self) -> Uuid {
        self.chat.seed_chat(ChatSeed::of_a().at(0)).await
    }

    pub async fn seed_chat_on(&self, model: &str) -> Uuid {
        self.chat
            .seed_chat(ChatSeed::of_a().with_model(model).at(0))
            .await
    }

    /// A ready document attachment of `chat` (`USER_A`, `TENANT_A`).
    pub async fn seed_document(&self, chat: Uuid, filename: &str) -> Uuid {
        self.chat
            .seed_attachment(AttachmentSeed::document(chat, filename))
            .await
    }

    /// The `chat_vector_stores` row that makes `file_search` available.
    pub async fn seed_vector_store(&self, chat: Uuid, vector_store_id: &str) {
        let conn = self.chat.db.conn().expect("db connection");
        secure_insert::<chat_vector_stores::Entity>(
            chat_vector_stores::ActiveModel {
                id: Set(Uuid::new_v4()),
                tenant_id: Set(TENANT_A),
                chat_id: Set(chat),
                vector_store_id: Set(Some(vector_store_id.to_owned())),
                provider: Set(PROVIDER_ID.to_owned()),
                file_count: Set(0),
                created_at: Set(to_db(ts(0))),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .expect("seed vector store");
    }

    /// Seed `pairs` completed turns (user + assistant, alternating, ids and
    /// timestamps ascending) and return the message ids in order.
    pub async fn seed_history(&self, chat: Uuid, pairs: usize) -> Vec<Uuid> {
        let mut ids = Vec::new();
        for i in 0..pairs {
            let rid = Uuid::new_v4();
            let base = i64::try_from(i).unwrap() * 2 + 1;
            ids.push(
                self.chat
                    .seed_message(
                        MessageSeed::user(chat)
                            .request(Some(rid))
                            .content(&format!("question {}", i + 1))
                            .at(base),
                    )
                    .await,
            );
            ids.push(
                self.chat
                    .seed_message(
                        MessageSeed::assistant(chat)
                            .request(Some(rid))
                            .content(&format!("answer {}", i + 1))
                            .model("gpt-premium")
                            .tokens(10, 4)
                            .at(base + 1),
                    )
                    .await,
            );
        }
        ids
    }

    /// Insert a `chat_turns` row directly.
    pub async fn seed_turn(&self, seed: TurnSeed) -> Uuid {
        let conn = self.chat.db.conn().expect("db connection");
        let terminal = seed.state != "running";
        secure_insert::<chat_turns::Entity>(
            chat_turns::ActiveModel {
                id: Set(seed.id),
                tenant_id: Set(TENANT_A),
                chat_id: Set(seed.chat_id),
                request_id: Set(seed.request_id),
                requester_type: Set("user".to_owned()),
                requester_user_id: Set(Some(USER_A)),
                state: Set(seed.state.clone()),
                provider_name: Set(None),
                provider_response_id: Set(None),
                assistant_message_id: Set(seed.assistant_message_id),
                error_code: Set(seed.error_code.clone()),
                reserve_tokens: Set(None),
                max_output_tokens_applied: Set(None),
                reserved_credits_micro: Set(None),
                policy_version_applied: Set(None),
                effective_model: Set(Some(seed.effective_model.clone())),
                minimal_generation_floor_applied: Set(None),
                error_detail: Set(None),
                deleted_at: Set(to_db_opt(seed.deleted_at)),
                replaced_by_request_id: Set(None),
                started_at: Set(to_db(seed.started_at)),
                last_progress_at: Set(Some(to_db(seed.started_at))),
                web_search_enabled: Set(false),
                web_search_completed_count: Set(0),
                code_interpreter_completed_count: Set(0),
                file_search_completed_count: Set(0),
                completed_at: Set(terminal.then(|| to_db(seed.updated_at))),
                updated_at: Set(Some(to_db(seed.updated_at))),
            },
            &AccessScope::allow_all(),
            &conn,
        )
        .await
        .expect("seed turn");
        seed.id
    }

    // ── sending ──────────────────────────────────────────────────────────────

    /// `POST messages:stream` as `ctx` with a JSON body; the body is not read.
    pub async fn open(&self, chat: Uuid, ctx: &SecurityContext, body: &Value) -> RawStream {
        self.open_raw(
            &self.stream_uri(chat),
            Some(ctx),
            Some("application/json"),
            &body.to_string(),
        )
        .await
    }

    pub async fn open_raw(
        &self,
        uri: &str,
        ctx: Option<&SecurityContext>,
        content_type: Option<&str>,
        body: &str,
    ) -> RawStream {
        let mut builder = Request::builder().method("POST").uri(uri);
        if let Some(content_type) = content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        let mut req = builder
            .body(AxumBody::from(body.to_owned()))
            .expect("request");
        if let Some(ctx) = ctx {
            req.extensions_mut().insert(ctx.clone());
        }
        let resp = self
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("router is infallible");
        RawStream::from_response(resp)
    }

    /// Send as `USER_A`, read the whole SSE answer.
    pub async fn send(&self, chat: Uuid, body: &Value) -> Sent {
        self.open(chat, &ctx_a(), body).await.collect().await
    }

    /// Send `text` as `USER_A` with a fresh server-generated request id.
    pub async fn say(&self, chat: Uuid, text: &str) -> Sent {
        self.send(chat, &json!({"content": text})).await
    }

    /// Send `text` with a client-given request id.
    pub async fn say_with(&self, chat: Uuid, text: &str, request_id: Uuid) -> Sent {
        self.send(chat, &json!({"content": text, "request_id": request_id}))
            .await
    }

    /// Send as `USER_A` and expect a JSON rejection (no SSE stream).
    pub async fn reject(&self, chat: Uuid, body: Value) -> Resp {
        let raw = self.open(chat, &ctx_a(), &body).await;
        assert!(
            !raw.is_sse(),
            "expected a JSON rejection, got an SSE stream ({})",
            raw.content_type()
        );
        raw.into_resp().await
    }

    pub async fn turn_status(&self, chat: Uuid, request_id: Uuid, ctx: &SecurityContext) -> Resp {
        crate::common::get(&self.router, &self.turn_uri(chat, request_id), ctx).await
    }

    // ── database readers (unscoped: they assert what the API did) ────────────

    pub async fn all_turns(&self) -> Vec<chat_turns::Model> {
        let conn = self.chat.db.conn().expect("db connection");
        chat_turns::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read turns")
    }

    pub async fn turn_of(&self, request_id: Uuid) -> Option<chat_turns::Model> {
        self.all_turns()
            .await
            .into_iter()
            .find(|t| t.request_id == request_id)
    }

    /// Messages of `chat` in `(created_at, id)` order, soft-deleted ones included.
    pub async fn messages_in(&self, chat: Uuid) -> Vec<messages::Model> {
        let mut rows: Vec<_> = self
            .chat
            .all_messages()
            .await
            .into_iter()
            .filter(|m| m.chat_id == chat)
            .collect();
        rows.sort_by_key(|m| (m.created_at, m.id));
        rows
    }

    pub async fn message_attachment_links(&self) -> Vec<message_attachments::Model> {
        let conn = self.chat.db.conn().expect("db connection");
        message_attachments::Entity::find()
            .secure()
            .scope_with(&AccessScope::allow_all())
            .all(&conn)
            .await
            .expect("read links")
    }

    /// Wait (10 s) until the turn of `request_id` reached `state`.
    pub async fn wait_turn_state(&self, request_id: Uuid, state: &str) -> chat_turns::Model {
        let mut last = None;
        for _ in 0..1000 {
            if let Some(turn) = self.turn_of(request_id).await {
                if turn.state == state {
                    return turn;
                }
                last = Some(turn.state.clone());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("turn {request_id} never reached `{state}` (last state {last:?})");
    }

    /// Counts of every table a rejected / replayed request must not touch.
    pub async fn row_counts(&self) -> (usize, usize, usize, usize) {
        (
            self.chat.all_messages().await.len(),
            self.all_turns().await.len(),
            self.message_attachment_links().await.len(),
            self.chat.all_chats().await.len(),
        )
    }

    /// Message list of `chat` through the public API (as `USER_A`).
    pub async fn listed_messages(&self, chat: Uuid) -> Vec<Value> {
        let resp = self.chat.list_messages(chat, "", &ctx_a()).await;
        assert_eq!(resp.status.as_u16(), 200, "{}", resp.raw);
        page_items(&resp.json())
    }
}

/// A `chat_turns` row to insert directly.
pub struct TurnSeed {
    pub id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub state: String,
    pub error_code: Option<String>,
    pub assistant_message_id: Option<Uuid>,
    pub effective_model: String,
    pub deleted_at: Option<OffsetDateTime>,
    pub started_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
}

impl TurnSeed {
    pub fn new(chat_id: Uuid, request_id: Uuid, state: &str) -> Self {
        Self {
            id: Uuid::new_v4(),
            chat_id,
            request_id,
            state: state.to_owned(),
            error_code: None,
            assistant_message_id: None,
            effective_model: "gpt-premium".to_owned(),
            deleted_at: None,
            started_at: ts(10),
            updated_at: ts_micros_later(),
        }
    }

    pub fn error(mut self, code: &str) -> Self {
        self.error_code = Some(code.to_owned());
        self
    }

    pub fn assistant(mut self, id: Uuid) -> Self {
        self.assistant_message_id = Some(id);
        self
    }

    pub fn effective_model(mut self, model: &str) -> Self {
        model.clone_into(&mut self.effective_model);
        self
    }

    pub fn deleted(mut self) -> Self {
        self.deleted_at = Some(ts(20));
        self
    }
}

fn ts_micros_later() -> OffsetDateTime {
    crate::chat_support::ts_micros(12, 345_678)
}

// ── assertions / small helpers ───────────────────────────────────────────────

/// `true` when `s` holds a raw provider identifier: `resp_`, `chatcmpl-`,
/// `file-`, `vs_`, `sk-` followed by a long alphanumeric tail, or `Bearer `.
pub fn contains_provider_identifier(s: &str) -> bool {
    let re = regex::Regex::new(
        r"(resp_[A-Za-z0-9]{6,}|chatcmpl-[A-Za-z0-9]{6,}|msg_[A-Za-z0-9]{6,}|file[-_][A-Za-z0-9]{12,}|vs_[A-Za-z0-9]{12,}|sk-[A-Za-z0-9]{10,}|Bearer\s)",
    )
    .expect("regex");
    re.is_match(s)
}

/// 32-hex simple form.
pub fn hex(id: Uuid) -> String {
    id.simple().to_string()
}

/// Strip hyphens and lower-case (ids may be spelled hyphenated or simple).
pub fn norm_id(s: &str) -> String {
    s.replace('-', "").to_ascii_lowercase()
}

/// Assert a 409 `aborted` problem with `context.reason`.
pub fn assert_conflict(resp: &Resp, reason: &str) -> Value {
    let problem = assert_problem(resp, 409, "aborted");
    assert_eq!(problem["context"]["reason"], json!(reason), "{}", resp.raw);
    problem
}

/// Assert a 400 `invalid_argument` problem carrying `field_violations[*].reason`.
pub fn assert_400_reason(resp: &Resp, category: &str, reason: &str) -> Value {
    let problem = assert_problem(resp, 400, category);
    let list = problem["context"]["field_violations"]
        .as_array()
        .unwrap_or_else(|| panic!("no field_violations: {}", resp.raw));
    assert!(
        list.iter().any(|v| v["reason"] == json!(reason)),
        "expected field violation reason {reason}, got {list:?}"
    );
    problem
}

/// Poll `check` every 10 ms for up to 10 s.
pub async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..1000 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

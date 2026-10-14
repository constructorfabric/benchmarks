//! The real provider task of the message-streaming slice S2-provider-path of
//! `cpt-cf-mini-chat-feature-message-streaming`: it builds the request through the adapter of the
//! resolved provider, calls OAGW under the gear's S2S security context, parses the provider SSE
//! incrementally, translates and accounts the events, hands them to the bounded relay channel,
//! finalizes the turn (first terminal wins) and emits the terminal event only after the
//! finalization result is known.
//!
//! Algorithms `cpt-cf-mini-chat-algo-message-streaming-provider-call`,
//! `-event-translation`, `-tool-accounting`, `-citations`, `-error-sanitization`,
//! `-terminal-emission` and `-streaming-metrics`; flows `send-message` (provider task part) and
//! `disconnect`.

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use mini_chat_sdk::UsageTokens;
use oagw_sdk::api::{ErrorSource, ServiceGatewayClientV1};
use oagw_sdk::{Body, ServerEvent, ServerEventsResponse, ServerEventsStream};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use toolkit::api::canonical_prelude::CanonicalError;
use toolkit_security::SecurityContext;

use crate::config::{MiniChatConfig, ProviderKind};
use crate::domain::attachments::contributions::{SEARCH_KNOWLEDGE, ToolResultFormat};
use crate::domain::attachments::knowledge::{
    KnowledgeLoop, KnowledgeLoopDeps, LoopStep, LoopStop, ToolCall, tool_result_item,
};
use crate::domain::error::{DomainError, StreamErrorCode};
use crate::domain::quota::QuotaService;
use crate::domain::streaming::citations::{FileCitationMap, map_citations};
use crate::domain::streaming::provider_errors::{
    MappedFailure, ProviderFailure, generic_message, map_provider_failure,
};
use crate::domain::streaming::provider_events::{
    ProviderCompletion, ProviderEvent, WireEvent, parser_for,
};
use crate::domain::streaming::provider_request::build_provider_request;
use crate::domain::streaming::terminal::{TerminalIntent, terminal_emission};
use crate::domain::streaming::tool_accounting::{StartDecision, ToolAccounting, ToolLimits};
use crate::domain::streaming::{
    CitationsPayload, Delta, DeltaKind, ProviderCallPlan, ProviderTaskPort, RelayEvent,
    StreamFinalization, StreamOutcome, StreamTerminal, ToolEvent, ToolKind, ToolPhase,
    TurnFinalizer, TurnProgress,
};
use crate::domain::turns::TurnState;
use crate::infra::metrics::MiniChatMetrics;

/// The most of a non-successful provider response body that is read (the rest is dropped).
const ERROR_BODY_LIMIT: usize = 64 * 1024;

/// The answer of a parallel `search_knowledge` call beyond the first of one iteration.
const ONE_SEARCH_AT_A_TIME: &str = "only one search per step is answered; ask again";

/// The gear's S2S security context, bound at gear start (after the services were built at
/// init). Every provider request goes out under this context; the user and tenant identity
/// travel only in the request's `user` and `metadata` fields.
#[derive(Debug, Default)]
pub struct S2sContext {
    cell: OnceLock<SecurityContext>,
}

impl S2sContext {
    /// A handle that is not bound yet: starting a provider task fails until it is.
    #[must_use]
    pub fn unbound() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A handle bound to `ctx`.
    #[must_use]
    pub fn bound(ctx: SecurityContext) -> Arc<Self> {
        let handle = Self::default();
        // a fresh cell: the first set always succeeds
        drop(handle.cell.set(ctx));
        Arc::new(handle)
    }

    /// Bind the context (the first bind wins).
    pub fn bind(&self, ctx: SecurityContext) {
        drop(self.cell.set(ctx));
    }

    /// The bound context, `None` before gear start.
    #[must_use]
    pub fn get(&self) -> Option<&SecurityContext> {
        self.cell.get()
    }
}

/// The values the provider task reads (`streaming.*`, `quota.*` limits and fixed timings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderTaskSettings {
    /// `streaming.sse_channel_capacity`: the capacity of the bounded relay channel.
    pub channel_capacity: usize,
    /// `quota.web_search_max_calls_per_message`.
    pub web_search_max_calls_per_message: u32,
    /// `quota.code_interpreter_max_calls_per_message`.
    pub code_interpreter_max_calls_per_message: u32,
    /// The provider stream idle timeout (mapped to `provider_timeout`), below the orphan
    /// watchdog timeout.
    pub idle_timeout: Duration,
    /// The minimum interval between two durable `last_progress_at` writes (30 s).
    pub progress_interval: Duration,
}

impl ProviderTaskSettings {
    /// The values of the validated gear configuration; the idle timeout is 60 s (below the
    /// 90 s minimum of `orphan_watchdog.timeout_secs`) and the progress interval 30 s.
    #[must_use]
    pub fn from_config(config: &MiniChatConfig) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-streaming
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-tool-limits
        Self {
            channel_capacity: usize::from(config.streaming.sse_channel_capacity),
            web_search_max_calls_per_message: config.quota.web_search_max_calls_per_message,
            code_interpreter_max_calls_per_message: config
                .quota
                .code_interpreter_max_calls_per_message,
            idle_timeout: Duration::from_secs(60),
            progress_interval: Duration::from_secs(30),
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-tool-limits
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-configuration:p1:inst-cfg-streaming
    }
}

/// The collaborators of the real provider task.
#[derive(Clone)]
pub struct ProviderTaskDeps {
    /// The in-process OAGW client (`proxy_request`).
    pub gateway: Arc<dyn ServiceGatewayClientV1>,
    pub s2s: Arc<S2sContext>,
    pub finalizer: Arc<dyn TurnFinalizer>,
    pub progress: Arc<dyn TurnProgress>,
    /// `quota_warnings` after the settlement and the application clock.
    pub quota: Arc<QuotaService>,
    pub metrics: Arc<MiniChatMetrics>,
    pub settings: ProviderTaskSettings,
    /// The collaborators of the knowledge search loop of FEATURE 0005 (`search_knowledge` tool
    /// calls); `None` keeps every tool call an `unexpected_tool_use` outcome.
    pub knowledge: Option<KnowledgeLoopDeps>,
}

/// The real provider task: builds the request through the adapter of the resolved provider,
/// calls OAGW under the S2S context, translates the provider events, accounts the tool events,
/// hands the events to the bounded relay channel, finalizes the turn and emits the terminal
/// event only after the finalization result is known.
pub struct StreamingProviderTask {
    deps: ProviderTaskDeps,
}

impl StreamingProviderTask {
    /// Create the task over its collaborators.
    #[must_use]
    pub fn new(deps: ProviderTaskDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl ProviderTaskPort for StreamingProviderTask {
    async fn start(
        &self,
        plan: ProviderCallPlan,
        cancel: CancellationToken,
    ) -> Result<mpsc::Receiver<RelayEvent>, DomainError> {
        // the context of the gear is bound at start: before it nothing is sent
        let s2s = self.deps.s2s.get().cloned().ok_or_else(|| {
            DomainError::internal("the S2S security context is not bound: the gear has not started")
        })?;
        // the request is built before the task spawns, so a failure fails the send
        let request = build_provider_request(&plan)?;
        let body = serde_json::to_vec(&request.body).map_err(|e| {
            DomainError::internal(format!("cannot serialize the provider request: {e}"))
        })?;
        let (tx, rx) = mpsc::channel(self.deps.settings.channel_capacity.max(1));
        let run = Run {
            deps: self.deps.clone(),
            plan,
            cancel,
            tx,
            s2s,
            uri: request.uri,
            body,
            began: Instant::now(),
        };
        tokio::spawn(run.drive());
        Ok(rx)
    }
}

/// How the provider stream (or the call before it) ended.
enum End {
    /// `response.completed`, `response.incomplete` or the adapter equivalent.
    Completed(Box<ProviderCompletion>),
    /// A provider failure, mapped and sanitized.
    Failed(MappedFailure),
    /// A per-message tool limit was breached.
    Breach(StreamErrorCode),
    /// The knowledge loop ended the turn after a completed iteration (the iteration cap, an
    /// unexpected tool use): the usage that iteration reported settles the turn.
    Stopped {
        code: StreamErrorCode,
        usage: Option<UsageTokens>,
    },
    /// The model asked for a function tool while knowledge search is not active.
    UnexpectedToolUse,
    /// An iteration ended with a function call and the knowledge loop is active.
    ToolUse(Box<ProviderCompletion>),
    /// The client went away: through the cancellation token or a failed channel send.
    Cancelled { via_token: bool, observed: Instant },
}

/// Why a send on the relay channel did not happen.
enum Stop {
    Token,
    SendFailed,
}

/// The mutable state of one stream.
struct State {
    /// The accumulated assistant text (never the reasoning).
    text: String,
    accounting: ToolAccounting,
    /// A `delta` was handed to the relay.
    delta_sent: bool,
    /// The last durable `last_progress_at` write.
    last_progress: OffsetDateTime,
    /// The `mini_chat_ttft_overhead_ms` sample was taken.
    overhead_recorded: bool,
    response_id: Option<String>,
    /// The function tool uses of the current iteration (answered by the knowledge loop).
    tool_uses: u32,
}

/// Counts an open stream in `mini_chat_active_streams` until it is dropped.
struct ActiveStream(Arc<MiniChatMetrics>);

impl ActiveStream {
    fn open(metrics: &Arc<MiniChatMetrics>) -> Self {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-active-streams
        // incremented when the stream opens, decremented when it closes (the drop below)
        metrics.active_streams(1);
        Self(Arc::clone(metrics))
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-active-streams
    }
}

impl Drop for ActiveStream {
    fn drop(&mut self) {
        self.0.active_streams(-1);
    }
}

/// One running provider task.
struct Run {
    deps: ProviderTaskDeps,
    plan: ProviderCallPlan,
    cancel: CancellationToken,
    tx: mpsc::Sender<RelayEvent>,
    s2s: SecurityContext,
    uri: String,
    body: Vec<u8>,
    /// The start of the task: the request receipt of `mini_chat_stream_total_latency_ms`.
    began: Instant,
}

impl Run {
    fn provider(&self) -> &str {
        &self.plan.provider.provider_id
    }

    fn model(&self) -> &str {
        &self.plan.decision.effective_model
    }

    /// The whole life of the stream: the call, the relay, the finalization and the terminal
    /// event. The channel closes when this returns.
    async fn drive(self) {
        let active = ActiveStream::open(&self.deps.metrics);
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-started-completed
        self.deps
            .metrics
            .stream_started(self.provider(), self.model());
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-started-completed
        let mut state = State {
            text: String::new(),
            accounting: ToolAccounting::new(ToolLimits {
                web_search: self.deps.settings.web_search_max_calls_per_message,
                code_interpreter: self.deps.settings.code_interpreter_max_calls_per_message,
            }),
            delta_sent: false,
            last_progress: self.plan.started_at,
            overhead_recorded: false,
            response_id: None,
            tool_uses: 0,
        };
        let end = self.run_iterations(&mut state).await;
        self.conclude(state, end, active).await;
    }

    /// Whether the knowledge search loop is active for this turn. The vLLM Responses adapter
    /// sends no tools at all, so the loop never runs on it.
    fn knowledge_active(&self) -> bool {
        self.deps.knowledge.is_some()
            && self.plan.provider.kind != ProviderKind::VllmResponses
            && self
                .plan
                .tools
                .tools
                .iter()
                .any(|tool| tool.kind == ToolKind::SearchKnowledge)
    }

    /// The provider iterations of the turn: one request, or one more after each answered
    /// `search_knowledge` call (the knowledge search loop). The loop is bounded by the hard cap
    /// of `KnowledgeLoop` (`max_calls_per_message` plus 2 iterations); without an active loop the
    /// first outcome is final.
    async fn run_iterations(&self, state: &mut State) -> End {
        let mut knowledge = match (&self.deps.knowledge, self.knowledge_active()) {
            (Some(deps), true) => Some(KnowledgeLoop::new(
                deps.clone(),
                self.plan.tenant_id,
                self.plan.turn_id,
            )),
            _ => None,
        };
        let mut body: serde_json::Value =
            serde_json::from_slice(&self.body).unwrap_or(serde_json::Value::Null);
        let mut payload = self.body.clone();
        let mut counted = 0;
        // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-loop
        loop {
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-next
            // the first request, or the next one with the same tools after an answered call
            let end = self.call_and_stream(state, &payload).await;
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-next
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-return
            // any outcome but a tool use is the final provider outcome for the finalization
            // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-return
            let End::ToolUse(completion) = end else {
                return end;
            };
            // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-return
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-return
            // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-knowledge
            // the completed iteration reported its usage: a stop of the loop settles on it
            let usage = completion.usage;
            let unexpected = End::Stopped {
                code: StreamErrorCode::UnexpectedToolUse,
                usage,
            };
            let Some(knowledge) = knowledge.as_mut() else {
                return unexpected;
            };
            let Some((first, extra)) = completion.tool_calls.split_first() else {
                return unexpected;
            };
            // a call of another tool name ends the turn, also next to a `search_knowledge` call
            if extra.iter().any(|call| call.name != SEARCH_KNOWLEDGE) {
                return unexpected;
            }
            let call = ToolCall {
                name: first.name.clone(),
                call_id: first.call_id.clone(),
                arguments: first.arguments.clone(),
            };
            // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-knowledge
            // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-knowledge-loop
            let step = self.answer_knowledge(knowledge, &call, state).await;
            // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-knowledge-loop
            // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-settle
            // the in-memory file search count is reported in the usage and audit events; only the
            // final iteration's usage is settled
            while counted < knowledge.file_search_calls() {
                state.accounting.on_done(ToolKind::SearchKnowledge.as_str());
                counted += 1;
            }
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-settle
            match step {
                Ok(LoopStep::Continue { output }) => {
                    // @cpt-begin:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-append
                    let format = self.plan.tools.knowledge_format;
                    let kind = self.plan.provider.kind;
                    append_tool_exchange(&mut body, kind, format, &call, &output);
                    // parallel calls beyond the first are answered without a search, so that the
                    // next request stays well-formed
                    for call in extra {
                        let call = ToolCall {
                            name: call.name.clone(),
                            call_id: call.call_id.clone(),
                            arguments: call.arguments.clone(),
                        };
                        append_tool_exchange(&mut body, kind, format, &call, ONE_SEARCH_AT_A_TIME);
                    }
                    payload = serde_json::to_vec(&body).unwrap_or_default();
                    state.tool_uses = 0;
                    // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-append
                }
                Ok(LoopStep::Stop(LoopStop::Failed(code))) => {
                    // the usage of the last completed iteration goes to the finalization
                    return End::Stopped { code, usage };
                }
                Ok(LoopStep::Stop(LoopStop::Cancelled)) => {
                    return End::Cancelled {
                        via_token: true,
                        observed: Instant::now(),
                    };
                }
                Err(end) => return end,
            }
            // @cpt-end:cpt-cf-mini-chat-algo-attachments-and-file-search-knowledge-loop:p1:inst-kl-loop
        }
    }

    /// The loop's answer to one tool call. The retrieval is raced against the turn cancellation
    /// (inside the loop) and the relay channel closing; while it runs the durable progress is
    /// refreshed (throttled), so the orphan watchdog does not finalize a live turn. Its time
    /// bound is the loop's `search_timeout`: a slower search is answered as a failed retrieval.
    async fn answer_knowledge(
        &self,
        knowledge: &mut KnowledgeLoop,
        call: &ToolCall,
        state: &mut State,
    ) -> Result<LoopStep, End> {
        let answer = knowledge.answer(call, &self.cancel);
        tokio::pin!(answer);
        loop {
            tokio::select! {
                biased;
                step = &mut answer => return Ok(step),
                () = self.tx.closed() => {
                    return Err(End::Cancelled { via_token: false, observed: Instant::now() });
                }
                () = tokio::time::sleep(self.deps.settings.progress_interval) => {
                    self.progress(state).await;
                }
            }
        }
    }

    /// The provider call and the read loop; the response stream is dropped (hard cancel of the
    /// outbound request) on every exit.
    async fn call_and_stream(&self, state: &mut State, payload: &[u8]) -> End {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-precheck-cancelled
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-return-precheck
        // a token cancelled before the request is sent: nothing goes to OAGW
        if self.cancel.is_cancelled() {
            return End::Cancelled {
                via_token: true,
                observed: Instant::now(),
            };
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-return-precheck
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-precheck-cancelled
        let images = self.plan.context.plan.user.images.len();
        if images > 0 {
            self.deps.metrics.image_inputs(images);
        }
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-proxy
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-oagw-only
        // `POST` of the JSON body to `/{upstream_alias}{api_path}` through the in-process
        // gateway client under the S2S context; OAGW injects the credentials, never retries
        // and never decides on quota
        let request = match http::Request::builder()
            .method(http::Method::POST)
            .uri(&self.uri)
            .header(http::header::CONTENT_TYPE, "application/json")
            .header(http::header::ACCEPT, "text/event-stream")
            .body(Body::from(payload.to_vec()))
        {
            Ok(request) => request,
            Err(error) => {
                return End::Failed(map_provider_failure(&ProviderFailure::Transport(format!(
                    "cannot build the provider request: {error}"
                ))));
            }
        };
        let sent_at = Instant::now();
        let call = self.deps.gateway.proxy_request(self.s2s.clone(), request);
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-oagw-only
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-proxy
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-cancelled
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-hard-cancel
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-dropped-before-stream
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-dropped-cancel-provider
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-hard-cancel
        // a cancellation (or a relay that went away) while the request is in flight drops the
        // call, which aborts the outbound request
        let response = tokio::select! {
            biased;
            () = self.cancel.cancelled() => {
                return End::Cancelled { via_token: true, observed: Instant::now() };
            }
            () = self.tx.closed() => {
                return End::Cancelled { via_token: false, observed: Instant::now() };
            }
            response = call => response,
        };
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-hard-cancel
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-dropped-cancel-provider
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-dropped-before-stream
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-hard-cancel
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-cancelled
        let response = match response {
            Ok(response) => response,
            Err(error) => return End::Failed(map_provider_failure(&gateway_failure(&error))),
        };
        self.read_response(response, state, sent_at).await
    }

    /// Classify the response: a non-successful status is a provider failure, an SSE stream is
    /// read event by event.
    async fn read_response(
        &self,
        response: http::Response<Body>,
        state: &mut State,
        sent_at: Instant,
    ) -> End {
        let status = response.status();
        let from_gateway = matches!(
            response.extensions().get::<ErrorSource>(),
            Some(ErrorSource::Gateway)
        );
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-http-error
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-report-error
        // the status, the `Retry-After` header and the (bounded) body go to the error mapping
        if !status.is_success() {
            let retry_after = response
                .headers()
                .get(http::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned);
            let body = self.read_error_body(response.into_body()).await;
            return End::Failed(map_provider_failure(&ProviderFailure::Http {
                status: status.as_u16(),
                retry_after,
                body,
                from_gateway,
            }));
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-report-error
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-http-error
        match ServerEventsStream::from_response::<ServerEvent>(response) {
            ServerEventsResponse::Events(events) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-return
                // the event stream goes to the read loop
                self.read_events(events, state, sent_at).await
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-return
            }
            ServerEventsResponse::Response(_) => {
                End::Failed(map_provider_failure(&ProviderFailure::Transport(
                    "the provider answered without an event stream".to_owned(),
                )))
            }
        }
    }

    /// The error body, read up to [`ERROR_BODY_LIMIT`] bytes and the idle timeout.
    async fn read_error_body(&self, body: Body) -> String {
        let read = async {
            match body {
                Body::Empty => Vec::new(),
                Body::Bytes(bytes) => bytes.iter().copied().take(ERROR_BODY_LIMIT).collect(),
                Body::Stream(mut stream) => {
                    let mut collected = Vec::new();
                    while collected.len() < ERROR_BODY_LIMIT {
                        match stream.next().await {
                            Some(Ok(chunk)) => {
                                let room = ERROR_BODY_LIMIT - collected.len();
                                collected.extend(chunk.iter().copied().take(room));
                            }
                            Some(Err(_)) | None => break,
                        }
                    }
                    collected
                }
            }
        };
        let bytes = tokio::time::timeout(self.deps.settings.idle_timeout, read)
            .await
            .unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// The read loop: each SSE event reaches the adapter parser as soon as it is complete and
    /// each translated event the relay channel before the next one is read.
    async fn read_events(
        &self,
        mut events: ServerEventsStream<ServerEvent>,
        state: &mut State,
        sent_at: Instant,
    ) -> End {
        let mut parser = parser_for(self.plan.provider.kind);
        let mut first_byte = true;
        loop {
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-incremental
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-premature-end
            let next = tokio::select! {
                biased;
                () = self.cancel.cancelled() => {
                    return End::Cancelled { via_token: true, observed: Instant::now() };
                }
                () = self.tx.closed() => {
                    return End::Cancelled { via_token: false, observed: Instant::now() };
                }
                next = tokio::time::timeout(self.deps.settings.idle_timeout, events.next()) => next,
            };
            let event = match next {
                // no byte within the provider stream idle timeout
                Err(_elapsed) => {
                    return End::Failed(map_provider_failure(&ProviderFailure::IdleTimeout));
                }
                // the stream ended without a terminal event (the token did not fire)
                Ok(None) => {
                    return End::Failed(map_provider_failure(&ProviderFailure::StreamBroken));
                }
                Ok(Some(Err(error))) => {
                    tracing::warn!(%error, "the provider stream broke");
                    return End::Failed(map_provider_failure(&ProviderFailure::StreamBroken));
                }
                Ok(Some(Ok(event))) => event,
            };
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-premature-end
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-incremental
            if first_byte {
                first_byte = false;
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-ttft-provider
                self.deps
                    .metrics
                    .ttft_provider(self.provider(), self.model(), sent_at.elapsed());
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-provider-call:p1:inst-call-ttft-provider
            }
            let received_at = Instant::now();
            // any provider event, an ignored one too, proves the stream is alive: the durable
            // progress is refreshed (throttled) before the event is translated or relayed
            self.progress(state).await;
            let wire = WireEvent {
                event: event.event,
                data: event.data,
            };
            // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-relay
            // each translated event goes to the client as it arrives, never collected first
            for translated in parser.feed(&wire) {
                if let Some(end) = self.handle(translated, state, received_at).await {
                    // the provider stream is dropped here: the outbound request is closed
                    drop(events);
                    return end;
                }
            }
            // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-relay
        }
    }

    /// One translated event: relayed and accounted, or the end of the stream.
    async fn handle(
        &self,
        event: ProviderEvent,
        state: &mut State,
        received_at: Instant,
    ) -> Option<End> {
        match event {
            ProviderEvent::Text(text) => {
                state.text.push_str(&text);
                let relay = RelayEvent::Delta(Delta {
                    kind: DeltaKind::Text,
                    content: text,
                });
                if let Err(stop) = self.relay(relay, state).await {
                    return Some(end_of(&stop));
                }
                state.delta_sent = true;
                if !state.overhead_recorded {
                    state.overhead_recorded = true;
                    self.deps.metrics.ttft_overhead(
                        self.provider(),
                        self.model(),
                        received_at.elapsed(),
                    );
                }
                self.progress(state).await;
                None
            }
            ProviderEvent::Reasoning(text) => {
                let relay = RelayEvent::Delta(Delta {
                    kind: DeltaKind::Reasoning,
                    content: text,
                });
                if let Err(stop) = self.relay(relay, state).await {
                    return Some(end_of(&stop));
                }
                state.delta_sent = true;
                self.progress(state).await;
                None
            }
            ProviderEvent::ToolStart { name, details } => {
                // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-web-search-turn:p1:inst-web-limit
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-breach
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-breach-finalize
                // a start above a per-message limit is not relayed: the provider stream is
                // cancelled before its usage arrives and the turn fails with the breach code
                if let StartDecision::Breach(code) = state.accounting.on_start(&name) {
                    return Some(End::Breach(code));
                }
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-breach-finalize
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-breach
                // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-web-search-turn:p1:inst-web-limit
                self.relay_tool(ToolPhase::Start, name, details, state)
                    .await
            }
            ProviderEvent::ToolDone { name, details } => {
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-done
                if let Some(kind) = state.accounting.on_done(&name)
                    && let Err(error) = self
                        .deps
                        .progress
                        .record_tool_done(self.plan.tenant_id, self.plan.turn_id, kind)
                        .await
                {
                    tracing::warn!(%error, "the durable tool counter could not be written");
                }
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-done
                self.relay_tool(ToolPhase::Done, name, details, state).await
            }
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-unexpected
            // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-unexpected-finalize
            // the knowledge-search loop belongs to the attachments feature: a function tool use
            // is unexpected here and fails the turn (the usage is not known at this point)
            ProviderEvent::ToolUse { .. } => self.on_tool_use(state),
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-unexpected-finalize
            // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-unexpected
            ProviderEvent::Completed(completion) => Some(Self::on_completed(completion, state)),
            ProviderEvent::Failed(failure) => Some(End::Failed(map_provider_failure(
                &ProviderFailure::Event(failure),
            ))),
        }
    }

    /// A completed response: a tool-use outcome when the iteration saw function tool uses.
    fn on_completed(completion: ProviderCompletion, state: &mut State) -> End {
        state.response_id.clone_from(&completion.response_id);
        if state.tool_uses > 0 {
            End::ToolUse(Box::new(completion))
        } else {
            End::Completed(Box::new(completion))
        }
    }

    /// A function tool use: an outcome the knowledge loop answers when it is active for the
    /// turn, otherwise unexpected.
    fn on_tool_use(&self, state: &mut State) -> Option<End> {
        if self.knowledge_active() {
            state.tool_uses += 1;
            None
        } else {
            Some(End::UnexpectedToolUse)
        }
    }

    async fn relay_tool(
        &self,
        phase: ToolPhase,
        name: String,
        details: serde_json::Value,
        state: &mut State,
    ) -> Option<End> {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-relay
        // every other `tool` event is relayed unchanged with its phase, name and details
        let relay = RelayEvent::Tool(ToolEvent {
            phase,
            name,
            details,
        });
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-relay
        if let Err(stop) = self.relay(relay, state).await {
            return Some(end_of(&stop));
        }
        self.progress(state).await;
        None
    }

    /// Hand an event to the bounded relay channel: a full channel blocks the producer (the
    /// durable progress is still refreshed, throttled, while it waits for a slow client), and a
    /// cancellation or a relay that went away ends the wait.
    async fn relay(&self, event: RelayEvent, state: &mut State) -> Result<(), Stop> {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-channel
        let send = self.tx.send(event);
        tokio::pin!(send);
        loop {
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => return Err(Stop::Token),
                result = &mut send => return result.map_err(|_| Stop::SendFailed),
                () = tokio::time::sleep(self.deps.settings.progress_interval) => {
                    self.progress(state).await;
                }
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-sse-relay:p1:inst-relay-channel
    }

    /// The durable `last_progress_at` write, at most every `progress_interval`.
    async fn progress(&self, state: &mut State) {
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-progress
        let now = self.deps.quota.clock.now();
        if now - state.last_progress < self.deps.settings.progress_interval {
            return;
        }
        state.last_progress = now;
        if let Err(error) = self
            .deps
            .progress
            .touch_progress(self.plan.tenant_id, self.plan.turn_id, now)
            .await
        {
            tracing::warn!(%error, "the durable progress could not be written");
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-tool-accounting:p1:inst-ta-progress
    }

    /// The end of the call: the disconnect metrics, the finalization through the first-terminal
    /// -wins contract and the terminal event.
    async fn conclude(self, state: State, end: End, active: ActiveStream) {
        let metrics = Arc::clone(&self.deps.metrics);
        let mut active = Some(active);
        let Concluded {
            intent,
            terminal,
            citations,
            observed,
        } = match end {
            End::Completed(completion) => {
                completed_of(*completion, state, &self.plan.tools.citation_files)
            }
            End::ToolUse(_) => failed_with_code(StreamErrorCode::UnexpectedToolUse, None, state),
            End::Failed(mapped) => failed_of(mapped, state),
            End::Breach(code) => failed_with_code(code, None, state),
            End::Stopped { code, usage } => failed_with_code(code, usage, state),
            End::UnexpectedToolUse => {
                failed_with_code(StreamErrorCode::UnexpectedToolUse, None, state)
            }
            End::Cancelled {
                via_token,
                observed,
            } => {
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-disconnected
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-cancel
                // the disconnect is counted before the finalization: `before_first_token` when
                // no `delta` was sent yet; `cancel_requested` only for a disconnect observed
                // through the token, never for one detected by a failed channel send
                metrics.stream_disconnected(if state.delta_sent {
                    "mid_stream"
                } else {
                    "before_first_token"
                });
                if via_token {
                    metrics.cancel_requested();
                }
                // the provider stream is already dropped: the time to abort excludes the
                // finalization
                metrics.time_to_abort(observed.elapsed());
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-cancel
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-disconnected
                // the client is gone: the stream is closed before the finalization starts
                drop(active.take());
                cancelled_of(state, observed)
            }
        };
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-finalize
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-finalize
        let result = self
            .deps
            .finalizer
            .finalize_stream(&self.plan, &terminal)
            .await;
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-finalize
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-finalize
        if let Err(error) = &result {
            tracing::error!(
                turn_id = %self.plan.turn_id,
                %error,
                "the stream finalization failed; the turn stays running until the orphan watchdog"
            );
        }
        if observed.is_some() && matches!(result, Ok(StreamFinalization::Committed(_))) {
            metrics.cancel_effective();
        }
        self.emit(&intent, &terminal, &result, citations).await;
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-return
        // the stream closes with its terminal event (nothing at all after a disconnect, which
        // the UI treats as an indeterminate delivery and resolves through the turn status
        // API): the gauge is down before the channel closes
        drop(active);
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-return
    }

    /// The terminal emission: the events after the committed finalization (or its failure).
    async fn emit(
        &self,
        intent: &TerminalIntent,
        terminal: &StreamTerminal,
        result: &Result<StreamFinalization, DomainError>,
        citations: Option<CitationsPayload>,
    ) {
        let metrics = &self.deps.metrics;
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-incomplete
        // a turn that committed as `completed` (a provider `incomplete` included)
        let completed = matches!(
            result,
            Ok(StreamFinalization::Committed(committed)) if committed.state == TurnState::Completed
        );
        if completed {
            metrics.stream_completed(self.provider(), self.model());
            if let Some(reason) = &terminal.incomplete_reason {
                metrics.stream_incomplete(self.provider(), self.model(), reason);
            }
        }
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-incomplete
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-warnings-failed
        // the warnings are read after the settlement of a committed completed turn; a failed
        // read is logged and `done` goes without them
        let warnings = if completed {
            self.read_warnings().await
        } else {
            None
        };
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-warnings-failed
        // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-return
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-terminal
        // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-web-search-turn:p1:inst-web-return
        // a web search turn ends by `done` or by the terminal `error`, like every turn
        let events = terminal_emission(&self.plan, intent, result, citations, warnings);
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-web-search-turn:p1:inst-web-return
        // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-send-message:p1:inst-send-terminal
        // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-return
        for event in events {
            let terminal_event = matches!(event, RelayEvent::Done(_) | RelayEvent::Error(_));
            if let RelayEvent::Error(error) = &event {
                metrics.stream_failed(self.provider(), self.model(), error.code.as_str());
            }
            if terminal_event {
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-latency
                metrics.stream_total_latency(self.provider(), self.model(), self.began.elapsed());
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-streaming-metrics:p1:inst-met-latency
            }
            // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-after-terminal
            // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-return-noop
            // the client is gone: nothing more is sent, the committed outcome stands
            if self.tx.send(event).await.is_err() {
                return;
            }
            // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-return-noop
            // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-disconnect:p1:inst-dc-after-terminal
        }
    }

    async fn read_warnings(&self) -> Option<Vec<crate::domain::quota::QuotaWarning>> {
        let ctx = SecurityContext::builder()
            .subject_id(self.plan.user_id)
            .subject_tenant_id(self.plan.tenant_id)
            .build()
            .ok()?;
        match self.deps.quota.quota_warnings(&ctx).await {
            Ok(warnings) => Some(warnings),
            Err(error) => {
                // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-warnings-omitted
                tracing::warn!(%error, "the quota warnings could not be read; `done` omits them");
                None
                // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-terminal-emission:p1:inst-emit-warnings-omitted
            }
        }
    }
}

/// Append the call and its output to the provider input, in the format of the adapter.
fn append_tool_exchange(
    body: &mut serde_json::Value,
    kind: ProviderKind,
    format: Option<ToolResultFormat>,
    call: &ToolCall,
    output: &str,
) {
    use serde_json::json;
    match kind {
        ProviderKind::OpenaiResponses | ProviderKind::VllmResponses => {
            let item = tool_result_item(
                format.unwrap_or(ToolResultFormat::FunctionCallOutput),
                call,
                output,
            );
            if let Some(input) = body
                .get_mut("input")
                .and_then(serde_json::Value::as_array_mut)
            {
                input.push(json!({
                    "type": "function_call", "call_id": call.call_id,
                    "name": call.name, "arguments": call.arguments,
                }));
                input.push(item);
            }
        }
        ProviderKind::AnthropicMessages => {
            let input: serde_json::Value =
                serde_json::from_str(&call.arguments).unwrap_or(serde_json::Value::Null);
            if let Some(messages) = body
                .get_mut("messages")
                .and_then(serde_json::Value::as_array_mut)
            {
                messages.push(json!({ "role": "assistant", "content": [
                    { "type": "tool_use", "id": call.call_id, "name": call.name, "input": input }
                ] }));
                messages.push(json!({ "role": "user", "content": [
                    tool_result_item(ToolResultFormat::ToolResult, call, output)
                ] }));
            }
        }
        ProviderKind::OpenaiChatCompletions => {
            if let Some(messages) = body
                .get_mut("messages")
                .and_then(serde_json::Value::as_array_mut)
            {
                // the assistant message that made the call, then the `tool` message answering it
                messages.push(
                    json!({ "role": "assistant", "content": null, "tool_calls": [{
                    "id": call.call_id, "type": "function",
                    "function": { "name": call.name, "arguments": call.arguments }
                }] }),
                );
                messages.push(json!({
                    "role": "tool", "tool_call_id": call.call_id, "content": output
                }));
            }
        }
    }
}

/// The end of a call that stopped because a send did not happen.
fn end_of(stop: &Stop) -> End {
    End::Cancelled {
        via_token: matches!(stop, Stop::Token),
        observed: Instant::now(),
    }
}

/// What the end of the call decided: the intent of the terminal event, the terminal state to
/// finalize, the mapped citations and the instant a disconnect was observed.
struct Concluded {
    intent: TerminalIntent,
    terminal: StreamTerminal,
    citations: Option<CitationsPayload>,
    observed: Option<Instant>,
}

/// A provider `completed` or `incomplete` response.
fn completed_of(
    completion: ProviderCompletion,
    state: State,
    files: &FileCitationMap,
) -> Concluded {
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-return
    // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-web-search-turn:p1:inst-web-citations
    // @cpt-begin:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-citations
    // file annotations resolve through the citation file map of the file search contribution;
    // unknown and soft-deleted attachments are omitted
    let citations = map_citations(&completion, files);
    // @cpt-end:cpt-cf-mini-chat-flow-attachments-and-file-search-ask-about-files:p1:inst-ask-citations
    // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-web-search-turn:p1:inst-web-citations
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-event-translation:p1:inst-tr-return
    Concluded {
        intent: TerminalIntent::Completed {
            usage: completion.usage,
        },
        terminal: StreamTerminal {
            outcome: StreamOutcome::Completed,
            usage: completion.usage,
            provider_response_id: completion.response_id,
            text: state.text,
            // @cpt-begin:cpt-cf-mini-chat-flow-message-streaming-web-search-turn:p1:inst-web-account
            // the completed call counts are settled by the finalization
            tool_counts: state.accounting.completed(),
            // @cpt-end:cpt-cf-mini-chat-flow-message-streaming-web-search-turn:p1:inst-web-account
            incomplete_reason: completion.incomplete_reason,
        },
        citations,
        observed: None,
    }
}

/// A mapped provider failure; the provider text stays in the log.
fn failed_of(mapped: MappedFailure, state: State) -> Concluded {
    tracing::warn!(
        code = mapped.code.as_str(),
        provider_detail = %mapped.internal,
        "the provider call failed"
    );
    // @cpt-begin:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-sanitize-stored
    // the stored `error_detail` is the sanitized client message, never the provider text
    let terminal = StreamTerminal {
        outcome: StreamOutcome::Failed {
            code: mapped.code,
            detail: mapped.message.clone(),
        },
        usage: mapped.usage,
        provider_response_id: state.response_id,
        // partial text of a failed turn is discarded
        text: String::new(),
        tool_counts: state.accounting.completed(),
        incomplete_reason: None,
    };
    // @cpt-end:cpt-cf-mini-chat-algo-message-streaming-error-sanitization:p1:inst-err-sanitize-stored
    Concluded {
        intent: TerminalIntent::Failed {
            code: mapped.code,
            message: mapped.message,
        },
        terminal,
        citations: None,
        observed: None,
    }
}

/// A disconnect: the accumulated text is kept as a partial message.
fn cancelled_of(state: State, observed: Instant) -> Concluded {
    Concluded {
        intent: TerminalIntent::Cancelled,
        terminal: StreamTerminal {
            outcome: StreamOutcome::Cancelled,
            usage: None,
            provider_response_id: state.response_id,
            text: state.text,
            tool_counts: state.accounting.completed(),
            incomplete_reason: None,
        },
        citations: None,
        observed: Some(observed),
    }
}

/// A failure of the turn by a code of the pipeline (a tool-limit breach, an unexpected tool
/// use, the iteration cap): the generic message of the code and no text. `usage` is the usage
/// of the last completed iteration when the failure came after one, otherwise `None`.
fn failed_with_code(code: StreamErrorCode, usage: Option<UsageTokens>, state: State) -> Concluded {
    let message = generic_message(code).to_owned();
    let terminal = StreamTerminal {
        outcome: StreamOutcome::Failed {
            code,
            detail: message.clone(),
        },
        usage,
        provider_response_id: state.response_id,
        text: String::new(),
        tool_counts: state.accounting.completed(),
        incomplete_reason: None,
    };
    Concluded {
        intent: TerminalIntent::Failed { code, message },
        terminal,
        citations: None,
        observed: None,
    }
}

/// A gateway-side error of `proxy_request`: a deadline is a timeout, anything else a transport
/// failure.
fn gateway_failure(error: &CanonicalError) -> ProviderFailure {
    match error {
        CanonicalError::DeadlineExceeded { .. } => ProviderFailure::GatewayTimeout,
        other => ProviderFailure::Transport(format!("gateway error: {other}")),
    }
}

#[cfg(test)]
#[path = "provider_task_tests.rs"]
mod provider_task_tests;

#[cfg(test)]
#[path = "provider_task_metrics_tests.rs"]
mod provider_task_metrics_tests;

#[cfg(test)]
#[path = "stream_settings_tests.rs"]
mod stream_settings_tests;

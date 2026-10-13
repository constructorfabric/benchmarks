#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]
#![allow(
    clippy::let_underscore_must_use,
    clippy::inconsistent_struct_constructor,
    clippy::type_complexity
)]
//! Fixtures of the stream pipeline unit tests: in-memory turn store with a
//! call log and gates, a channel-fed fake provider (events are released by the
//! test, the dropped stream is observable), a recording metrics sink, a fake
//! chat store and provider directory, and the whole service assembled over
//! them and the quota fakes.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use futures::future::BoxFuture;
use mini_chat_sdk::{TerminalState, UsageTokens};
use serde_json::json;
use time::OffsetDateTime;
use tokio::sync::Notify;
use toolkit_odata::{ODataQuery, Page};
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::context::ContextPlan;
use super::finalize::{TaskDeps, TurnRun};
use super::inputs::InputLimits;
use super::pipeline::{StreamDeps, StreamService, StreamSettings};
use super::ports::{
    AttachmentInfo, AttachmentKind, BuiltRequest, CreateTurnInput, DisconnectStage, FinalizeError,
    FinalizeWrites, HistoryMessage, HistoryRole, KnowledgeChunk, KnowledgeRetriever,
    KnowledgeSearchRequest, MessageBoundary, ProgressUpdate, ProviderClient, ProviderDirectory,
    ProviderStream, ProviderTarget, RecentQuery, StoredAssistantMessage, StreamMetrics,
    ThreadSummaryInfo, ToolCallOutput, TurnRepository,
};
use super::request::ProviderRequestInput;
use super::tools::{GuardTexts, KnowledgeInput, ToolsDecision};
use super::types::{ProviderEvent, ProviderFailure, StreamErrorCode, TurnRecord, TurnState};
use super::validate::ValidationLimits;
use crate::config::ProviderKind;
use crate::domain::authz::AuthzService;
use crate::domain::error::{DomainError, ResourceKind};
use crate::domain::model::Chat;
use crate::domain::ports::{ChatRepository, PolicyCatalog};
use crate::domain::service::quota::finalize::{FinalizeRequest, FinalizeResult, FinalizeWon};
use crate::domain::service::quota::preflight::PreflightDecision;
use crate::domain::service::quota::reserve::ReservedColumns;
use crate::domain::service::quota::service::{Clock, QuotaDeps, QuotaService};
use crate::domain::service::quota::test_support::{
    FakePolicy, FakeRepo, RecordingMetrics, default_config, design_catalog, design_limits,
    design_rows, fixed_clock, noon, premium_decision, snapshot, standard_decision,
};
use crate::domain::service::workers::summary_trigger::SummaryTriggerPlan;
use crate::test_doubles::{FakePdp, PdpReply, USER, USER_TENANT};

// ---- constants and row builders -----------------------------------------------------------------

/// The chat of the fixtures.
pub const CHAT: Uuid = Uuid::from_u128(0xC1);
/// The request id of the fixtures.
pub const REQUEST: Uuid = Uuid::from_u128(0xA1);
/// The turn id of the fixtures.
pub const TURN: Uuid = Uuid::from_u128(0xB1);
/// The pre-allocated assistant message id of the fixtures.
pub const ASSISTANT: Uuid = Uuid::from_u128(0xD1);
/// Alias of the fake provider directory.
pub const ALIAS: &str = "api.openai.test";

/// `noon()` plus `secs` seconds.
pub fn at(secs: i64) -> OffsetDateTime {
    noon() + time::Duration::seconds(secs)
}

/// The chat of the default user on `model`.
pub fn chat(model: &str) -> Chat {
    Chat {
        id: CHAT,
        tenant_id: USER_TENANT,
        user_id: USER,
        model: model.to_owned(),
        title: None,
        is_temporary: false,
        created_at: at(-3600),
        updated_at: at(-3600),
    }
}

/// A turn of `CHAT` with request id [`REQUEST`].
pub fn turn(state: TurnState) -> TurnRecord {
    TurnRecord {
        id: TURN,
        tenant_id: USER_TENANT,
        chat_id: CHAT,
        request_id: REQUEST,
        requester_user_id: Some(USER),
        state,
        assistant_message_id: (state == TurnState::Completed).then_some(ASSISTANT),
        error_code: None,
        effective_model: Some("p".to_owned()),
        started_at: at(-60),
        updated_at: at(-30),
        deleted: false,
    }
}

/// A different running turn of the chat.
pub fn other_running() -> TurnRecord {
    TurnRecord {
        id: Uuid::from_u128(0xB2),
        request_id: Uuid::from_u128(0xA2),
        ..turn(TurnState::Running)
    }
}

/// A ready document attachment.
pub fn doc(n: u128) -> AttachmentInfo {
    AttachmentInfo {
        id: Uuid::from_u128(0x1000 + n),
        kind: AttachmentKind::Document,
        status: "ready".to_owned(),
        filename: format!("doc{n}.pdf"),
        provider_file_id: Some(format!("file-doc{n}")),
        secondary_file_id: None,
        for_file_search: true,
        for_code_interpreter: false,
        created_at: at(-1000 + i64::try_from(n).unwrap()),
    }
}

/// A ready image attachment.
pub fn image(n: u128) -> AttachmentInfo {
    AttachmentInfo {
        id: Uuid::from_u128(0x2000 + n),
        kind: AttachmentKind::Image,
        status: "ready".to_owned(),
        filename: format!("img{n}.png"),
        provider_file_id: Some(format!("file-img{n}")),
        secondary_file_id: Some(format!("sec-img{n}")),
        for_file_search: false,
        for_code_interpreter: false,
        created_at: at(-900 + i64::try_from(n).unwrap()),
    }
}

/// A ready spreadsheet for the code interpreter.
pub fn sheet(n: u128) -> AttachmentInfo {
    AttachmentInfo {
        id: Uuid::from_u128(0x3000 + n),
        kind: AttachmentKind::Document,
        status: "ready".to_owned(),
        filename: format!("sheet{n}.xlsx"),
        provider_file_id: Some(format!("file-sheet{n}")),
        secondary_file_id: None,
        for_file_search: false,
        for_code_interpreter: true,
        created_at: at(-800 + i64::try_from(n).unwrap()),
    }
}

/// A history message `secs` seconds after [`noon`] minus one hour.
pub fn history(n: u128, role: HistoryRole, content: &str, secs: i64) -> HistoryMessage {
    HistoryMessage {
        id: Uuid::from_u128(0x4000 + n),
        role,
        content: content.to_owned(),
        created_at: at(-3000 + secs),
    }
}

/// Provider usage with the given input and output tokens.
pub fn usage(input: u64, output: u64) -> UsageTokens {
    UsageTokens {
        input_tokens: input,
        output_tokens: output,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 0,
        reasoning_tokens: 0,
    }
}

/// A `Completed` provider event with usage.
pub fn completed(input: u64, output: u64) -> ProviderEvent {
    ProviderEvent::Completed {
        usage: Some(usage(input, output)),
        response_id: Some("resp_internal".to_owned()),
        incomplete_reason: None,
    }
}

/// A text delta event.
pub fn text(content: &str) -> ProviderEvent {
    ProviderEvent::TextDelta {
        text: content.to_owned(),
        item_id: Some("msg_1".to_owned()),
        content_index: Some(0),
    }
}

/// A provider failure of `code` with the given client message.
pub fn failure(code: StreamErrorCode, message: &str) -> ProviderFailure {
    ProviderFailure {
        code,
        message: message.to_owned(),
        detail: None,
        usage: None,
    }
}

// ---- fake turn store ----------------------------------------------------------------------------

/// A call of the fake turn store.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnsCall {
    FindByRequestId(Uuid, Uuid),
    FindRunning(Uuid),
    LoadAssistant(Uuid, Uuid),
    RequestAttachments(Vec<Uuid>),
    ReadyAttachments,
    VectorStore,
    Boundary,
    Summary,
    Recent(RecentQuery),
    CreateTurn,
    Finalize,
    TurnState(Uuid, Uuid, Uuid),
    Progress(ProgressUpdate),
}

/// What `create_turn` was called with, owned.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateRecord {
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
    pub request_id: Uuid,
    pub turn_id: Uuid,
    pub user_message_id: Uuid,
    pub content: String,
    pub attachment_ids: Vec<Uuid>,
    pub web_search_enabled: bool,
    pub now: OffsetDateTime,
    pub effective_model: String,
    pub send_mode: bool,
}

/// The data and scripts of the fake turn store.
#[derive(Default)]
pub struct TurnsInner {
    pub by_request: Vec<TurnRecord>,
    pub running: Option<TurnRecord>,
    pub assistant: Option<StoredAssistantMessage>,
    pub request_attachments: Vec<AttachmentInfo>,
    pub ready: Vec<AttachmentInfo>,
    pub vector_store: Option<String>,
    pub boundary: Option<MessageBoundary>,
    pub summary: Option<ThreadSummaryInfo>,
    pub recent: Vec<HistoryMessage>,
    pub create_results: VecDeque<Result<ReservedColumns, DomainError>>,
    pub finalize_results: VecDeque<Result<FinalizeResult, FinalizeError>>,
    pub turn_states: VecDeque<Result<Option<TurnState>, DomainError>>,
    pub default_turn_state: Option<TurnState>,
    pub read_error: Option<String>,
    /// Committed by a concurrent winner when `create_turn` returns a scripted
    /// unique violation: visible to the re-reads that follow.
    pub race_winner: Option<TurnRecord>,
    /// Same, as the running turn of the chat.
    pub race_running: Option<TurnRecord>,
    /// The first N `record_progress` calls fail.
    pub progress_failures: usize,
    pub calls: Vec<TurnsCall>,
    pub creates: Vec<CreateRecord>,
    pub finalizations: Vec<(FinalizeRequest, FinalizeWrites)>,
    pub progress: Vec<ProgressUpdate>,
    pub scopes: Vec<AccessScope>,
}

/// Gates and hooks of the fake turn store.
#[derive(Default)]
pub struct TurnsGates {
    /// `create_turn` awaits this hook after the (fake) commit.
    pub after_create: Mutex<Option<Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>>>,
    /// `finalize` waits for a permit of this notify before it answers.
    pub finalize_gate: Mutex<Option<Arc<Notify>>>,
    /// Signalled when `finalize` was entered.
    pub finalize_entered: Notify,
    /// `turn_state` waits for this notify before it answers.
    pub state_gate: Mutex<Option<Arc<Notify>>>,
    /// `record_progress` waits for this notify before it answers.
    pub progress_gate: Mutex<Option<Arc<Notify>>>,
}

/// In-memory [`TurnRepository`].
pub struct FakeTurns {
    pub inner: Mutex<TurnsInner>,
    pub gates: TurnsGates,
    pub state_checks_in_flight: AtomicUsize,
    pub max_state_checks_in_flight: AtomicUsize,
}

impl FakeTurns {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(TurnsInner {
                default_turn_state: Some(TurnState::Running),
                ..TurnsInner::default()
            }),
            gates: TurnsGates::default(),
            state_checks_in_flight: AtomicUsize::new(0),
            max_state_checks_in_flight: AtomicUsize::new(0),
        })
    }

    /// Mutates the data and scripts.
    pub fn with<R>(&self, edit: impl FnOnce(&mut TurnsInner) -> R) -> R {
        edit(&mut self.inner.lock().unwrap())
    }

    pub fn calls(&self) -> Vec<TurnsCall> {
        self.with(|inner| inner.calls.clone())
    }

    pub fn count(&self, wanted: impl Fn(&TurnsCall) -> bool) -> usize {
        self.with(|inner| inner.calls.iter().filter(|call| wanted(call)).count())
    }

    pub fn creates(&self) -> Vec<CreateRecord> {
        self.with(|inner| inner.creates.clone())
    }

    pub fn finalizations(&self) -> Vec<(FinalizeRequest, FinalizeWrites)> {
        self.with(|inner| inner.finalizations.clone())
    }

    pub fn progress(&self) -> Vec<ProgressUpdate> {
        self.with(|inner| inner.progress.clone())
    }

    pub fn state_check_count(&self) -> usize {
        self.count(|call| matches!(call, TurnsCall::TurnState(..)))
    }

    fn log(&self, call: TurnsCall, scope: Option<&AccessScope>) -> Result<(), DomainError> {
        self.with(|inner| {
            inner.calls.push(call);
            if let Some(scope) = scope {
                inner.scopes.push(scope.clone());
            }
            inner
                .read_error
                .clone()
                .map_or(Ok(()), |message| Err(DomainError::Database(message)))
        })
    }
}

/// A `FinalizeWon` for the terminal state of `request`.
pub fn won_for(request: &FinalizeRequest) -> FinalizeWon {
    use crate::domain::service::quota::finalize::derive_billing_outcome;
    let derived = derive_billing_outcome(&request.classification, request.usage.as_ref());
    FinalizeWon {
        terminal_state: derived.terminal_state,
        billing_outcome: derived.billing_outcome,
        settlement_method: derived.settlement_method,
        committed_credits_micro: 1_000,
        settled: true,
        usage_event_enqueued: true,
        actual_input_tokens: request.usage.map_or(0, |usage| {
            i64::try_from(usage.input_tokens).unwrap_or(i64::MAX)
        }),
        actual_output_tokens: request.usage.map_or(0, |usage| {
            i64::try_from(usage.output_tokens).unwrap_or(i64::MAX)
        }),
        reserve_tokens: 1500,
        overshoot_capped: false,
        fallback: None,
        effective_model: Some("p".to_owned()),
        code_interpreter_calls: request.tool_calls.code_interpreter,
        summary_trigger: None,
    }
}

#[async_trait]
impl TurnRepository for FakeTurns {
    async fn find_by_request_id(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError> {
        self.log(TurnsCall::FindByRequestId(chat_id, request_id), Some(scope))?;
        Ok(self.with(|inner| {
            inner
                .by_request
                .iter()
                .find(|turn| turn.chat_id == chat_id && turn.request_id == request_id)
                .cloned()
        }))
    }

    async fn find_running(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError> {
        self.log(TurnsCall::FindRunning(chat_id), Some(scope))?;
        Ok(self.with(|inner| inner.running.clone()))
    }

    async fn load_assistant_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        message_id: Uuid,
    ) -> Result<Option<StoredAssistantMessage>, DomainError> {
        self.log(TurnsCall::LoadAssistant(chat_id, message_id), Some(scope))?;
        Ok(self.with(|inner| inner.assistant.clone()))
    }

    async fn request_attachments(
        &self,
        scope: &AccessScope,
        _chat_id: Uuid,
        ids: &[Uuid],
    ) -> Result<Vec<AttachmentInfo>, DomainError> {
        self.log(TurnsCall::RequestAttachments(ids.to_vec()), Some(scope))?;
        Ok(self.with(|inner| {
            inner
                .request_attachments
                .iter()
                .filter(|row| ids.contains(&row.id))
                .cloned()
                .collect()
        }))
    }

    async fn ready_attachments(
        &self,
        scope: &AccessScope,
        _chat_id: Uuid,
    ) -> Result<Vec<AttachmentInfo>, DomainError> {
        self.log(TurnsCall::ReadyAttachments, Some(scope))?;
        Ok(self.with(|inner| inner.ready.clone()))
    }

    async fn vector_store_id(
        &self,
        scope: &AccessScope,
        _chat_id: Uuid,
    ) -> Result<Option<String>, DomainError> {
        self.log(TurnsCall::VectorStore, Some(scope))?;
        Ok(self.with(|inner| inner.vector_store.clone()))
    }

    async fn snapshot_boundary(
        &self,
        scope: &AccessScope,
        _chat_id: Uuid,
    ) -> Result<Option<MessageBoundary>, DomainError> {
        self.log(TurnsCall::Boundary, Some(scope))?;
        Ok(self.with(|inner| inner.boundary))
    }

    async fn thread_summary(
        &self,
        scope: &AccessScope,
        _chat_id: Uuid,
    ) -> Result<Option<ThreadSummaryInfo>, DomainError> {
        self.log(TurnsCall::Summary, Some(scope))?;
        Ok(self.with(|inner| inner.summary.clone()))
    }

    async fn recent_messages(
        &self,
        scope: &AccessScope,
        _chat_id: Uuid,
        query: &RecentQuery,
    ) -> Result<Vec<HistoryMessage>, DomainError> {
        self.log(TurnsCall::Recent(*query), Some(scope))?;
        Ok(self.with(|inner| inner.recent.clone()))
    }

    async fn create_turn(
        &self,
        input: &CreateTurnInput<'_>,
    ) -> Result<ReservedColumns, DomainError> {
        self.log(TurnsCall::CreateTurn, None)?;
        self.with(|inner| {
            inner.creates.push(CreateRecord {
                tenant_id: input.reserve.tenant_id,
                user_id: input.reserve.user_id,
                chat_id: input.chat_id,
                request_id: input.request_id,
                turn_id: input.turn_id,
                user_message_id: input.user_message_id,
                content: input.content.to_owned(),
                attachment_ids: input.attachment_ids.to_vec(),
                web_search_enabled: input.web_search_enabled,
                now: input.reserve.now,
                effective_model: input.reserve.decision.effective_model.clone(),
                send_mode: matches!(
                    input.reserve.mode,
                    crate::domain::service::quota::reserve::ReserveMode::Send
                ),
            });
        });
        let scripted = self.with(|inner| inner.create_results.pop_front());
        if matches!(scripted, Some(Err(DomainError::UniqueViolation))) {
            self.with(|inner| {
                if let Some(winner) = inner.race_winner.take() {
                    inner.by_request.push(winner);
                }
                if let Some(running) = inner.race_running.take() {
                    inner.running = Some(running);
                }
            });
        }
        let result =
            scripted.unwrap_or_else(|| Ok(ReservedColumns::from_decision(input.reserve.decision)));
        if result.is_ok() {
            let hook = self.gates.after_create.lock().unwrap().clone();
            if let Some(hook) = hook {
                hook().await;
            }
        }
        result
    }

    async fn finalize(
        &self,
        request: &FinalizeRequest,
        writes: &FinalizeWrites,
    ) -> Result<FinalizeResult, FinalizeError> {
        self.with(|inner| {
            inner.calls.push(TurnsCall::Finalize);
            inner.finalizations.push((request.clone(), writes.clone()));
        });
        self.gates.finalize_entered.notify_waiters();
        let gate = self.gates.finalize_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        let scripted = self.with(|inner| inner.finalize_results.pop_front());
        scripted.unwrap_or_else(|| Ok(FinalizeResult::Won(Box::new(won_for(request)))))
    }

    async fn turn_state(
        &self,
        tenant_id: Uuid,
        chat_id: Uuid,
        turn_id: Uuid,
    ) -> Result<Option<TurnState>, DomainError> {
        self.log(TurnsCall::TurnState(tenant_id, chat_id, turn_id), None)?;
        let now = self.state_checks_in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_state_checks_in_flight
            .fetch_max(now, Ordering::SeqCst);
        let gate = self.gates.state_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.state_checks_in_flight.fetch_sub(1, Ordering::SeqCst);
        self.with(|inner| {
            inner
                .turn_states
                .pop_front()
                .unwrap_or(Ok(inner.default_turn_state))
        })
    }

    async fn record_progress(
        &self,
        _tenant_id: Uuid,
        _chat_id: Uuid,
        _turn_id: Uuid,
        update: &ProgressUpdate,
    ) -> Result<(), DomainError> {
        self.log(TurnsCall::Progress(*update), None)?;
        let gate = self.gates.progress_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate.notified().await;
        }
        self.with(|inner| {
            if inner.progress_failures > 0 {
                inner.progress_failures -= 1;
                return Err(DomainError::Database("progress write failed".to_owned()));
            }
            inner.progress.push(*update);
            Ok(())
        })
    }
}

// ---- fake provider ------------------------------------------------------------------------------

struct StreamState {
    dropped: AtomicBool,
    notify: Notify,
    pulled: AtomicUsize,
}

/// The test side of one provider stream: events are released one at a time.
pub struct ProviderFeed {
    tx: tokio::sync::mpsc::UnboundedSender<Result<ProviderEvent, ProviderFailure>>,
    state: Arc<StreamState>,
}

impl ProviderFeed {
    pub fn push(&self, event: ProviderEvent) {
        let _ = self.tx.send(Ok(event));
    }

    pub fn push_all(&self, events: impl IntoIterator<Item = ProviderEvent>) {
        for event in events {
            self.push(event);
        }
    }

    pub fn fail(&self, failure: ProviderFailure) {
        let _ = self.tx.send(Err(failure));
    }

    /// Ends the stream (EOF).
    pub fn end(self) {
        drop(self.tx);
    }

    /// The gear dropped the stream (the body, upstream).
    pub fn is_dropped(&self) -> bool {
        self.state.dropped.load(Ordering::SeqCst)
    }

    pub async fn dropped(&self) {
        loop {
            let notified = self.state.notify.notified();
            if self.is_dropped() {
                return;
            }
            notified.await;
        }
    }

    /// Number of events the gear has pulled so far.
    pub fn pulled(&self) -> usize {
        self.state.pulled.load(Ordering::SeqCst)
    }
}

struct FeedStream {
    rx: tokio::sync::mpsc::UnboundedReceiver<Result<ProviderEvent, ProviderFailure>>,
    state: Arc<StreamState>,
}

#[async_trait]
impl ProviderStream for FeedStream {
    async fn next_event(&mut self) -> Option<Result<ProviderEvent, ProviderFailure>> {
        let next = self.rx.recv().await;
        if next.is_some() {
            self.state.pulled.fetch_add(1, Ordering::SeqCst);
        }
        next
    }
}

impl Drop for FeedStream {
    fn drop(&mut self) {
        self.state.dropped.store(true, Ordering::SeqCst);
        self.state.notify.notify_waiters();
    }
}

/// What the fake provider recorded of one request build.
#[derive(Debug, Clone)]
pub struct RecordedBuild {
    pub target: ProviderTarget,
    pub model: String,
    pub plan: ContextPlan,
    pub tools: ToolsDecision,
    pub max_output_tokens_applied: i64,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub chat_id: Uuid,
}

#[derive(Default)]
struct ProviderInner {
    continuations: Vec<(BuiltRequest, Vec<ToolCallOutput>)>,
    continue_error: Option<String>,
    streams: VecDeque<StreamScript>,
    opened: Vec<BuiltRequest>,
    builds: Vec<RecordedBuild>,
    contexts: Vec<SecurityContext>,
    build_error: Option<String>,
    hang_open: bool,
    /// Every `open` after this many calls never answers.
    hang_after: Option<usize>,
}

enum StreamScript {
    Stream(FeedStream),
    Fail(ProviderFailure),
}

/// Sets a counter when a hanging `open` future is dropped.
struct HangGuard(Arc<AtomicUsize>);

impl Drop for HangGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// The fake provider client: `open` pops the next scripted stream.
#[derive(Default)]
pub struct FakeProvider {
    inner: Mutex<ProviderInner>,
    open_dropped: Arc<AtomicUsize>,
}

impl FakeProvider {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Scripts the next `open` to return a stream fed by the returned feed.
    pub fn expect_stream(&self) -> ProviderFeed {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let state = Arc::new(StreamState {
            dropped: AtomicBool::new(false),
            notify: Notify::new(),
            pulled: AtomicUsize::new(0),
        });
        self.inner
            .lock()
            .unwrap()
            .streams
            .push_back(StreamScript::Stream(FeedStream {
                rx,
                state: Arc::clone(&state),
            }));
        ProviderFeed { tx, state }
    }

    /// Scripts the next `open` to fail.
    pub fn fail_open(&self, failure: ProviderFailure) {
        self.inner
            .lock()
            .unwrap()
            .streams
            .push_back(StreamScript::Fail(failure));
    }

    /// Every `open` never answers.
    pub fn hang_open(&self) {
        self.inner.lock().unwrap().hang_open = true;
    }

    /// Every `open` after the first `opens` ones never answers.
    pub fn hang_open_after(&self, opens: usize) {
        self.inner.lock().unwrap().hang_after = Some(opens);
    }

    /// `build_request` fails.
    pub fn fail_build(&self, message: &str) {
        self.inner.lock().unwrap().build_error = Some(message.to_owned());
    }

    /// `continue_request` fails.
    pub fn fail_continue(&self, message: &str) {
        self.inner.lock().unwrap().continue_error = Some(message.to_owned());
    }

    /// The `(previous request, outputs)` of every `continue_request` call.
    pub fn continuations(&self) -> Vec<(BuiltRequest, Vec<ToolCallOutput>)> {
        self.inner.lock().unwrap().continuations.clone()
    }

    pub fn open_count(&self) -> usize {
        self.inner.lock().unwrap().opened.len()
    }

    pub fn opened(&self) -> Vec<BuiltRequest> {
        self.inner.lock().unwrap().opened.clone()
    }

    pub fn builds(&self) -> Vec<RecordedBuild> {
        self.inner.lock().unwrap().builds.clone()
    }

    pub fn contexts(&self) -> Vec<SecurityContext> {
        self.inner.lock().unwrap().contexts.clone()
    }

    /// Number of hanging `open` futures dropped by the caller.
    pub fn open_calls_dropped(&self) -> usize {
        self.open_dropped.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ProviderClient for FakeProvider {
    fn build_request(&self, input: &ProviderRequestInput<'_>) -> Result<BuiltRequest, DomainError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(message) = &inner.build_error {
            return Err(DomainError::Internal(message.clone()));
        }
        inner.builds.push(RecordedBuild {
            target: input.target.clone(),
            model: input.entry.provider_model_id.clone(),
            plan: input.plan.clone(),
            tools: input.tools.clone(),
            max_output_tokens_applied: input.max_output_tokens_applied,
            tenant_id: input.tenant_id,
            user_id: input.user_id,
            chat_id: input.chat_id,
        });
        Ok(BuiltRequest {
            kind: input.target.kind,
            uri: format!("/{}{}", input.target.alias, input.target.api_path),
            headers: Vec::new(),
            body: json!({
                "model": input.entry.provider_model_id,
                "instructions": input.plan.instructions,
            }),
        })
    }

    /// Records the call; the next request is `previous` with the outputs
    /// appended to the array `body.continued` as `{ call_id, name, output }`.
    fn continue_request(
        &self,
        previous: &BuiltRequest,
        outputs: &[ToolCallOutput],
    ) -> Result<BuiltRequest, DomainError> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(message) = &inner.continue_error {
            return Err(DomainError::Internal(message.clone()));
        }
        inner
            .continuations
            .push((previous.clone(), outputs.to_vec()));
        let mut next = previous.clone();
        let continued = next.body["continued"].take();
        let mut items = continued.as_array().cloned().unwrap_or_default();
        for output in outputs {
            items.push(json!({
                "call_id": output.call.call_id,
                "name": output.call.name,
                "output": output.output,
            }));
        }
        next.body["continued"] = serde_json::Value::Array(items);
        Ok(next)
    }

    async fn open(
        &self,
        ctx: &SecurityContext,
        request: &BuiltRequest,
    ) -> Result<Box<dyn ProviderStream>, ProviderFailure> {
        let (script, hang) = {
            let mut inner = self.inner.lock().unwrap();
            inner.opened.push(request.clone());
            inner.contexts.push(ctx.clone());
            let hang = inner.hang_open
                || inner
                    .hang_after
                    .is_some_and(|opens| inner.opened.len() > opens);
            (inner.streams.pop_front(), hang)
        };
        if hang {
            let _guard = HangGuard(Arc::clone(&self.open_dropped));
            std::future::pending::<()>().await;
        }
        match script {
            Some(StreamScript::Stream(stream)) => Ok(Box::new(stream)),
            Some(StreamScript::Fail(failure)) => Err(failure),
            None => Err(ProviderFailure::generic(StreamErrorCode::ProviderError)),
        }
    }
}

// ---- the knowledge retriever --------------------------------------------------------------------

/// The scripted answer of one retrieval.
pub enum RetrieverReply {
    /// These chunk texts.
    Chunks(Vec<String>),
    /// The retrieval fails.
    Fail,
    /// The chunks after a pause of the (virtual) clock.
    After(Duration, Vec<String>),
    /// The retrieval never answers (the pending future is counted when it is
    /// dropped).
    Hang,
}

/// One retrieval the fake received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedSearch {
    pub tenant_id: Uuid,
    pub query: String,
    pub top_k: usize,
}

struct SearchGuard(Arc<AtomicUsize>);

impl Drop for SearchGuard {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// The fake knowledge retriever: `search` pops the next scripted reply.
#[derive(Default)]
pub struct FakeRetriever {
    replies: Mutex<VecDeque<RetrieverReply>>,
    searches: Mutex<Vec<RecordedSearch>>,
    dropped: Arc<AtomicUsize>,
}

impl FakeRetriever {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queues the answer of the next retrieval.
    pub fn push(&self, reply: RetrieverReply) {
        self.replies.lock().unwrap().push_back(reply);
    }

    /// Queues chunks with these texts.
    pub fn push_chunks(&self, texts: &[&str]) {
        self.push(RetrieverReply::Chunks(
            texts.iter().map(|text| (*text).to_owned()).collect(),
        ));
    }

    pub fn searches(&self) -> Vec<RecordedSearch> {
        self.searches.lock().unwrap().clone()
    }

    /// Number of hanging retrievals dropped by the caller.
    pub fn dropped_searches(&self) -> usize {
        self.dropped.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl KnowledgeRetriever for FakeRetriever {
    async fn search(
        &self,
        _ctx: &SecurityContext,
        request: &KnowledgeSearchRequest<'_>,
    ) -> Result<Vec<KnowledgeChunk>, DomainError> {
        self.searches.lock().unwrap().push(RecordedSearch {
            tenant_id: request.tenant_id,
            query: request.query.to_owned(),
            top_k: request.top_k,
        });
        let reply = self.replies.lock().unwrap().pop_front();
        let chunks = |texts: Vec<String>| {
            texts
                .into_iter()
                .map(|text| KnowledgeChunk { text })
                .collect()
        };
        match reply {
            Some(RetrieverReply::Chunks(texts)) => Ok(chunks(texts)),
            Some(RetrieverReply::After(pause, texts)) => {
                tokio::time::sleep(pause).await;
                Ok(chunks(texts))
            }
            Some(RetrieverReply::Hang) => {
                let _guard = SearchGuard(Arc::clone(&self.dropped));
                std::future::pending::<()>().await;
                Ok(Vec::new())
            }
            Some(RetrieverReply::Fail) | None => {
                Err(DomainError::Internal("the retrieval failed".to_owned()))
            }
        }
    }
}

// ---- recording stream metrics -------------------------------------------------------------------

/// One observation of the stream metrics.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamMetric {
    Started(String, String),
    Active(i64),
    TtftProvider(String, String),
    TtftOverhead(String, String),
    Completed(String, String),
    Incomplete(String, String, String),
    Failed(String, String, String),
    TotalLatency(String, String),
    CancelRequested,
    Disconnected(DisconnectStage),
    TimeToAbort,
    CancelEffective,
    StreamsAborted,
    ImageInputs(u32),
    FinalizationLatency,
    KnowledgeSearch(String),
    KnowledgeLatency,
    KnowledgeChunks(u64),
    SummaryTrigger(String),
}

/// Records every observation and the numeric values of the histograms.
#[derive(Default)]
pub struct RecordingStreamMetrics {
    pub events: Mutex<Vec<StreamMetric>>,
    pub millis: Mutex<Vec<(&'static str, f64)>>,
}

impl RecordingStreamMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn events(&self) -> Vec<StreamMetric> {
        self.events.lock().unwrap().clone()
    }

    pub fn count(&self, wanted: impl Fn(&StreamMetric) -> bool) -> usize {
        self.events().iter().filter(|metric| wanted(metric)).count()
    }

    /// Sum of the `active_streams` deltas.
    pub fn active(&self) -> i64 {
        self.events()
            .iter()
            .map(|metric| match metric {
                StreamMetric::Active(delta) => *delta,
                _ => 0,
            })
            .sum()
    }

    /// The recorded values of histogram `name`.
    pub fn values(&self, name: &str) -> Vec<f64> {
        self.millis
            .lock()
            .unwrap()
            .iter()
            .filter(|(key, _)| *key == name)
            .map(|(_, value)| *value)
            .collect()
    }

    fn push(&self, metric: StreamMetric) {
        self.events.lock().unwrap().push(metric);
    }

    fn value(&self, name: &'static str, value: f64) {
        self.millis.lock().unwrap().push((name, value));
    }
}

impl StreamMetrics for RecordingStreamMetrics {
    fn thread_summary_trigger(&self, result: &str) {
        self.push(StreamMetric::SummaryTrigger(result.to_owned()));
    }

    fn stream_started(&self, provider: &str, model: &str) {
        self.push(StreamMetric::Started(provider.to_owned(), model.to_owned()));
    }

    fn active_streams_add(&self, delta: i64) {
        self.push(StreamMetric::Active(delta));
    }

    fn ttft_provider_ms(&self, provider: &str, model: &str, millis: f64) {
        self.value("ttft_provider_ms", millis);
        self.push(StreamMetric::TtftProvider(
            provider.to_owned(),
            model.to_owned(),
        ));
    }

    fn ttft_overhead_ms(&self, provider: &str, model: &str, millis: f64) {
        self.value("ttft_overhead_ms", millis);
        self.push(StreamMetric::TtftOverhead(
            provider.to_owned(),
            model.to_owned(),
        ));
    }

    fn stream_completed(&self, provider: &str, model: &str) {
        self.push(StreamMetric::Completed(
            provider.to_owned(),
            model.to_owned(),
        ));
    }

    fn stream_incomplete(&self, provider: &str, model: &str, reason: &str) {
        self.push(StreamMetric::Incomplete(
            provider.to_owned(),
            model.to_owned(),
            reason.to_owned(),
        ));
    }

    fn stream_failed(&self, provider: &str, model: &str, error_code: &str) {
        self.push(StreamMetric::Failed(
            provider.to_owned(),
            model.to_owned(),
            error_code.to_owned(),
        ));
    }

    fn stream_total_latency_ms(&self, provider: &str, model: &str, millis: f64) {
        self.value("stream_total_latency_ms", millis);
        self.push(StreamMetric::TotalLatency(
            provider.to_owned(),
            model.to_owned(),
        ));
    }

    fn cancel_requested(&self) {
        self.push(StreamMetric::CancelRequested);
    }

    fn stream_disconnected(&self, stage: DisconnectStage) {
        self.push(StreamMetric::Disconnected(stage));
    }

    fn time_to_abort_ms(&self, millis: f64) {
        self.value("time_to_abort_ms", millis);
        self.push(StreamMetric::TimeToAbort);
    }

    fn cancel_effective(&self) {
        self.push(StreamMetric::CancelEffective);
    }

    fn streams_aborted(&self) {
        self.push(StreamMetric::StreamsAborted);
    }

    fn image_inputs_per_turn(&self, count: u32) {
        self.push(StreamMetric::ImageInputs(count));
    }

    fn finalization_latency_ms(&self, millis: f64) {
        self.value("finalization_latency_ms", millis);
        self.push(StreamMetric::FinalizationLatency);
    }

    fn knowledge_search(&self, result: &str) {
        self.push(StreamMetric::KnowledgeSearch(result.to_owned()));
    }

    fn knowledge_search_latency_ms(&self, millis: f64) {
        self.value("knowledge_search_latency_ms", millis);
        self.push(StreamMetric::KnowledgeLatency);
    }

    fn knowledge_search_chunks(&self, chunks: u64) {
        self.push(StreamMetric::KnowledgeChunks(chunks));
    }
}

// ---- chat store and provider directory ----------------------------------------------------------

/// A chat store that holds at most one chat.
pub struct FakeChats {
    pub chat: Mutex<Option<Chat>>,
    pub loads: Mutex<Vec<(AccessScope, Uuid)>>,
}

impl FakeChats {
    pub fn with(chat: Option<Chat>) -> Arc<Self> {
        Arc::new(Self {
            chat: Mutex::new(chat),
            loads: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl ChatRepository for FakeChats {
    async fn insert(&self, _scope: &AccessScope, _chat: &Chat) -> Result<(), DomainError> {
        panic!("not used by the stream pipeline")
    }

    async fn load(&self, scope: &AccessScope, chat_id: Uuid) -> Result<Chat, DomainError> {
        self.loads.lock().unwrap().push((scope.clone(), chat_id));
        self.chat
            .lock()
            .unwrap()
            .clone()
            .filter(|chat| chat.id == chat_id)
            .ok_or(DomainError::NotFound {
                resource: ResourceKind::Chat,
            })
    }

    async fn message_counts(
        &self,
        _scope: &AccessScope,
        _chat_ids: &[Uuid],
    ) -> Result<std::collections::HashMap<Uuid, u64>, DomainError> {
        panic!("not used by the stream pipeline")
    }

    async fn rename(
        &self,
        _scope: &AccessScope,
        _chat_id: Uuid,
        _title: &str,
        _at: OffsetDateTime,
    ) -> Result<u64, DomainError> {
        panic!("not used by the stream pipeline")
    }

    async fn page(
        &self,
        _scope: &AccessScope,
        _query: &ODataQuery,
    ) -> Result<Page<Chat>, DomainError> {
        panic!("not used by the stream pipeline")
    }

    async fn soft_delete_with_cleanup(
        &self,
        _scope: &AccessScope,
        _chat_id: Uuid,
    ) -> Result<(), DomainError> {
        panic!("not used by the stream pipeline")
    }
}

/// A provider directory that resolves every provider to the same target.
pub struct FakeDirectory {
    pub target: Mutex<ProviderTarget>,
    pub fail: AtomicBool,
    /// Provider ids that do not resolve (the others do).
    pub fail_ids: Mutex<Vec<String>>,
    pub resolved: Mutex<Vec<(String, Uuid)>>,
}

impl FakeDirectory {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            target: Mutex::new(ProviderTarget {
                kind: ProviderKind::OpenaiResponses,
                alias: ALIAS.to_owned(),
                api_path: "/v1/responses".to_owned(),
                api_version: None,
            }),
            fail: AtomicBool::new(false),
            fail_ids: Mutex::new(Vec::new()),
            resolved: Mutex::new(Vec::new()),
        })
    }
}

impl ProviderDirectory for FakeDirectory {
    fn resolve(&self, provider_id: &str, tenant_id: Uuid) -> Result<ProviderTarget, DomainError> {
        self.resolved
            .lock()
            .unwrap()
            .push((provider_id.to_owned(), tenant_id));
        if self.fail.load(Ordering::SeqCst)
            || self
                .fail_ids
                .lock()
                .unwrap()
                .iter()
                .any(|id| id == provider_id)
        {
            return Err(DomainError::ProviderResolution(format!(
                "provider `{provider_id}` is not configured"
            )));
        }
        Ok(self.target.lock().unwrap().clone())
    }
}

// ---- the assembled world ------------------------------------------------------------------------

/// The stream settings of the unit tests: ping every 5 s, a channel of 8,
/// 50 documents and 4 images, 10 recent messages, knowledge search off.
pub fn settings() -> StreamSettings {
    StreamSettings {
        ping_interval: Duration::from_secs(5),
        channel_capacity: 8,
        validation: ValidationLimits {
            max_documents_per_chat: 50,
            max_images_per_message: 4,
        },
        inputs: InputLimits {
            recent_messages_limit: 10,
            max_images_per_message: 4,
        },
        guards: GuardTexts {
            file_search: "FS-GUARD".to_owned(),
            web_search: "WS-GUARD".to_owned(),
        },
        knowledge: KnowledgeInput {
            enabled: false,
            provider: None,
            retriever_configured: false,
            guard: "KS-GUARD".to_owned(),
            max_calls_per_message: 3,
            top_k: 5,
            max_chunk_chars: 2000,
        },
        knowledge_provider_id: None,
        summary_enabled: false,
        summary_threshold_pct: 80,
    }
}

/// Everything of a unit test of the pipeline.
pub struct World {
    pub service: Arc<StreamService>,
    pub turns: Arc<FakeTurns>,
    pub provider: Arc<FakeProvider>,
    pub metrics: Arc<RecordingStreamMetrics>,
    pub quota: Arc<QuotaService>,
    pub quota_metrics: Arc<RecordingMetrics>,
    pub quota_repo: Arc<FakeRepo>,
    pub policy: Arc<FakePolicy>,
    pub pdp: Arc<FakePdp>,
    pub chats: Arc<FakeChats>,
    pub directory: Arc<FakeDirectory>,
}

impl World {
    /// The DESIGN catalog (`p` premium, `s` standard), no bucket usage, a chat
    /// on `p`, a fixed clock at [`noon`].
    pub fn new() -> Self {
        Self::build(Some(chat("p")), PdpReply::AllowScoped, settings())
    }

    pub fn build(chat: Option<Chat>, pdp_reply: PdpReply, settings: StreamSettings) -> Self {
        Self::build_with(chat, pdp_reply, settings, None)
    }

    /// [`World::build`] with a knowledge retriever.
    pub fn build_with(
        chat: Option<Chat>,
        pdp_reply: PdpReply,
        settings: StreamSettings,
        retriever: Option<Arc<dyn KnowledgeRetriever>>,
    ) -> Self {
        let turns = FakeTurns::new();
        let provider = FakeProvider::new();
        let metrics = RecordingStreamMetrics::new();
        let policy = FakePolicy::serving(snapshot(7, design_catalog()), design_limits(7));
        let quota_repo = FakeRepo::with_rows(Vec::new(), 0);
        let quota_metrics = RecordingMetrics::new();
        let pdp = FakePdp::new(pdp_reply);
        let authz = Arc::new(AuthzService::with_timeout(
            Arc::clone(&pdp) as Arc<dyn AuthZResolverApi>,
            Duration::from_secs(5),
        ));
        let quota = Arc::new(
            QuotaService::new(QuotaDeps {
                authz: Arc::clone(&authz),
                policy: Arc::clone(&policy) as Arc<dyn PolicyCatalog>,
                repo: Arc::clone(&quota_repo)
                    as Arc<dyn crate::domain::service::quota::ports::QuotaRepository>,
                metrics: Arc::clone(&quota_metrics)
                    as Arc<dyn crate::domain::service::quota::ports::QuotaMetrics>,
                config: default_config(),
            })
            .with_clock(fixed_clock(noon())),
        );
        let chats = FakeChats::with(chat);
        let directory = FakeDirectory::new();
        let clock: Clock = fixed_clock(noon());
        let service = Arc::new(StreamService::new(StreamDeps {
            authz,
            chats: Arc::clone(&chats) as Arc<dyn ChatRepository>,
            policy: Arc::clone(&policy) as Arc<dyn PolicyCatalog>,
            quota: Arc::clone(&quota),
            turns: Arc::clone(&turns) as Arc<dyn TurnRepository>,
            providers: Arc::clone(&directory) as Arc<dyn ProviderDirectory>,
            client: Arc::clone(&provider) as Arc<dyn ProviderClient>,
            retriever,
            metrics: Arc::clone(&metrics) as Arc<dyn StreamMetrics>,
            settings,
            clock,
        }));
        Self {
            service,
            turns,
            provider,
            metrics,
            quota,
            quota_metrics,
            quota_repo,
            policy,
            pdp,
            chats,
            directory,
        }
    }

    /// Replaces the catalog of policy version 7.
    pub fn with_catalog(self, catalog: Vec<mini_chat_sdk::ModelCatalogEntry>) -> Self {
        self.policy
            .snapshots
            .lock()
            .unwrap()
            .insert(7, snapshot(7, catalog));
        self
    }

    /// Replaces the snapshot of policy version 7.
    pub fn with_snapshot(self, snapshot: mini_chat_sdk::PolicySnapshot) -> Self {
        self.policy.snapshots.lock().unwrap().insert(7, snapshot);
        self
    }

    /// Daily and monthly usage rows of DESIGN 5.10 (premium nearly used up).
    pub fn with_design_usage(self) -> Self {
        *self.quota_repo.rows.lock().unwrap() =
            crate::domain::service::quota::ports::PreflightRows {
                prior_context_tokens: 0,
                buckets: design_rows(),
            };
        self
    }
}

/// The security context of the default user.
pub fn ctx() -> SecurityContext {
    crate::test_doubles::user_ctx()
}

// ---- provider task fixtures ---------------------------------------------------------------------

/// The `TurnRun` of a standard turn on model `p` (decision [`premium_decision`]).
pub fn run_premium() -> TurnRun {
    run_with(premium_decision(), "p")
}

/// The `TurnRun` of the downgraded decision [`standard_decision`] (chat on `p`,
/// effective model `s`).
pub fn run_downgraded() -> TurnRun {
    run_with(standard_decision(), "p")
}

pub fn run_with(decision: PreflightDecision, selected: &str) -> TurnRun {
    TurnRun {
        ctx: ctx(),
        chat: chat(selected),
        user_id: USER,
        request_id: REQUEST,
        turn_id: TURN,
        assistant_message_id: ASSISTANT,
        user_message_at: at(0),
        provider_id: decision.provider_id.clone(),
        request: BuiltRequest {
            kind: ProviderKind::OpenaiResponses,
            uri: format!("/{ALIAS}/v1/responses"),
            headers: Vec::new(),
            body: json!({ "model": "provider-p" }),
        },
        tools: ToolsDecision::default(),
        decision,
        accepted_at: tokio::time::Instant::now(),
        summary: SummaryTriggerPlan::disabled(),
    }
}

/// Task collaborators over fresh fakes.
pub struct TaskWorld {
    pub deps: Arc<TaskDeps>,
    pub turns: Arc<FakeTurns>,
    pub provider: Arc<FakeProvider>,
    pub metrics: Arc<RecordingStreamMetrics>,
    pub quota: Arc<QuotaService>,
    pub quota_metrics: Arc<RecordingMetrics>,
    pub quota_repo: Arc<FakeRepo>,
}

impl TaskWorld {
    pub fn new() -> Self {
        Self::with_ping(Duration::from_secs(5))
    }

    pub fn with_ping(ping_interval: Duration) -> Self {
        let world = World::new();
        let deps = Arc::new(TaskDeps {
            turns: Arc::clone(&world.turns) as Arc<dyn TurnRepository>,
            client: Arc::clone(&world.provider) as Arc<dyn ProviderClient>,
            quota: Arc::clone(&world.quota),
            metrics: Arc::clone(&world.metrics) as Arc<dyn StreamMetrics>,
            ping_interval,
            clock: fixed_clock(noon()),
            tool_use: Arc::new(super::task::RejectToolUse),
        });
        Self {
            deps,
            turns: world.turns,
            provider: world.provider,
            metrics: world.metrics,
            quota: world.quota,
            quota_metrics: world.quota_metrics,
            quota_repo: world.quota_repo,
        }
    }

    /// The same collaborators with another tool-use handler.
    pub fn with_tool_use(mut self, handler: Arc<dyn super::task::ToolUseHandler>) -> Self {
        let mut deps = (*self.deps).clone();
        deps.tool_use = handler;
        self.deps = Arc::new(deps);
        self
    }
}

/// A terminal state of the usage event, for assertions on `FinalizeRequest`s.
pub fn terminal(request: &FinalizeRequest) -> TerminalState {
    request.columns.state
}

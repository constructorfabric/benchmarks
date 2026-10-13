#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, dead_code)]
#![allow(clippy::type_complexity)]
//! Fixtures of the turn-mutation unit tests: an in-memory mutation store with
//! a call log and scripts, a recording metrics sink, and the whole mutation
//! service assembled over them, the stream fakes and the quota fakes.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use authz_resolver_sdk::AuthZResolverApi;
use futures::future::BoxFuture;
use time::OffsetDateTime;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::ports::{DeletePlan, MutationMetrics, MutationRepository, ReplacementPlan};
use super::service::{MutationDeps, MutationService};
use super::types::{LatestTurn, MutationOp, MutationResult, TargetMessage, TargetTurn};
use crate::domain::authz::AuthzService;
use crate::domain::error::DomainError;
use crate::domain::model::Chat;
use crate::domain::ports::{ChatRepository, PolicyCatalog};
use crate::domain::service::quota::finalize::{FinalizeRequest, FinalizeResult};
use crate::domain::service::quota::periods::PeriodKeys;
use crate::domain::service::quota::ports::{PreflightRead, PreflightRows, QuotaRepository};
use crate::domain::service::quota::reserve::{ReserveMode, ReserveRequest, ReservedColumns};
use crate::domain::service::quota::service::{Clock, QuotaDeps, QuotaService};
use crate::domain::service::quota::test_support::{
    FakePolicy, FakeRepo, RecordingMetrics, default_config, design_catalog, design_limits,
    fixed_clock, noon, premium_decision, snapshot,
};
use crate::domain::service::quota::types::BucketRow;
use crate::domain::service::stream::pipeline::{StreamDeps, StreamService};
use crate::domain::service::stream::ports::{
    MessageBoundary, ProviderClient, ProviderDirectory, StreamMetrics, TurnRepository,
};
use crate::domain::service::stream::test_support::{
    CHAT, FakeChats, FakeDirectory, FakeProvider, FakeTurns, RecordingStreamMetrics, at, chat,
    settings, turn,
};
use crate::domain::service::stream::types::{TurnRecord, TurnState};
use crate::test_doubles::{FakePdp, PdpReply, USER, USER_TENANT};

/// The live user message of the target turn of the fixtures.
pub const TARGET_USER_MESSAGE: Uuid = Uuid::from_u128(0x5001);
/// Content of the target user message.
pub const ORIGINAL: &str = "the original question";

/// The user message of the fixtures' target turn: `61 s` before noon.
pub fn target_message() -> TargetMessage {
    TargetMessage {
        id: TARGET_USER_MESSAGE,
        content: ORIGINAL.to_owned(),
        created_at: at(-61),
    }
}

/// The target turn of the fixtures in `state`: id [`TURN`], request
/// [`REQUEST`], started 60 s before noon, requester [`USER`].
pub fn target(state: TurnState) -> TargetTurn {
    TargetTurn {
        turn: turn(state),
        web_search_enabled: false,
    }
}

/// The latest-turn row of [`target`].
pub fn latest_of(target: &TargetTurn) -> LatestTurn {
    LatestTurn {
        id: target.turn.id,
        started_at: target.turn.started_at,
    }
}

/// A call of the fake mutation store.
#[derive(Debug, Clone, PartialEq)]
pub enum MutationCall {
    FindTarget(Uuid, Uuid),
    Latest(Uuid),
    UserMessage(Uuid, Uuid),
    Carried(Uuid, Uuid),
    Boundary(Uuid, Uuid),
    Replace(Box<ReplacementPlan>),
    Delete(Box<DeletePlan>),
    Mark {
        tenant: Uuid,
        turn: Uuid,
        code: String,
        at: OffsetDateTime,
    },
}

/// The data and scripts of the fake mutation store.
#[derive(Default)]
pub struct MutationsInner {
    pub targets: Vec<TargetTurn>,
    pub latest: Option<LatestTurn>,
    pub user_message: Option<TargetMessage>,
    pub carried: Vec<Uuid>,
    pub boundary: Option<MessageBoundary>,
    pub replace_results: VecDeque<Result<(), DomainError>>,
    pub delete_results: VecDeque<Result<(), DomainError>>,
    pub mark_results: VecDeque<Result<u64, DomainError>>,
    pub read_error: Option<String>,
    /// `history_boundary` (the first read after the commit) fails.
    pub history_error: Option<String>,
    /// `history_boundary` panics (a bug after the commit).
    pub history_panics: bool,
    pub calls: Vec<MutationCall>,
    pub scopes: Vec<AccessScope>,
    /// `replace_turn` committed (history reads and marks need it).
    pub replaced: bool,
}

/// A hook awaited after the (fake) replacement commit.
pub type AfterReplace = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// In-memory [`MutationRepository`].
pub struct FakeMutations {
    pub inner: Mutex<MutationsInner>,
    /// `replace_turn` awaits this hook after the (fake) commit.
    pub after_replace: Mutex<Option<AfterReplace>>,
}

impl FakeMutations {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(MutationsInner::default()),
            after_replace: Mutex::new(None),
        })
    }

    pub fn with<R>(&self, edit: impl FnOnce(&mut MutationsInner) -> R) -> R {
        edit(&mut self.inner.lock().unwrap())
    }

    pub fn calls(&self) -> Vec<MutationCall> {
        self.with(|inner| inner.calls.clone())
    }

    pub fn count(&self, wanted: impl Fn(&MutationCall) -> bool) -> usize {
        self.with(|inner| inner.calls.iter().filter(|call| wanted(call)).count())
    }

    /// The plan of the only replacement.
    pub fn replacement(&self) -> ReplacementPlan {
        let plans: Vec<ReplacementPlan> = self.with(|inner| {
            inner
                .calls
                .iter()
                .filter_map(|call| match call {
                    MutationCall::Replace(plan) => Some((**plan).clone()),
                    _ => None,
                })
                .collect()
        });
        assert_eq!(plans.len(), 1, "exactly one replacement transaction");
        plans.into_iter().next().unwrap()
    }

    /// The plan of the only delete.
    pub fn deletion(&self) -> DeletePlan {
        let plans: Vec<DeletePlan> = self.with(|inner| {
            inner
                .calls
                .iter()
                .filter_map(|call| match call {
                    MutationCall::Delete(plan) => Some((**plan).clone()),
                    _ => None,
                })
                .collect()
        });
        assert_eq!(plans.len(), 1, "exactly one delete transaction");
        plans.into_iter().next().unwrap()
    }

    /// The `(turn, code)` of every setup-failure mark attempt.
    pub fn marks(&self) -> Vec<(Uuid, String)> {
        self.with(|inner| {
            inner
                .calls
                .iter()
                .filter_map(|call| match call {
                    MutationCall::Mark { turn, code, .. } => Some((*turn, code.clone())),
                    _ => None,
                })
                .collect()
        })
    }

    fn read(&self, call: MutationCall, scope: &AccessScope) -> Result<(), DomainError> {
        self.with(|inner| {
            inner.calls.push(call);
            inner.scopes.push(scope.clone());
            inner
                .read_error
                .clone()
                .map_or(Ok(()), |message| Err(DomainError::Database(message)))
        })
    }
}

#[async_trait]
impl MutationRepository for FakeMutations {
    async fn find_target(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TargetTurn>, DomainError> {
        self.read(MutationCall::FindTarget(chat_id, request_id), scope)?;
        Ok(self.with(|inner| {
            inner
                .targets
                .iter()
                .find(|row| row.turn.chat_id == chat_id && row.turn.request_id == request_id)
                .cloned()
        }))
    }

    async fn latest_turn(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
    ) -> Result<Option<LatestTurn>, DomainError> {
        self.read(MutationCall::Latest(chat_id), scope)?;
        Ok(self.with(|inner| inner.latest))
    }

    async fn user_message(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        request_id: Uuid,
    ) -> Result<Option<TargetMessage>, DomainError> {
        self.read(MutationCall::UserMessage(chat_id, request_id), scope)?;
        Ok(self.with(|inner| inner.user_message.clone()))
    }

    async fn carried_attachment_ids(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        user_message_id: Uuid,
    ) -> Result<Vec<Uuid>, DomainError> {
        self.read(MutationCall::Carried(chat_id, user_message_id), scope)?;
        Ok(self.with(|inner| inner.carried.clone()))
    }

    async fn history_boundary(
        &self,
        scope: &AccessScope,
        chat_id: Uuid,
        new_request_id: Uuid,
    ) -> Result<Option<MessageBoundary>, DomainError> {
        self.read(MutationCall::Boundary(chat_id, new_request_id), scope)?;
        assert!(
            self.with(|inner| inner.replaced),
            "the history is read after the replacement commit"
        );
        assert!(
            !self.with(|inner| inner.history_panics),
            "a bug in the setup after the commit"
        );
        if let Some(message) = self.with(|inner| inner.history_error.clone()) {
            return Err(DomainError::Database(message));
        }
        Ok(self.with(|inner| inner.boundary))
    }

    async fn replace_turn(&self, plan: &ReplacementPlan) -> Result<(), DomainError> {
        self.with(|inner| {
            inner
                .calls
                .push(MutationCall::Replace(Box::new(plan.clone())));
        });
        let scripted = self.with(|inner| inner.replace_results.pop_front());
        let result = scripted.unwrap_or(Ok(()));
        if result.is_ok() {
            self.with(|inner| inner.replaced = true);
            let hook = self.after_replace.lock().unwrap().clone();
            if let Some(hook) = hook {
                hook().await;
            }
        }
        result
    }

    async fn delete_turn(&self, plan: &DeletePlan) -> Result<(), DomainError> {
        self.with(|inner| {
            inner
                .calls
                .push(MutationCall::Delete(Box::new(plan.clone())));
        });
        self.with(|inner| inner.delete_results.pop_front())
            .unwrap_or(Ok(()))
    }

    async fn mark_setup_failed(
        &self,
        tenant_id: Uuid,
        turn_id: Uuid,
        error_code: &str,
        at: OffsetDateTime,
    ) -> Result<u64, DomainError> {
        self.with(|inner| {
            inner.calls.push(MutationCall::Mark {
                tenant: tenant_id,
                turn: turn_id,
                code: error_code.to_owned(),
                at,
            });
        });
        self.with(|inner| inner.mark_results.pop_front())
            .unwrap_or(Ok(1))
    }
}

/// Records every mutation metric observation.
#[derive(Default)]
pub struct RecordingMutationMetrics {
    pub results: Mutex<Vec<(MutationOp, MutationResult)>>,
    pub latencies: Mutex<Vec<(MutationOp, f64)>>,
}

impl RecordingMutationMetrics {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn results(&self) -> Vec<(MutationOp, MutationResult)> {
        self.results.lock().unwrap().clone()
    }

    pub fn latencies(&self) -> Vec<(MutationOp, f64)> {
        self.latencies.lock().unwrap().clone()
    }
}

impl MutationMetrics for RecordingMutationMetrics {
    fn turn_mutation(&self, op: MutationOp, result: MutationResult) {
        self.results.lock().unwrap().push((op, result));
    }

    fn turn_mutation_latency_ms(&self, op: MutationOp, millis: f64) {
        self.latencies.lock().unwrap().push((op, millis));
    }
}

/// What a reserve was asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedReserve {
    pub mode: ReserveMode,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub now: OffsetDateTime,
    pub effective_model: String,
}

/// A quota store over [`FakeRepo`] that records the reserves and answers them
/// from a script (the default answer is the fake's).
pub struct ScriptedQuota {
    pub inner: Arc<FakeRepo>,
    pub reserve_script: Mutex<VecDeque<Result<ReservedColumns, DomainError>>>,
    pub reserves: Mutex<Vec<RecordedReserve>>,
}

impl ScriptedQuota {
    pub fn reserves(&self) -> Vec<RecordedReserve> {
        self.reserves.lock().unwrap().clone()
    }

    pub fn script_reserve(&self, result: Result<ReservedColumns, DomainError>) {
        self.reserve_script.lock().unwrap().push_back(result);
    }
}

#[async_trait]
impl QuotaRepository for ScriptedQuota {
    async fn read_preflight(&self, read: &PreflightRead) -> Result<PreflightRows, DomainError> {
        self.inner.read_preflight(read).await
    }

    async fn read_buckets(
        &self,
        scope: &AccessScope,
        keys: PeriodKeys,
    ) -> Result<Vec<BucketRow>, DomainError> {
        self.inner.read_buckets(scope, keys).await
    }

    async fn reserve(&self, request: &ReserveRequest<'_>) -> Result<ReservedColumns, DomainError> {
        self.reserves.lock().unwrap().push(RecordedReserve {
            mode: request.mode,
            tenant_id: request.tenant_id,
            user_id: request.user_id,
            now: request.now,
            effective_model: request.decision.effective_model.clone(),
        });
        let scripted = self.reserve_script.lock().unwrap().pop_front();
        match scripted {
            Some(result) => result,
            None => self.inner.reserve(request).await,
        }
    }

    async fn finalize_turn(
        &self,
        request: &FinalizeRequest,
    ) -> Result<FinalizeResult, DomainError> {
        self.inner.finalize_turn(request).await
    }
}

/// Everything of a unit test of the mutation service.
pub struct MutationWorld {
    pub service: Arc<MutationService>,
    pub stream: Arc<StreamService>,
    pub mutations: Arc<FakeMutations>,
    pub turns: Arc<FakeTurns>,
    pub provider: Arc<FakeProvider>,
    pub metrics: Arc<RecordingMutationMetrics>,
    pub stream_metrics: Arc<RecordingStreamMetrics>,
    pub quota: Arc<QuotaService>,
    pub quota_metrics: Arc<RecordingMetrics>,
    pub quota_repo: Arc<FakeRepo>,
    pub quota_store: Arc<ScriptedQuota>,
    pub policy: Arc<FakePolicy>,
    pub pdp: Arc<FakePdp>,
    pub chats: Arc<FakeChats>,
    pub directory: Arc<FakeDirectory>,
}

impl MutationWorld {
    /// The DESIGN catalog, a chat on `p`, a completed latest target turn
    /// ([`target`] with [`target_message`]), a reserve that succeeds, a fixed
    /// clock at [`noon`].
    pub fn new() -> Self {
        Self::build(Some(chat("p")), PdpReply::AllowScoped)
    }

    pub fn build(chat: Option<Chat>, pdp_reply: PdpReply) -> Self {
        let provider = FakeProvider::new();
        Self::build_with_client(
            chat,
            pdp_reply,
            Arc::clone(&provider) as Arc<dyn ProviderClient>,
            provider,
        )
    }

    /// [`Self::build`] with another provider client (the fake provider is kept
    /// for the fields only).
    pub fn build_with_client(
        chat: Option<Chat>,
        pdp_reply: PdpReply,
        client: Arc<dyn ProviderClient>,
        provider: Arc<FakeProvider>,
    ) -> Self {
        let turns = FakeTurns::new();
        let stream_metrics = RecordingStreamMetrics::new();
        let policy = FakePolicy::serving(snapshot(7, design_catalog()), design_limits(7));
        let quota_repo = FakeRepo::with_rows(Vec::new(), 0);
        *quota_repo.reserve_result.lock().unwrap() =
            Ok(ReservedColumns::from_decision(&premium_decision()));
        let quota_store = Arc::new(ScriptedQuota {
            inner: Arc::clone(&quota_repo),
            reserve_script: Mutex::new(VecDeque::new()),
            reserves: Mutex::new(Vec::new()),
        });
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
                repo: Arc::clone(&quota_store) as Arc<dyn QuotaRepository>,
                metrics: Arc::clone(&quota_metrics)
                    as Arc<dyn crate::domain::service::quota::ports::QuotaMetrics>,
                config: default_config(),
            })
            .with_clock(fixed_clock(noon())),
        );
        let chats = FakeChats::with(chat);
        let directory = FakeDirectory::new();
        let clock: Clock = fixed_clock(noon());
        let stream = Arc::new(StreamService::new(StreamDeps {
            authz: Arc::clone(&authz),
            chats: Arc::clone(&chats) as Arc<dyn ChatRepository>,
            policy: Arc::clone(&policy) as Arc<dyn PolicyCatalog>,
            quota: Arc::clone(&quota),
            turns: Arc::clone(&turns) as Arc<dyn TurnRepository>,
            providers: Arc::clone(&directory) as Arc<dyn ProviderDirectory>,
            client,
            retriever: None,
            metrics: Arc::clone(&stream_metrics) as Arc<dyn StreamMetrics>,
            settings: settings(),
            clock: Arc::clone(&clock),
        }));
        let mutations = FakeMutations::new();
        let default_target = target(TurnState::Completed);
        mutations.with(|inner| {
            inner.latest = Some(latest_of(&default_target));
            inner.targets.push(default_target);
            inner.user_message = Some(target_message());
        });
        let metrics = RecordingMutationMetrics::new();
        let service = Arc::new(MutationService::new(MutationDeps {
            authz,
            chats: Arc::clone(&chats) as Arc<dyn ChatRepository>,
            policy: Arc::clone(&policy) as Arc<dyn PolicyCatalog>,
            quota: Arc::clone(&quota),
            turns: Arc::clone(&turns) as Arc<dyn TurnRepository>,
            mutations: Arc::clone(&mutations) as Arc<dyn MutationRepository>,
            stream: Arc::clone(&stream),
            metrics: Arc::clone(&metrics) as Arc<dyn MutationMetrics>,
            clock,
        }));
        Self {
            service,
            stream,
            mutations,
            turns,
            provider,
            metrics,
            stream_metrics,
            quota,
            quota_metrics,
            quota_repo,
            quota_store,
            policy,
            pdp,
            chats,
            directory,
        }
    }

    /// Replaces the target turn row (and the latest-turn row with it).
    pub fn set_target(&self, row: TargetTurn) {
        self.mutations.with(|inner| {
            inner.latest = Some(latest_of(&row));
            inner.targets = vec![row];
        });
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
}

/// A terminal turn row of the default chat with its own ids, started
/// `secs_before_noon` seconds before noon.
pub fn other_turn(n: u128, state: TurnState, secs_before_noon: i64) -> TurnRecord {
    TurnRecord {
        id: Uuid::from_u128(0xB100 + n),
        request_id: Uuid::from_u128(0xA100 + n),
        state,
        started_at: at(-secs_before_noon),
        updated_at: at(-secs_before_noon),
        ..turn(state)
    }
}

/// Constants of the fixtures re-exported for the tests.
pub const FIXTURE_CHAT: Uuid = CHAT;
pub const FIXTURE_TENANT: Uuid = USER_TENANT;
pub const FIXTURE_USER: Uuid = USER;

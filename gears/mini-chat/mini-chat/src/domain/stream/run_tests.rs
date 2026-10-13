#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream;
use mini_chat_sdk::{SettlementMethod, TierLimits, TokenUsage, UserLimits};
use parking_lot::Mutex;
use time::OffsetDateTime;
use tokio::sync::mpsc;
use toolkit_security::{AccessScope, SecurityContext};
use uuid::Uuid;

use super::{RunDeps, RunSettings, TurnRun};
use crate::config::QuotaConfig;
use crate::domain::billing::SettlementBasis;
use crate::domain::error::DomainError;
use crate::domain::llm::{
    ApiParams, Completion, LlmCompletion, LlmEvent, LlmProvider, LlmRequest, LlmStream,
    ProviderError, RequestIdentity, RequestType,
};
use crate::domain::metrics::NullMetrics;
use crate::domain::ports::TraceIdSource;
use crate::domain::quota::{QuotaPeriod, QuotaRepository, QuotaService, QuotaSnapshot};
use crate::domain::stream::{
    FinalizeCommand, FinalizeOutcome, Finalizer, ReserveCommand, StoredAssistantMessage,
    StreamEvent, TurnFacts, TurnRepository,
};
use crate::domain::turn::{TurnRecord, TurnState};

/// Records the finalize commands; the first `panics` calls panic instead.
struct Turns {
    panics: Mutex<u32>,
    commands: Mutex<Vec<FinalizeCommand>>,
}

#[async_trait]
impl TurnRepository for Turns {
    async fn find_by_request(
        &self,
        _: &AccessScope,
        _: Uuid,
        _: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError> {
        Ok(None)
    }
    async fn find_running(
        &self,
        _: &AccessScope,
        _: Uuid,
    ) -> Result<Option<TurnRecord>, DomainError> {
        Ok(None)
    }
    async fn assistant_message(
        &self,
        _: &AccessScope,
        _: Uuid,
        _: Uuid,
    ) -> Result<Option<StoredAssistantMessage>, DomainError> {
        Ok(None)
    }
    async fn reserve(&self, _: &AccessScope, _: ReserveCommand) -> Result<(), DomainError> {
        Ok(())
    }
    async fn finalize(
        &self,
        _: &AccessScope,
        command: FinalizeCommand,
    ) -> Result<FinalizeOutcome, DomainError> {
        {
            let mut panics = self.panics.lock();
            if *panics > 0 {
                *panics -= 1;
                panic!("injected finalize panic");
            }
        }
        self.commands.lock().push(command);
        Ok(FinalizeOutcome::Committed)
    }
    async fn touch_progress(
        &self,
        _: &AccessScope,
        _: Uuid,
        _: OffsetDateTime,
    ) -> Result<(), DomainError> {
        Ok(())
    }
    async fn record_tool_completed(
        &self,
        _: &AccessScope,
        _: Uuid,
        _: &str,
    ) -> Result<(), DomainError> {
        Ok(())
    }
}

struct EmptyQuota;

#[async_trait]
impl QuotaRepository for EmptyQuota {
    async fn usage(
        &self,
        _: &AccessScope,
        _: Uuid,
        _: Uuid,
        _: &[QuotaPeriod; 2],
    ) -> Result<QuotaSnapshot, DomainError> {
        Ok(QuotaSnapshot::default())
    }
}

/// A provider that answers with one completed event.
struct Provider;

#[async_trait]
impl LlmProvider for Provider {
    fn provider_id(&self) -> &'static str {
        "test"
    }
    async fn stream(
        &self,
        _: &SecurityContext,
        _: &LlmRequest,
    ) -> Result<LlmStream, ProviderError> {
        let events = vec![Ok(LlmEvent::Completed {
            completion: Completion::Complete,
            response_id: None,
            usage: Some(TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                ..TokenUsage::default()
            }),
            citations: Vec::new(),
        })];
        Ok(Box::pin(stream::iter(events)))
    }
    async fn complete(
        &self,
        _: &SecurityContext,
        _: &LlmRequest,
    ) -> Result<LlmCompletion, ProviderError> {
        Err(ProviderError::provider("not used"))
    }
}

struct NoTrace;

impl TraceIdSource for NoTrace {
    fn current_trace_id(&self) -> Option<String> {
        None
    }
}

fn tier_limits() -> TierLimits {
    TierLimits {
        limit_daily_credits_micro: 1_000_000,
        limit_monthly_credits_micro: 1_000_000,
    }
}

fn run(turns: Arc<Turns>, tx: mpsc::Sender<StreamEvent>) -> TurnRun {
    let metrics = Arc::new(NullMetrics);
    let tenant_id = Uuid::new_v4();
    let user_id = Uuid::new_v4();
    TurnRun {
        deps: RunDeps {
            turns: turns.clone(),
            quota: Arc::new(QuotaService::new(
                Arc::new(EmptyQuota),
                QuotaConfig::default(),
            )),
            finalizer: Arc::new(Finalizer::new(turns, metrics.clone(), 1.10)),
            metrics,
            trace: Arc::new(NoTrace),
            settings: RunSettings {
                web_search_max_calls: 2,
                code_interpreter_max_calls: 10,
                progress_refresh: Duration::from_secs(30),
            },
        },
        ctx: SecurityContext::builder()
            .subject_id(user_id)
            .subject_tenant_id(tenant_id)
            .build()
            .unwrap(),
        scope: AccessScope::for_tenant(tenant_id),
        provider: Arc::new(Provider),
        request: LlmRequest {
            model: "m".to_owned(),
            system_prompt: String::new(),
            input: Vec::new(),
            max_output_tokens: 4096,
            tools: Vec::new(),
            max_tool_calls: None,
            identity: RequestIdentity {
                tenant_id,
                user_id,
                chat_id: Uuid::new_v4(),
            },
            request_type: RequestType::Chat,
            api_params: ApiParams::default(),
            feature: "none".to_owned(),
            secondary_file_ids: HashMap::new(),
        },
        facts: TurnFacts {
            turn_id: Uuid::new_v4(),
            tenant_id,
            user_id,
            chat_id: Uuid::new_v4(),
            request_id: Uuid::new_v4(),
            assistant_message_id: Uuid::new_v4(),
            selected_model: "m".to_owned(),
            effective_model: "m".to_owned(),
            quota_decision: "allow",
            downgrade_from: None,
            downgrade_reason: None,
            policy_version: 1,
            started_at: OffsetDateTime::UNIX_EPOCH,
            basis: SettlementBasis {
                reserve_tokens: 4209,
                max_output_tokens_applied: 4096,
                reserved_credits_micro: 12_401,
                minimal_generation_floor_applied: 50,
                in_mult: 1_000_000,
                out_mult: 3_000_000,
            },
            premium: false,
            thread_summary_tokens: None,
            trace_id: None,
            summary_trigger: None,
        },
        limits: UserLimits {
            user_id: Uuid::nil(),
            policy_version: 1,
            standard: tier_limits(),
            premium: tier_limits(),
        },
        file_map: HashMap::new(),
        tx,
    }
}

#[tokio::test]
async fn a_panic_of_the_conclusion_finalizes_the_turn_failed_with_the_estimated_settlement() {
    let turns = Arc::new(Turns {
        panics: Mutex::new(1),
        commands: Mutex::new(Vec::new()),
    });
    let (tx, mut rx) = mpsc::channel(16);

    run(turns.clone(), tx).run().await;

    let commands = turns.commands.lock();
    assert_eq!(
        commands.len(),
        1,
        "the supervised finalization committed once"
    );
    let command = &commands[0];
    assert_eq!(command.state, TurnState::Failed);
    assert_eq!(command.error_code.as_deref(), Some("provider_error"));
    assert_eq!(command.settlement.method, SettlementMethod::Estimated);
    assert_eq!(command.settlement.committed_credits_micro, 263);
    assert!(command.assistant_message.is_none());
    assert!(command.book_quota);
    assert!(command.orphan_cutoff.is_none());
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    assert!(matches!(events.first(), Some(StreamEvent::Started { .. })));
    assert!(matches!(
        events.last(),
        Some(StreamEvent::Error { code, .. }) if code == "provider_error"
    ));
}

#[tokio::test]
async fn a_conclusion_without_a_panic_finalizes_once_as_completed() {
    let turns = Arc::new(Turns {
        panics: Mutex::new(0),
        commands: Mutex::new(Vec::new()),
    });
    let (tx, mut rx) = mpsc::channel(16);

    run(turns.clone(), tx).run().await;

    let commands = turns.commands.lock();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].state, TurnState::Completed);
    let mut last = None;
    while let Ok(event) = rx.try_recv() {
        last = Some(event);
    }
    assert!(matches!(last, Some(StreamEvent::Done(_))));
}

#[tokio::test]
async fn when_the_supervised_finalization_panics_too_the_turn_is_left_for_the_watchdog() {
    let turns = Arc::new(Turns {
        panics: Mutex::new(2),
        commands: Mutex::new(Vec::new()),
    });
    let (tx, _rx) = mpsc::channel(16);

    run(turns.clone(), tx).run().await;

    assert!(turns.commands.lock().is_empty());
}

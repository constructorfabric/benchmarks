#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

//! Thread-summary handler tests (DESIGN section 3.6 "Thread Summary Update",
//! "Asynchronous execution"): payload decoding, the half-set base frontier
//! rejection and the 1-based attempt numbering against
//! `thread_summary_worker.max_attempts`.

use std::sync::atomic::{AtomicUsize, Ordering};

use mini_chat_sdk::{
    MiniChatModelPolicyPluginClientV1, MiniChatModelPolicyPluginError, PolicySnapshot,
    PublishError, UsageEvent, UserLimits,
};
use time::OffsetDateTime;
use time::macros::datetime;
use toolkit_security::AccessScope;
use uuid::Uuid;

use super::*;
use crate::domain::error::DomainError;
use crate::domain::llm::LlmProvider;
use crate::domain::ports::{
    ContextMessageRecord, ThreadSummaryCommit, ThreadSummaryCommitOutcome, ThreadSummaryRecord,
    ThreadSummaryStore,
};
use crate::domain::stream::service::tests::{FakeProvider, FakeResolver};
use crate::domain::summary_service::SummarySettings;
use crate::infra::outbox::SystemTaskType;

const MAX_ATTEMPTS: u32 = 3;
const FRONTIER_AT: OffsetDateTime = datetime!(2026-10-11 12:00 UTC);
const TARGET_AT: OffsetDateTime = datetime!(2026-10-11 12:05 UTC);

/// Policy plugin whose every call fails transiently (the service's first
/// step, so a delivery that reaches the service yields a retryable
/// outcome), counting the calls.
#[derive(Default)]
struct DownPolicy {
    calls: AtomicUsize,
}

impl DownPolicy {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn fail(&self) -> MiniChatModelPolicyPluginError {
        self.calls.fetch_add(1, Ordering::SeqCst);
        MiniChatModelPolicyPluginError::Transient("policy source down".to_owned())
    }
}

#[async_trait]
impl MiniChatModelPolicyPluginClientV1 for DownPolicy {
    async fn get_current_policy_version(
        &self,
        _user_id: Uuid,
    ) -> Result<u64, MiniChatModelPolicyPluginError> {
        Err(self.fail())
    }

    async fn get_policy_snapshot(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<PolicySnapshot, MiniChatModelPolicyPluginError> {
        Err(self.fail())
    }

    async fn get_user_limits(
        &self,
        _user_id: Uuid,
        _policy_version: u64,
    ) -> Result<UserLimits, MiniChatModelPolicyPluginError> {
        Err(self.fail())
    }

    async fn publish_usage(&self, _payload: UsageEvent) -> Result<(), PublishError> {
        Ok(())
    }
}

/// Store that is never expected to be reached; every call fails.
struct UnreachableStore;

#[async_trait]
impl ThreadSummaryStore for UnreachableStore {
    async fn current_summary(
        &self,
        _scope: &AccessScope,
        _chat_id: Uuid,
    ) -> Result<Option<ThreadSummaryRecord>, DomainError> {
        Err(DomainError::internal("store not expected"))
    }

    async fn summary_range(
        &self,
        _scope: &AccessScope,
        _chat_id: Uuid,
        _base: Option<MessageKey>,
        _target: MessageKey,
    ) -> Result<Vec<ContextMessageRecord>, DomainError> {
        Err(DomainError::internal("store not expected"))
    }

    async fn commit_summary(
        &self,
        _scope: &AccessScope,
        _commit: ThreadSummaryCommit,
    ) -> Result<ThreadSummaryCommitOutcome, DomainError> {
        Err(DomainError::internal("store not expected"))
    }
}

struct Fx {
    policy: Arc<DownPolicy>,
    handler: ThreadSummaryHandler,
}

impl Fx {
    fn new() -> Self {
        let policy = Arc::new(DownPolicy::default());
        let service = Arc::new(ThreadSummaryService::new(
            Arc::new(UnreachableStore),
            Arc::clone(&policy) as Arc<dyn MiniChatModelPolicyPluginClientV1>,
            Arc::new(FakeResolver),
            Arc::new(FakeProvider::default()) as Arc<dyn LlmProvider>,
            SummarySettings {
                model_id: "standard".to_owned(),
                system_prompt: "Summarize.".to_owned(),
                message_content_limit: 0,
                max_attempts: MAX_ATTEMPTS,
            },
        ));
        Self {
            policy,
            handler: ThreadSummaryHandler::new(service, "mini_chat_test"),
        }
    }
}

fn payload() -> ThreadSummaryPayload {
    ThreadSummaryPayload {
        tenant_id: Uuid::new_v4(),
        chat_id: Uuid::new_v4(),
        system_request_id: Uuid::new_v4(),
        system_task_type: SystemTaskType::ThreadSummaryUpdate,
        base_frontier_created_at: Some(FRONTIER_AT),
        base_frontier_message_id: Some(Uuid::new_v4()),
        frozen_target_created_at: TARGET_AT,
        frozen_target_message_id: Uuid::new_v4(),
    }
}

fn message(payload: Vec<u8>, attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 0,
        seq: 1,
        payload,
        payload_type: "application/json".to_owned(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

fn encoded(payload: &ThreadSummaryPayload, attempts: i16) -> OutboxMessage {
    message(serde_json::to_vec(payload).unwrap(), attempts)
}

fn reject_reason(result: MessageResult) -> String {
    match result {
        MessageResult::Reject(reason) => reason,
        other => panic!("expected Reject, got {other:?}"),
    }
}

// ── task_of ──────────────────────────────────────────────────────────────

#[test]
fn task_of_maps_a_set_or_null_base_frontier() {
    let full = payload();
    let task = task_of(&full).unwrap();
    assert_eq!(
        task.base_frontier,
        Some(MessageKey {
            created_at: FRONTIER_AT,
            id: full.base_frontier_message_id.unwrap(),
        })
    );
    assert_eq!(
        task.frozen_target,
        MessageKey {
            created_at: TARGET_AT,
            id: full.frozen_target_message_id,
        }
    );
    assert_eq!(
        (task.tenant_id, task.chat_id, task.system_request_id),
        (full.tenant_id, full.chat_id, full.system_request_id)
    );

    let first = ThreadSummaryPayload {
        base_frontier_created_at: None,
        base_frontier_message_id: None,
        ..payload()
    };
    assert_eq!(task_of(&first).unwrap().base_frontier, None);
}

#[test]
fn task_of_rejects_a_half_set_base_frontier() {
    for half in [
        ThreadSummaryPayload {
            base_frontier_created_at: None,
            ..payload()
        },
        ThreadSummaryPayload {
            base_frontier_message_id: None,
            ..payload()
        },
    ] {
        assert_eq!(
            task_of(&half).unwrap_err(),
            "base frontier fields must be both set or both null",
            "{half:?}"
        );
    }
}

// ── Delivery ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn half_set_base_frontier_is_rejected_without_calling_the_service() {
    let fx = Fx::new();
    for half in [
        ThreadSummaryPayload {
            base_frontier_created_at: None,
            ..payload()
        },
        ThreadSummaryPayload {
            base_frontier_message_id: None,
            ..payload()
        },
    ] {
        let reason = reject_reason(fx.handler.handle(&encoded(&half, 0)).await);
        assert!(
            reason.starts_with("malformed thread summary payload")
                && reason.contains("base frontier fields must be both set or both null"),
            "{reason}"
        );
    }
    assert_eq!(fx.policy.calls(), 0, "the service never ran");
}

#[tokio::test]
async fn malformed_json_is_rejected_without_calling_the_service() {
    let fx = Fx::new();
    for body in [b"not json".to_vec(), b"{}".to_vec()] {
        let reason = reject_reason(fx.handler.handle(&message(body, 0)).await);
        assert!(
            reason.starts_with("malformed thread summary payload"),
            "{reason}"
        );
    }
    assert_eq!(fx.policy.calls(), 0);
}

#[tokio::test]
async fn retryable_outcome_retries_before_the_last_delivery() {
    let fx = Fx::new();
    // Outbox `attempts` counts retries from 0: deliveries 1 and 2 of 3.
    for attempts in [0, 1] {
        let result = fx.handler.handle(&encoded(&payload(), attempts)).await;
        assert!(matches!(result, MessageResult::Retry), "{result:?}");
    }
    assert!(fx.policy.calls() >= 2, "the service ran on every delivery");
}

#[tokio::test]
async fn retryable_outcome_is_rejected_on_the_last_delivery() {
    let fx = Fx::new();
    let last = i16::try_from(MAX_ATTEMPTS - 1).unwrap();

    let reason = reject_reason(fx.handler.handle(&encoded(&payload(), last)).await);

    assert!(reason.contains("policy version read failed"), "{reason}");
    assert!(
        reason.ends_with(&format!("(attempt {MAX_ATTEMPTS} of {MAX_ATTEMPTS})")),
        "{reason}"
    );
    assert_eq!(fx.policy.calls(), 1);
}

#[tokio::test]
async fn process_uses_the_given_attempt_number() {
    let fx = Fx::new();
    let task = task_of(&payload()).unwrap();

    let result = fx.handler.process(&task, MAX_ATTEMPTS - 1).await;
    assert!(matches!(result, MessageResult::Retry), "{result:?}");
    let reason = reject_reason(fx.handler.process(&task, MAX_ATTEMPTS).await);
    assert!(reason.contains(&format!("(attempt {MAX_ATTEMPTS} of {MAX_ATTEMPTS})")));
}

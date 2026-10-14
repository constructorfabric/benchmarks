//! The mutation audit enqueue of the transactional enqueuer: the audit queue, the payload type,
//! the JSON payload of the three mutation events, the partition by the `tenant_id` of the chat
//! (not by the chat, as the streaming audit enqueue does for its own events), the commit or
//! rollback with the caller's transaction and the unbound outbox.
//!
//! Slice S1-all of `cpt-cf-mini-chat-feature-turn-mutations-and-reactions`: algorithm
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-audit-event` (`inst-ae-match`,
//! `inst-ae-retry`, `inst-ae-edit`, `inst-ae-delete`, `inst-ae-enqueue`, `inst-ae-exactly-one`),
//! `cpt-cf-mini-chat-algo-turn-mutations-and-reactions-mutation-commit` (`inst-mc-audit`) and
//! flow `cpt-cf-mini-chat-flow-turn-mutations-and-reactions-admin-observability`
//! (`inst-obs-audit-health`: the delivery of a committed mutation event and the `dropped` count
//! without a plugin); definition of done
//! `cpt-cf-mini-chat-dod-turn-mutations-and-reactions-audit-events`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use mini_chat_sdk::{AuditEvent, TurnDeleteAuditEvent, TurnMutationAuditEvent};
use uuid::Uuid;

use crate::config::OutboxConfig;
use crate::domain::error::DomainError;
use crate::domain::streaming::TurnOutbox;
use crate::infra::audit_gateway::AUDIT_PAYLOAD_TYPE;
use crate::infra::outbox::{OutboxEnqueuer, partition_for, queue_specs};
use crate::test_support::chats::{outbox_body_count, outbox_rows};
use crate::test_support::quota::{QuotaRig, quota_rig};
use crate::test_support::send::t;
use crate::test_support::{TENANT_A, TENANT_B, USER_A1};

fn retry_event(chat_id: Uuid) -> AuditEvent {
    AuditEvent::TurnRetry(TurnMutationAuditEvent {
        actor_user_id: USER_A1,
        chat_id,
        original_request_id: Uuid::from_u128(0x7A),
        new_request_id: Uuid::from_u128(0x4E),
        timestamp: t(10),
    })
}

fn edit_event(chat_id: Uuid) -> AuditEvent {
    match retry_event(chat_id) {
        AuditEvent::TurnRetry(event) => AuditEvent::TurnEdit(event),
        other => other,
    }
}

fn delete_event(chat_id: Uuid) -> AuditEvent {
    AuditEvent::TurnDelete(TurnDeleteAuditEvent {
        actor_user_id: USER_A1,
        chat_id,
        request_id: Uuid::from_u128(0x7A),
        timestamp: t(10),
    })
}

async fn enqueue(rig: &QuotaRig, tenant: Uuid, event: AuditEvent) -> Result<(), DomainError> {
    let enqueuer = Arc::clone(&rig.enqueuer);
    let wake = rig
        .db
        .provider
        .transaction(move |tx| {
            let enqueuer = Arc::clone(&enqueuer);
            Box::pin(async move { enqueuer.enqueue_mutation_audit(tx, tenant, &event).await })
        })
        .await?;
    wake.fire();
    Ok(())
}

/// A chat id whose partition by chat differs from the partition by tenant.
fn chat_off_the_tenant_partition(partitions: u32) -> Uuid {
    (1_u128..1_000)
        .map(Uuid::from_u128)
        .find(|id| partition_for(*id, partitions) != partition_for(TENANT_A, partitions))
        .unwrap()
}

#[tokio::test]
async fn each_mutation_event_lands_on_the_audit_queue_as_its_json() {
    // inst-ae-retry, inst-ae-edit, inst-ae-delete
    let rig = quota_rig().await;
    let chat_id = Uuid::from_u128(0xC4A7);
    for event in [
        retry_event(chat_id),
        edit_event(chat_id),
        delete_event(chat_id),
    ] {
        enqueue(&rig, TENANT_A, event.clone()).await.unwrap();
        let rows = outbox_rows(&rig.db).await;
        let row = rows.last().unwrap();
        assert_eq!(row.queue, rig.outbox.audit_queue_name);
        assert_eq!(row.payload_type, AUDIT_PAYLOAD_TYPE);
        let back: AuditEvent = serde_json::from_value(row.payload.clone()).unwrap();
        assert_eq!(back, event);
    }
    assert_eq!(outbox_rows(&rig.db).await.len(), 3);
}

#[tokio::test]
async fn the_mutation_audit_queue_is_partitioned_by_the_tenant_of_the_chat() {
    // inst-ae-enqueue: the FEATURE partitions by the chat's `tenant_id`
    let rig = quota_rig().await;
    let partitions = rig.outbox.num_partitions;
    let chat_id = chat_off_the_tenant_partition(partitions);
    assert_ne!(
        partition_for(chat_id, partitions),
        partition_for(TENANT_A, partitions)
    );
    for event in [
        retry_event(chat_id),
        edit_event(chat_id),
        delete_event(chat_id),
    ] {
        enqueue(&rig, TENANT_A, event).await.unwrap();
    }
    for row in outbox_rows(&rig.db).await {
        assert_eq!(
            row.partition,
            i64::from(partition_for(TENANT_A, partitions)),
            "partitioned by the tenant, not by the chat"
        );
    }
}

#[tokio::test]
async fn events_of_one_tenant_share_a_partition_whatever_their_chat() {
    let rig = quota_rig().await;
    let partitions = rig.outbox.num_partitions;
    for chat in 1_u128..=8 {
        enqueue(&rig, TENANT_B, retry_event(Uuid::from_u128(chat)))
            .await
            .unwrap();
    }
    let rows = outbox_rows(&rig.db).await;
    assert_eq!(rows.len(), 8);
    let expected = i64::from(partition_for(TENANT_B, partitions));
    assert!(rows.iter().all(|row| row.partition == expected), "{rows:?}");
}

#[tokio::test]
async fn a_mutation_audit_enqueue_rolls_back_with_the_caller_transaction() {
    // inst-mc-audit: the enqueue commits or rolls back with the mutation transaction
    let rig = quota_rig().await;
    let enqueuer = Arc::clone(&rig.enqueuer);
    let outcome: Result<(), DomainError> = rig
        .db
        .provider
        .transaction(move |tx| {
            let enqueuer = Arc::clone(&enqueuer);
            Box::pin(async move {
                let wake = enqueuer
                    .enqueue_mutation_audit(tx, TENANT_A, &retry_event(Uuid::from_u128(1)))
                    .await?;
                drop(wake);
                Err(DomainError::internal(
                    "the mutation failed after the enqueue",
                ))
            })
        })
        .await;
    assert!(outcome.is_err());
    assert_eq!(outbox_body_count(&rig.db).await, 0);
}

#[tokio::test]
async fn an_unbound_outbox_fails_the_mutation_audit_enqueue_internally() {
    let rig = quota_rig().await;
    let unbound = Arc::new(OutboxEnqueuer::new(queue_specs(
        &OutboxConfig::default(),
        300,
    )));
    let outcome = rig
        .db
        .provider
        .transaction(move |tx| {
            let unbound = Arc::clone(&unbound);
            Box::pin(async move {
                unbound
                    .enqueue_mutation_audit(tx, TENANT_A, &retry_event(Uuid::from_u128(1)))
                    .await
            })
        })
        .await;
    assert!(
        matches!(outcome, Err(DomainError::Internal { .. })),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn the_payload_carries_only_the_documented_fields() {
    // inst-ae-no-content
    let rig = quota_rig().await;
    enqueue(&rig, TENANT_A, retry_event(Uuid::from_u128(5)))
        .await
        .unwrap();
    let payload = outbox_rows(&rig.db).await.remove(0).payload;
    let keys: std::collections::BTreeSet<&str> = payload
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        std::collections::BTreeSet::from([
            "event_type",
            "actor_user_id",
            "chat_id",
            "original_request_id",
            "new_request_id",
            "timestamp"
        ])
    );
}

// ── the delivery of a committed mutation event ───────────────────────────────

mod delivery {
    use std::sync::Arc;

    use mini_chat_sdk::MiniChatAuditPluginClientV1;
    use toolkit::client_hub::{ClientHub, ClientScope};
    use toolkit_db::outbox::{LeasedMessageHandler, MessageResult};
    use types_registry_sdk::TypesRegistryClient;
    use types_registry_sdk::testing::MockTypesRegistryClient;

    use crate::infra::audit_gateway::AuditGateway;
    use crate::test_support::RecordingRegistry;
    use crate::test_support::mutations::{seed, turn};
    use crate::test_support::s2::{
        AuditScript, ScriptedAuditPlugin, audit_instance, audit_message, metrics_harness,
    };
    use crate::test_support::send::send_rig;

    const VENDOR: &str = "constructorfabric";

    /// A committed delete whose audit message is read back from the outbox as the handler gets it.
    async fn committed_message() -> (Vec<u8>, mini_chat_sdk::AuditEvent) {
        let rig = send_rig().await;
        let chat = rig.std_chat().await;
        let target = seed(&rig.quota.db, &chat, turn("q")).await;
        rig.mutations
            .delete(
                &crate::test_support::chats::a1(),
                chat.id,
                target.request_id,
            )
            .await
            .unwrap();
        let payload = rig.audit_payloads().await.remove(0);
        let event = rig.audit_events().await.remove(0);
        (serde_json::to_vec(&payload).unwrap(), event)
    }

    #[tokio::test]
    async fn a_committed_mutation_event_is_delivered_unchanged_and_counted_ok() {
        // inst-obs-audit-health
        let (bytes, event) = committed_message().await;
        let registry = RecordingRegistry::new(MockTypesRegistryClient::new());
        let hub = Arc::new(ClientHub::new());
        let dynamic: Arc<dyn TypesRegistryClient> = registry.clone();
        hub.register::<dyn TypesRegistryClient>(dynamic);
        let (id, instance) = audit_instance("acme.core._.audit_a.v1", VENDOR, 100);
        registry.add_instance(instance);
        let plugin = ScriptedAuditPlugin::new(AuditScript::Accept);
        let client: Arc<dyn MiniChatAuditPluginClientV1> = plugin.clone();
        hub.register_scoped::<dyn MiniChatAuditPluginClientV1>(ClientScope::gts_id(&id), client);
        let metrics = metrics_harness("mini_chat");
        let gateway = AuditGateway::new(hub, VENDOR.to_owned(), Arc::clone(&metrics.metrics));
        let result = gateway.handle(&audit_message(bytes, 0)).await;
        assert!(matches!(result, MessageResult::Ok), "{result:?}");
        assert_eq!(plugin.received(), vec![event]);
        assert_eq!(metrics.audit_emit("ok"), 1);
    }

    #[tokio::test]
    async fn without_a_plugin_a_committed_mutation_event_is_counted_dropped_and_nothing_blocks() {
        // inst-obs-audit-health, flow error scenario: no plugin registered for audit
        let (bytes, _) = committed_message().await;
        let registry = RecordingRegistry::new(MockTypesRegistryClient::new());
        let hub = Arc::new(ClientHub::new());
        let dynamic: Arc<dyn TypesRegistryClient> = registry.clone();
        hub.register::<dyn TypesRegistryClient>(dynamic);
        let metrics = metrics_harness("mini_chat");
        let gateway = AuditGateway::new(hub, VENDOR.to_owned(), Arc::clone(&metrics.metrics));
        let result = gateway.handle(&audit_message(bytes, 0)).await;
        assert!(matches!(result, MessageResult::Ok), "{result:?}");
        assert_eq!(metrics.audit_emit("dropped"), 1);
    }
}

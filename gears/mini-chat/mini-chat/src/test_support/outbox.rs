//! Recording wrapper for outbox handlers: every delivered payload is stored per queue before the
//! wrapped handler runs, so tests can assert on what a queue received (in delivery order) no
//! matter what the handler does with it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::Value;
use toolkit_db::outbox::{LeasedMessageHandler, MessageResult, OutboxMessage};

/// Decoded payloads per queue name, in delivery order.
#[derive(Default)]
pub struct RecordedPayloads(Mutex<HashMap<String, Vec<Value>>>);

impl RecordedPayloads {
    /// Payloads delivered to `queue` so far (a redelivery appears again). A payload that is not
    /// JSON is recorded as a JSON string.
    pub fn payloads(&self, queue: &str) -> Vec<Value> {
        self.0
            .lock()
            .expect("recorded payloads lock")
            .get(queue)
            .cloned()
            .unwrap_or_default()
    }

    fn record(&self, queue: &str, msg: &OutboxMessage) {
        let value = serde_json::from_slice(&msg.payload)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&msg.payload).into_owned()));
        self.0
            .lock()
            .expect("recorded payloads lock")
            .entry(queue.to_owned())
            .or_default()
            .push(value);
    }
}

/// Records each message for `queue`, then delegates to `inner`.
pub struct RecordingHandler {
    queue: String,
    inner: Arc<dyn LeasedMessageHandler>,
    recorded: Arc<RecordedPayloads>,
}

impl RecordingHandler {
    pub fn new(
        queue: &str,
        inner: Arc<dyn LeasedMessageHandler>,
        recorded: Arc<RecordedPayloads>,
    ) -> Self {
        Self {
            queue: queue.to_owned(),
            inner,
            recorded,
        }
    }
}

#[async_trait]
impl LeasedMessageHandler for RecordingHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        self.recorded.record(&self.queue, msg);
        self.inner.handle(msg).await
    }
}

/// An outbox message carrying `payload` as JSON; `attempts` is the number of earlier deliveries
/// (`0` on the first one).
pub fn outbox_message<T: serde::Serialize>(
    payload_type: &str,
    payload: &T,
    attempts: i16,
) -> OutboxMessage {
    let bytes = serde_json::to_vec(payload).expect("serialize payload");
    raw_outbox_message(payload_type, &bytes, attempts)
}

/// An outbox message carrying `payload` as is.
pub fn raw_outbox_message(payload_type: &str, payload: &[u8], attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 1,
        seq: 1,
        payload: payload.to_vec(),
        payload_type: payload_type.to_owned(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::outbox::LoggingAckHandler;

    fn message(payload: &[u8]) -> OutboxMessage {
        OutboxMessage {
            partition_id: 1,
            seq: 1,
            payload: payload.to_vec(),
            payload_type: "t.v1".to_owned(),
            created_at: chrono::Utc::now(),
            attempts: 0,
        }
    }

    struct Rejecting;

    #[async_trait]
    impl LeasedMessageHandler for Rejecting {
        async fn handle(&self, _msg: &OutboxMessage) -> MessageResult {
            MessageResult::Reject("no".to_owned())
        }
    }

    #[tokio::test]
    async fn records_in_order_then_delegates_the_result() {
        let recorded = Arc::new(RecordedPayloads::default());
        let ack = RecordingHandler::new("q1", Arc::new(LoggingAckHandler), Arc::clone(&recorded));
        let reject = RecordingHandler::new("q2", Arc::new(Rejecting), Arc::clone(&recorded));

        assert!(matches!(
            ack.handle(&message(br#"{"n":1}"#)).await,
            MessageResult::Ok
        ));
        assert!(matches!(
            ack.handle(&message(b"plain")).await,
            MessageResult::Ok
        ));
        assert!(matches!(
            reject.handle(&message(br#"{"n":2}"#)).await,
            MessageResult::Reject(r) if r == "no"
        ));

        assert_eq!(
            recorded.payloads("q1"),
            vec![
                serde_json::json!({"n": 1}),
                Value::String("plain".to_owned())
            ]
        );
        assert_eq!(recorded.payloads("q2"), vec![serde_json::json!({"n": 2})]);
        assert!(recorded.payloads("other").is_empty());
    }
}

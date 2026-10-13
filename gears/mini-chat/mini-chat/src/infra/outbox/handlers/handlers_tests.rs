use toolkit_db::outbox::{MessageResult, OutboxMessage};

use super::*;

fn msg(attempts: i16) -> OutboxMessage {
    OutboxMessage {
        partition_id: 1,
        seq: 1,
        payload: Vec::new(),
        payload_type: "application/json".to_owned(),
        created_at: chrono::Utc::now(),
        attempts,
    }
}

#[test]
fn delivery_attempt_is_one_based() {
    assert_eq!(delivery_attempt(&msg(0)), 1);
    assert_eq!(delivery_attempt(&msg(119)), 120);
    assert_eq!(delivery_attempt(&msg(-1)), 1);
}

#[test]
fn outcome_maps_to_message_result() {
    assert!(matches!(
        MessageResult::from(HandlerOutcome::Ok),
        MessageResult::Ok
    ));
    assert!(matches!(
        MessageResult::from(HandlerOutcome::Retry),
        MessageResult::Retry
    ));
    assert!(matches!(
        MessageResult::from(HandlerOutcome::Reject("why".to_owned())),
        MessageResult::Reject(r) if r == "why"
    ));
}

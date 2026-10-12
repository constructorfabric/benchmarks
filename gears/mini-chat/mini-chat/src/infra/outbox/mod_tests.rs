#![allow(clippy::unwrap_used, clippy::expect_used)]

use toolkit_db::outbox::OutboxError;
use uuid::Uuid;

use super::{map_outbox_error, partition_of};
use crate::domain::error::DomainError;

#[test]
fn a_chat_always_maps_to_the_same_partition_inside_the_configured_range() {
    for partitions in [1_u32, 2, 4, 8, 64] {
        for _ in 0..64 {
            let chat = Uuid::new_v4();

            let partition = partition_of(chat, partitions);

            assert!(partition < partitions);
            assert_eq!(partition, partition_of(chat, partitions));
        }
    }
}

#[test]
fn chats_spread_over_the_partitions() {
    let used: std::collections::HashSet<u32> =
        (0..256).map(|_| partition_of(Uuid::new_v4(), 4)).collect();

    assert_eq!(used.len(), 4);
}

#[test]
fn an_oversized_payload_is_reported_as_such_and_other_failures_are_internal() {
    let too_large = map_outbox_error(OutboxError::PayloadTooLarge {
        size: 70_000,
        max: 65_536,
    });
    let unregistered = map_outbox_error(OutboxError::QueueNotRegistered("q".to_owned()));

    assert!(matches!(
        too_large,
        DomainError::OutboxPayloadTooLarge { ref detail } if detail.contains("70000") && detail.contains("65536")
    ));
    assert!(matches!(unregistered, DomainError::Internal { .. }));
}

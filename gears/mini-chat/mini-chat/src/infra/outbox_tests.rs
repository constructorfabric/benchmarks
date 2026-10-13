//! Unit tests of the chat-cleanup partitioning.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn partition_is_stable_and_within_range() {
    for partitions in [1_u16, 4, 64] {
        for n in 0..200_u128 {
            let chat = Uuid::from_u128(n.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let partition = partition_for(chat, partitions);
            assert!(partition < u32::from(partitions));
            assert_eq!(partition, partition_for(chat, partitions));
        }
    }
}

#[test]
fn consecutive_chats_spread_over_the_partitions() {
    let seen: std::collections::BTreeSet<u32> = (0..8_u128)
        .map(|n| partition_for(Uuid::from_u128(n), 4))
        .collect();
    assert_eq!(seen.len(), 4);
}

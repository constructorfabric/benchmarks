use uuid::Uuid;

use super::partition_for;

#[test]
fn partition_is_stable_and_in_range() {
    let key = Uuid::parse_str("6f1c1f7e-0d5b-4a43-9a3e-2f0d6c1b9a10").unwrap();
    // Stable across calls and processes (pure function of the UUID bytes).
    assert_eq!(partition_for(key, 4), partition_for(key, 4));
    for n in [1_u32, 2, 4, 8, 16, 32, 64] {
        for _ in 0..200 {
            assert!(partition_for(Uuid::new_v4(), n) < n);
        }
    }
    assert_eq!(partition_for(key, 1), 0);
}

#[test]
fn partition_is_pinned_for_a_known_key() {
    // Pins the hash (FNV-1a 64 over the 16 UUID bytes): changing it would
    // re-route in-flight keys to other partitions across a deploy.
    let key = Uuid::nil();
    assert_eq!(partition_for(key, 64), (fnv_nil() % 64) as u32);
}

fn fnv_nil() -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for _ in 0..16 {
        h ^= 0;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[test]
fn partition_spreads_keys() {
    let mut seen = [false; 4];
    for _ in 0..500 {
        seen[partition_for(Uuid::new_v4(), 4) as usize] = true;
    }
    assert!(
        seen.iter().all(|s| *s),
        "some partition never chosen: {seen:?}"
    );
}

#[test]
fn zero_partitions_maps_to_zero() {
    assert_eq!(partition_for(Uuid::new_v4(), 0), 0);
}

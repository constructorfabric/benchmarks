//! The byte counter of the streamed provider upload: counting, the per-file limit, and the kept
//! copy of image bytes.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`;
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-provider-file-upload` (steps
//! `inst-pfu-stream`, `inst-pfu-too-large`); definitions of done
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-limits-and-provider-upload` and
//! `cpt-cf-mini-chat-dod-attachments-and-file-search-images`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::{BodyCounter, LimitExceeded};

#[test]
fn every_chunk_is_counted() {
    let mut counter = BodyCounter::new(100, None);
    counter.push(&[0; 10]).unwrap();
    counter.push(&[0; 25]).unwrap();
    counter.push(&[]).unwrap();
    assert_eq!(counter.count(), 35);
}

#[test]
fn a_body_of_exactly_the_limit_passes() {
    let mut counter = BodyCounter::new(10, None);
    counter.push(&[1; 4]).unwrap();
    counter.push(&[1; 6]).unwrap();
    assert_eq!(counter.count(), 10);
}

#[test]
fn one_byte_above_the_limit_fails() {
    let mut counter = BodyCounter::new(10, None);
    counter.push(&[1; 10]).unwrap();
    assert_eq!(counter.push(&[1]), Err(LimitExceeded));
}

#[test]
fn a_single_oversized_chunk_fails_at_once() {
    let mut counter = BodyCounter::new(10, None);
    assert_eq!(counter.push(&[1; 11]), Err(LimitExceeded));
}

#[test]
fn no_copy_is_kept_unless_requested() {
    let mut counter = BodyCounter::new(100, None);
    counter.push(&[7; 10]).unwrap();
    assert_eq!(counter.into_kept(), None);
}

#[test]
fn the_whole_body_is_kept_while_it_stays_within_the_keep_limit() {
    // inst-img-provider-upload: keep the bytes while the count is at or below the decode limit
    let mut counter = BodyCounter::new(100, Some(8));
    counter.push(&[1, 2, 3, 4]).unwrap();
    counter.push(&[5, 6, 7, 8]).unwrap();
    assert_eq!(counter.into_kept(), Some(vec![1, 2, 3, 4, 5, 6, 7, 8]));
}

#[test]
fn the_copy_is_dropped_once_the_count_passes_the_keep_limit() {
    let mut counter = BodyCounter::new(100, Some(8));
    counter.push(&[1; 8]).unwrap();
    counter.push(&[1; 1]).unwrap();
    assert_eq!(counter.count(), 9);
    assert_eq!(counter.into_kept(), None);
}

#[test]
fn a_copy_that_was_dropped_is_never_resumed() {
    let mut counter = BodyCounter::new(100, Some(4));
    counter.push(&[1; 5]).unwrap();
    counter.push(&[]).unwrap();
    assert_eq!(counter.into_kept(), None);
}

#[test]
fn an_empty_body_keeps_an_empty_copy() {
    let counter = BodyCounter::new(100, Some(8));
    assert_eq!(counter.count(), 0);
    assert_eq!(counter.into_kept(), Some(Vec::new()));
}

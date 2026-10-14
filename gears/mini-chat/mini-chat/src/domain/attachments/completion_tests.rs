//! Upload request completion: the permit is released exactly once on every return path (also
//! when the request is dropped), `mini_chat_attachment_upload` is recorded once per request that
//! took a permit with the bounded `kind` label, and a refused request records nothing.
//!
//! Slice S1-upload-lifecycle of `cpt-cf-mini-chat-feature-attachments-and-file-search`;
//! algorithm `cpt-cf-mini-chat-algo-attachments-and-file-search-upload-completion`; definition of
//! done `cpt-cf-mini-chat-dod-attachments-and-file-search-limits-and-provider-upload`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use super::{UploadOutcome, UploadPermits};
use crate::test_support::attach_fakes::RecordingAttachmentMetrics;

const PDF: &str = "application/pdf";
const PNG: &str = "image/png";

fn permits(max: u16) -> (Arc<UploadPermits>, Arc<RecordingAttachmentMetrics>) {
    let metrics = RecordingAttachmentMetrics::new();
    (UploadPermits::new(max, metrics.clone()), metrics)
}

#[test]
fn the_permits_are_sized_by_the_configuration() {
    // inst-intake-permit: rag.max_concurrent_uploads
    let (permits, _) = permits(3);
    assert_eq!(permits.available(), 3);
    let a = permits.try_acquire().expect("first permit");
    let b = permits.try_acquire().expect("second permit");
    let c = permits.try_acquire().expect("third permit");
    assert_eq!(permits.available(), 0);
    drop((a, b, c));
}

#[test]
fn no_permit_is_handed_out_beyond_the_limit_and_nothing_is_recorded() {
    // inst-intake-return-busy, inst-uc-no-permit: no permit held, no metric
    let (permits, metrics) = permits(1);
    let _held = permits.try_acquire().expect("the only permit");
    assert!(permits.try_acquire().is_none());
    assert!(permits.try_acquire().is_none());
    assert!(metrics.events().is_empty());
}

#[test]
fn completing_a_created_upload_releases_the_permit_and_records_ok_once() {
    // inst-uc-release, inst-uc-metric, inst-uc-bytes
    let (permits, metrics) = permits(2);
    let mut request = permits.try_acquire().unwrap();
    assert_eq!(permits.available(), 1);
    request.complete(PDF, UploadOutcome::Created { bytes: 1234 });
    assert_eq!(permits.available(), 2);
    assert_eq!(
        metrics.uploads(),
        vec![("document".to_owned(), "ok".to_owned())]
    );
    assert_eq!(metrics.upload_bytes(), vec![("document".to_owned(), 1234)]);
}

#[test]
fn completing_a_problem_releases_the_permit_and_records_error_without_bytes() {
    let (permits, metrics) = permits(2);
    let mut request = permits.try_acquire().unwrap();
    request.complete(PDF, UploadOutcome::Problem);
    assert_eq!(permits.available(), 2);
    assert_eq!(
        metrics.uploads(),
        vec![("document".to_owned(), "error".to_owned())]
    );
    assert!(metrics.upload_bytes().is_empty());
}

#[test]
fn the_kind_label_is_image_only_for_the_four_image_types() {
    // inst-uc-kind
    for (mime, kind) in [
        ("image/png", "image"),
        ("image/jpeg", "image"),
        ("image/webp", "image"),
        ("image/gif", "image"),
        (PDF, "document"),
        ("text/plain", "document"),
        ("application/octet-stream", "document"),
        ("image/bmp", "document"),
    ] {
        let (permits, metrics) = permits(1);
        let mut request = permits.try_acquire().unwrap();
        request.complete(mime, UploadOutcome::Created { bytes: 1 });
        assert_eq!(
            metrics.uploads(),
            vec![(kind.to_owned(), "ok".to_owned())],
            "{mime}"
        );
        assert_eq!(metrics.upload_bytes()[0].0, kind, "{mime}");
    }
}

#[test]
fn a_failure_before_the_kind_was_classified_counts_as_a_document() {
    // inst-uc-kind: also for a failure before the kind was classified
    let (permits, metrics) = permits(1);
    let mut request = permits.try_acquire().unwrap();
    request.complete("", UploadOutcome::Problem);
    assert_eq!(
        metrics.uploads(),
        vec![("document".to_owned(), "error".to_owned())]
    );
}

#[test]
fn an_image_problem_is_recorded_with_the_image_kind() {
    let (permits, metrics) = permits(1);
    let mut request = permits.try_acquire().unwrap();
    request.complete(PNG, UploadOutcome::Problem);
    assert_eq!(
        metrics.uploads(),
        vec![("image".to_owned(), "error".to_owned())]
    );
}

#[test]
fn a_second_completion_does_nothing() {
    // inst-uc-release: exactly once; a second call for the same request does nothing
    let (permits, metrics) = permits(2);
    let mut first = permits.try_acquire().unwrap();
    first.complete(PDF, UploadOutcome::Created { bytes: 5 });
    let _second = permits.try_acquire().unwrap();
    assert_eq!(permits.available(), 1);
    first.complete(PDF, UploadOutcome::Problem);
    assert_eq!(
        permits.available(),
        1,
        "the second call released a permit again"
    );
    assert_eq!(metrics.uploads().len(), 1);
    assert_eq!(metrics.upload_bytes().len(), 1);
}

#[test]
fn a_dropped_request_releases_its_permit() {
    // inst-uc-release: a client disconnect or a gear stop
    let (permits, _) = permits(1);
    let request = permits.try_acquire().unwrap();
    assert_eq!(permits.available(), 0);
    drop(request);
    assert_eq!(permits.available(), 1);
    assert!(permits.try_acquire().is_some());
}

#[test]
fn a_completed_request_does_not_release_twice_when_it_is_dropped() {
    let (permits, _) = permits(1);
    let mut request = permits.try_acquire().unwrap();
    request.complete(PDF, UploadOutcome::Problem);
    let held = permits.try_acquire().unwrap();
    drop(request);
    assert_eq!(
        permits.available(),
        0,
        "the drop released the permit of another request"
    );
    drop(held);
    assert_eq!(permits.available(), 1);
}

#[test]
fn completion_never_touches_the_pending_gauge() {
    // inst-uc-no-gauge
    let (permits, metrics) = permits(1);
    let mut request = permits.try_acquire().unwrap();
    request.complete(PDF, UploadOutcome::Created { bytes: 1 });
    assert_eq!(metrics.pending_total(), 0);
    assert!(metrics.events().iter().all(|event| !matches!(
        event,
        crate::test_support::attach_fakes::MetricEvent::Pending(_)
    )));
}

#[test]
fn many_sequential_uploads_never_exhaust_the_permits() {
    // more than max_concurrent_uploads sequential requests are never refused
    let (permits, metrics) = permits(2);
    for _ in 0..25 {
        let mut request = permits.try_acquire().expect("a free permit");
        request.complete(PDF, UploadOutcome::Created { bytes: 1 });
    }
    assert_eq!(permits.available(), 2);
    assert_eq!(metrics.uploads().len(), 25);
}

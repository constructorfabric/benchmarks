#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Leader elector tests (DESIGN B.9.1 "Implementations").

use super::*;

#[test]
fn lease_names_are_prefixed_per_role() {
    assert_eq!(
        WorkerRole::OrphanWatchdog.lease_name(),
        "mini-chat-orphan-watchdog"
    );
    assert_eq!(
        WorkerRole::UploadReaper.lease_name(),
        "mini-chat-upload-reaper"
    );
}

#[test]
fn lease_timing_is_hardcoded() {
    assert_eq!(LEASE_DURATION, std::time::Duration::from_secs(15));
    assert_eq!(RENEW_PERIOD, std::time::Duration::from_secs(2));
}

#[test]
fn noop_elector_is_always_the_leader() {
    assert!(NoopElector.is_leader());
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn build_without_k8s_uses_the_noop_elector() {
    let cancel = CancellationToken::new();
    for role in [WorkerRole::OrphanWatchdog, WorkerRole::UploadReaper] {
        let elector = elector_for(role, &cancel).await.unwrap();
        assert!(elector.is_leader());
    }
}

// ── Kubernetes Lease elector ─────────────────────────────────────────────

#[cfg(feature = "k8s")]
mod lease_elector {
    use std::time::Duration;

    use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
    use k8s_openapi::jiff::Timestamp;
    use kube::core::Status;
    use tokio::time::Instant;

    use super::super::lease::{
        LeaseElector, conflict_is_false, expired, lease_seconds, valid_until,
    };
    use super::*;

    const RENEWED_US: i64 = 1_760_000_000_000_000;
    const SECOND_US: i64 = 1_000_000;

    fn micro(us: i64) -> MicroTime {
        MicroTime(Timestamp::from_microsecond(us).unwrap())
    }

    fn spec(renewed_us: Option<i64>, duration_seconds: Option<i32>) -> LeaseSpec {
        LeaseSpec {
            holder_identity: Some("pod-a".to_owned()),
            renew_time: renewed_us.map(micro),
            lease_duration_seconds: duration_seconds,
            ..LeaseSpec::default()
        }
    }

    #[test]
    fn lease_without_renew_time_is_expired() {
        assert!(expired(&spec(None, Some(15)), &micro(RENEWED_US)));
    }

    #[test]
    fn lease_lapses_exactly_at_renew_time_plus_duration() {
        let lease = spec(Some(RENEWED_US), Some(15));
        let lapse = RENEWED_US + 15 * SECOND_US;
        assert!(!expired(&lease, &micro(RENEWED_US)));
        assert!(!expired(&lease, &micro(lapse - 1)));
        assert!(expired(&lease, &micro(lapse)));
        assert!(expired(&lease, &micro(lapse + 1)));
    }

    #[test]
    fn missing_duration_defaults_to_fifteen_seconds_and_custom_ones_apply() {
        let default = spec(Some(RENEWED_US), None);
        assert!(!expired(&default, &micro(RENEWED_US + 15 * SECOND_US - 1)));
        assert!(expired(&default, &micro(RENEWED_US + 15 * SECOND_US)));

        let long = spec(Some(RENEWED_US), Some(30));
        assert!(!expired(&long, &micro(RENEWED_US + 29 * SECOND_US)));
        assert!(expired(&long, &micro(RENEWED_US + 30 * SECOND_US)));
    }

    #[test]
    fn written_lease_duration_is_the_hardcoded_one() {
        assert_eq!(lease_seconds(), 15);
    }

    fn api_error(code: u16, reason: &str) -> kube::Error {
        kube::Error::Api(
            Status::failure("lease write", reason)
                .with_code(code)
                .boxed(),
        )
    }

    #[test]
    fn successful_write_holds_the_lease() {
        assert!(conflict_is_false(Ok(Lease::default())).unwrap());
    }

    #[test]
    fn conflict_and_already_exists_mean_not_the_leader() {
        for err in [api_error(409, "Conflict"), api_error(409, "AlreadyExists")] {
            assert!(!conflict_is_false(Err(err)).unwrap());
        }
    }

    #[test]
    fn other_api_failures_are_errors() {
        for err in [api_error(500, "InternalError"), api_error(403, "Forbidden")] {
            assert!(conflict_is_false(Err(err)).is_err());
        }
    }

    #[test]
    fn validity_window_ends_two_renew_periods_before_the_lease_lapses() {
        let renewed = Instant::now();
        let until = valid_until(renewed);
        assert_eq!(
            until - renewed,
            LEASE_DURATION.saturating_sub(RENEW_PERIOD * 2)
        );
        assert_eq!(until - renewed, Duration::from_secs(11));
        assert!(until - renewed < LEASE_DURATION);
    }

    #[test]
    fn leadership_holds_only_within_the_validity_window() {
        let elector = LeaseElector::new(WorkerRole::OrphanWatchdog, "pod-a".to_owned());
        assert!(!elector.is_leader(), "no renewal yet");

        elector.hold(Instant::now());
        assert!(elector.is_leader());

        // A renewal older than the window no longer counts.
        let stale = Instant::now()
            .checked_sub(Duration::from_secs(11) + Duration::from_millis(1))
            .unwrap();
        elector.hold(stale);
        assert!(!elector.is_leader());

        elector.hold(Instant::now());
        elector.drop_leadership();
        assert!(
            !elector.is_leader(),
            "a lost lease drops leadership at once"
        );
    }
}

//! Leader elector tests. The Kubernetes Lease elector (feature `k8s`) needs a cluster and is
//! out of scope for in-process tests; its tests are compiled out under that feature.
//!
//! Slice S2: algorithm `gear-start`, steps `inst-start-leader-needed` and
//! `inst-start-leader-elector`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::json;

#[cfg(not(feature = "k8s"))]
use tokio_util::sync::CancellationToken;

#[cfg(not(feature = "k8s"))]
use super::prepare_elector;
use super::{LeaderElector, LeaderRole, NoopLeaderElector, enabled_roles, needs_leader_election};
use crate::config::MiniChatConfig;
#[cfg(not(feature = "k8s"))]
use crate::infra::tasks::BackgroundTasks;

fn config_with(orphan_watchdog: bool, upload_reaper: bool) -> MiniChatConfig {
    MiniChatConfig::from_value(json!({
        "client_credentials": { "client_id": "c", "client_secret": "s" },
        "orphan_watchdog": { "enabled": orphan_watchdog },
        "upload_reaper": { "enabled": upload_reaper }
    }))
    .unwrap()
}

#[tokio::test]
async fn the_noop_elector_always_reports_leadership() {
    let elector = NoopLeaderElector;
    for role in [LeaderRole::OrphanWatchdog, LeaderRole::UploadReaper] {
        assert!(elector.is_leader(role).await);
        assert!(elector.is_leader(role).await);
    }
}

#[test]
fn every_role_has_its_own_design_lease() {
    assert_eq!(
        LeaderRole::OrphanWatchdog.lease_name(),
        "mini-chat-orphan-watchdog"
    );
    assert_eq!(
        LeaderRole::UploadReaper.lease_name(),
        "mini-chat-upload-reaper"
    );
}

#[test]
fn only_the_enabled_workers_are_elected() {
    let roles = |watchdog, reaper| enabled_roles(&config_with(watchdog, reaper));
    assert_eq!(
        roles(true, true),
        vec![LeaderRole::OrphanWatchdog, LeaderRole::UploadReaper]
    );
    assert_eq!(roles(true, false), vec![LeaderRole::OrphanWatchdog]);
    assert_eq!(roles(false, true), vec![LeaderRole::UploadReaper]);
    assert!(roles(false, false).is_empty());
}

#[test]
fn leader_election_is_needed_when_either_leader_only_worker_is_enabled() {
    // (orphan_watchdog, upload_reaper, needed)
    let cases = vec![
        (true, true, true),
        (true, false, true),
        (false, true, true),
        (false, false, false),
    ];
    for (watchdog, reaper, needed) in cases {
        assert_eq!(
            needs_leader_election(&config_with(watchdog, reaper)),
            needed,
            "watchdog={watchdog} reaper={reaper}"
        );
    }
}

#[test]
fn the_default_config_needs_leader_election() {
    let config = MiniChatConfig::from_value(json!({
        "client_credentials": { "client_id": "c", "client_secret": "s" }
    }))
    .unwrap();
    assert!(needs_leader_election(&config));
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn without_the_k8s_feature_the_elector_is_a_leader_that_never_loses_leadership() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    let elector = prepare_elector(&config_with(true, true), &tasks)
        .await
        .unwrap()
        .expect("an elector is prepared when a leader-only worker is enabled");
    assert!(elector.is_leader(LeaderRole::OrphanWatchdog).await);
    assert!(elector.is_leader(LeaderRole::UploadReaper).await);
    assert_eq!(tasks.live_count(), 0, "the no-op elector needs no task");
}

#[cfg(not(feature = "k8s"))]
#[tokio::test]
async fn no_elector_is_prepared_when_no_leader_only_worker_is_enabled() {
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    assert!(
        prepare_elector(&config_with(false, false), &tasks)
            .await
            .unwrap()
            .is_none()
    );
}

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Slice 1: the Lease election decisions
//! (`cpt-cf-mini-chat-algo-background-workers-elect-leader`,
//! `cpt-cf-mini-chat-state-background-workers-leadership`) against a fake Lease
//! API and a manual monotonic clock: create, renew, observed-time take-over, the
//! renew deadline, 409 conflicts, release, per-role state and the renew task.
//! No cluster, no network.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;
use tracing::Level;

use super::{
    LEASE_API_TIMEOUT, LEASE_DURATION, LEASE_PREFIX, LEASE_RENEW_DEADLINE, LEASE_RENEW_PERIOD,
    LeaseApi, LeaseApiError, LeaseElection, LeaseRecord, PodIdentity, RoundOutcome,
    start_lease_elector,
};
use crate::domain::service::workers::clock::{SharedWorkerClock, WorkerClock};
use crate::domain::service::workers::test_support::ManualClock;
use crate::infra::background_tasks::BackgroundTasks;
use crate::infra::workers::leader::{ElectorError, LeaderElector, LeaderRole};
use crate::test_doubles::LogCapture;

const WATCHDOG_LEASE: &str = "mini-chat-orphan-watchdog";
const REAPER_LEASE: &str = "mini-chat-upload-reaper";

/// What the fake Lease API was asked to do.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Get(String),
    Create(String),
    Replace(String),
}

/// A fake `coordination.k8s.io/v1` Lease API: optimistic concurrency by
/// `resourceVersion`, switchable failures and a hanging GET.
struct FakeLeaseApi {
    leases: Mutex<HashMap<String, LeaseRecord>>,
    version: Mutex<u64>,
    calls: Mutex<Vec<Call>>,
    fail_all: Mutex<Option<LeaseApiError>>,
    conflict_next_write: AtomicBool,
    hang_get: Mutex<Option<SharedWorkerClock>>,
}

impl FakeLeaseApi {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            leases: Mutex::new(HashMap::new()),
            version: Mutex::new(0),
            calls: Mutex::new(Vec::new()),
            fail_all: Mutex::new(None),
            conflict_next_write: AtomicBool::new(false),
            hang_get: Mutex::new(None),
        })
    }

    fn next_version(&self) -> String {
        let mut version = self.version.lock().unwrap();
        *version += 1;
        version.to_string()
    }

    fn record(&self, name: &str) -> Option<LeaseRecord> {
        self.leases.lock().unwrap().get(name).cloned()
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn count(&self, wanted: impl Fn(&Call) -> bool) -> usize {
        self.calls().iter().filter(|call| wanted(call)).count()
    }

    fn fail_everything(&self, error: Option<LeaseApiError>) {
        *self.fail_all.lock().unwrap() = error;
    }

    /// Another pod writes `holder` into the Lease behind the elections' backs.
    fn force_holder(&self, name: &str, holder: Option<&str>) {
        let version = self.next_version();
        let mut leases = self.leases.lock().unwrap();
        let record = leases.get_mut(name).expect("the lease exists");
        record.holder = holder.map(str::to_owned);
        record.resource_version = version;
    }
}

#[async_trait]
impl LeaseApi for FakeLeaseApi {
    async fn get(&self, name: &str) -> Result<Option<LeaseRecord>, LeaseApiError> {
        self.calls.lock().unwrap().push(Call::Get(name.to_owned()));
        let hang = self.hang_get.lock().unwrap().clone();
        if let Some(clock) = hang {
            clock.sleep(Duration::from_secs(3600)).await;
        }
        if let Some(error) = self.fail_all.lock().unwrap().clone() {
            return Err(error);
        }
        Ok(self.record(name))
    }

    async fn create(&self, name: &str, record: &LeaseRecord) -> Result<LeaseRecord, LeaseApiError> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Create(name.to_owned()));
        if let Some(error) = self.fail_all.lock().unwrap().clone() {
            return Err(error);
        }
        if self.conflict_next_write.swap(false, Ordering::SeqCst) {
            return Err(LeaseApiError::Conflict);
        }
        let mut leases = self.leases.lock().unwrap();
        if leases.contains_key(name) {
            return Err(LeaseApiError::Conflict);
        }
        let stored = LeaseRecord {
            resource_version: {
                drop(leases);
                let version = self.next_version();
                leases = self.leases.lock().unwrap();
                version
            },
            ..record.clone()
        };
        leases.insert(name.to_owned(), stored.clone());
        Ok(stored)
    }

    async fn replace(
        &self,
        name: &str,
        record: &LeaseRecord,
    ) -> Result<LeaseRecord, LeaseApiError> {
        self.calls
            .lock()
            .unwrap()
            .push(Call::Replace(name.to_owned()));
        if let Some(error) = self.fail_all.lock().unwrap().clone() {
            return Err(error);
        }
        if self.conflict_next_write.swap(false, Ordering::SeqCst) {
            return Err(LeaseApiError::Conflict);
        }
        let current = self
            .record(name)
            .ok_or_else(|| LeaseApiError::Failed("gone".into()))?;
        if current.resource_version != record.resource_version {
            return Err(LeaseApiError::Conflict);
        }
        let stored = LeaseRecord {
            resource_version: self.next_version(),
            ..record.clone()
        };
        self.leases
            .lock()
            .unwrap()
            .insert(name.to_owned(), stored.clone());
        Ok(stored)
    }
}

/// One pod: an election of `role` with its own manual clock.
struct Pod {
    clock: Arc<ManualClock>,
    election: Arc<LeaseElection>,
}

fn pod(api: &Arc<FakeLeaseApi>, role: LeaderRole, identity: &str, clock_start: Duration) -> Pod {
    let clock = ManualClock::new();
    clock.advance(clock_start);
    let election = Arc::new(LeaseElection::new(
        role,
        identity,
        Arc::clone(api) as Arc<dyn LeaseApi>,
        Arc::clone(&clock) as Arc<dyn WorkerClock>,
    ));
    Pod { clock, election }
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

// ================================================================================================
// the designed constants and names
// ================================================================================================

#[test]
fn the_timing_constants_are_the_designed_ones() {
    assert_eq!(LEASE_DURATION, secs(15));
    assert_eq!(LEASE_RENEW_PERIOD, secs(2));
    assert_eq!(LEASE_RENEW_DEADLINE, secs(10));
    assert_eq!(LEASE_API_TIMEOUT, secs(5));
    assert_eq!(LEASE_PREFIX, "mini-chat");
}

#[test]
fn the_lease_names_are_prefix_and_role() {
    assert_eq!(LeaderRole::OrphanWatchdog.lease_name(), WATCHDOG_LEASE);
    assert_eq!(LeaderRole::UploadReaper.lease_name(), REAPER_LEASE);
    for role in LeaderRole::ALL {
        assert_eq!(
            role.lease_name(),
            format!("{LEASE_PREFIX}-{}", role.as_str())
        );
        assert_eq!(role.task_name(), format!("lease-{}", role.as_str()));
    }
}

// ================================================================================================
// create, renew, follow
// ================================================================================================

#[tokio::test]
async fn the_first_pod_creates_the_lease_with_15_seconds_and_becomes_leader() {
    let api = FakeLeaseApi::new();
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);
    assert!(!a.election.is_leader(), "a pod starts as a follower");

    let outcome = a.election.round().await;

    assert_eq!(outcome, RoundOutcome::Created);
    assert!(a.election.is_leader());
    let record = api.record(WATCHDOG_LEASE).expect("the lease was created");
    assert_eq!(record.holder.as_deref(), Some("pod-a"));
    assert_eq!(record.lease_duration_secs, 15);
    assert_eq!(record.transitions, 0);
    assert!(record.acquire_time.is_some() && record.renew_time.is_some());
    assert_eq!(
        api.calls(),
        vec![
            Call::Get(WATCHDOG_LEASE.to_owned()),
            Call::Create(WATCHDOG_LEASE.to_owned())
        ]
    );
}

#[tokio::test]
async fn the_holder_renews_under_the_resource_version_it_read() {
    let api = FakeLeaseApi::new();
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);
    a.election.round().await;
    let created = api.record(WATCHDOG_LEASE).unwrap();

    a.clock.advance(secs(2));
    let outcome = a.election.round().await;

    assert_eq!(outcome, RoundOutcome::Renewed);
    let renewed = api.record(WATCHDOG_LEASE).unwrap();
    assert_ne!(renewed.resource_version, created.resource_version);
    assert_eq!(renewed.holder.as_deref(), Some("pod-a"));
    assert_eq!(renewed.transitions, 0);
    assert!(renewed.renew_time >= created.renew_time);
    assert_eq!(renewed.acquire_time, created.acquire_time);
}

#[tokio::test]
async fn a_second_pod_stays_follower_while_the_first_renews_every_two_seconds() {
    let api = FakeLeaseApi::new();
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);
    let b = pod(&api, LeaderRole::OrphanWatchdog, "pod-b", secs(1000));

    for _ in 0..30 {
        a.election.round().await;
        assert_eq!(b.election.round().await, RoundOutcome::Follower);
        a.clock.advance(secs(2));
        b.clock.advance(secs(2));
    }

    assert!(a.election.is_leader());
    assert!(!b.election.is_leader());
    let record = api.record(WATCHDOG_LEASE).unwrap();
    assert_eq!(record.holder.as_deref(), Some("pod-a"));
    assert_eq!(record.transitions, 0, "the holder never changed");
}

// ================================================================================================
// the observed-time take-over rule
// ================================================================================================

#[tokio::test]
async fn a_pod_takes_over_only_after_the_record_stayed_unchanged_for_15_seconds_on_its_own_clock() {
    let api = FakeLeaseApi::new();
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);
    // pod b's monotonic clock has an unrelated origin
    let b = pod(&api, LeaderRole::OrphanWatchdog, "pod-b", secs(7777));
    a.election.round().await; // pod a creates the lease and is never heard of again

    assert_eq!(
        b.election.round().await,
        RoundOutcome::Follower,
        "first seen"
    );
    b.clock.advance(Duration::from_millis(14_999));
    assert_eq!(
        b.election.round().await,
        RoundOutcome::Follower,
        "14.999 s unchanged is not enough"
    );
    b.clock.advance(Duration::from_millis(1));
    let outcome = b.election.round().await;

    assert_eq!(outcome, RoundOutcome::TookOver, "15 s unchanged");
    assert!(b.election.is_leader());
    let record = api.record(WATCHDOG_LEASE).unwrap();
    assert_eq!(record.holder.as_deref(), Some("pod-b"));
    assert_eq!(record.transitions, 1);
    assert_eq!(record.lease_duration_secs, 15);
}

#[tokio::test]
async fn a_record_that_keeps_changing_is_never_taken_over() {
    let api = FakeLeaseApi::new();
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);
    let b = pod(&api, LeaderRole::OrphanWatchdog, "pod-b", Duration::ZERO);

    for _ in 0..40 {
        a.election.round().await;
        a.clock.advance(secs(2));
        assert_eq!(b.election.round().await, RoundOutcome::Follower);
        b.clock.advance(secs(2));
    }

    assert_eq!(
        api.record(WATCHDOG_LEASE).unwrap().holder.as_deref(),
        Some("pod-a"),
        "80 s of renewals never look expired"
    );
}

#[tokio::test]
async fn an_empty_holder_is_taken_over_at_once() {
    let api = FakeLeaseApi::new();
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);
    let b = pod(&api, LeaderRole::OrphanWatchdog, "pod-b", Duration::ZERO);
    a.election.round().await;
    api.force_holder(WATCHDOG_LEASE, None);

    let outcome = b.election.round().await;

    assert_eq!(outcome, RoundOutcome::TookOver);
    assert_eq!(
        api.record(WATCHDOG_LEASE).unwrap().holder.as_deref(),
        Some("pod-b")
    );
    assert_eq!(api.record(WATCHDOG_LEASE).unwrap().transitions, 1);
}

// ================================================================================================
// the renew deadline
// ================================================================================================

#[tokio::test]
async fn the_leader_stops_reporting_leadership_ten_seconds_after_its_last_successful_renewal() {
    let api = FakeLeaseApi::new();
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);
    a.election.round().await;
    assert!(a.election.is_leader());
    api.fail_everything(Some(LeaseApiError::Failed("api server down".to_owned())));

    a.clock.advance(secs(2));
    assert_eq!(a.election.round().await, RoundOutcome::Failed);
    assert!(
        a.election.is_leader(),
        "a failed round keeps the leadership for now"
    );
    a.clock.advance(Duration::from_millis(7_999));
    assert!(a.election.is_leader(), "9.999 s after the last renewal");
    a.clock.advance(Duration::from_millis(1));
    assert!(
        !a.election.is_leader(),
        "10 s after the last renewal the leadership is no longer reported"
    );
}

#[tokio::test]
async fn a_leader_that_cannot_renew_stops_before_another_pod_may_take_over() {
    assert!(LEASE_RENEW_DEADLINE < LEASE_DURATION);
    let api = FakeLeaseApi::new();
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);
    let b = pod(&api, LeaderRole::OrphanWatchdog, "pod-b", Duration::ZERO);
    a.election.round().await;
    assert_eq!(b.election.round().await, RoundOutcome::Follower);
    // a is cut off from the API: it never renews again
    for _ in 0..7 {
        a.clock.advance(secs(2));
        b.clock.advance(secs(2));
    }
    assert!(!a.election.is_leader(), "14 s: a stopped at 10 s");
    // b is only allowed to take over from 15 s on
    assert_eq!(b.election.round().await, RoundOutcome::Follower);
    b.clock.advance(secs(1));
    assert_eq!(b.election.round().await, RoundOutcome::TookOver);
}

#[tokio::test]
async fn a_hanging_api_call_is_cut_after_five_seconds_of_the_monotonic_clock() {
    let api = FakeLeaseApi::new();
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);
    *api.hang_get.lock().unwrap() = Some(Arc::clone(&a.clock) as SharedWorkerClock);
    let election = Arc::clone(&a.election);
    let round = tokio::spawn(async move { election.round().await });

    // the hanging GET and the 5 s bound are both pending
    let cut_at = a.clock.advance_when_pending(2).await;
    let outcome = tokio::time::timeout(secs(10), round)
        .await
        .unwrap()
        .unwrap();

    assert_eq!(cut_at, LEASE_API_TIMEOUT);
    assert_eq!(outcome, RoundOutcome::Failed);
    assert!(!a.election.is_leader());
}

#[tokio::test]
async fn a_failing_api_logs_at_most_one_warning_per_minute_and_never_fails_the_gear() {
    let logs = LogCapture::start();
    let api = FakeLeaseApi::new();
    api.fail_everything(Some(LeaseApiError::Failed("boom".to_owned())));
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);

    for _ in 0..30 {
        assert_eq!(a.election.round().await, RoundOutcome::Failed);
        a.clock.advance(secs(2));
    }
    let after_one_minute = logs.at(Level::WARN).len();
    assert_eq!(after_one_minute, 1, "{:?}", logs.records());
    a.election.round().await;
    assert_eq!(logs.at(Level::WARN).len(), 2, "the next minute warns again");
}

// ================================================================================================
// conflicts
// ================================================================================================

#[tokio::test]
async fn a_409_on_create_leaves_the_pod_follower() {
    let api = FakeLeaseApi::new();
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);
    api.conflict_next_write.store(true, Ordering::SeqCst);

    let outcome = a.election.round().await;

    assert_eq!(outcome, RoundOutcome::Conflict);
    assert!(!a.election.is_leader());
}

#[tokio::test]
async fn a_409_on_renew_leaves_the_pod_follower_at_once() {
    let api = FakeLeaseApi::new();
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);
    a.election.round().await;
    a.clock.advance(secs(2));
    api.conflict_next_write.store(true, Ordering::SeqCst);

    let outcome = a.election.round().await;

    assert_eq!(outcome, RoundOutcome::Conflict);
    assert!(
        !a.election.is_leader(),
        "another holder: the leadership is not kept until the deadline"
    );
}

#[tokio::test]
async fn a_409_on_take_over_leaves_the_pod_follower() {
    let api = FakeLeaseApi::new();
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);
    let b = pod(&api, LeaderRole::OrphanWatchdog, "pod-b", Duration::ZERO);
    a.election.round().await;
    b.election.round().await;
    b.clock.advance(secs(16));
    api.conflict_next_write.store(true, Ordering::SeqCst);

    let outcome = b.election.round().await;

    assert_eq!(outcome, RoundOutcome::Conflict);
    assert!(!b.election.is_leader());
    assert_eq!(
        api.record(WATCHDOG_LEASE).unwrap().holder.as_deref(),
        Some("pod-a")
    );
}

#[tokio::test]
async fn a_leader_that_sees_another_holder_becomes_a_follower() {
    let api = FakeLeaseApi::new();
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);
    a.election.round().await;
    api.force_holder(WATCHDOG_LEASE, Some("pod-b"));
    a.clock.advance(secs(2));

    let outcome = a.election.round().await;

    assert_eq!(outcome, RoundOutcome::Follower);
    assert!(!a.election.is_leader());
}

// ================================================================================================
// release
// ================================================================================================

#[tokio::test]
async fn on_stop_the_holder_clears_the_identity_and_the_other_pod_takes_over_in_one_round() {
    let api = FakeLeaseApi::new();
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);
    let b = pod(&api, LeaderRole::OrphanWatchdog, "pod-b", Duration::ZERO);
    a.election.round().await;
    assert_eq!(b.election.round().await, RoundOutcome::Follower);

    a.election.release().await;

    assert!(!a.election.is_leader(), "the releasing pod stops at once");
    let released = api.record(WATCHDOG_LEASE).unwrap();
    assert!(released.holder.as_deref().is_none_or(str::is_empty));
    b.clock.advance(secs(2));
    assert_eq!(
        b.election.round().await,
        RoundOutcome::TookOver,
        "no wait for the 15 s of the observed-time rule"
    );
    assert!(b.election.is_leader());
}

#[tokio::test]
async fn a_pod_that_does_not_hold_the_lease_releases_nothing() {
    let api = FakeLeaseApi::new();
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);
    let b = pod(&api, LeaderRole::OrphanWatchdog, "pod-b", Duration::ZERO);
    a.election.round().await;
    b.election.round().await;
    let writes_before = api.count(|call| matches!(call, Call::Replace(_)));

    b.election.release().await;

    assert_eq!(
        api.count(|call| matches!(call, Call::Replace(_))),
        writes_before
    );
    assert_eq!(
        api.record(WATCHDOG_LEASE).unwrap().holder.as_deref(),
        Some("pod-a")
    );
}

#[tokio::test]
async fn a_release_that_fails_is_logged_and_never_panics() {
    let logs = LogCapture::start();
    let api = FakeLeaseApi::new();
    let a = pod(&api, LeaderRole::OrphanWatchdog, "pod-a", Duration::ZERO);
    a.election.round().await;
    api.fail_everything(Some(LeaseApiError::Failed("down".to_owned())));

    a.election.release().await;

    assert!(!a.election.is_leader());
    assert!(!logs.at(Level::WARN).is_empty(), "{:?}", logs.records());
}

// ================================================================================================
// roles and the renew task
// ================================================================================================

#[tokio::test]
async fn each_role_has_its_own_lease_and_its_own_leadership() {
    let api = FakeLeaseApi::new();
    // another pod already holds the reaper lease
    let other = pod(&api, LeaderRole::UploadReaper, "pod-x", Duration::ZERO);
    other.election.round().await;
    let clock = ManualClock::new();
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    let identity = PodIdentity {
        namespace: "ns".to_owned(),
        name: "pod-a".to_owned(),
    };
    let api_port: Arc<dyn LeaseApi> = Arc::clone(&api) as Arc<dyn LeaseApi>;
    let clock_port: SharedWorkerClock = Arc::clone(&clock) as SharedWorkerClock;

    let elector = start_lease_elector(&LeaderRole::ALL, &identity, &api_port, &clock_port, &tasks);

    wait_until("both roles rounded", || {
        api.record(WATCHDOG_LEASE).is_some()
            && api.count(|call| call == &Call::Get(REAPER_LEASE.to_owned())) >= 2
    })
    .await;
    assert!(elector.is_leader(LeaderRole::OrphanWatchdog));
    assert!(
        !elector.is_leader(LeaderRole::UploadReaper),
        "the reaper lease is held by pod-x"
    );
    assert_eq!(elector.name(), "kubernetes-lease");
    assert_eq!(
        api.record(REAPER_LEASE).unwrap().holder.as_deref(),
        Some("pod-x")
    );
    tasks.cancel_and_join(secs(5)).await;
}

#[tokio::test]
async fn a_role_that_is_not_enabled_never_reports_leadership() {
    let api = FakeLeaseApi::new();
    let clock = ManualClock::new();
    let tasks = BackgroundTasks::new(&CancellationToken::new());
    let identity = PodIdentity {
        namespace: "ns".to_owned(),
        name: "pod-a".to_owned(),
    };
    let api_port: Arc<dyn LeaseApi> = Arc::clone(&api) as Arc<dyn LeaseApi>;
    let clock_port: SharedWorkerClock = Arc::clone(&clock) as SharedWorkerClock;

    let elector = start_lease_elector(
        &[LeaderRole::UploadReaper],
        &identity,
        &api_port,
        &clock_port,
        &tasks,
    );

    wait_until("the reaper lease exists", || {
        api.record(REAPER_LEASE).is_some()
    })
    .await;
    assert!(elector.is_leader(LeaderRole::UploadReaper));
    assert!(!elector.is_leader(LeaderRole::OrphanWatchdog));
    assert!(
        api.record(WATCHDOG_LEASE).is_none(),
        "no Lease is created for a disabled role"
    );
    assert_eq!(tasks.running(), vec!["lease-upload-reaper"]);
    tasks.cancel_and_join(secs(5)).await;
}

#[tokio::test]
async fn the_renew_task_rounds_every_two_seconds_and_releases_the_lease_when_the_gear_stops() {
    let api = FakeLeaseApi::new();
    let clock = ManualClock::new();
    let stop = CancellationToken::new();
    let tasks = BackgroundTasks::new(&stop);
    let identity = PodIdentity {
        namespace: "ns".to_owned(),
        name: "pod-a".to_owned(),
    };
    let api_port: Arc<dyn LeaseApi> = Arc::clone(&api) as Arc<dyn LeaseApi>;
    let clock_port: SharedWorkerClock = Arc::clone(&clock) as SharedWorkerClock;
    let elector = start_lease_elector(
        &[LeaderRole::OrphanWatchdog],
        &identity,
        &api_port,
        &clock_port,
        &tasks,
    );
    assert_eq!(tasks.running(), vec!["lease-orphan-watchdog"]);
    wait_until("the lease is created", || {
        api.record(WATCHDOG_LEASE).is_some()
    })
    .await;

    for round in 1..=3 {
        let at = clock.advance_when_pending(1).await;
        assert_eq!(at, secs(2 * round), "one round per renew period");
        wait_until("the renewal", || {
            api.count(|call| matches!(call, Call::Replace(_))) == usize::try_from(round).unwrap()
        })
        .await;
    }
    assert!(elector.is_leader(LeaderRole::OrphanWatchdog));

    let stuck = tasks.cancel_and_join(secs(5)).await;

    assert!(
        stuck.is_empty(),
        "the renew task ends with the gear: {stuck:?}"
    );
    let record = api.record(WATCHDOG_LEASE).unwrap();
    assert!(
        record.holder.as_deref().is_none_or(str::is_empty),
        "the Lease is released on stop: {record:?}"
    );
    assert!(!elector.is_leader(LeaderRole::OrphanWatchdog));
}

// ================================================================================================
// the pod identity
// ================================================================================================

fn lookup<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
    move |name| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
    }
}

#[test]
fn the_pod_identity_is_read_from_pod_namespace_and_pod_name() {
    let identity = PodIdentity::from_lookup(lookup(&[
        ("POD_NAMESPACE", "chat"),
        ("POD_NAME", "mini-chat-7f9c"),
    ]))
    .unwrap();

    assert_eq!(identity.namespace, "chat");
    assert_eq!(identity.name, "mini-chat-7f9c");
}

#[test]
fn a_missing_or_empty_variable_fails_naming_it() {
    for (pairs, missing) in [
        (vec![("POD_NAME", "p")], "POD_NAMESPACE"),
        (vec![("POD_NAMESPACE", "ns")], "POD_NAME"),
        (
            vec![("POD_NAMESPACE", ""), ("POD_NAME", "p")],
            "POD_NAMESPACE",
        ),
        (
            vec![("POD_NAMESPACE", "ns"), ("POD_NAME", "  ")],
            "POD_NAME",
        ),
        (vec![], "POD_NAMESPACE"),
    ] {
        let error = PodIdentity::from_lookup(lookup(&pairs)).unwrap_err();

        assert_eq!(error, ElectorError::MissingEnv(missing), "{pairs:?}");
        assert!(error.to_string().contains(missing), "{error}");
    }
}

async fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..1000 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for: {what}");
}

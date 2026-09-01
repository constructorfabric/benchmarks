// Created: 2026-08-31 by Constructor Tech
// @cpt-dod:cpt-cf-oagw-dod-testing-proxy-data-plane:p2
//! The per-upstream circuit breaker (PRD `cpt-cf-oagw-nfr-high-availability`).
//!
//! The PRD asks that "circuit breaker trips within 5 failed requests in 30s
//! window" and that the refusal it answers with is a retriable `503`; DESIGN
//! §4.2 asks for a state gauge and a transition counter per `host`, DESIGN §4.3
//! for the transitions to be logged. Every test here drives real proxy requests
//! through the data plane, because what is asserted is a *sequence*: five
//! failures, then a refusal that must not dial, then a cooldown, then a probe.
//!
//! The upstream the tests use serves **two routes behind one alias**, a failing
//! one and a healthy one. The breaker is keyed by upstream and not by route, so
//! changing what the upstream answers by moving to the other path — rather than
//! by re-defining a mock — is also what proves the two routes share one state.
//!
//! The failure windows stay at the PRD's thirty seconds, which is longer than a
//! test, so no failure the tests cause can fall out of a window on its own —
//! with one deliberate exception, the test that proves a failure *does* age out
//! and therefore needs a window short enough to age inside it. The cooldowns
//! are two seconds, because a trip is what the tests wait out and the
//! thresholds they use are smaller too.

mod common;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use common::{LogCapture, ProxyHarness, Reply, domain_route, domain_upstream, loopback_endpoint};
use httpmock::prelude::{GET, MockServer};
use oagw::config::{CircuitBreakerConfig, OagwConfig};
use oagw::domain::model::HttpMethod;
use oagw::domain::proxy::chain::NoChain;
use opentelemetry_sdk::metrics::{
    InMemoryMetricExporter, PeriodicReader, SdkMeterProvider,
    data::{AggregatedMetrics, MetricData},
};

/// Alias every test routes through: the `host` label an operator reads.
const ALIAS: &str = "api.vendor.com";
/// The route the upstream fails on.
const ROUTE_FLAKY: &str = "/v1/flaky";
/// The route the upstream serves.
const ROUTE_OK: &str = "/v1/ok";
/// Proxy path addressing [`ROUTE_FLAKY`].
const FLAKY_PATH: &str = "/oagw/v1/proxy/api.vendor.com/v1/flaky";
/// Proxy path addressing [`ROUTE_OK`].
const OK_PATH: &str = "/oagw/v1/proxy/api.vendor.com/v1/ok";
/// Problem type the open breaker answers with (DESIGN §3.3).
const BREAKER_OPEN: &str = "gts.cf.core.errors.err.v1~cf.oagw.circuit_breaker.open.v1";
/// Problem type of a dial that never reached the upstream.
const LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";
/// `X-OAGW-Error-Source` of an answer the gateway generated.
const GATEWAY: &str = "gateway";
/// `X-OAGW-Error-Source` of an answer the upstream gave itself.
const UPSTREAM: &str = "upstream";
/// Cooldown of the tests that wait one out.
///
/// Two seconds rather than one, so a refusal a test asserts *before* it waits
/// has comfortably more than the time a handful of loopback dials take.
const COOLDOWN: u64 = 2;
/// How long the probe's upstream withholds its answer.
///
/// Long enough that a second request issued while the probe is still waiting is
/// comfortably inside the probe, short enough to stay inside the harness's
/// two-second head budget, so the probe is answered rather than timed out.
const PROBE_DELAY: Duration = Duration::from_millis(1_200);
/// How long a test waits out a cooldown of `secs`.
///
/// Half a second of slack over the cooldown itself: the breaker only has to be
/// *past* its cooldown, not exactly at it.
fn lapse(secs: u64) -> Duration {
    Duration::from_millis(secs * 1_000 + 500)
}

// ── Configuration ────────────────────────────────────────────────────────

/// A config whose breaker trips after `threshold` failures and re-probes after
/// `cooldown_secs`.
///
/// The failure window stays at the PRD's thirty seconds, which is longer than a
/// test: no failure the tests cause can fall out of the window on its own, so
/// every count a test asserts is one it caused.
fn config_with(enabled: bool, threshold: u32, cooldown_secs: u64) -> OagwConfig {
    config_windowed(enabled, threshold, 30, cooldown_secs)
}

/// [`config_with`] with an explicit failure window, for the test that proves a
/// failure stops counting once it ages out.
fn config_windowed(
    enabled: bool,
    threshold: u32,
    window_secs: u64,
    cooldown_secs: u64,
) -> OagwConfig {
    OagwConfig {
        circuit_breaker: CircuitBreakerConfig {
            enabled,
            failure_threshold: threshold,
            failure_window_secs: window_secs,
            cooldown_secs,
        },
        ..common::proxy_config()
    }
}

// ── Seeding ──────────────────────────────────────────────────────────────

/// A harness whose upstream answers `port`, with the breaker of `config`.
///
/// Both routes of [`ALIAS`] are seeded for `GET`, so a test can flip the
/// upstream's answer by addressing the other path.
fn harness_with(port: u16, config: &OagwConfig) -> Result<ProxyHarness> {
    let harness = ProxyHarness::with_config_and_chain(config, Arc::new(NoChain));
    let upstream = harness.seed_upstream(domain_upstream(
        harness.tenant(),
        ALIAS,
        vec![loopback_endpoint(port)],
        true,
    ));
    for path in [ROUTE_FLAKY, ROUTE_OK] {
        harness
            .store()
            .insert_route_checked(domain_route(
                harness.tenant(),
                upstream,
                &[HttpMethod::Get],
                path,
                &[],
            ))
            .with_context(|| format!("the route {path} must seed"))?;
    }
    Ok(harness)
}

/// A loopback port nothing is listening on.
fn refused_port() -> Result<u16> {
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).context("binding a throwaway listener")?;
    Ok(listener.local_addr().context("the local address")?.port())
}

// ── Requests ─────────────────────────────────────────────────────────────

/// Proxy `path` and assert the source the answer came from.
async fn send(harness: &ProxyHarness, path: &str, expected_source: &str) -> Result<Reply> {
    let reply = harness.proxy("GET", path, &[], b"").await?;
    assert_eq!(
        reply.header("x-oagw-error-source"),
        Some(expected_source),
        "{path} was answered by the wrong side"
    );
    Ok(reply)
}

// ── Reading the recorded data points back ────────────────────────────────

/// Serialises the tests of this file against the process-global provider.
///
/// `ProxyService::new` pulls its instruments from the **global** meter
/// provider, which is one slot shared by every test in the process, so **every**
/// test of this file holds the lock, not only the ones that read data points
/// back: a test that never asserts on the exporter still *writes* into it,
/// because tripping a breaker is what publishes a state point. A test that
/// installs its provider must be alone in the process from before it installs
/// it to after its last assertion. It is asynchronous because a guard of a
/// `std` mutex may not be held across an `await`.
static METER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The lock of [`METER_LOCK`], held for the whole of a test.
async fn meter_guard() -> tokio::sync::MutexGuard<'static, ()> {
    METER_LOCK.lock().await
}

/// Install an in-memory meter provider as the OpenTelemetry global.
///
/// The returned provider must outlive the assertions: dropping it shuts the
/// reader down, and the data points of a shut-down reader are gone.
fn install_meter_provider() -> (SdkMeterProvider, InMemoryMetricExporter) {
    let exporter = InMemoryMetricExporter::default();
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter.clone()).build())
        .build();
    opentelemetry::global::set_meter_provider(provider.clone());
    (provider, exporter)
}

/// Whether `attributes` is exactly `expected`, name and value.
fn attributes_match(attributes: &[opentelemetry::KeyValue], expected: &[(&str, &str)]) -> bool {
    attributes.len() == expected.len()
        && expected.iter().all(|(name, value)| {
            attributes
                .iter()
                .any(|pair| pair.key.as_str() == *name && pair.value.to_string() == *value)
        })
}

/// Value of the `u64` data points of `name` whose attributes are exactly
/// `expected`.
///
/// The exporter keeps **every** collection it made and a counter is cumulative,
/// so each collection repeats the total the reader had gathered up to then:
/// summing the points would count one transition once per collection. The
/// collections are therefore walked backwards and the newest point is the
/// total the counter carries now.
fn counter_value(exporter: &InMemoryMetricExporter, name: &str, expected: &[(&str, &str)]) -> u64 {
    let collected = exporter.get_finished_metrics().unwrap_or_default();
    let metrics: Vec<_> = metrics_of(&collected, name).collect();
    for metric in metrics.iter().rev() {
        if let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() {
            for point in sum.data_points() {
                let attributes: Vec<_> = point.attributes().cloned().collect();
                if attributes_match(&attributes, expected) {
                    return point.value();
                }
            }
        }
    }
    0
}

/// Value of the `u64` gauge data point of `name` whose attributes are exactly
/// `expected`.
///
/// The state gauge is a gauge and not a counter, so it is read through this
/// rather than through [`counter_value`]: a series it never published must read
/// as *absent*, not as zero, and a sum of the collections would read as
/// nonsense. The exporter keeps **every** collection it made, and each one
/// carries the gauge as it was at that moment, so the collections are walked
/// backwards and the newest point is the state the upstream is in now.
fn gauge_value(
    exporter: &InMemoryMetricExporter,
    name: &str,
    expected: &[(&str, &str)],
) -> Option<u64> {
    let collected = exporter.get_finished_metrics().unwrap_or_default();
    let metrics: Vec<_> = metrics_of(&collected, name).collect();
    for metric in metrics.iter().rev() {
        if let AggregatedMetrics::U64(MetricData::Gauge(gauge)) = metric.data() {
            for point in gauge.data_points() {
                let attributes: Vec<_> = point.attributes().cloned().collect();
                if attributes_match(&attributes, expected) {
                    return Some(point.value());
                }
            }
        }
    }
    None
}

/// Every metric of `collected` named `name`.
fn metrics_of<'a>(
    collected: &'a [opentelemetry_sdk::metrics::data::ResourceMetrics],
    name: &'a str,
) -> impl Iterator<Item = &'a opentelemetry_sdk::metrics::data::Metric> + 'a {
    collected
        .iter()
        .flat_map(opentelemetry_sdk::metrics::data::ResourceMetrics::scope_metrics)
        .flat_map(opentelemetry_sdk::metrics::data::ScopeMetrics::metrics)
        .filter(move |metric| metric.name() == name)
}

/// Whether the exporter recorded `name` at all.
///
/// The state gauge has no zero to fall back on: an upstream whose breaker never
/// moved has no series, and only "the metric is not there" says that honestly.
fn has_metric(exporter: &InMemoryMetricExporter, name: &str) -> bool {
    let collected = exporter.get_finished_metrics().unwrap_or_default();
    metrics_of(&collected, name).next().is_some()
}

// ── Tripping ─────────────────────────────────────────────────────────────

/// Five consecutive upstream failures trip the breaker, and the request behind
/// them is refused without the upstream being asked again.
#[tokio::test]
async fn five_failures_trip_the_breaker_and_the_next_request_is_refused_without_a_dial()
-> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let flaky = server.mock(|when, then| {
        when.method(GET).path(ROUTE_FLAKY);
        then.status(503);
    });
    let harness = harness_with(server.port(), &config_with(true, 5, 5))?;

    for _ in 0..5 {
        let reply = send(&harness, FLAKY_PATH, UPSTREAM).await?;
        assert_eq!(reply.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }
    assert_eq!(flaky.calls(), 5, "every failure was a real dial");

    let refused = send(&harness, FLAKY_PATH, GATEWAY).await?;
    assert_eq!(refused.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.problem_type().as_deref(), Some(BREAKER_OPEN));
    assert_eq!(flaky.calls(), 5, "the refusal dialled nothing");
    Ok(())
}

/// The refusal is the gateway's own: a `503` problem document that names how
/// long the client has to wait, not the upstream's answer.
#[tokio::test]
async fn the_refusal_is_a_gateway_problem_that_names_the_cooldown() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let flaky = server.mock(|when, then| {
        when.method(GET).path(ROUTE_FLAKY);
        then.status(503);
    });
    let harness = harness_with(server.port(), &config_with(true, 1, 10))?;

    let first = send(&harness, FLAKY_PATH, UPSTREAM).await?;
    assert_eq!(
        first.problem_type().as_deref(),
        None,
        "a passthrough answer"
    );

    let refused = send(&harness, FLAKY_PATH, GATEWAY).await?;
    assert_eq!(refused.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.problem_type().as_deref(), Some(BREAKER_OPEN));
    assert!(
        refused
            .header("content-type")
            .unwrap_or_default()
            .starts_with("application/problem+json"),
        "the refusal is a problem document"
    );
    let retry_after = refused
        .header("retry-after")
        .context("the refusal names the cooldown")?
        .parse::<u64>()
        .context("the Retry-After is a whole number of seconds")?;
    assert!(
        (1..=10).contains(&retry_after),
        "the guidance is the cooldown still to run, in [1, 10]: {retry_after}"
    );
    assert_eq!(flaky.calls(), 1);
    Ok(())
}

/// A 4xx is the client's error and the upstream answered it, so it is not the
/// upstream's health and never trips the breaker.
#[tokio::test]
async fn a_client_error_is_not_the_upstreams_health() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let flaky = server.mock(|when, then| {
        when.method(GET).path(ROUTE_FLAKY);
        then.status(404);
    });
    let harness = harness_with(server.port(), &config_with(true, 2, 5))?;

    for _ in 0..3 {
        let reply = send(&harness, FLAKY_PATH, UPSTREAM).await?;
        assert_eq!(reply.status, axum::http::StatusCode::NOT_FOUND);
    }
    assert_eq!(flaky.calls(), 3, "the third request was still dialled");
    Ok(())
}

/// A success does not empty the window.
///
/// The window is what forgets, and only time does: an upstream that answers
/// 200 on its cheap requests and 503 on its expensive ones is exactly the
/// partial failure the breaker exists to stop, so a healthy answer in between
/// leaves the failures it already owed in place.
#[tokio::test]
async fn a_success_does_not_empty_the_window() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let flaky = server.mock(|when, then| {
        when.method(GET).path(ROUTE_FLAKY);
        then.status(503);
    });
    let ok = server.mock(|when, then| {
        when.method(GET).path(ROUTE_OK);
        then.status(200);
    });
    let harness = harness_with(server.port(), &config_with(true, 2, 5))?;

    let failure = send(&harness, FLAKY_PATH, UPSTREAM).await?;
    assert_eq!(failure.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    let recovered = send(&harness, OK_PATH, UPSTREAM).await?;
    assert_eq!(recovered.status, axum::http::StatusCode::OK);

    // The success is still in the window's past, so this failure is the second
    // the threshold asks for. It trips the breaker *after* the upstream has
    // answered it, which is why this request is a passthrough and the next one
    // is a refusal.
    let second = send(&harness, FLAKY_PATH, UPSTREAM).await?;
    assert_eq!(second.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    let refused = send(&harness, FLAKY_PATH, GATEWAY).await?;
    assert_eq!(refused.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.problem_type().as_deref(), Some(BREAKER_OPEN));
    assert_eq!(flaky.calls(), 2, "the success did not buy a fresh count");
    assert_eq!(ok.calls(), 1);
    Ok(())
}

/// A failure stops counting once it has aged out of the window.
///
/// The window is one second here rather than the PRD's thirty, so a failure can
/// be watched leaving it: after the lapse the upstream is dialled again, which
/// it would not be if the first failure were still being counted.
#[tokio::test]
async fn a_failure_stops_counting_once_it_has_aged_out_of_the_window() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let flaky = server.mock(|when, then| {
        when.method(GET).path(ROUTE_FLAKY);
        then.status(503);
    });
    let harness = harness_with(server.port(), &config_windowed(true, 2, 1, 5))?;

    let first = send(&harness, FLAKY_PATH, UPSTREAM).await?;
    assert_eq!(first.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    tokio::time::sleep(lapse(1)).await;

    // The first failure is older than the window, so this second one is the
    // only one the breaker is counting and the upstream is dialled once more.
    let second = send(&harness, FLAKY_PATH, UPSTREAM).await?;
    assert_eq!(second.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(flaky.calls(), 2, "the aged-out failure did not trip it");

    let third = send(&harness, FLAKY_PATH, UPSTREAM).await?;
    assert_eq!(third.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    let refused = send(&harness, FLAKY_PATH, GATEWAY).await?;
    assert_eq!(refused.problem_type().as_deref(), Some(BREAKER_OPEN));
    assert_eq!(flaky.calls(), 3, "the two live failures tripped it");
    Ok(())
}

/// A dial failure — an upstream nothing is listening on — is a health failure
/// like a 5xx is.
#[tokio::test]
async fn a_dial_failure_counts_as_a_health_failure() -> Result<()> {
    let _meter = meter_guard().await;
    let port = refused_port()?;
    let harness = harness_with(port, &config_with(true, 1, 5))?;

    let failed = send(&harness, FLAKY_PATH, GATEWAY).await?;
    assert_eq!(failed.problem_type().as_deref(), Some(LINK_UNAVAILABLE));

    let refused = send(&harness, FLAKY_PATH, GATEWAY).await?;
    assert_eq!(refused.problem_type().as_deref(), Some(BREAKER_OPEN));
    Ok(())
}

// ── Half-open ────────────────────────────────────────────────────────────

/// Once the cooldown has run out the breaker admits one probe, and a probe that
/// succeeds closes it: the requests behind it are dialled again.
#[tokio::test]
async fn after_the_cooldown_a_successful_probe_closes_the_breaker() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let _flaky = server.mock(|when, then| {
        when.method(GET).path(ROUTE_FLAKY);
        then.status(503);
    });
    let ok = server.mock(|when, then| {
        when.method(GET).path(ROUTE_OK);
        then.status(200);
    });
    let harness = harness_with(server.port(), &config_with(true, 1, COOLDOWN))?;

    send(&harness, FLAKY_PATH, UPSTREAM).await?;
    send(&harness, OK_PATH, GATEWAY).await?;
    assert_eq!(ok.calls(), 0, "the open breaker refused without dialling");

    tokio::time::sleep(lapse(COOLDOWN)).await;
    let probe = send(&harness, OK_PATH, UPSTREAM).await?;
    assert_eq!(probe.status, axum::http::StatusCode::OK);
    let next = send(&harness, OK_PATH, UPSTREAM).await?;
    assert_eq!(next.status, axum::http::StatusCode::OK);
    assert_eq!(
        ok.calls(),
        2,
        "the breaker closed: normal requests are dialled"
    );
    Ok(())
}

/// A probe that fails re-opens the breaker: the next request is refused again,
/// on the other route too, because the breaker is per upstream.
#[tokio::test]
async fn after_the_cooldown_a_failed_probe_reopens_the_breaker() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let flaky = server.mock(|when, then| {
        when.method(GET).path(ROUTE_FLAKY);
        then.status(503);
    });
    let ok = server.mock(|when, then| {
        when.method(GET).path(ROUTE_OK);
        then.status(200);
    });
    let harness = harness_with(server.port(), &config_with(true, 1, COOLDOWN))?;

    send(&harness, FLAKY_PATH, UPSTREAM).await?;
    send(&harness, FLAKY_PATH, GATEWAY).await?;
    tokio::time::sleep(lapse(COOLDOWN)).await;

    let probe = send(&harness, FLAKY_PATH, UPSTREAM).await?;
    assert_eq!(probe.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(flaky.calls(), 2, "the probe was dialled");

    let refused = send(&harness, OK_PATH, GATEWAY).await?;
    assert_eq!(refused.problem_type().as_deref(), Some(BREAKER_OPEN));
    assert_eq!(
        ok.calls(),
        0,
        "the other route is refused by the same breaker"
    );
    Ok(())
}

/// A request that arrives behind an in-flight probe is refused.
///
/// Half-open admits the probe and nobody else, so the second request of this
/// test — issued only once the mock server has *received* the probe, whose
/// answer it withholds for [`PROBE_DELAY`] — is answered by the gateway while
/// the probe is still waiting. The probe itself closes the breaker afterwards,
/// which is what proves the refusal was the half-open slot and not an open
/// breaker that happened to be there anyway.
#[tokio::test]
async fn a_request_behind_an_in_flight_probe_is_refused() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let flaky = server.mock(|when, then| {
        when.method(GET).path(ROUTE_FLAKY);
        then.status(503);
    });
    let slow = server.mock(|when, then| {
        when.method(GET).path(ROUTE_OK);
        then.status(200).delay(PROBE_DELAY);
    });
    let harness = harness_with(server.port(), &config_with(true, 1, COOLDOWN))?;

    send(&harness, FLAKY_PATH, UPSTREAM).await?;
    send(&harness, OK_PATH, GATEWAY).await?;
    tokio::time::sleep(lapse(COOLDOWN)).await;

    // The probe and the refusal are issued together, and the refusal starts
    // only once the upstream is holding the probe's unanswered request.
    let wait_for_the_probe = async {
        for _ in 0..100 {
            if slow.calls_async().await > 0 {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Err(anyhow::anyhow!("the probe never reached the upstream"))
    };
    let refused_behind_it = async {
        wait_for_the_probe.await?;
        send(&harness, FLAKY_PATH, GATEWAY).await
    };
    let (probe, refused) = tokio::join!(send(&harness, OK_PATH, UPSTREAM), refused_behind_it);

    let probe = probe?;
    assert_eq!(probe.status, axum::http::StatusCode::OK);
    assert_eq!(slow.calls(), 1, "the probe was dialled exactly once");
    let refused = refused?;
    assert_eq!(refused.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(refused.problem_type().as_deref(), Some(BREAKER_OPEN));
    assert_eq!(flaky.calls(), 1, "the refusal dialled nothing");

    let after = send(&harness, OK_PATH, UPSTREAM).await?;
    assert_eq!(after.status, axum::http::StatusCode::OK);
    assert_eq!(slow.calls(), 2, "the probe closed the breaker behind it");
    Ok(())
}

/// A deployment that turns the breaker off is dialled every time, whatever the
/// upstream answers.
#[tokio::test]
async fn a_disabled_breaker_never_trips() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let flaky = server.mock(|when, then| {
        when.method(GET).path(ROUTE_FLAKY);
        then.status(503);
    });
    let harness = harness_with(server.port(), &config_with(false, 1, 1))?;

    for _ in 0..5 {
        let reply = send(&harness, FLAKY_PATH, UPSTREAM).await?;
        assert_eq!(reply.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    }
    assert_eq!(flaky.calls(), 5, "every request was dialled");
    Ok(())
}

// ── Telemetry ────────────────────────────────────────────────────────────

/// The state gauge reads the state an upstream's breaker is in and the
/// transition counter records each transition with both states, and a
/// transition is logged at `WARN` with the alias and the two states.
#[tokio::test]
async fn the_state_gauge_and_the_transition_counter_report_each_transition() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let _flaky = server.mock(|when, then| {
        when.method(GET).path(ROUTE_FLAKY);
        then.status(503);
    });
    let _ok = server.mock(|when, then| {
        when.method(GET).path(ROUTE_OK);
        then.status(200);
    });
    let capture = LogCapture::default();
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    let (provider, exporter, harness) = {
        let (provider, exporter) = install_meter_provider();
        let harness = harness_with(server.port(), &config_with(true, 1, COOLDOWN))?;
        (provider, exporter, harness)
    };

    send(&harness, FLAKY_PATH, UPSTREAM).await?;
    provider.force_flush().context("the meter must flush")?;
    let host = [("host", ALIAS)];
    assert_eq!(
        gauge_value(&exporter, "oagw_circuit_breaker_state", &host),
        Some(2),
        "the breaker is open, the largest value an operator alerts on"
    );
    assert_eq!(
        counter_value(
            &exporter,
            "oagw_circuit_breaker_transitions_total",
            &[
                ("host", ALIAS),
                ("from_state", "closed"),
                ("to_state", "open")
            ],
        ),
        1
    );
    let opened = capture
        .lines()
        .into_iter()
        .find(|line| line.contains("circuit breaker state changed"))
        .context("the transition was not logged")?;
    assert!(
        opened.contains("[WARN]"),
        "an open breaker is a warning: {opened}"
    );
    assert!(
        opened.contains(&format!("host={ALIAS}")),
        "it names the alias: {opened}"
    );
    assert!(opened.contains("from_state=closed"), "{opened}");
    assert!(opened.contains("to_state=open"), "{opened}");

    tokio::time::sleep(lapse(COOLDOWN)).await;
    send(&harness, OK_PATH, UPSTREAM).await?;
    provider.force_flush().context("the meter must flush")?;
    assert_eq!(
        gauge_value(&exporter, "oagw_circuit_breaker_state", &host),
        Some(0),
        "the probe closed the breaker"
    );
    assert_eq!(
        counter_value(
            &exporter,
            "oagw_circuit_breaker_transitions_total",
            &[
                ("host", ALIAS),
                ("from_state", "open"),
                ("to_state", "half_open")
            ],
        ),
        1
    );
    assert_eq!(
        counter_value(
            &exporter,
            "oagw_circuit_breaker_transitions_total",
            &[
                ("host", ALIAS),
                ("from_state", "half_open"),
                ("to_state", "closed")
            ],
        ),
        1
    );
    assert_eq!(
        counter_value(
            &exporter,
            "oagw_circuit_breaker_transitions_total",
            &[
                ("host", ALIAS),
                ("from_state", "closed"),
                ("to_state", "open")
            ],
        ),
        1,
        "the trip was not counted twice"
    );
    Ok(())
}

/// A breaker that never moved publishes no series at all, so an operator does
/// not read a zero as "this upstream tripped and recovered".
#[tokio::test]
async fn a_breaker_that_never_moved_publishes_no_series() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let _ok = server.mock(|when, then| {
        when.method(GET).path(ROUTE_OK);
        then.status(200);
    });
    let (provider, exporter, harness) = {
        let (provider, exporter) = install_meter_provider();
        let harness = harness_with(server.port(), &config_with(true, 5, 5))?;
        (provider, exporter, harness)
    };

    let reply = send(&harness, OK_PATH, UPSTREAM).await?;
    assert_eq!(reply.status, axum::http::StatusCode::OK);
    provider.force_flush().context("the meter must flush")?;

    assert_eq!(
        gauge_value(&exporter, "oagw_circuit_breaker_state", &[("host", ALIAS)]),
        None,
        "no state was published for a breaker that stayed closed"
    );
    assert!(
        !has_metric(&exporter, "oagw_circuit_breaker_transitions_total"),
        "a breaker that never moved recorded no transition"
    );
    Ok(())
}

/// The refusal is measured like any other request the data plane answered: it
/// is counted in `oagw_requests_total` and marked in `oagw_errors_total` under
/// its own problem type, next to the failure that tripped the breaker.
#[tokio::test]
async fn the_refusal_is_counted_in_the_request_and_error_families() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let _flaky = server.mock(|when, then| {
        when.method(GET).path(ROUTE_FLAKY);
        then.status(503);
    });
    let (provider, exporter, harness) = {
        let (provider, exporter) = install_meter_provider();
        let harness = harness_with(server.port(), &config_with(true, 1, 5))?;
        (provider, exporter, harness)
    };

    send(&harness, FLAKY_PATH, UPSTREAM).await?;
    send(&harness, FLAKY_PATH, GATEWAY).await?;
    provider.force_flush().context("the meter must flush")?;

    // Both requests were answered with a 503, the dialled one with the
    // upstream's and the refused one with the gateway's: the request family has
    // no label for who answered, which is what `oagw_errors_total` is for.
    assert_eq!(
        counter_value(
            &exporter,
            "oagw_requests_total",
            &[
                ("host", ALIAS),
                ("http.request.method", "GET"),
                ("http.route", ROUTE_FLAKY),
                ("http.response.status_code", "503"),
            ],
        ),
        2
    );
    assert_eq!(
        counter_value(
            &exporter,
            "oagw_errors_total",
            &[
                ("host", ALIAS),
                ("http.route", ROUTE_FLAKY),
                ("error_type", BREAKER_OPEN),
            ],
        ),
        1,
        "only the refusal is a gateway failure; the upstream 503 is a passthrough"
    );
    Ok(())
}

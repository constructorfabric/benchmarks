// Created: 2026-08-31 by Constructor Tech
// @cpt-dod:cpt-cf-oagw-dod-testing-proxy-data-plane:p2
//! OpenTelemetry instruments of the proxy data plane (DESIGN §4.2).
//!
//! Every assertion is made on the data points an in-memory exporter recorded
//! behind the **global** meter provider: `ProxyService::new` pulls its
//! instruments from `opentelemetry::global`, so a test installs its provider
//! first and builds the harness second — an instrument is bound to whatever
//! provider was global when it was built and is never re-targeted.
//!
//! Attribute sets are matched *exactly*, not merely "contains". A label that
//! appears where §4.2 declares none is as much a cardinality bug as a value
//! that is missing, and the exact match is what pins `http.route` to the
//! matched route's prefix rather than to the raw request path.
//!
//! A method that can never match a route never reaches the request counter
//! end-to-end, and the tests say so rather than pretending otherwise. Two
//! classes are involved: a registered verb the route model does not name
//! (`HEAD`, `OPTIONS` — `HttpMethod::parse` resolves it, but no route allows
//! it) and an unregistered one (`CONNECT`, `TRACE`, `BREW`), which the router's
//! fallback answers before the data plane is reached at all. Both are a 404
//! with no upstream behind them, and a request without an upstream is not
//! recorded (see the module docs of `src/infra/metrics.rs`), so neither the
//! `_OTHER` mapping nor the `TRACE` verb can be observed through a counter.
//! They are pinned where they live: `_OTHER` in the unit tests of
//! `src/infra/metrics.rs`, and the audit record of an unregistered method in
//! `an_unregistered_method_is_audited_like_any_other_request`.

mod common;

use anyhow::{Context as _, Result};
use common::{LogCapture, ProxyHarness, domain_route, domain_upstream, loopback_endpoint};
use httpmock::prelude::{GET, MockServer};
use oagw::domain::model::{
    BurstConfig, CorsConfig, Endpoint, HttpMethod, RateLimitConfig, SharingMode, SustainedRate,
};
use opentelemetry_sdk::metrics::{
    InMemoryMetricExporter, PeriodicReader, SdkMeterProvider,
    data::{AggregatedMetrics, MetricData},
};
use uuid::Uuid;

/// Alias every test routes through.
const ALIAS: &str = "api.vendor.com";
/// Path prefix every test's route matches.
const ROUTE: &str = "/v1/chat";
/// The proxy path that addresses [`ROUTE`].
const PROXY_PATH: &str = "/oagw/v1/proxy/api.vendor.com/v1/chat";
/// The `traceparent` of the tests that need a correlation id.
const TRACEPARENT: &str = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
/// Problem type the data plane answers with when the dial fails.
const LINK_UNAVAILABLE: &str = "gts.cf.core.errors.err.v1~cf.oagw.link.unavailable.v1";

// ── The meter provider the instruments are read back from ────────────────

/// Serialises the tests of this file against the process-global provider.
///
/// The global provider is one slot shared by every test in the process, and the
/// instruments of a request read from it, so two tests issuing proxy requests
/// at the same moment would be counted into whichever provider was installed
/// last. An asynchronous mutex, because a guard of a `std` one may not be held
/// across an `await`: a test holds it from before it installs its provider to
/// after its last assertion.
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

// ── Reading the recorded data points back ────────────────────────────────

/// Whether `attributes` is exactly `expected`, name and value.
///
/// Values are compared as rendered, which is what makes the numeric
/// `http.response.status_code` comparable with the `"200"` the test spells.
fn attributes_match(attributes: &[opentelemetry::KeyValue], expected: &[(&str, &str)]) -> bool {
    attributes.len() == expected.len()
        && expected.iter().all(|(name, value)| {
            attributes
                .iter()
                .any(|pair| pair.key.as_str() == *name && pair.value.to_string() == *value)
        })
}

/// Sum of the `u64` data points of `name` whose attributes are exactly `expected`.
fn counter_value(exporter: &InMemoryMetricExporter, name: &str, expected: &[(&str, &str)]) -> u64 {
    let collected = collected(exporter);
    let mut total = 0;
    for metric in metrics_of(&collected, name) {
        if let AggregatedMetrics::U64(MetricData::Sum(sum)) = metric.data() {
            for point in sum.data_points() {
                let attributes: Vec<_> = point.attributes().cloned().collect();
                if attributes_match(&attributes, expected) {
                    total += point.value();
                }
            }
        }
    }
    total
}

/// Values of the `i64` data points of `name` whose attributes are exactly
/// `expected`.
///
/// The in-flight gauge is asserted through this rather than through a sum: a
/// single point at zero is the observation the Drop guard owes, and a sum would
/// hide a second point that never came back down.
fn point_values(
    exporter: &InMemoryMetricExporter,
    name: &str,
    expected: &[(&str, &str)],
) -> Vec<i64> {
    let collected = collected(exporter);
    let mut values = Vec::new();
    for metric in metrics_of(&collected, name) {
        if let AggregatedMetrics::I64(MetricData::Sum(sum)) = metric.data() {
            for point in sum.data_points() {
                let attributes: Vec<_> = point.attributes().cloned().collect();
                if attributes_match(&attributes, expected) {
                    values.push(point.value());
                }
            }
        }
    }
    values
}

/// Value of the `f64` gauge data point of `name` whose attributes are exactly
/// `expected`.
fn gauge_value(
    exporter: &InMemoryMetricExporter,
    name: &str,
    expected: &[(&str, &str)],
) -> Option<f64> {
    let collected = collected(exporter);
    for metric in metrics_of(&collected, name) {
        if let AggregatedMetrics::F64(MetricData::Gauge(gauge)) = metric.data() {
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

/// Number of samples the histogram `name` recorded under exactly `expected`.
fn histogram_count(
    exporter: &InMemoryMetricExporter,
    name: &str,
    expected: &[(&str, &str)],
) -> u64 {
    let collected = collected(exporter);
    for metric in metrics_of(&collected, name) {
        if let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = metric.data() {
            for point in histogram.data_points() {
                let attributes: Vec<_> = point.attributes().cloned().collect();
                if attributes_match(&attributes, expected) {
                    return point.count();
                }
            }
        }
    }
    0
}

/// Whether any data point of `name` carries `value` as one of its label values.
///
/// The exact match of [`attributes_match`] can never see a label that arrives
/// *next to* the expected ones, so an assertion built on it is vacuous for the
/// "nothing else is labelled" question: this one looks at every attribute of
/// every data point, whatever the instrument.
fn any_label_value(exporter: &InMemoryMetricExporter, name: &str, value: &str) -> bool {
    let collected = collected(exporter);
    metrics_of(&collected, name).any(|metric| {
        attribute_sets(metric)
            .iter()
            .any(|set| set.iter().any(|pair| pair.value.to_string() == value))
    })
}

/// Every attribute set of `metric`, whatever kind of instrument it is.
fn attribute_sets(
    metric: &opentelemetry_sdk::metrics::data::Metric,
) -> Vec<Vec<opentelemetry::KeyValue>> {
    let mut sets = Vec::new();
    let data = metric.data();
    if let AggregatedMetrics::U64(MetricData::Sum(sum)) = data {
        sets.extend(
            sum.data_points()
                .map(|point| point.attributes().cloned().collect()),
        );
    }
    if let AggregatedMetrics::I64(MetricData::Sum(sum)) = data {
        sets.extend(
            sum.data_points()
                .map(|point| point.attributes().cloned().collect()),
        );
    }
    if let AggregatedMetrics::F64(MetricData::Sum(sum)) = data {
        sets.extend(
            sum.data_points()
                .map(|point| point.attributes().cloned().collect()),
        );
    }
    if let AggregatedMetrics::F64(MetricData::Gauge(gauge)) = data {
        sets.extend(
            gauge
                .data_points()
                .map(|point| point.attributes().cloned().collect()),
        );
    }
    if let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = data {
        sets.extend(
            histogram
                .data_points()
                .map(|point| point.attributes().cloned().collect()),
        );
    }
    sets
}

/// Every collection the exporter holds, in the order it received them.
fn collected(
    exporter: &InMemoryMetricExporter,
) -> Vec<opentelemetry_sdk::metrics::data::ResourceMetrics> {
    exporter.get_finished_metrics().unwrap_or_default()
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

// ── Seeding ──────────────────────────────────────────────────────────────

/// A token-bucket policy of `rate` per second with a `capacity` burst.
fn rate_limit(rate: u64, capacity: u64) -> RateLimitConfig {
    RateLimitConfig {
        sharing: SharingMode::Private,
        algorithm: "token_bucket".to_owned(),
        sustained: SustainedRate {
            rate,
            window: "second".to_owned(),
        },
        burst: Some(BurstConfig { capacity }),
        scope: "global".to_owned(),
        strategy: "reject".to_owned(),
        cost: 1,
        response_headers: true,
    }
}

/// A harness whose upstream answers `port`, with `rate_limit` on the upstream.
///
/// The route is seeded with the default suffix mode, so `/v1/chat/anything`
/// resolves to the same prefix [`ROUTE`] names — which is what the route-label
/// test needs.
fn harness_with(port: u16, rate_limit: Option<RateLimitConfig>) -> Result<ProxyHarness> {
    seeded(Vec::from([loopback_endpoint(port)]), rate_limit).map(|(harness, _)| harness)
}

/// Seed `endpoints` as [`ALIAS`] with a route of [`ROUTE`], and return the
/// upstream's id.
///
/// The id is the `upstream_id` label of the two routing families, so the tests
/// that read them need it.
fn seeded(
    endpoints: Vec<Endpoint>,
    rate_limit: Option<RateLimitConfig>,
) -> Result<(ProxyHarness, Uuid)> {
    let harness = ProxyHarness::new();
    let mut record = domain_upstream(harness.tenant(), ALIAS, endpoints, true);
    record.rate_limit = rate_limit;
    let upstream = harness.seed_upstream(record);
    harness
        .store()
        .insert_route_checked(domain_route(
            harness.tenant(),
            upstream,
            &[HttpMethod::Get],
            ROUTE,
            &[],
        ))
        .context("the test route must seed")?;
    Ok((harness, upstream))
}

/// A CORS policy that allows `origin` and nothing else.
fn cors_policy(origin: &str) -> CorsConfig {
    CorsConfig {
        sharing: SharingMode::Private,
        enabled: true,
        allowed_origins: Vec::from([origin.to_owned()]),
        allowed_methods: Vec::from(["GET".to_owned()]),
        expose_headers: Vec::new(),
        allow_credentials: false,
    }
}

/// A loopback port nothing is listening on.
fn refused_port() -> Result<u16> {
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).context("binding a throwaway listener")?;
    Ok(listener.local_addr().context("the local address")?.port())
}

/// The first audit record whose severity is `marker`, if one was emitted.
fn audit_line(capture: &LogCapture, marker: &str) -> Option<String> {
    capture
        .lines()
        .into_iter()
        .find(|line| line.contains("proxied request") && line.contains(marker))
}

// ── Requests ─────────────────────────────────────────────────────────────

/// A 200 answered by the upstream is counted with the upstream's own status,
/// the normalized method and the matched route prefix.
#[tokio::test]
async fn a_proxied_request_is_counted_with_the_upstream_status_and_method() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path(ROUTE);
        then.status(200);
    });
    let (provider, exporter, harness) = {
        let (provider, exporter) = install_meter_provider();
        let harness = harness_with(server.port(), None)?;
        (provider, exporter, harness)
    };

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;
    assert_eq!(reply.status, axum::http::StatusCode::OK);
    provider.force_flush().context("the meter must flush")?;

    assert_eq!(
        counter_value(
            &exporter,
            "oagw_requests_total",
            &[
                ("host", ALIAS),
                ("http.request.method", "GET"),
                ("http.route", ROUTE),
                ("http.response.status_code", "200"),
            ],
        ),
        1,
        "one request, under the four labels of section 4.2"
    );
    mock.assert();
    Ok(())
}

/// The duration histogram records one sample under the route prefix and the
/// one phase the data plane can honestly name.
#[tokio::test]
async fn the_duration_histogram_records_one_sample_per_request() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path(ROUTE);
        then.status(200);
    });
    let (provider, exporter, harness) = {
        let (provider, exporter) = install_meter_provider();
        let harness = harness_with(server.port(), None)?;
        (provider, exporter, harness)
    };

    harness.proxy("GET", PROXY_PATH, &[], b"").await?;
    provider.force_flush().context("the meter must flush")?;

    assert_eq!(
        histogram_count(
            &exporter,
            "oagw_request_duration_seconds",
            &[("host", ALIAS), ("http.route", ROUTE), ("phase", "total")],
        ),
        1,
        "one sample, under the three labels of section 4.2"
    );
    mock.assert();
    Ok(())
}

/// The in-flight gauge is back to zero once the request has been served.
#[tokio::test]
async fn in_flight_returns_to_zero_when_the_request_ends() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(GET).path(ROUTE);
        then.status(200);
    });
    let (provider, exporter, harness) = {
        let (provider, exporter) = install_meter_provider();
        let harness = harness_with(server.port(), None)?;
        (provider, exporter, harness)
    };

    harness.proxy("GET", PROXY_PATH, &[], b"").await?;
    provider.force_flush().context("the meter must flush")?;

    assert_eq!(
        point_values(&exporter, "oagw_requests_in_flight", &[("host", ALIAS)]),
        Vec::from([0]),
        "the request entered and left the pool exactly once"
    );
    Ok(())
}

/// A refused dial also leaves the gauge where it found it.
#[tokio::test]
async fn in_flight_returns_to_zero_after_a_failed_request() -> Result<()> {
    let _meter = meter_guard().await;
    let port = refused_port()?;
    let (provider, exporter, harness) = {
        let (provider, exporter) = install_meter_provider();
        let harness = harness_with(port, None)?;
        (provider, exporter, harness)
    };

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;
    assert_eq!(
        reply.status,
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "a dial that cannot be made is a 503, not a 500"
    );
    provider.force_flush().context("the meter must flush")?;

    assert_eq!(
        point_values(&exporter, "oagw_requests_in_flight", &[("host", ALIAS)]),
        Vec::from([0]),
        "the Drop guard runs on the failure path too"
    );
    Ok(())
}

/// A dial that fails is counted as a gateway failure under the problem type the
/// client was answered with — and, the request still having had an upstream, as
/// a request answered with the status the gateway chose.
#[tokio::test]
async fn an_upstream_failure_records_the_problem_type() -> Result<()> {
    let _meter = meter_guard().await;
    let port = refused_port()?;
    let capture = LogCapture::default();
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    let (provider, exporter, harness) = {
        let (provider, exporter) = install_meter_provider();
        let harness = harness_with(port, None)?;
        (provider, exporter, harness)
    };

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;
    assert_eq!(reply.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    provider.force_flush().context("the meter must flush")?;

    assert_eq!(
        counter_value(
            &exporter,
            "oagw_errors_total",
            &[
                ("host", ALIAS),
                ("http.route", ROUTE),
                ("error_type", LINK_UNAVAILABLE),
            ],
        ),
        1,
        "one failure, named by the problem type the client saw"
    );
    assert_eq!(
        counter_value(
            &exporter,
            "oagw_requests_total",
            &[
                ("host", ALIAS),
                ("http.request.method", "GET"),
                ("http.route", ROUTE),
                ("http.response.status_code", "503"),
            ],
        ),
        1,
        "no upstream status exists, so the counter carries the gateway's"
    );
    let line = audit_line(&capture, "[ERROR]").context("the failure was not audited")?;
    assert!(
        line.starts_with("[ERROR] "),
        "the level of an upstream failure: {line}"
    );
    assert!(
        line.contains("error_type="),
        "the record names the problem type: {line}"
    );
    Ok(())
}

/// A rate limit that refuses records the refusal, and the bucket state it gave
/// up on is the one number the usage gauge can carry.
#[tokio::test]
async fn a_rate_limited_request_records_the_refusal_and_the_bucket_usage() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path(ROUTE);
        then.status(200);
    });
    let capture = LogCapture::default();
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    let (provider, exporter, harness) = {
        let (provider, exporter) = install_meter_provider();
        let harness = harness_with(server.port(), Some(rate_limit(1, 1)))?;
        (provider, exporter, harness)
    };

    assert_eq!(
        harness.proxy("GET", PROXY_PATH, &[], b"").await?.status,
        axum::http::StatusCode::OK,
        "the first request spends the only token of the bucket"
    );
    let refused = harness.proxy("GET", PROXY_PATH, &[], b"").await?;
    assert_eq!(refused.status, axum::http::StatusCode::TOO_MANY_REQUESTS);
    provider.force_flush().context("the meter must flush")?;

    assert_eq!(
        counter_value(
            &exporter,
            "oagw_rate_limit_exceeded_total",
            &[("host", ALIAS), ("path", ROUTE)],
        ),
        1,
        "one refusal, under the alias and the route prefix"
    );
    let usage = gauge_value(
        &exporter,
        "oagw_rate_limit_usage_ratio",
        &[("host", ALIAS), ("path", ROUTE)],
    )
    .context("the refusal recorded no bucket usage")?;
    assert!(
        (usage - 1.0).abs() < 1e-9,
        "a bucket whose token was spent is fully used, not {usage}"
    );
    let refused = audit_line(&capture, "[WARN]").context("the refusal was not audited")?;
    assert!(
        refused.starts_with("[WARN] "),
        "the level of a refusal: {refused}"
    );
    let admitted = audit_line(&capture, "[INFO]").context("the admission was not audited")?;
    assert!(
        admitted.starts_with("[INFO] "),
        "the level of an answered request: {admitted}"
    );
    assert_eq!(mock.calls(), 1, "the refusal never reached the upstream");
    Ok(())
}

/// `http.route` is the matched route's prefix, and the raw request path — whose
/// segments are client input — is never a label.
#[tokio::test]
async fn the_route_label_is_the_matched_prefix_not_the_raw_path() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path("/v1/chat/extra");
        then.status(200);
    });
    let (provider, exporter, harness) = {
        let (provider, exporter) = install_meter_provider();
        let harness = harness_with(server.port(), None)?;
        (provider, exporter, harness)
    };

    let reply = harness
        .proxy("GET", &format!("{PROXY_PATH}/extra"), &[], b"")
        .await?;
    assert_eq!(reply.status, axum::http::StatusCode::OK);
    provider.force_flush().context("the meter must flush")?;

    assert_eq!(
        counter_value(
            &exporter,
            "oagw_requests_total",
            &[
                ("host", ALIAS),
                ("http.request.method", "GET"),
                ("http.route", ROUTE),
                ("http.response.status_code", "200"),
            ],
        ),
        1,
        "the suffix the client invented is not part of the route"
    );
    assert!(
        !any_label_value(&exporter, "oagw_requests_total", "/v1/chat/extra"),
        "the request path the client invented is a label value of no data point"
    );
    assert!(
        !any_label_value(&exporter, "oagw_request_duration_seconds", "/v1/chat/extra"),
        "nor of the histogram"
    );
    mock.assert();
    Ok(())
}

/// The audit record carries every field of DESIGN §4.3 and none of the things
/// it is forbidden to carry: not the query, not a header value, not the body.
#[tokio::test]
async fn the_audit_record_carries_the_fields_of_4_3_and_nothing_secret() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path(ROUTE);
        then.status(200).body("the upstream answer");
    });
    let capture = LogCapture::default();
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    let (provider, harness) = {
        let (provider, _exporter) = install_meter_provider();
        let harness = harness_with(server.port(), None)?;
        (provider, harness)
    };

    let reply = harness
        .proxy(
            "GET",
            &format!("{PROXY_PATH}?allowed=yes&api_key=shhh"),
            &[("traceparent", TRACEPARENT), ("x-private", "hunter2")],
            b"the request body",
        )
        .await?;
    assert_eq!(reply.status, axum::http::StatusCode::OK);
    provider.force_flush().context("the meter must flush")?;

    let line = audit_line(&capture, "[INFO]").context("the request was not audited")?;
    assert!(
        line.starts_with("[INFO] "),
        "the level of an answered request: {line}"
    );
    assert!(
        line.contains("event=oagw_proxy_request"),
        "the record is named: {line}"
    );
    assert!(
        line.contains("request_id=0af7651916cd43dd8448eb211c80319c"),
        "the record carries a correlation id: {line}"
    );
    assert!(
        line.contains(&format!("tenant_id={}", harness.tenant())),
        "the record carries the tenant: {line}"
    );
    assert!(
        line.contains("principal_id="),
        "the record carries the subject: {line}"
    );
    assert!(
        line.contains(&format!("host={ALIAS}")),
        "the record names the upstream: {line}"
    );
    assert!(
        line.contains(&format!("path={ROUTE}")),
        "the record names the path: {line}"
    );
    assert!(
        line.contains("method=GET"),
        "the record names the method: {line}"
    );
    assert!(
        line.contains("status=200"),
        "the record names the status: {line}"
    );
    assert!(
        line.contains("duration_ms="),
        "the record names the duration: {line}"
    );
    assert!(
        line.contains("request_size=16"),
        "the declared request size is recorded: {line}"
    );
    assert!(
        line.contains("response_size=19"),
        "the declared response size is recorded: {line}"
    );
    assert!(
        !line.contains("error_type="),
        "a success names no error: {line}"
    );
    assert!(
        !line.contains("shhh") && !line.contains("api_key"),
        "the record carries no query string: {line}"
    );
    assert!(
        !line.contains("hunter2"),
        "the record carries no header value: {line}"
    );
    assert!(
        !line.contains("the request body") && !line.contains("the upstream answer"),
        "the record carries no body: {line}"
    );
    mock.assert();
    Ok(())
}

/// The correlation id of the audit record is the one the trace headers named,
/// not an id the gateway invented.
#[tokio::test]
async fn the_audit_record_reports_the_trace_id_the_client_sent() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(GET).path(ROUTE);
        then.status(200);
    });
    let capture = LogCapture::default();
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    let (provider, harness) = {
        let (provider, _exporter) = install_meter_provider();
        let harness = harness_with(server.port(), None)?;
        (provider, harness)
    };

    harness
        .proxy("GET", PROXY_PATH, &[("traceparent", TRACEPARENT)], b"")
        .await?;
    provider.force_flush().context("the meter must flush")?;

    let line = audit_line(&capture, "[INFO]").context("the request was not audited")?;
    assert!(
        line.contains("request_id=0af7651916cd43dd8448eb211c80319c"),
        "the correlation id is the client's trace id: {line}"
    );
    Ok(())
}

/// A streamed request declares no length, so the audit record omits the
/// request size rather than counting the bytes that happened to arrive.
#[tokio::test]
async fn a_streamed_request_omits_the_size_it_declared() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(GET).path(ROUTE);
        then.status(200).body("chunked answer");
    });
    let capture = LogCapture::default();
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    let (provider, harness) = {
        let (provider, _exporter) = install_meter_provider();
        let harness = harness_with(server.port(), None)?;
        (provider, harness)
    };

    let reply = harness
        .proxy_chunked("GET", PROXY_PATH, &[b"chunk one"])
        .await?;
    assert_eq!(reply.status, axum::http::StatusCode::OK);
    provider.force_flush().context("the meter must flush")?;

    let line = audit_line(&capture, "[INFO]").context("the request was not audited")?;
    assert!(
        !line.contains("request_size="),
        "a request that declares no length names no size: {line}"
    );
    Ok(())
}

/// A request a rate limit refuses is still a request the data plane served, so
/// it is counted and timed under the status the client was given — the same
/// rule the module docs commit to for a dial failure.
///
/// The bucket is emptied by a request to a *different* route of the same
/// upstream, so the only request [`ROUTE`] ever sees is the refusal: the
/// histogram carries no status to tell two requests of one route apart.
#[tokio::test]
async fn a_rate_limited_request_is_counted_and_timed() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(GET);
        then.status(200);
    });
    let (provider, exporter, harness) = {
        let (provider, exporter) = install_meter_provider();
        let harness = ProxyHarness::new();
        let mut record = domain_upstream(
            harness.tenant(),
            ALIAS,
            Vec::from([loopback_endpoint(server.port())]),
            true,
        );
        record.rate_limit = Some(rate_limit(1, 1));
        let upstream = harness.seed_upstream(record);
        // The policy is the upstream's, so both routes share one bucket.
        for path in ["/v1/warmup", ROUTE] {
            harness
                .store()
                .insert_route_checked(domain_route(
                    harness.tenant(),
                    upstream,
                    &[HttpMethod::Get],
                    path,
                    &[],
                ))
                .with_context(|| format!("the route '{path}' must seed"))?;
        }
        (provider, exporter, harness)
    };

    let warmed = harness
        .proxy("GET", "/oagw/v1/proxy/api.vendor.com/v1/warmup", &[], b"")
        .await?;
    assert_eq!(warmed.status, axum::http::StatusCode::OK);
    let refused = harness.proxy("GET", PROXY_PATH, &[], b"").await?;
    assert_eq!(refused.status, axum::http::StatusCode::TOO_MANY_REQUESTS);
    provider.force_flush().context("the meter must flush")?;

    assert_eq!(
        counter_value(
            &exporter,
            "oagw_requests_total",
            &[
                ("host", ALIAS),
                ("http.request.method", "GET"),
                ("http.route", ROUTE),
                ("http.response.status_code", "429"),
            ],
        ),
        1,
        "the refusal is a request, answered with the status the limiter chose"
    );
    assert_eq!(
        histogram_count(
            &exporter,
            "oagw_request_duration_seconds",
            &[("host", ALIAS), ("http.route", ROUTE), ("phase", "total")],
        ),
        1,
        "and it took time"
    );
    assert_eq!(
        counter_value(
            &exporter,
            "oagw_requests_total",
            &[
                ("host", ALIAS),
                ("http.request.method", "GET"),
                ("http.route", "/v1/warmup"),
                ("http.response.status_code", "200"),
            ],
        ),
        1,
        "the request that emptied the bucket is a request of its own route"
    );
    Ok(())
}

/// A request a CORS policy refuses is counted and timed like any other refusal.
#[tokio::test]
async fn a_cors_refusal_is_counted_and_timed() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(GET).path(ROUTE);
        then.status(200);
    });
    let (provider, exporter, harness) = {
        let (provider, exporter) = install_meter_provider();
        let harness = ProxyHarness::new();
        let mut record = domain_upstream(
            harness.tenant(),
            ALIAS,
            Vec::from([loopback_endpoint(server.port())]),
            true,
        );
        record.cors = Some(cors_policy("https://app.example.com"));
        let upstream = harness.seed_upstream(record);
        harness
            .store()
            .insert_route_checked(domain_route(
                harness.tenant(),
                upstream,
                &[HttpMethod::Get],
                ROUTE,
                &[],
            ))
            .context("the test route must seed")?;
        (provider, exporter, harness)
    };

    let reply = harness
        .proxy(
            "GET",
            PROXY_PATH,
            &[("origin", "https://evil.example.org")],
            b"",
        )
        .await?;
    assert_eq!(reply.status, axum::http::StatusCode::FORBIDDEN);
    provider.force_flush().context("the meter must flush")?;

    assert_eq!(
        counter_value(
            &exporter,
            "oagw_requests_total",
            &[
                ("host", ALIAS),
                ("http.request.method", "GET"),
                ("http.route", ROUTE),
                ("http.response.status_code", "403"),
            ],
        ),
        1,
        "a refused origin is still a request the data plane served"
    );
    assert_eq!(
        histogram_count(
            &exporter,
            "oagw_request_duration_seconds",
            &[("host", ALIAS), ("http.route", ROUTE), ("phase", "total")],
        ),
        1,
        "and it took time"
    );
    Ok(())
}

/// A method the data plane does not register is refused by the router's
/// fallback, and the fallback audits it: the same record, the same correlation
/// id, the same 404 (PRD §9, complete audit trail).
#[tokio::test]
async fn an_unregistered_method_is_audited_like_any_other_request() -> Result<()> {
    let _meter = meter_guard().await;
    let capture = LogCapture::default();
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    // No meter is installed: the fallback is audited, and no instrument can
    // attribute a request it never attributed to a host and a route.
    let harness = harness_with(1, None)?;

    let reply = harness
        .proxy("TRACE", PROXY_PATH, &[("traceparent", TRACEPARENT)], b"")
        .await?;
    assert_eq!(reply.status, axum::http::StatusCode::NOT_FOUND);

    let line = audit_line(&capture, "[INFO]").context("the refusal was not audited")?;
    assert!(
        line.contains("method=TRACE"),
        "the record names the method: {line}"
    );
    assert!(
        line.contains("status=404"),
        "the record names the status: {line}"
    );
    assert!(
        line.contains("event=oagw_proxy_request"),
        "the record is named: {line}"
    );
    assert!(
        line.contains("request_id=0af7651916cd43dd8448eb211c80319c"),
        "the record carries a correlation id: {line}"
    );
    assert!(
        line.contains(&format!("tenant_id={}", harness.tenant())),
        "the record carries the tenant: {line}"
    );
    assert!(
        line.contains(&format!("host={ALIAS}")) && line.contains(&format!("path={ROUTE}")),
        "the record names what the request asked for: {line}"
    );
    assert!(
        line.contains(&format!(
            "error_message=no upstream of the calling tenant answers to the alias '{ALIAS}'"
        )),
        "the 404 carries the message the client was given: {line}"
    );
    Ok(())
}

/// A pool dialled through the target-host header reports `explicit_header` and
/// the pinning counter — at the dial, so a request that never dialled reports
/// nothing.
#[tokio::test]
async fn a_pinned_endpoint_is_reported_at_the_dial() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path(ROUTE);
        then.status(200);
    });
    let (provider, exporter, harness, upstream) = {
        let (provider, exporter) = install_meter_provider();
        let (harness, upstream) = seeded(
            Vec::from([
                loopback_endpoint(server.port()),
                loopback_endpoint(server.port()),
            ]),
            None,
        )?;
        (provider, exporter, harness, upstream)
    };

    let reply = harness
        .proxy(
            "GET",
            PROXY_PATH,
            &[("x-oagw-target-host", "127.0.0.1")],
            b"",
        )
        .await?;
    assert_eq!(reply.status, axum::http::StatusCode::OK);
    provider.force_flush().context("the meter must flush")?;

    assert_eq!(
        counter_value(
            &exporter,
            "oagw_routing_endpoint_selected",
            &[
                ("upstream_id", &upstream.to_string()),
                ("endpoint_host", "127.0.0.1"),
                ("selection_method", "explicit_header"),
            ],
        ),
        1,
        "one dial, pinned by the header"
    );
    assert_eq!(
        counter_value(
            &exporter,
            "oagw_routing_target_host_used",
            &[
                ("upstream_id", &upstream.to_string()),
                ("endpoint_host", "127.0.0.1")
            ],
        ),
        1,
        "the header was what dialled it"
    );
    mock.assert();
    Ok(())
}

/// A pool of more than one endpoint dialled without the header reports the
/// round-robin cursor, and no pinning.
#[tokio::test]
async fn a_round_robin_pool_reports_the_selection_method() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let mock = server.mock(|when, then| {
        when.method(GET).path(ROUTE);
        then.status(200);
    });
    let (provider, exporter, harness, upstream) = {
        let (provider, exporter) = install_meter_provider();
        let (harness, upstream) = seeded(
            Vec::from([
                loopback_endpoint(server.port()),
                loopback_endpoint(server.port()),
            ]),
            None,
        )?;
        (provider, exporter, harness, upstream)
    };

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;
    assert_eq!(reply.status, axum::http::StatusCode::OK);
    provider.force_flush().context("the meter must flush")?;

    assert_eq!(
        counter_value(
            &exporter,
            "oagw_routing_endpoint_selected",
            &[
                ("upstream_id", &upstream.to_string()),
                ("endpoint_host", "127.0.0.1"),
                ("selection_method", "round_robin"),
            ],
        ),
        1,
        "the cursor decided, so it says so"
    );
    assert_eq!(
        counter_value(
            &exporter,
            "oagw_routing_target_host_used",
            &[("upstream_id", &upstream.to_string())]
        ),
        0,
        "no header was involved"
    );
    mock.assert();
    Ok(())
}

/// A request that sends no trace header is still given a correlation id: the
/// id of the span it lives in.
#[tokio::test]
async fn a_request_without_a_trace_header_still_gets_a_correlation_id() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(GET).path(ROUTE);
        then.status(200);
    });
    let capture = LogCapture::default();
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    let harness = harness_with(server.port(), None)?;

    harness.proxy("GET", PROXY_PATH, &[], b"").await?;

    let line = audit_line(&capture, "[INFO]").context("the request was not audited")?;
    assert!(
        line.contains("request_id=") && !line.contains("request_id=<none>"),
        "the span the request lives in is the fallback id: {line}"
    );
    Ok(())
}

/// A failed request's record names the message behind the problem type, which
/// is what makes a shared slug actionable.
#[tokio::test]
async fn a_failure_record_names_the_message_behind_the_problem_type() -> Result<()> {
    let _meter = meter_guard().await;
    let port = refused_port()?;
    let capture = LogCapture::default();
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    let harness = harness_with(port, None)?;

    let reply = harness.proxy("GET", PROXY_PATH, &[], b"").await?;
    assert_eq!(reply.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);

    let line = audit_line(&capture, "[ERROR]").context("the failure was not audited")?;
    assert!(
        line.contains("error_message=upstream request failed"),
        "the record carries the detail the problem document carries: {line}"
    );
    assert!(line.contains("error_type="), "and its type: {line}");
    Ok(())
}

/// The span the audit event is emitted in carries what the data plane decided
/// and nothing the client sent: the production subscriber formats the span into
/// the same ingested record as the event.
#[tokio::test]
async fn the_request_span_carries_no_header_value_and_no_query() -> Result<()> {
    let _meter = meter_guard().await;
    let server = MockServer::start();
    let _mock = server.mock(|when, then| {
        when.method(GET).path(ROUTE);
        then.status(200);
    });
    let capture = LogCapture::default();
    let _subscriber = tracing::subscriber::set_default(capture.clone());
    let harness = harness_with(server.port(), None)?;

    let reply = harness
        .proxy(
            "GET",
            &format!("{PROXY_PATH}?api_key=shhh"),
            &[("x-private", "hunter2")],
            b"",
        )
        .await?;
    assert_eq!(reply.status, axum::http::StatusCode::OK);

    let spans = capture.spans();
    let request_span = spans
        .iter()
        .find(|span| span.contains("oagw_proxy_request"))
        .context("the request opened no span")?;
    assert!(
        request_span.contains(&format!("alias={ALIAS}")),
        "the span names the alias: {spans:?}"
    );
    assert!(
        request_span.contains("method=GET"),
        "the span names the method: {spans:?}"
    );
    assert!(
        request_span.contains("status=200"),
        "the span names the status: {spans:?}"
    );
    assert!(
        !request_span.contains("authority="),
        "the span carries no header, not even under another name: {spans:?}"
    );
    assert!(
        !spans
            .iter()
            .any(|span| span.contains("hunter2") || span.contains("shhh")),
        "no span carries a header value or a query string: {spans:?}"
    );
    Ok(())
}

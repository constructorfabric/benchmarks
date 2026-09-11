//! The correlation identifier of the OAGW gear's records (entry 2.7).
//!
//! `cpt-cf-oagw-algo-correlation-propagate` reads the identifier the request
//! context already carries and places it on the audit line and on the problem
//! body's `trace_id` extension field: it never reads a header value to obtain
//! it, never generates one, and injects or strips no correlation, routing or
//! tenant header on an outbound request — the outbound header set is the
//! entry-2.5 RequestId transform's output, not this feature's.
//!
//! The identifier is the only header-derived value on any line this feature
//! emits, and no metric label carries it.

use crate::infra::proxy::context::RequestContext;

// @cpt-begin:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-01
// The actor sends the proxy request optionally carrying this header: it is
// read by the entry-2.5 RequestId transform, never by this feature, which
// takes the identifier from the request context alone.
/// The header the correlation identifier is propagated with, which the
/// entry-2.5 RequestId transform owns.
pub const CORRELATION_HEADER: &str = "x-request-id";
// @cpt-end:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-01

/// The correlation identifier of one request
/// (`cpt-cf-oagw-algo-correlation-propagate`).
///
/// The identifier the entry-2.5 RequestId transform recorded on the context
/// (`inst-ari-05`) is the correlation identifier when it is present, because
/// the outbound request and the response carry that one; otherwise the
/// identifier the pipeline generated at `inst-pe-req-03` is used, which is
/// present on every request including one rejected before authentication. An
/// identifier that arrives empty is replaced by the context's own identifier
/// and is never recorded as an empty string.
#[must_use]
pub fn correlation_id(context: &RequestContext) -> Option<String> {
    // @cpt-begin:cpt-cf-oagw-algo-correlation-propagate:p1:inst-ob-acorr-02
    // @cpt-begin:cpt-cf-oagw-algo-correlation-propagate:p1:inst-ob-acorr-03
    let propagated = context.request_id.as_deref();
    match propagated {
        Some(identifier) if !identifier.is_empty() => Some(identifier.to_owned()),
        _ => {
            // @cpt-begin:cpt-cf-oagw-algo-correlation-propagate:p1:ob-acorr-04
            // The pipeline's own identifier, opened at `inst-pe-req-03` and
            // present on every request including one rejected before
            // authentication. Recorded as it arrived, never truncated or
            // escaped into another field.
            let opened = context.trace_id.as_str();
            (!opened.is_empty()).then(|| opened.to_owned())
            // @cpt-end:cpt-cf-oagw-algo-correlation-propagate:p1:ob-acorr-04
        }
    }
    // @cpt-end:cpt-cf-oagw-algo-correlation-propagate:p1:inst-ob-acorr-03
    // @cpt-end:cpt-cf-oagw-algo-correlation-propagate:p1:inst-ob-acorr-02
}

// The identifier is the only header-derived value the records carry: a record
// that carries a second header-derived field would fail the audit line's key
// check, which is the only way a header value can reach a line.
// @cpt-begin:cpt-cf-oagw-algo-correlation-propagate:p1:inst-ob-acorr-05
// @cpt-begin:cpt-cf-oagw-algo-correlation-propagate:p1:inst-ob-acorr-06
// @cpt-begin:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-04
// The outbound header set is the entry-2.5 RequestId transform's output: this
// algorithm injects no correlation, routing or tenant header into an outbound
// request and strips none, and it records no second identifier, so no metric
// label carries one either.
// @cpt-end:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-04
// @cpt-end:cpt-cf-oagw-algo-correlation-propagate:p1:inst-ob-acorr-06
// @cpt-end:cpt-cf-oagw-algo-correlation-propagate:p1:inst-ob-acorr-05

// @cpt-begin:cpt-cf-oagw-algo-correlation-propagate:p1:inst-ob-acorr-07
// @cpt-begin:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-09
// The identifier is returned as recorded: one identifier per request, on the
// request context, on the outbound request and the response when the
// entry-2.5 transform propagated it, on the audit line's `request_id` key and
// in the problem body's `trace_id` extension field on a gateway failure. No
// second identifier is added and no metric label carries one.
// @cpt-end:cpt-cf-oagw-flow-correlation-propagation:p1:inst-ob-corr-09
// @cpt-end:cpt-cf-oagw-algo-correlation-propagate:p1:inst-ob-acorr-07

// @cpt-begin:cpt-cf-oagw-dod-correlation-ids:p1:inst-full
// The identifier is present on every recorded request, including one rejected
// before authentication, because the fallback below is the identifier the
// pipeline opened before resolution; it is read from the request context and
// is never generated, rewritten, injected or stripped here.
// @cpt-end:cpt-cf-oagw-dod-correlation-ids:p1:inst-full

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::obs::audit::{AuditInput, AuditRecord};
    use std::time::Duration;

    #[test]
    fn the_transform_recorded_identifier_is_the_correlation_identifier() {
        let mut context = RequestContext::new(
            "opened-by-pipeline".to_owned(),
            "/oagw/v1/proxy/api.vendor.com/v1".to_owned(),
            "GET".to_owned(),
        );
        assert_eq!(
            correlation_id(&context).as_deref(),
            Some("opened-by-pipeline"),
            "the identifier the pipeline generated at inst-pe-req-03"
        );
        context.request_id = Some("recorded-by-transform".to_owned());
        assert_eq!(
            correlation_id(&context).as_deref(),
            Some("recorded-by-transform"),
            "the identifier the RequestId transform recorded at inst-ari-05"
        );
    }

    #[test]
    fn an_empty_identifier_is_replaced_and_never_recorded_as_empty() {
        let mut context = RequestContext::new(
            String::new(),
            "/oagw/v1/proxy/api.vendor.com/v1".to_owned(),
            "GET".to_owned(),
        );
        context.request_id = Some(String::new());
        assert_eq!(correlation_id(&context), None);
    }

    #[test]
    fn the_identifier_reaches_the_audit_line_and_no_metric_label() {
        let mut context = RequestContext::new(
            "corr-1".to_owned(),
            "/oagw/v1/proxy/api.vendor.com/v1".to_owned(),
            "GET".to_owned(),
        );
        context.request_id = Some("corr-1".to_owned());
        context.alias = Some("api.vendor.com".to_owned());
        let input = AuditInput {
            context: &context,
            status: 200,
            duration: Duration::from_millis(1),
            response_bytes: None,
            error: None,
            tenant_id: None,
            principal_id: None,
            stream: None,
        };
        let record = AuditRecord::build(&input);
        assert_eq!(record.request_id.as_deref(), Some("corr-1"));
        let line = record.to_line().expect("the line serializes");
        assert!(line.contains("\"request_id\":\"corr-1\""), "{line}");
        // The metric label set carries no request identifier: the registry's
        // recorded label keys have no such key.
        for key in super::super::metrics::FAMILIES {
            assert!(
                !key.label_keys.contains(&"request_id"),
                "{} carries no request identifier",
                key.name
            );
        }
        // One identifier in one place: the key set names `request_id` once and
        // carries no second correlation field.
        let keys = record.keys();
        assert_eq!(
            keys.iter().filter(|key| **key == "request_id").count(),
            1,
            "{keys:?}"
        );
        assert!(!keys.contains(&"trace_id"), "{keys:?}");
        assert!(!keys.contains(&"correlation_id"), "{keys:?}");
    }

    #[test]
    fn the_correlation_header_is_the_one_the_transform_owns() {
        assert_eq!(CORRELATION_HEADER, "x-request-id");
    }
}

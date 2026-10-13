//! The OpenTelemetry trace id of the current request.

use opentelemetry::trace::TraceContextExt;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::domain::ports::TraceIdSource;

/// Reads the trace id from the OpenTelemetry context of the current `tracing` span.
pub struct OtelTraceIds;

impl TraceIdSource for OtelTraceIds {
    fn current_trace_id(&self) -> Option<String> {
        let context = tracing::Span::current().context();
        let span = context.span();
        let span_context = span.span_context();
        span_context
            .is_valid()
            .then(|| span_context.trace_id().to_string())
    }
}

#[cfg(test)]
#[path = "trace_tests.rs"]
mod tests;

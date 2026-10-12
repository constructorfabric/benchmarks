use super::OtelTraceIds;
use crate::domain::ports::TraceIdSource;

#[test]
fn without_an_active_opentelemetry_span_there_is_no_trace_id() {
    assert_eq!(OtelTraceIds.current_trace_id(), None);
}

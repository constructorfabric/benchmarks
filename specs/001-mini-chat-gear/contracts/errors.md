# REST error contract (ADR-0004; PRD §7.2 table; DESIGN §3.3)

RFC 9457 Problem `{type,title,status,detail,instance,trace_id,context}` from toolkit-canonical-errors.
Machine reasons: `context.reason` (aborted, permission_denied), `context.field_violations[].{field,reason}`
(invalid_argument, out_of_range), `context.violations[]` (failed_precondition {subject,description,type};
resource_exhausted {subject,description}), `context.resource_type` (not_found), `context.resource_name`
(already_exists), `context.retry_after_seconds` (service_unavailable, with `Retry-After` header).

Resource types: `gts.cf.core.mini_chat.{chat,message,turn,attachment,model}.v1~`, OData `gts.cf.core.odata.query.v1~`.
Full condition table: PRD.md lines 1158–1201 (authoritative), mirrored in research-design-notes.md.

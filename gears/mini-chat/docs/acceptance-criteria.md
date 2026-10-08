# Acceptance Criteria: Mini Chat

High-level coverage checklist for `mini-chat`, grouped by area. Each item names a
decision already specified in `PRD.md` / `DESIGN.md` / `docs/ADR/*.md` that MUST
be both implemented and tested — it is a pointer to a requirement, not a
restatement of it. See the design documents for the exact behavior, codes and
values.

---

## Principles & Constraints

- [ ] Tenant and owner isolation enforced on every resource
- [ ] Context window budget enforced, for both the input message and the full assembled request
- [ ] Streaming responses are never buffered before relaying
- [ ] A chat's model is immutable once set
- [ ] Quota is checked before any outbound provider call

## Chat CRUD

- [ ] Create / get / list / update / delete lifecycle for chats, including model and title validation
- [ ] List endpoint supports filtering, ordering, and pagination, with validation of malformed input
- [ ] Chat ordering reflects most recent activity

## Messages API

- [ ] List messages with filtering, ordering, and pagination
- [ ] Message response contract: identity, attachments, and reaction fields are always consistently present
- [ ] Message count and chronological ordering are tracked correctly across turns

## Streaming: Send Message

- [ ] Send-message endpoint streams a response, correlated by a request id
- [ ] Preflight validation (content, attachments, limits) runs before any provider call
- [ ] Assistant message and usage are persisted once a stream completes

## SSE Event Contract

- [ ] Full streaming event contract (start, delta, tool activity, citations, completion, error, keepalive) and its ordering
- [ ] Completion event exposes usage and quota/downgrade outcome without leaking internal identifiers
- [ ] Error event is terminal and carries a sanitized message

## Idempotency & Replay

- [ ] Replaying a known request id returns the stored result without side effects (no provider call, no quota change)
- [ ] Conflicting reuse of a request id across turn states is rejected consistently
- [ ] Replay is checked before the parallel-turn guard

## Parallel Turn Enforcement

- [ ] Only one turn may run per chat at a time
- [ ] A new turn is accepted once the previous one reaches a terminal state

## Turn Mutations

- [ ] Retry / edit / delete act only on the latest, terminal turn
- [ ] A mutation goes through the full send pipeline (quota, context budget, attachment checks) and gets a new request id
- [ ] Concurrent mutations resolve deterministically
- [ ] Mutated turns correctly carry forward attachment and tool-usage history

## Turn Lifecycle

- [ ] Turn state machine (running → completed / cancelled / failed) is consistent end to end
- [ ] Partial and null-content cases on cancellation or failure are handled correctly

## Attachments

- [ ] Upload / get / delete lifecycle for documents and images, with size, type, and per-chat limit validation
- [ ] Asynchronous indexing lifecycle, including provider failure and timeout handling
- [ ] Attachments are correctly made available to the relevant provider tools
- [ ] Cleanup and abandoned-upload recovery behave correctly under failure

## Models API

- [ ] Read-only model list/get reflects only enabled catalog entries, without exposing internal fields

## Reactions API

- [ ] Set/remove reaction on assistant messages only, idempotently

## Quota Status API

- [ ] Quota status reporting is accurate and consistent with actual usage

## Quota Enforcement

- [ ] Reserve-before-execute quota flow enforced on every provider call
- [ ] Tier downgrade applied when a higher tier is exhausted
- [ ] Credits and tokens are accounted correctly per model and tier

## Settlement & Finalization

- [ ] Every terminal outcome settles quota exactly once, using actual or estimated usage as appropriate
- [ ] Usage is published reliably and exactly once per turn

## Context Assembly

- [ ] System prompt, thread summary, and recent history are assembled and truncated deterministically within budget
- [ ] Tool availability and guidance are reflected correctly in the assembled request

## Error Mapping & Sanitization

- [ ] All errors map to the canonical error contract, consistently across REST and streaming
- [ ] Provider-originated error details are sanitized before reaching the client

## Web Search

- [ ] Web search tool use is reported, cited, accounted, and quota-limited correctly

## Cleanup & Recovery

- [ ] Chat deletion triggers reliable background cleanup of provider-side resources
- [ ] Thread summary generation, failure/retry, and mutation-driven invalidation behave correctly

## Authorization

- [ ] Every operation is scoped to its owner; cross-tenant or foreign access is rejected consistently

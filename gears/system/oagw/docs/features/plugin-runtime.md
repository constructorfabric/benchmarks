# Feature: Plugin Runtime and Rate Limiting


<!-- toc -->

- [1. Feature Context](#1-feature-context)
  - [1.1 Overview](#11-overview)
  - [1.2 Purpose](#12-purpose)
  - [1.3 Actors](#13-actors)
  - [1.4 References](#14-references)
- [2. Actor Flows (CDSL)](#2-actor-flows-cdsl)
  - [Proxied Request With Injected Credentials](#proxied-request-with-injected-credentials)
  - [Request Rejected By A Guard Plugin](#request-rejected-by-a-guard-plugin)
  - [Request Rejected By Rate Limiting](#request-rejected-by-rate-limiting)
- [3. Processes / Business Logic (CDSL)](#3-processes--business-logic-cdsl)
  - [Chain Assembly From Merged Bindings](#chain-assembly-from-merged-bindings)
  - [Plugin Reference Resolution](#plugin-reference-resolution)
  - [No-Operation Auth Injection](#no-operation-auth-injection)
  - [API Key Credential Injection](#api-key-credential-injection)
  - [Client Credentials Token Acquisition](#client-credentials-token-acquisition)
  - [Token Cache Lookup](#token-cache-lookup)
  - [Required Headers Guard Check](#required-headers-guard-check)
  - [Request Identifier Propagation](#request-identifier-propagation)
  - [Effective Rate Limit Selection](#effective-rate-limit-selection)
  - [Token Bucket Admission](#token-bucket-admission)
  - [Rate Limit Response Headers](#rate-limit-response-headers)
  - [Plugin And Rate Limit Failure Mapping](#plugin-and-rate-limit-failure-mapping)
- [4. States (CDSL)](#4-states-cdsl)
  - [Cached Access Token State Machine](#cached-access-token-state-machine)
  - [Token Bucket State Machine](#token-bucket-state-machine)
- [5. Definitions of Done](#5-definitions-of-done)
  - [Single Chain Invocation Point](#single-chain-invocation-point)
  - [Deterministic Chain Order](#deterministic-chain-order)
  - [Three Plugin Kinds With Bounded Capabilities](#three-plugin-kinds-with-bounded-capabilities)
  - [Plugin Reference Resolution And Unknown Identifiers](#plugin-reference-resolution-and-unknown-identifiers)
  - [Built-In Static Credential Auth Plugins](#built-in-static-credential-auth-plugins)
  - [Built-In Client Credentials Auth Plugins](#built-in-client-credentials-auth-plugins)
  - [Token Cache Behaviour](#token-cache-behaviour)
  - [Credential Resolution By Reference Only](#credential-resolution-by-reference-only)
  - [Built-In Required Headers Guard Plugin](#built-in-required-headers-guard-plugin)
  - [Built-In Request Identifier Transform Plugin](#built-in-request-identifier-transform-plugin)
  - [Effective Rate Limit And Counter Keys](#effective-rate-limit-and-counter-keys)
  - [Token Bucket Admission](#token-bucket-admission-1)
  - [Rate Limit Rejection Response](#rate-limit-rejection-response)
  - [Chain Failure Mapping](#chain-failure-mapping)
- [6. Acceptance Criteria](#6-acceptance-criteria)

<!-- /toc -->

- [ ] `p2` - **ID**: `cpt-cf-oagw-featstatus-plugin-runtime-implemented`

<!-- reference to DECOMPOSITION entry -->
- [ ] `p2` - `cpt-cf-oagw-feature-plugin-runtime`

## 1. Feature Context

### 1.1 Overview

This feature executes the auth, guard and transform plugin chain against every proxied request, and admits or rejects that request through token-bucket rate limiting. It injects upstream credentials by reference, enforces presence-only header guards, propagates a correlation identifier, and answers exhausted buckets with the documented rate-limit response.

The chain runs at the single invocation point that the HTTP proxy data-plane feature establishes inside the proxy request lifecycle. Plain HTTP request and response exchanges, server-sent-event streams and WebSocket upgrades all reach that same invocation point, so plugin behaviour is identical across the three transport modes.

Four areas stay outside this feature. Sandboxed execution of custom Starlark plugin source is excluded entirely, so only built-in plugins execute here. Runtime gRPC proxying and the Redis second-level control-plane cache are excluded. Distributed, cross-node synchronization of rate-limit counters is excluded, leaving per-instance counters only. Retrying the upstream call after an upstream 401 response is deferred by the OAuth2 client-credentials decision record, and is therefore not implemented.

### 1.2 Purpose

Application developers must reach external services without ever handling an API key or a bearer token, so credential injection has to happen inside the gateway. Operators must cap outbound traffic to protect cost budgets and third-party service agreements, so admission control has to happen before the upstream call. This feature supplies both, plus the ordered extension points that guards and transforms hook into.

Plugin binding surfaces, upstream and route configuration merge, and the proxy request lifecycle already exist. This feature adds the runtime that consumes them: it resolves plugin references, orders the resulting chain, executes each built-in plugin, and maps every rejection or failure onto the gateway error contract.

**Requirements**: `cpt-cf-oagw-fr-auth-injection`, `cpt-cf-oagw-fr-rate-limiting`, `cpt-cf-oagw-fr-plugin-system`, `cpt-cf-oagw-fr-builtin-plugins`, `cpt-cf-oagw-fr-header-transform`, `cpt-cf-oagw-nfr-credential-isolation`, `cpt-cf-oagw-nfr-starlark-sandbox`, `cpt-cf-oagw-contract-cred-store`

**Principles**: `cpt-cf-oagw-principle-cred-isolation`, `cpt-cf-oagw-principle-no-retry`, `cpt-cf-oagw-principle-plugin-immutable`, `cpt-cf-oagw-principle-rfc9457`, `cpt-cf-oagw-principle-error-source`

`cpt-cf-oagw-nfr-starlark-sandbox` is cited as a boundary marker only. Custom-plugin sandboxing is excluded from this feature, so the requirement is satisfied here by never executing stored plugin source at request time.

**Design**: `cpt-cf-oagw-component-model`, `cpt-cf-oagw-seq-proxy-flow`, `cpt-cf-oagw-design-domain-model`, `cpt-cf-oagw-adr-plugin-system`, `cpt-cf-oagw-adr-rate-limiting`, `cpt-cf-oagw-adr-oauth2-client-credentials-auth-plugin`, `cpt-cf-oagw-adr-required-headers-guard-plugin`, `cpt-cf-oagw-adr-state-management`

### 1.3 Actors

| Actor | Role in Feature |
|-------|-----------------|
| `cpt-cf-oagw-actor-app-developer` | Sends proxy requests, receives injected credentials transparently, and observes guard and rate-limit rejections |
| `cpt-cf-oagw-actor-cred-store` | Resolves each `cred://` reference to secret material at request time, subject to tenant access checks |
| `cpt-cf-oagw-actor-upstream-service` | Receives the credentialed, transformed request and returns the response that response-phase plugins inspect |
| `cpt-cf-oagw-actor-platform-operator` | Binds built-in plugins and sets the rate limits that descendant tenants cannot exceed |
| `cpt-cf-oagw-actor-tenant-admin` | Appends tenant plugin bindings and sets stricter tenant-level rate limits within granted permissions |

### 1.4 References

- **PRD**: [PRD.md](../PRD.md)
- **Design**: [DESIGN.md](../DESIGN.md)
- **Dependencies**: `cpt-cf-oagw-feature-http-proxy`, `cpt-cf-oagw-feature-plugin-management`

The dependency on `cpt-cf-oagw-feature-http-proxy` supplies the proxy request lifecycle and the single chain invocation point that this feature hooks into, plus the error table and the error-source header used for every rejection. The dependency on `cpt-cf-oagw-feature-plugin-management` supplies the plugin definitions and binding records that reference resolution reads. Merged effective configuration arrives from the configuration-resolution feature through the proxy lifecycle, carrying one merged `rate_limit` value plus the plugin bindings concatenated as upstream, then route, then tenant. That feature owns the merge, so this feature never recomputes sharing modes and never re-derives a hierarchy-wide strictest limit.

**Limitations**: The `queue` and `degrade` rate-limit strategies are schema-accepted but not implemented here, and each falls back to reject semantics. Custom Starlark plugin execution, runtime gRPC proxying, the Redis second-level cache and distributed counter synchronization are excluded as well.

Routes named in this document are gear-relative, for example `/oagw/v1/proxy/{alias}/{path}`, and never carry an `/api` prefix. Control-plane state, including plugin bindings, is held in process because the graded deployment configures no database.

## 2. Actor Flows (CDSL)

User-facing interactions that start with an actor (human or external system) and describe the end-to-end flow of a use case. Each flow has a triggering actor and shows how the system responds to actor actions.

**Use cases**:

- [ ] `p2` - `cpt-cf-oagw-usecase-proxy-request`
- [ ] `p2` - `cpt-cf-oagw-usecase-rate-limit-exceeded`

### Proxied Request With Injected Credentials

- [ ] `p2` - **ID**: `cpt-cf-oagw-flow-credentialed-proxy-request`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The developer sends a request without any credential, and the upstream receives the credential that the auth binding resolved from the credential store.
- A second request within the token cache lifetime reuses the cached access token, so no second token-endpoint call occurs.
- Upstream-level transform bindings run before route-level transform bindings, and the response returns with the correlation identifier set.

**Error Scenarios**:
- The referenced secret does not exist, so the request fails with status 500 and the secret-not-found problem document.
- The credential store denies access to the reference for the calling tenant, so the request fails with status 401.
- The token endpoint is unreachable, so the request fails with status 401 and no token is cached.
- The binding configuration is structurally invalid, so the request fails with status 503 and the plugin-not-found problem document.

**Steps**:
1. [ ] - `p2` - Developer sends a proxy request carrying no upstream credential - `inst-credentialed-proxy-request-send`
2. [ ] - `p2` - API: `{METHOD} /oagw/v1/proxy/{alias}/{path}` (inbound request, proxied response) - `inst-credentialed-proxy-request-endpoint`
3. [ ] - `p2` - Proxy lifecycle hands the merged effective configuration to the chain invocation point - `inst-credentialed-proxy-request-invoke`
4. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-chain-assembly` to order auth, guard and transform bindings - `inst-credentialed-proxy-request-assemble`
5. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-runtime-plugin-resolution` for every binding in the assembled chain - `inst-credentialed-proxy-request-resolve`
6. [ ] - `p2` - **IF** the effective auth binding is an api-key binding - `inst-credentialed-proxy-request-if-apikey`
   1. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-apikey-injection` - `inst-credentialed-proxy-request-call-apikey`
7. [ ] - `p2` - **ELSE** the effective auth binding is a client-credentials binding - `inst-credentialed-proxy-request-else-oauth2`
   1. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-oauth2-token-acquisition` - `inst-credentialed-proxy-request-call-oauth2`
8. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-token-bucket-admission` at the start of the guard phase - `inst-credentialed-proxy-request-admit`
9. [ ] - `p2` - Execute guard bindings in chain order, all of which admit the request - `inst-credentialed-proxy-request-guards`
10. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-request-id-propagation` in the request transform phase - `inst-credentialed-proxy-request-reqid`
11. [ ] - `p2` - Forward the credentialed, transformed request to the resolved upstream endpoint - `inst-credentialed-proxy-request-forward`
12. [ ] - `p2` - Execute response transform bindings against the upstream response head - `inst-credentialed-proxy-request-response-phase`
13. [ ] - `p2` - **RETURN** the upstream response with the correlation identifier and rate-limit headers - `inst-credentialed-proxy-request-return`

### Request Rejected By A Guard Plugin

- [ ] `p2` - **ID**: `cpt-cf-oagw-flow-guard-rejected-request`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- A request missing a configured required header is rejected with status 400 before the upstream call is attempted.
- An upstream response missing a configured required response header is rejected with status 502 before reaching the caller.
- A request satisfying every configured header name proceeds to the transform phase unchanged.

**Error Scenarios**:
- A bound guard identifier has no backing implementation, so the request fails with status 503 and the plugin-not-found problem document.
- Guard configuration lists only blank header names, so the phase admits the request and the operator sees no enforcement.

**Steps**:
1. [ ] - `p2` - Developer sends a proxy request omitting a configured required header - `inst-guard-rejected-request-send`
2. [ ] - `p2` - API: `{METHOD} /oagw/v1/proxy/{alias}/{path}` (inbound request, problem document) - `inst-guard-rejected-request-endpoint`
3. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-chain-assembly` and inject credentials as in the credentialed flow - `inst-guard-rejected-request-prefix`
4. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-token-bucket-admission`, which admits the request - `inst-guard-rejected-request-admit`
5. [ ] - `p2` - **FOR EACH** guard binding in chain order - `inst-guard-rejected-request-loop`
   1. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-required-headers-check` in the request phase - `inst-guard-rejected-request-check`
6. [ ] - `p2` - **IF** a guard reports a rejection - `inst-guard-rejected-request-if-reject`
   1. [ ] - `p2` - Abandon the chain without contacting the upstream service - `inst-guard-rejected-request-abandon`
   2. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-plugin-failure-mapping` with the rejection and its phase - `inst-guard-rejected-request-map`
7. [ ] - `p2` - **ELSE** every guard admits the request - `inst-guard-rejected-request-else`
   1. [ ] - `p2` - Continue into the request transform phase - `inst-guard-rejected-request-continue`
8. [ ] - `p2` - **RETURN** status 400 with the problem document and `X-OAGW-Error-Source: gateway` - `inst-guard-rejected-request-return`

### Request Rejected By Rate Limiting

- [ ] `p2` - **ID**: `cpt-cf-oagw-flow-rate-limited-request`

**Actor**: `cpt-cf-oagw-actor-app-developer`

**Success Scenarios**:
- The first request finds sufficient tokens, consumes its cost, and reaches the upstream service normally.
- A later request against an exhausted bucket is rejected with status 429 and a positive `Retry-After` value.
- After enough time elapses for replenishment, an equivalent request is admitted again without operator intervention.

**Error Scenarios**:
- The configured cost exceeds the effective burst capacity, so every request against that counter is rejected while the configuration stands.
- The process restarts, so in-process counters reset and previously exhausted buckets admit traffic immediately.

**Steps**:
1. [ ] - `p2` - Developer sends proxy requests faster than the effective sustained rate allows - `inst-rate-limited-request-send`
2. [ ] - `p2` - API: `{METHOD} /oagw/v1/proxy/{alias}/{path}` (inbound request, problem document) - `inst-rate-limited-request-endpoint`
3. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-effective-rate-limit` to obtain rate, window, capacity and cost - `inst-rate-limited-request-effective`
4. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-token-bucket-admission` with the counter key and cost - `inst-rate-limited-request-admit`
5. [ ] - `p2` - **IF** the bucket holds at least the request cost - `inst-rate-limited-request-if-admit`
   1. [ ] - `p2` - Deduct the cost and continue the guard phase - `inst-rate-limited-request-deduct`
6. [ ] - `p2` - **ELSE** the bucket is exhausted, so reject semantics apply for every configured strategy - `inst-rate-limited-request-else`
   1. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-rate-limit-headers` to compute the advisory header values - `inst-rate-limited-request-headers`
   2. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-plugin-failure-mapping` with the rate-limit rejection - `inst-rate-limited-request-map`
7. [ ] - `p2` - **RETURN** status 429 with `Retry-After` and the rate-limit headers - `inst-rate-limited-request-return`

## 3. Processes / Business Logic (CDSL)

Internal system functions and procedures that do not interact with actors directly. Examples: database layer operations, authorization logic, middleware, validation routines, library functions, background jobs. These are reusable building blocks called by Actor Flows or other processes.

### Chain Assembly From Merged Bindings

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-chain-assembly`

**Input**: Merged effective configuration for the request, holding at most one auth binding plus the plugin bindings concatenated by configuration resolution as upstream, then route, then tenant

**Output**: An ordered execution plan with an auth slot, a guard list, a request transform list, a response transform list and an error transform list

**Steps**:
1. [ ] - `p2` - Read the effective auth binding and the merged binding list from the resolved configuration - `inst-chain-assembly-read`
2. [ ] - `p2` - Keep upstream-level bindings first, then route-level, then tenant-level, preserving each level's own order - `inst-chain-assembly-order`
3. [ ] - `p2` - **FOR EACH** binding in the ordered list - `inst-chain-assembly-loop`
   1. [ ] - `p2` - Classify the binding as guard or transform from the base part of its plugin identifier - `inst-chain-assembly-classify`
   2. [ ] - `p2` - Append the binding to the guard list or to every transform phase it declares - `inst-chain-assembly-append`
4. [ ] - `p2` - Place rate-limit admission at the head of the guard phase, ahead of all guard bindings - `inst-chain-assembly-ratelimit-slot`
5. [ ] - `p2` - Keep the response and error transform lists in the same relative order as the request phase - `inst-chain-assembly-response-order`
6. [ ] - `p2` - **RETURN** the ordered execution plan for this request - `inst-chain-assembly-return`

The three-tier binding order arrives already concatenated from configuration resolution, so this feature preserves that order and never re-merges the binding lists. The assembled plan is invoked once per request at the invocation point that the HTTP proxy feature owns. For server-sent-event streams and WebSocket upgrades the plan runs against the request and against the response head only. Streamed events and WebSocket frames never re-enter the chain, which keeps long-lived connections free of per-frame plugin cost.

### Plugin Reference Resolution

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-runtime-plugin-resolution`

**Input**: A plugin reference string from a binding, plus the expected plugin kind for the slot being filled

**Output**: An executable plugin handle, or an unresolved outcome that becomes a plugin-not-found failure

**Steps**:
1. [ ] - `p2` - Split the reference into its base type part and its instance part - `inst-runtime-plugin-resolution-split`
2. [ ] - `p2` - **IF** the instance part parses as a UUID - `inst-runtime-plugin-resolution-if-uuid`
   1. [ ] - `p2` - Look up the stored plugin definition by that identifier in control-plane state - `inst-runtime-plugin-resolution-lookup-uuid`
   2. [ ] - `p2` - Confirm the stored definition's kind matches the base type part of the reference - `inst-runtime-plugin-resolution-kind-check`
   3. [ ] - `p2` - Report unresolved, because executing stored plugin source is out of scope for this feature - `inst-runtime-plugin-resolution-custom-unresolved`
3. [ ] - `p2` - **ELSE** the instance part is a named identifier - `inst-runtime-plugin-resolution-else-named`
   1. [ ] - `p2` - Look the identifier up in the in-process registry for the expected plugin kind - `inst-runtime-plugin-resolution-lookup-named`
4. [ ] - `p2` - **IF** no executable handle was found for the reference - `inst-runtime-plugin-resolution-if-missing`
   1. [ ] - `p2` - **RETURN** unresolved so the caller raises status 503 with the plugin-not-found problem document - `inst-runtime-plugin-resolution-return-missing`
5. [ ] - `p2` - **RETURN** the executable plugin handle bound to the binding's configuration - `inst-runtime-plugin-resolution-return`

Six named identifiers are catalogued in the types registry with no backing implementation in any registry. The auth identifiers `cf.core.oagw.basic.v1` and `cf.core.oagw.bearer.v1` are catalogue entries only. The guard identifiers `cf.core.oagw.timeout.v1` and `cf.core.oagw.cors.v1` describe core data-plane behaviour that is configured elsewhere. The transform identifiers `cf.core.oagw.logging.v1` and `cf.core.oagw.metrics.v1` describe core instrumentation. Requesting any of the six yields the plugin-not-found failure rather than silently passing the request through unchanged.

### No-Operation Auth Injection

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-noop-auth`

**Input**: The request context for an upstream whose effective auth binding names the no-operation identifier

**Output**: The same request context, unchanged, with the auth phase reported as complete

**Steps**:
1. [ ] - `p2` - Accept the request context without reading any configuration key - `inst-noop-auth-accept`
2. [ ] - `p2` - Resolve no credential reference and contact no external service - `inst-noop-auth-no-resolve`
3. [ ] - `p2` - Add, remove and rewrite no request header and no query parameter - `inst-noop-auth-no-mutate`
4. [ ] - `p2` - **RETURN** success so the guard phase begins immediately - `inst-noop-auth-return`

### API Key Credential Injection

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-apikey-injection`

**Input**: The request context plus the api-key binding configuration naming a placement, a target name and a credential reference

**Output**: The request carrying the resolved key in the configured header or query parameter

**Steps**:
1. [ ] - `p2` - Read the placement, the target header or parameter name, and the credential reference - `inst-apikey-injection-read`
2. [ ] - `p2` - **IF** the configuration carries a literal secret value instead of a reference - `inst-apikey-injection-if-inline`
   1. [ ] - `p2` - Reject the binding, because inline secret values are never accepted - `inst-apikey-injection-reject-inline`
   2. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-plugin-failure-mapping` with the invalid-binding failure - `inst-apikey-injection-map-invalid-binding`
3. [ ] - `p2` - **CALL** the credential store to resolve the reference for the calling tenant - `inst-apikey-injection-resolve`
4. [ ] - `p2` - **IF** the reference is unknown or access is denied - `inst-apikey-injection-if-denied`
   1. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-plugin-failure-mapping` with the credential failure - `inst-apikey-injection-map-failure`
5. [ ] - `p2` - **IF** the placement is header - `inst-apikey-injection-if-header`
   1. [ ] - `p2` - Set the configured header name to the resolved value, replacing any inbound value - `inst-apikey-injection-set-header`
6. [ ] - `p2` - **ELSE** the placement is query parameter - `inst-apikey-injection-else-query`
   1. [ ] - `p2` - Set the configured query parameter on the forwarded request, replacing any inbound value - `inst-apikey-injection-set-query`
7. [ ] - `p2` - **RETURN** success without writing the resolved value to any log record - `inst-apikey-injection-return`

Replacing rather than appending matters for security. A caller cannot smuggle its own value into the credential slot, because the injected value always wins over an inbound header or parameter of the same name.

### Client Credentials Token Acquisition

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-oauth2-token-acquisition`

**Input**: The request context plus a client-credentials binding naming a token endpoint or an issuer, two credential references and optional scopes

**Output**: The request carrying `Authorization: Bearer` with a valid access token, and a possibly updated token cache

**Steps**:
1. [ ] - `p2` - Read the binding configuration and reject a configuration naming both endpoint and issuer - `inst-oauth2-token-acquisition-read`
2. [ ] - `p2` - **IF** the binding configuration was rejected as structurally invalid - `inst-oauth2-token-acquisition-if-invalid-binding`
   1. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-plugin-failure-mapping` with the invalid-binding failure - `inst-oauth2-token-acquisition-map-invalid-binding`
3. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-token-cache-lookup` with the request identity and configuration - `inst-oauth2-token-acquisition-lookup`
4. [ ] - `p2` - **IF** the lookup returned a live token - `inst-oauth2-token-acquisition-if-hit`
   1. [ ] - `p2` - Inject the cached token and skip the token endpoint entirely - `inst-oauth2-token-acquisition-inject-cached`
   2. [ ] - `p2` - **RETURN** success - `inst-oauth2-token-acquisition-return-cached`
5. [ ] - `p2` - **CALL** the credential store twice to resolve the client identifier and the client secret - `inst-oauth2-token-acquisition-resolve`
6. [ ] - `p2` - **TRY** - `inst-oauth2-token-acquisition-try`
   1. [ ] - `p2` - **IF** the binding names an issuer rather than a token endpoint - `inst-oauth2-token-acquisition-if-issuer`
      1. [ ] - `p2` - Discover the token endpoint from the issuer's published discovery document - `inst-oauth2-token-acquisition-discover`
   2. [ ] - `p2` - Exchange the client credentials for an access token in a single request - `inst-oauth2-token-acquisition-exchange`
   3. [ ] - `p2` - Send the credentials in the request body for the form variant identifier - `inst-oauth2-token-acquisition-form`
   4. [ ] - `p2` - Send the credentials in an `Authorization` request header for the basic variant identifier - `inst-oauth2-token-acquisition-basic`
7. [ ] - `p2` - **CATCH** token acquisition failure - `inst-oauth2-token-acquisition-catch`
   1. [ ] - `p2` - Cache nothing, so the next request for the same key retries the token endpoint - `inst-oauth2-token-acquisition-no-negative-cache`
   2. [ ] - `p2` - **CALL** `cpt-cf-oagw-algo-plugin-failure-mapping` with the authentication failure - `inst-oauth2-token-acquisition-map-failure`
8. [ ] - `p2` - Compute the cache lifetime as the lower of the configured ceiling and the reported expiry minus the safety margin - `inst-oauth2-token-acquisition-ttl`
9. [ ] - `p2` - **IF** the computed lifetime is not positive - `inst-oauth2-token-acquisition-if-short`
   1. [ ] - `p2` - Use the token for this request only and store no cache entry - `inst-oauth2-token-acquisition-skip-cache`
10. [ ] - `p2` - **ELSE** store the token under its cache key for the computed lifetime - `inst-oauth2-token-acquisition-store`
11. [ ] - `p2` - Set `Authorization` to the bearer scheme followed by the token value - `inst-oauth2-token-acquisition-inject`
12. [ ] - `p2` - **RETURN** success without logging the token or either resolved credential - `inst-oauth2-token-acquisition-return`

The configured ceiling defaults to 300 seconds and the safety margin is 30 seconds. A token reporting an expiry at or below the safety margin is therefore never cached, which keeps the gateway from serving a token that expires mid-flight. Both variants share one cache configuration and differ only in how client credentials reach the token endpoint.

### Token Cache Lookup

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-token-cache-lookup`

**Input**: The calling subject's tenant and subject identifiers, the client-authentication variant, and the binding configuration

**Output**: A live token for reuse, or a miss that forces a fresh token exchange

**Steps**:
1. [ ] - `p2` - Build the cache key from subject tenant, subject, variant tag and configuration hash - `inst-token-cache-lookup-key`
2. [ ] - `p2` - Derive the configuration hash deterministically from the sorted configuration key and value pairs - `inst-token-cache-lookup-hash`
3. [ ] - `p2` - Read the cache entry stored under that key, if any entry is present and unexpired - `inst-token-cache-lookup-read`
4. [ ] - `p2` - **IF** an entry was returned - `inst-token-cache-lookup-if-entry`
   1. [ ] - `p2` - Compare the key recorded inside the entry with the lookup key - `inst-token-cache-lookup-verify`
   2. [ ] - `p2` - **IF** the two keys differ - `inst-token-cache-lookup-if-mismatch`
      1. [ ] - `p2` - **RETURN** a miss, never the mismatched entry's token - `inst-token-cache-lookup-return-mismatch`
5. [ ] - `p2` - **RETURN** the verified token, or a miss when no unexpired entry exists - `inst-token-cache-lookup-return`

Each key component isolates a distinct boundary. Subject tenant separates tenants, subject separates callers sharing a tenant, the variant tag separates the form and basic variants, and the configuration hash separates differing endpoints or scopes. Verifying the recorded key on every hit makes a hashed-key collision degrade into a miss instead of a cross-tenant credential leak.

### Required Headers Guard Check

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-required-headers-check`

**Input**: The phase under evaluation, the headers of that phase, and the guard binding's two independent configuration keys

**Output**: An admit decision, or a rejection naming the first missing header for the current phase

**Steps**:
1. [ ] - `p2` - Read `required_request_headers` in the request phase and `required_response_headers` in the response phase - `inst-required-headers-check-read`
2. [ ] - `p2` - **IF** the key for this phase is absent or blank after trimming - `inst-required-headers-check-if-unconfigured`
   1. [ ] - `p2` - **RETURN** admit, because an unconfigured phase fails open by design - `inst-required-headers-check-return-open`
3. [ ] - `p2` - Split the value on commas, trim each entry, lowercase it and drop empty entries - `inst-required-headers-check-parse`
4. [ ] - `p2` - **FOR EACH** required name in configured order - `inst-required-headers-check-loop`
   1. [ ] - `p2` - Search the phase's headers for that name, comparing names case-insensitively - `inst-required-headers-check-search`
   2. [ ] - `p2` - Check presence only, never comparing or validating the header's value - `inst-required-headers-check-presence`
   3. [ ] - `p2` - **IF** the name is absent - `inst-required-headers-check-if-missing`
      1. [ ] - `p2` - **RETURN** a rejection naming this header and stop scanning further names - `inst-required-headers-check-return-reject`
5. [ ] - `p2` - **RETURN** admit, because every configured name was present - `inst-required-headers-check-return-admit`

The two configuration keys are fully independent. Configuring request names alone leaves the response phase a no-operation, and configuring response names alone leaves the request phase a no-operation. A request-phase rejection maps to status 400 and a response-phase rejection maps to status 502, both carrying the error code `REQUIRED_HEADER_MISSING`.

### Request Identifier Propagation

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-request-id-propagation`

**Input**: The request context in the request phase, and the response context in the response phase

**Output**: A forwarded request and a returned response that both carry the same correlation identifier

**Steps**:
1. [ ] - `p2` - **IF** the inbound request already carries `X-Request-ID` with a non-blank value - `inst-request-id-propagation-if-present`
   1. [ ] - `p2` - Keep that value and forward it to the upstream service unchanged - `inst-request-id-propagation-keep`
2. [ ] - `p2` - **ELSE** no usable inbound value exists - `inst-request-id-propagation-else`
   1. [ ] - `p2` - Generate a fresh unique value and set `X-Request-ID` on the forwarded request - `inst-request-id-propagation-generate`
3. [ ] - `p2` - Record the chosen value in the request context for the response phase to read - `inst-request-id-propagation-record`
4. [ ] - `p2` - **IF** the upstream response omits `X-Request-ID` - `inst-request-id-propagation-if-response-missing`
   1. [ ] - `p2` - Set the recorded value on the response returned to the caller - `inst-request-id-propagation-set-response`
5. [ ] - `p2` - **RETURN** success so the caller can correlate its request with gateway logs - `inst-request-id-propagation-return`

### Effective Rate Limit Selection

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-effective-rate-limit`

**Input**: The single merged `rate_limit` value that configuration resolution emitted for this request, plus the request's identity and route attributes

**Output**: The effective sustained rate, window, burst capacity, scope, strategy, cost and counter key

**Steps**:
1. [ ] - `p2` - **IF** the merged configuration carries no rate limit - `inst-effective-rate-limit-if-absent`
   1. [ ] - `p2` - **RETURN** an unlimited outcome so admission is skipped for this request - `inst-effective-rate-limit-return-unlimited`
2. [ ] - `p2` - Read the sustained rate and burst capacity from the merged value, defaulting capacity to the sustained rate - `inst-effective-rate-limit-rate`
3. [ ] - `p2` - Normalise the sustained rate and window into a replenishment rate expressed per second - `inst-effective-rate-limit-normalise`
4. [ ] - `p2` - Read scope, strategy and cost from the merged value, defaulting the request cost to one - `inst-effective-rate-limit-fields`
5. [ ] - `p2` - **IF** the scope is global - `inst-effective-rate-limit-if-global`
   1. [ ] - `p2` - Use a single fixed scope value shared by every caller of the resource - `inst-effective-rate-limit-scope-global`
6. [ ] - `p2` - **ELSE** derive the scope value from the tenant, the subject, the client address or the matched route - `inst-effective-rate-limit-scope-other`
7. [ ] - `p2` - Build the counter key from resource kind, resource identifier, scope name and scope value - `inst-effective-rate-limit-key`
8. [ ] - `p2` - **RETURN** the effective limit inputs together with the counter key - `inst-effective-rate-limit-return`

Configuration resolution owns the hierarchy merge, so this feature consumes one already-merged value and never re-derives a strictest limit across tiers. Sharing modes and their tightening behaviour are therefore settled before admission runs. The resource kind and identifier lead the counter key, so every counter for one upstream or route shares a prefix and can be dropped together when that resource is deleted. This feature implements the token-bucket algorithm, which is the configuration default; the optional sliding-window algorithm is not implemented here.

### Token Bucket Admission

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-token-bucket-admission`

**Input**: The counter key, the replenishment rate, the burst capacity, the request cost and the exhaustion strategy

**Output**: An admit decision with the remaining token count, or a rejection with advisory retry timing

**Steps**:
1. [ ] - `p2` - Look the bucket up by counter key in the in-process bucket map for this instance - `inst-token-bucket-admission-lookup`
2. [ ] - `p2` - **IF** no bucket exists for the key - `inst-token-bucket-admission-if-new`
   1. [ ] - `p2` - Create one holding a full burst capacity and stamped with the current time - `inst-token-bucket-admission-create`
3. [ ] - `p2` - Replenish tokens for the elapsed interval at the replenishment rate, capped at burst capacity - `inst-token-bucket-admission-refill`
4. [ ] - `p2` - **IF** the available tokens are at least the request cost - `inst-token-bucket-admission-if-enough`
   1. [ ] - `p2` - Subtract the cost, record the new level and **RETURN** admit with the remaining tokens - `inst-token-bucket-admission-admit`
5. [ ] - `p2` - **ELSE** the bucket cannot cover the cost - `inst-token-bucket-admission-else`
   1. [ ] - `p2` - Subtract nothing, so a rejected request consumes no tokens at all - `inst-token-bucket-admission-no-consume`
   2. [ ] - `p2` - **IF** the strategy is reject - `inst-token-bucket-admission-if-reject`
      1. [ ] - `p2` - **RETURN** a rejection that becomes status 429 for the caller - `inst-token-bucket-admission-return-reject`
   3. [ ] - `p2` - **ELSE** the strategy is queue or degrade - `inst-token-bucket-admission-else-strategy`
      1. [ ] - `p2` - Fall back to reject semantics, because neither strategy is implemented in this feature - `inst-token-bucket-admission-other-strategy`
      2. [ ] - `p2` - **RETURN** the same rejection that the reject strategy produces for the caller - `inst-token-bucket-admission-fallback-reject`
6. [ ] - `p2` - **RETURN** the admission outcome to the guard phase - `inst-token-bucket-admission-return`

Counters live in the process that handled the request, are never shared with other instances, and are lost on restart. Operators sizing limits for several instances must account for that, because each instance enforces the effective limit independently.

The `queue` and `degrade` strategy values are schema-accepted but not implemented here, so no queue depth, wait bound or degraded response is defined. A configuration selecting either value is honoured by falling back to reject semantics, exactly as the reject strategy behaves.

### Rate Limit Response Headers

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-rate-limit-headers`

**Input**: The admission outcome, the effective limit inputs and the bucket's token level after the attempt

**Output**: The advisory rate-limit header values, plus the retry delay for a rejection

**Steps**:
1. [ ] - `p2` - Set `X-RateLimit-Limit` to the effective sustained rate for the configured window - `inst-rate-limit-headers-limit`
2. [ ] - `p2` - Set `X-RateLimit-Remaining` to the whole tokens left in the bucket after the attempt - `inst-rate-limit-headers-remaining`
3. [ ] - `p2` - Set `X-RateLimit-Reset` to the epoch second at which the bucket returns to full capacity - `inst-rate-limit-headers-reset`
4. [ ] - `p2` - **IF** the outcome is a rejection - `inst-rate-limit-headers-if-reject`
   1. [ ] - `p2` - Compute the seconds needed to replenish the request cost and round that value up - `inst-rate-limit-headers-compute-retry`
   2. [ ] - `p2` - Set `Retry-After` to that whole number of seconds, never below one second - `inst-rate-limit-headers-retry-after`
   3. [ ] - `p2` - Mirror the same number into the problem document's retry guidance field - `inst-rate-limit-headers-problem-field`
5. [ ] - `p2` - **RETURN** the header set for the response builder to emit - `inst-rate-limit-headers-return`

The three advisory headers accompany admitted and rejected requests alike, so callers can pace themselves before exhaustion. The checked-in configuration schema exposes no toggle for them, so the graded deployment always emits them.

### Plugin And Rate Limit Failure Mapping

- [ ] `p2` - **ID**: `cpt-cf-oagw-algo-plugin-failure-mapping`

**Input**: A rejection or failure raised anywhere in the chain, together with its phase and originating plugin kind

**Output**: A problem document response with the documented status, type identifier and error-source header

**Steps**:
1. [ ] - `p2` - **IF** a plugin reference could not be resolved to an implementation - `inst-plugin-failure-mapping-if-unresolved`
   1. [ ] - `p2` - Map to status 503 with the plugin-not-found type identifier - `inst-plugin-failure-mapping-unresolved`
2. [ ] - `p2` - **IF** a binding configuration is structurally invalid for its plugin kind - `inst-plugin-failure-mapping-if-invalid-binding`
   1. [ ] - `p2` - Map to status 503 with the plugin-not-found type identifier, because the binding yields no usable plugin - `inst-plugin-failure-mapping-invalid-binding`
   2. [ ] - `p2` - Treat an inline secret value, and a configuration naming both a token endpoint and an issuer, as structurally invalid - `inst-plugin-failure-mapping-invalid-binding-cases`
3. [ ] - `p2` - **IF** a referenced secret does not exist in the credential store - `inst-plugin-failure-mapping-if-secret`
   1. [ ] - `p2` - Map to status 500 with the secret-not-found type identifier - `inst-plugin-failure-mapping-secret`
4. [ ] - `p2` - **IF** credential resolution was denied, token acquisition failed, or either outbound call exceeded the configured proxy timeout - `inst-plugin-failure-mapping-if-auth`
   1. [ ] - `p2` - Map to status 401 with the authentication-failed type identifier - `inst-plugin-failure-mapping-auth`
5. [ ] - `p2` - **IF** a guard rejected during the request phase - `inst-plugin-failure-mapping-if-guard-request`
   1. [ ] - `p2` - Map to status 400 with the validation-error type identifier and the guard's error code - `inst-plugin-failure-mapping-guard-request`
6. [ ] - `p2` - **IF** a guard rejected during the response phase - `inst-plugin-failure-mapping-if-guard-response`
   1. [ ] - `p2` - Map to status 502 with the downstream-error type identifier and the guard's error code - `inst-plugin-failure-mapping-guard-response`
7. [ ] - `p2` - **IF** admission rejected the request - `inst-plugin-failure-mapping-if-ratelimit`
   1. [ ] - `p2` - Map to status 429 with the rate-limit-exceeded type identifier - `inst-plugin-failure-mapping-ratelimit`
8. [ ] - `p2` - Build the problem document body and set `X-OAGW-Error-Source: gateway` on the response - `inst-plugin-failure-mapping-body`
9. [ ] - `p2` - Redact every credential, token and secret reference value from body, detail and logs - `inst-plugin-failure-mapping-redact`
10. [ ] - `p2` - Run the error transform phase over the failure before the response leaves the gateway - `inst-plugin-failure-mapping-error-phase`
11. [ ] - `p2` - **RETURN** the mapped response to the proxy lifecycle - `inst-plugin-failure-mapping-return`

An upstream 401 response is passed through with the upstream error source, and the request is not retried with fresh credentials. That retry is deferred by the OAuth2 client-credentials decision record, which notes that the auth interface carries no signal for deciding whether a retry would help.

Every credential-store lookup and every token-endpoint call is bounded by the configured proxy timeout, applied per outbound call. An elapsed bound maps to status 401 with the authentication-failed type identifier, and no token is cached. Discovering a token endpoint from an issuer is one such outbound call and carries the same bound.

## 4. States (CDSL)

Optional: Include when entities have explicit lifecycle states.

### Cached Access Token State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-cached-token`

**States**: Absent, Live, Expired, Evicted

**Initial State**: Absent

**Transitions**:
1. [ ] - `p2` - **FROM** Absent **TO** Live **WHEN** a token exchange succeeds and the computed lifetime is positive - `inst-cached-token-absent-to-live`
2. [ ] - `p2` - **FROM** Absent **TO** Absent **WHEN** the token exchange fails, because failures are never cached - `inst-cached-token-absent-to-absent`
3. [ ] - `p2` - **FROM** Absent **TO** Absent **WHEN** the reported expiry is at or below the safety margin - `inst-cached-token-short-lived`
4. [ ] - `p2` - **FROM** Live **TO** Live **WHEN** a lookup verifies the recorded key and reuses the token - `inst-cached-token-live-to-live`
5. [ ] - `p2` - **FROM** Live **TO** Expired **WHEN** the computed lifetime elapses for the entry - `inst-cached-token-live-to-expired`
6. [ ] - `p2` - **FROM** Live **TO** Evicted **WHEN** capacity pressure removes the entry before its lifetime elapses - `inst-cached-token-live-to-evicted`
7. [ ] - `p2` - **FROM** Expired **TO** Absent **WHEN** the next lookup observes the entry as unusable and discards it - `inst-cached-token-expired-to-absent`
8. [ ] - `p2` - **FROM** Evicted **TO** Absent **WHEN** the entry's secret material is cleared from memory - `inst-cached-token-evicted-to-absent`

A key mismatch on lookup is treated as a miss and leaves the observed entry untouched. There is no invalidation path, so a rotated or revoked token stays live until its lifetime elapses, which is why the lifetime ceiling is kept short.

### Token Bucket State Machine

- [ ] `p2` - **ID**: `cpt-cf-oagw-state-token-bucket`

**States**: Unallocated, Full, Partial, Exhausted

**Initial State**: Unallocated

**Transitions**:
1. [ ] - `p2` - **FROM** Unallocated **TO** Full **WHEN** the first request for the counter key creates the bucket - `inst-token-bucket-unallocated-to-full`
2. [ ] - `p2` - **FROM** Full **TO** Partial **WHEN** an admitted request consumes part of the burst capacity - `inst-token-bucket-full-to-partial`
3. [ ] - `p2` - **FROM** Partial **TO** Exhausted **WHEN** the remaining tokens fall below the next request cost - `inst-token-bucket-partial-to-exhausted`
4. [ ] - `p2` - **FROM** Exhausted **TO** Exhausted **WHEN** a further request arrives before enough tokens replenish - `inst-token-bucket-exhausted-to-exhausted`
5. [ ] - `p2` - **FROM** Exhausted **TO** Partial **WHEN** replenishment restores at least the next request cost - `inst-token-bucket-exhausted-to-partial`
6. [ ] - `p2` - **FROM** Partial **TO** Full **WHEN** replenishment reaches the configured burst capacity - `inst-token-bucket-partial-to-full`
7. [ ] - `p2` - **FROM** Partial **TO** Unallocated **WHEN** the owning upstream or route is deleted, or the process restarts - `inst-token-bucket-partial-to-unallocated`
8. [ ] - `p2` - **FROM** Exhausted **TO** Unallocated **WHEN** the owning upstream or route is deleted, or the process restarts - `inst-token-bucket-exhausted-to-unallocated`

## 5. Definitions of Done

Specific implementation tasks derived from flows/algorithms above.

### Single Chain Invocation Point

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-chain-invocation-point`

The system **MUST** invoke the plugin chain from exactly one point in the proxy request lifecycle, after configuration merge and route matching and inbound request validation, and before the outbound request is dispatched. Plain HTTP exchanges, server-sent-event streams and WebSocket upgrades **MUST** all use that one invocation point, with response-phase plugins running against the response head only.

**Implements**:
- `cpt-cf-oagw-flow-credentialed-proxy-request`
- `cpt-cf-oagw-algo-chain-assembly`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: `Plugin execution plan`

### Deterministic Chain Order

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-chain-order`

The system **MUST** execute the chain as auth, then guards, then request transforms, then the upstream call, then response transforms, with error transforms running on a failed exchange. Upstream-level bindings **MUST** execute before route-level bindings, and route-level bindings before tenant-level bindings, matching the concatenation order that configuration resolution emits. The original order of bindings within each level **MUST** be preserved.

**Implements**:
- `cpt-cf-oagw-algo-chain-assembly`

**Touches**:
- Entities: `Plugin binding`, `Plugin execution plan`

### Three Plugin Kinds With Bounded Capabilities

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-plugin-kinds`

The system **MUST** distinguish three plugin kinds and confine each to its own capability. An auth plugin **MUST** only inject credentials, at most one per upstream. A guard plugin **MUST** only admit or reject, in the request phase or the response phase. A transform plugin **MUST** only mutate the request, the response or the error, in the phases it declares.

**Implements**:
- `cpt-cf-oagw-algo-chain-assembly`
- `cpt-cf-oagw-algo-runtime-plugin-resolution`

**Touches**:
- Entities: `Plugin binding`

### Plugin Reference Resolution And Unknown Identifiers

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-runtime-plugin-resolution`

The system **MUST** resolve each binding reference by splitting base type from instance, resolving named identifiers through the in-process registry for the expected kind, and confirming that a stored definition's kind matches its reference. A reference with no backing implementation, including the six catalogue-only identifiers, **MUST** fail with status 503 and the plugin-not-found problem document rather than passing the request through.

**Implements**:
- `cpt-cf-oagw-algo-runtime-plugin-resolution`

**Touches**:
- Entities: `Plugin definition`, `Plugin binding`

### Built-In Static Credential Auth Plugins

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-static-auth-plugins`

The system **MUST** register a no-operation auth plugin that completes the auth phase without resolving any credential, contacting any external service, or mutating any header or query parameter. The system **MUST** also register an api-key auth plugin that resolves a credential reference at request time and injects the resolved key into either the configured request header or the configured query parameter, replacing any inbound value of that name.

**Implements**:
- `cpt-cf-oagw-algo-noop-auth`
- `cpt-cf-oagw-algo-apikey-injection`

**Touches**:
- Entities: `Auth plugin binding`

### Built-In Client Credentials Auth Plugins

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-oauth2-client-cred-auth`

The system **MUST** register two client-credentials auth plugins that differ only in client-authentication placement: one sending credentials in the request body, one sending them in an `Authorization` request header. Each **MUST** obtain an access token from a configured token endpoint or from an endpoint discovered through a configured issuer, then inject it using the bearer scheme.

**Implements**:
- `cpt-cf-oagw-algo-oauth2-token-acquisition`

**Touches**:
- Entities: `Auth plugin binding`

### Token Cache Behaviour

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-token-cache`

The system **MUST** cache access tokens under a key combining subject tenant, subject, client-authentication variant and a deterministic configuration hash. The cache lifetime **MUST** be the lower of the configured ceiling and the reported expiry minus the safety margin. A failed acquisition **MUST NOT** be cached, and an entry whose recorded key differs from the lookup key **MUST** be treated as a miss.

**Implements**:
- `cpt-cf-oagw-algo-token-cache-lookup`
- `cpt-cf-oagw-state-cached-token`

**Touches**:
- Entities: `Cached access token`

### Credential Resolution By Reference Only

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-credential-isolation`

The system **MUST** accept credentials in plugin configuration only as credential-store references, and **MUST** reject a configuration carrying an inline secret value. A rejected, structurally invalid binding **MUST** yield status 503 with the plugin-not-found type identifier, because the binding resolves to no usable plugin. Resolved secret material **MUST NOT** appear in logs, problem documents or management API responses, and a denied or unknown reference **MUST** map onto the documented authentication-failed or secret-not-found response. Each credential-store lookup and each token-endpoint call **MUST** be bounded by the configured proxy timeout, and an elapsed bound **MUST** map to status 401 with the authentication-failed type identifier.

**Implements**:
- `cpt-cf-oagw-algo-apikey-injection`
- `cpt-cf-oagw-algo-oauth2-token-acquisition`
- `cpt-cf-oagw-algo-plugin-failure-mapping`

**Touches**:
- Entities: `Credential reference`

### Built-In Required Headers Guard Plugin

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-required-headers-guard`

The system **MUST** register a stateless guard plugin that checks presence, and only presence, of configured header names case-insensitively. Each phase **MUST** be configured independently and **MUST** fail open when its key is absent or blank. The first missing name **MUST** cause rejection with status 400 in the request phase and status 502 in the response phase, both carrying error code `REQUIRED_HEADER_MISSING`.

**Implements**:
- `cpt-cf-oagw-algo-required-headers-check`
- `cpt-cf-oagw-flow-guard-rejected-request`

**Touches**:
- Entities: `Guard plugin binding`

### Built-In Request Identifier Transform Plugin

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-request-id-transform`

The system **MUST** register a transform plugin that guarantees an `X-Request-ID` value on the forwarded request, reusing a non-blank inbound value and generating a fresh unique value otherwise. The same value **MUST** be present on the response returned to the caller, set by the gateway when the upstream response omits it.

**Implements**:
- `cpt-cf-oagw-algo-request-id-propagation`

**Touches**:
- Entities: `Transform plugin binding`

### Effective Rate Limit And Counter Keys

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-effective-rate-limit`

The system **MUST** derive one effective limit per request from the single merged `rate_limit` value that configuration resolution emits, without recomputing any sharing mode or hierarchy-wide reduction. It **MUST** normalise the sustained rate and window into a per-second replenishment rate and default burst capacity to the sustained rate. The counter key **MUST** combine resource kind, resource identifier, scope name and the scope value selected by `global`, `tenant`, `user`, `ip` or `route`.

**Implements**:
- `cpt-cf-oagw-algo-effective-rate-limit`

**Touches**:
- Entities: `Rate limit configuration`

### Token Bucket Admission

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-token-bucket-admission`

The system **MUST** admit requests through a per-counter token bucket that starts full at burst capacity, replenishes continuously at the sustained rate normalised per second, and deducts the configured cost from an admitted request. A rejected request **MUST** consume no tokens. Buckets **MUST** live in process, per instance, with no cross-node synchronization, and admission **MUST** run at the head of the guard phase.

**Implements**:
- `cpt-cf-oagw-algo-token-bucket-admission`
- `cpt-cf-oagw-state-token-bucket`
- `cpt-cf-oagw-flow-rate-limited-request`

**Touches**:
- Entities: `Token bucket`

### Rate Limit Rejection Response

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-rate-limit-response`

The system **MUST** answer an exhausted bucket under the `reject` strategy with status 429, the rate-limit-exceeded problem document and a `Retry-After` value in whole seconds of at least one. `X-RateLimit-Limit`, `X-RateLimit-Remaining` and `X-RateLimit-Reset` **MUST** accompany admitted and rejected responses alike. The `queue` and `degrade` strategy values are schema-accepted but not implemented in this feature, and a configuration selecting either **MUST** be honoured by falling back to reject semantics.

**Implements**:
- `cpt-cf-oagw-algo-rate-limit-headers`
- `cpt-cf-oagw-flow-rate-limited-request`

**Touches**:
- Entities: `Rate limit configuration`, `Token bucket`

### Chain Failure Mapping

- [x] `p2` - **ID**: `cpt-cf-oagw-dod-failure-mapping`

The system **MUST** map every chain rejection and failure onto the documented status and problem type: 503 for an unresolvable plugin reference or a structurally invalid binding configuration, 500 for a missing secret, 401 for denied credential access, a failed token acquisition or a credential-dependency timeout, 400 or 502 for a guard rejection by phase, and 429 for a rate-limit rejection. Each response **MUST** carry `X-OAGW-Error-Source: gateway`, and no upstream call **MUST** be retried after an upstream 401.

**Implements**:
- `cpt-cf-oagw-algo-plugin-failure-mapping`
- `cpt-cf-oagw-flow-guard-rejected-request`

**Touches**:
- API: `{METHOD} /oagw/v1/proxy/{alias}/{path}`
- Entities: `Problem Details error response`

## 6. Acceptance Criteria

- [ ] Given an upstream whose auth binding names the api-key identifier with header placement, header name `X-Api-Key` and a credential reference, a proxied request without that header reaches the upstream carrying `X-Api-Key` set to the value resolved from the credential store.
- [ ] Given the same binding configured with query-parameter placement and parameter name `api_key`, the forwarded request URL carries `api_key` set to the resolved value, and the inbound request headers carry no injected credential.
- [ ] Given an api-key binding with header placement and header name `X-Api-Key`, a request that already carries `X-Api-Key: caller-supplied` reaches the upstream with the resolved credential value, not the caller-supplied value.
- [ ] Given a client-credentials auth binding naming a token endpoint, two proxied requests from the same subject within the cache lifetime cause exactly one token-endpoint call, and both forwarded requests carry the same `Authorization` bearer value.
- [ ] Given a token endpoint that reports an expiry of 20 seconds while the safety margin is 30 seconds, no cache entry is stored, and a second proxied request triggers a second token-endpoint call.
- [ ] Given a token endpoint that returns a failure, the proxied request fails with status 401 and the authentication-failed problem type, nothing is cached, and the next request calls the token endpoint again.
- [ ] Given two subjects in different tenants sharing one client-credentials binding, each subject causes its own token-endpoint call, because their cache keys differ in the subject tenant component.
- [ ] Given a guard binding with `required_request_headers` set to `x-correlation-id,accept`, a proxied request omitting `X-Correlation-ID` is rejected with status 400, error code `REQUIRED_HEADER_MISSING`, `X-OAGW-Error-Source: gateway`, and no upstream call.
- [ ] Given the same binding, a proxied request carrying `x-correlation-id` and `Accept` in lowercase and mixed case respectively is admitted, proving case-insensitive matching of configured names.
- [ ] Given a guard binding with `required_response_headers` set to `content-type` and an upstream response omitting that header, the caller receives status 502 with error code `REQUIRED_HEADER_MISSING`.
- [ ] Given a guard binding whose only configured value is `", , ,"`, the phase admits every request, confirming that a blank configuration fails open.
- [ ] Given a request-id transform binding and an inbound request without `X-Request-ID`, the forwarded request carries a generated `X-Request-ID`, and the returned response carries that same value.
- [ ] Given a request-id transform binding and an inbound request carrying `X-Request-ID: abc-123`, both the forwarded request and the returned response carry `X-Request-ID: abc-123`.
- [ ] Given a binding referencing `gts.cf.core.oagw.auth_plugin.v1~cf.core.oagw.bearer.v1`, the proxied request fails with status 503 and the plugin-not-found problem type, and no request reaches the upstream service.
- [ ] Given a binding referencing `gts.cf.core.oagw.guard_plugin.v1~cf.core.oagw.cors.v1`, the proxied request fails with status 503 and the plugin-not-found problem type rather than being admitted unchecked.
- [ ] Given a rate limit with sustained rate 1 per second, burst capacity 1 and strategy `reject`, the first proxied request returns the upstream status, and a second identical request sent immediately returns status 429 with `Retry-After` present and at least 1.
- [ ] Given that same 429 response, it carries the rate-limit-exceeded problem type, `X-RateLimit-Limit: 1`, `X-RateLimit-Remaining: 0`, an `X-RateLimit-Reset` epoch second value, and `X-OAGW-Error-Source: gateway`.
- [ ] Given a rate limit with sustained rate 10 per second, burst capacity 10 and a route cost of 10, one proxied request is admitted and the immediately following request is rejected with status 429.
- [ ] Given a merged `rate_limit` value of 5 per minute supplied by configuration resolution, the sixth request within that minute over the same counter key is rejected with status 429.
- [ ] Given a merged `rate_limit` value with sustained rate 1 per second, burst capacity 1 and strategy `queue`, a second immediate request returns status 429 with `Retry-After` at least 1, proving the reject fallback.
- [ ] Given a rate limit scoped to `tenant`, exhausting the bucket for one tenant leaves a request from a second tenant admitted, because the counter key differs in its scope value.
- [ ] Given an upstream binding list of two transforms and a route binding list of two transforms, the recorded execution order is both upstream bindings in configured order, followed by both route bindings in configured order.
- [ ] Given one upstream transform binding, one route transform binding and one tenant transform binding, the recorded execution order is the upstream binding, then the route binding, then the tenant binding.
- [ ] Given an api-key auth binding whose credential field carries an inline value instead of a `cred://` reference, the proxied request fails with status 503 and the plugin-not-found problem type, and no credential-store call occurs.
- [ ] Given a client-credentials auth binding naming both a token endpoint and an issuer, the proxied request fails with status 503 and the plugin-not-found problem type, and no token-endpoint call occurs.
- [ ] Given an upstream-level required-headers guard that rejects and a route-level guard that would admit, the rejection is attributed to the upstream-level binding, proving upstream guards run first.
- [ ] Given any chain rejection, no problem document field, response header or log record contains a resolved credential value, an access token, or any part of either.
- [ ] Given an upstream that answers a credentialed request with status 401, the caller receives that 401 with `X-OAGW-Error-Source: upstream`, and exactly one upstream call is recorded.

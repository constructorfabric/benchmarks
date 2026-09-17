# FRAMEWORK-FALLBACK

This marker documents the fallback taken while implementing the `oagw` gear
during the `cf-coding-gen` / `cf-coding-tests` run for this repository.

## Framework workflow executed

The Constructor Studio `<cf-coding-gen>` code-generation workflow was run for
the `oagw` gear change request, including:

1. `cf-coding-gen: implement the task` — sub-agents dispatched; blocked menu
   resolved via the `override` option (planning artifacts: `phase-plan`,
   `phase-dod`, `acceptance-criteria`, `relevant-files-map` were absent; the
   task specification served as the acceptance criteria, per the run preamble).
2. `cf-coding-tests: write automated tests for the implemented behavior`.
3. Direct build/validation (`cargo check`, `cargo test`, `cargo build
   --release`) until green.

## What stalled (and the direct fallback)

### Stalled step: gear lifecycle capability (tokio-util missing from frozen lockfile)

The toolkit's standard gear template declares a lifecycle capability
(`entry = "serve"`), whose runtime requires `tokio-util`. The workspace
`Cargo.toml`/`Cargo.lock` are frozen for this run and do **not** include
`tokio-util`, so the `#[toolkit::gear]` macro invocation with
`capabilities = [rest, lifecycle]` cannot compile.

**Resolution (implemented directly):** the gear is registered as a **rest-only**
gear — `#[toolkit::gear(name = "oagw", capabilities = [rest], deps = [...])]` — in
`/app/gears/system/oagw/oagw/src/gear.rs`. This is the same shape as the
`types-registry` gear already in the workspace. There is no `entry = "serve"`
lifecycle; all state (repositories, plugin registries, rate-limit buckets)
lives in the control/data-plane services published into `OnceLock`s during
`Gear::init`. The host composes the gear's REST router, which is exactly what
the task requires (`/oagw/v1/...` served by the composed server binary).

### Implementation deviation: upstream TLS connectors are not wired

The frozen dependency set includes no `hyper-rustls`/`tokio-rustls` client
connector. Per the crate's deviation notes, the data plane forwards upstream
requests over cleartext `http` only; `https` endpoints are rejected with
`502 Bad Gateway` (`X-OAGW-Error-Source: gateway`) instead of performing a TLS
handshake. See `/app/gears/system/oagw/oagw/src/infra/proxy/service.rs`
(`validate_endpoint_scheme`) and the crate README deviations section. The
OAuth2 client-credentials plugin likewise talks to the token endpoint over the
HTTP client (no TLS).

Everything else specified by `gears/system/oagw/docs/` (PRD, DESIGN, ADR
0001–0009, schemas) is implemented in the crate; see the crate's unit-test
suite and the completion report.

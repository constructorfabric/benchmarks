---
title: Gear anatomy
description: The crate and layer structure of a gear — SDK contract, domain, API, and infrastructure — and which layer to change.
sidebar:
  label: Gear anatomy
  order: 5
---

Once you've built or read a gear, this page is the map: what each crate and layer is for, and where a given change belongs. It uses the same `users-info` example as [Build your first gear](../your-first-gear/).

## Crate structure

A gear is two crates plus a thin server crate that composes gears into a runnable binary:

```text
examples/toolkit/users-info/
├─ users-info-sdk/        # public contract: trait, models, errors, OData schema
├─ users-info/            # the gear: domain, infra (DB), REST api, gear wiring
└─ users-info-server/     # registers gears + boots the runtime
```

- **SDK crate (`<name>-sdk`)** — the public, transport-agnostic surface: the facade trait, request/response and shared models, error types, and the OData filter schema. Consumers depend on this and nothing else. It is the boundary between gears.
- **Implementation crate (`<name>`)** — the gear itself, kept in strictly separated layers.
- **Server crate** — assembles selected gears and boots `HostRuntime`.

## Layers inside the gear

The implementation follows a DDD-light layering with a one-way dependency direction:

```text
api/rest/     adapts HTTP  ─┐
                            ├─ depend on ─▶ domain/   (business rules, no HTTP/DB types)
infra/storage/ persistence ─┘                         ▲
                                                      └─ depends on nothing framework-specific
```

- **`domain/`** — business rules, validation, and authorization entry points. It never imports `api/` or raw infrastructure, and contains no HTTP, database, or framework types.
- **`api/rest/`** — route definitions (`OperationBuilder`), handlers, and DTOs. Extracts the `SecurityContext` and request DTOs; returns response DTOs, never internal domain types.
- **`infra/storage/`** — SeaORM entities, repositories, and migrations; tenant-aware data access through `SecureConn`.
- **gear wiring (`gear.rs`)** — declares capabilities and `deps`, builds services in `init`, resolves dependencies via `ClientHub`, and registers the gear's own SDK implementation.

## Contract-first design

The public contract (the SDK trait) stays stable while the implementation evolves. Consumers and shared models are the boundary between gears; error shapes are transport-agnostic (`CanonicalError`). This is what lets a gear be replaced, moved out-of-process, or re-implemented without breaking callers.

## Which layer do I change?

- **New or changed endpoint?** → `api/rest/` (and the SDK trait if the public contract changes).
- **New business rule or validation?** → `domain/`.
- **New persistence or query?** → `infra/storage/` (add a migration if the schema changes).
- **New dependency on another gear?** → declare it in `deps` and resolve via `ClientHub` in `init`.
- **New public capability for other gears?** → the SDK crate.

## See also

- [SDK contracts and ClientHub](../../concepts/sdk-and-clienthub/) — the contract mechanism.
- [Add a database](../add-a-database/), [Add authorization](../add-authorization/) — layer-specific how-tos.
- [Gears and composition](../../concepts/gears-and-composition/) — the conceptual model.

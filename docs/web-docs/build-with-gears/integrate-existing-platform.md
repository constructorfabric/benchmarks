---
title: Integrate into an existing platform
description: Embed Gears capabilities into an existing SaaS or platform using plugins, adapters, GTS custom types, and hooks — without forking the core.
sidebar:
  label: Integrate into an existing platform
  order: 12
---

You don't have to start greenfield necessarily. Gears is designed to be embedded into an existing SaaS or platform whose identity, licensing, tenancy, secrets, UI, or search may already be provided by other systems. This is **Adoption Path B** (see [Where Gears fits](../../introduction/where-gears-fits/)).

## When to embed instead of greenfield

Embed when you already have an IdP, license engine, tenant directory, secret vault, billing platform, native UI shell, or search provider — and you want Gears capabilities without replacing those systems.

## The plugin model

Gears' extension model is open-closed:

- The **main gear** exposes a stable public API (its SDK trait).
- **Plugins** implement replaceable behavior behind that surface (e.g. an auth provider, a storage backend, a search provider).
- **Consumers call the main gear only** — they never call plugins directly.

Plugin implementations register a scoped client in `ClientHub`; the host gear selects a plugin at runtime by vendor config, tenant context, request parameter, priority, or fallback. See [Plugins and extension points](../../concepts/plugins-and-extension-points/).

## Adapters for external platform services

Integration/adapter gears are the only components that talk to external systems. Wire an adapter to your existing IdP, policy manager, license system, or credentials store so the rest of your gears stay provider-agnostic and reusable.

## Custom data types with GTS

Extend events, settings, permissions, license types, and provider-specific models **without modifying the core gear** by registering new [GTS](https://github.com/GlobalTypeSystem/gts-rust) types and instances. GTS becomes the contract for safe, platform-specific customization. See [Type system (GTS)](../../concepts/type-system-gts/).

## Custom logic, hooks, and FaaS-style extensibility

Add custom workflows, callbacks, policy hooks, or tenant-specific automation around the stable public surface, instead of hardcoding customer-specific branches into the main gear. This keeps the integration replaceable.

## Native UI integration

Keep business capabilities in gears while integrating with an existing web console, admin UI, or product shell. Reuse the generated REST APIs and typed contracts rather than duplicating backend logic in the UI layer.

## See also

- [Use existing gears](../use-existing-gears/) — composing ready-made gears.
- [Plugins and extension points](../../concepts/plugins-and-extension-points/) — the model.
- [Capabilities: core platform integration gears](../../capabilities/) — the adapter category.

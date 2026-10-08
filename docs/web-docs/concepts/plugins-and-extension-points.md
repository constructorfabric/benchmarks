---
title: Plugins and extension points
description: The open-closed plugin model — replaceable implementations behind a stable public surface, keyed by scope and selected at runtime.
sidebar:
  label: Plugins and extension points
  order: 13
---

Gears' extension model is **open-closed**: host gears define interfaces in their SDK crates, plugins implement those interfaces, and the runtime discovers implementations through typed registration. Adding a new provider or integration should usually mean adding a plugin, not modifying the host gear.

## The model

- The **host gear** exposes a stable public API (its SDK trait) and defines one or more plugin interfaces.
- **Plugins** implement replaceable behavior behind that surface — for example authentication providers, authorization backends, credential stores, or search providers.
- **Consumers call the host gear only**; they never call plugins directly.

A "backend" is simply one of the implementations behind a facade trait — see [SDK contracts and ClientHub](../sdk-and-clienthub/).

## Scoped registration and selection

Some gears accept multiple implementations of an interface keyed by **scope**, typically a [GTS](../type-system-gts/) instance id. The plugin registers a **scoped** client in `ClientHub`, and the host gear selects one at runtime by:

- vendor configuration,
- tenant context,
- request parameter,
- priority, or
- fallback.

This lets you mix built-in plugins with external integrations and roll out changes per tenant or in phases.

## Typed extension points

Beyond swappable behavior, GTS provides typed extension points for **settings, permissions, events, license types, and provider contracts**. Product teams extend these without changing existing gears, which is what keeps a secure platform safely customizable.

## See also

- [SDK contracts and ClientHub](../sdk-and-clienthub/) — the facade the plugins sit behind.
- [Type system (GTS)](../type-system-gts/) — how plugins and types are identified.
- [Integrate into an existing platform](../../build-with-gears/integrate-existing-platform/) — plugins in an adoption context.

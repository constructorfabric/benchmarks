---
title: Licensing, usage, and quotas
description: Entitlement checks at request time and usage metering — the XaaS commercial backbone Gears models as platform concerns.
sidebar:
  label: Licensing, usage, and quotas
  order: 8
---

A platform for many paying tenants needs to answer two commercial questions on every meaningful scenario: **is this tenant allowed to do this?** (licensing / entitlement) and **how much are they using?** (usage metering). Gears models both as platform concerns rather than per-gear bespoke code.

:::note[Implementation status]
The licensing/entitlement posture is expressed in the API contract today; the Usage Collector and Quota Enforcer gears are **designed but not fully implemented**. See [Status and roadmap](../../capabilities/status-and-roadmap/). Treat the runtime enforcement details below as the target model.
:::

## Licensing and entitlements

An **entitlement** is the set of features or limits a tenant or user is allowed to use. Entitlement checks happen at request time: routes carry a license posture (declared alongside other `OperationBuilder` metadata), and the gateway/enforcer validates it as part of the request path before business logic runs.

Because licensing is part of the request path, a feature that requires a plan cannot be reached simply by calling its endpoint — the check is not something each gear re-implements.

## Usage metering

**Usage metering** measures consumption — API calls, compute, storage, tokens — using a push model where gears report usage to the Usage Collector. Collected usage feeds two purposes:

- **Quotas** now — enforce per-tenant limits and rate ceilings.
- **Billing** later — the same measurements integrate with a billing platform when one is connected.

## Relationship to tenancy

Entitlements and usage are always tenant-scoped: they hang off the same `SecurityContext` and tenant tree as authorization, so limits and metering roll up through the tenant hierarchy (usage/billing data can roll up to a parent even when business data is behind a [barrier](../security-and-tenancy/)).

## See also

- [Security and multi-tenancy](../security-and-tenancy/) — the tenant model these attach to.
- [Request path](../request-path/) — where license checks run.
- [Integrate into an existing platform](../../build-with-gears/integrate-existing-platform/) — wiring an existing license or billing system via an adapter.

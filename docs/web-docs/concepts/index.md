---
title: Core concepts
description: The mental model behind Gears — composition, runtime, security, and the type system.
sidebar:
  label: Overview
  order: 1
---

These pages are the mental model behind Gears — the "why" behind the how-to pages in [Build with Gears](../build-with-gears/). Read them in order, or jump to what you need.

## Composition and runtime

- **[Gears and composition](./gears-and-composition/)** — what a gear is, the three-tier hierarchy, and in-process vs out-of-process.
- **[SDK contracts and ClientHub](./sdk-and-clienthub/)** — the facade-trait pattern and how gears resolve each other.
- **[Runtime and lifecycle](./runtime-and-lifecycle/)** — capabilities and the ordered lifecycle the runtime drives every gear through.

## The request path

- **[Request path](./request-path/)** — how a request flows from client to data and back.
- **[API Gateway and OpenAPI](./api-gateway-openapi/)** — ingress, code-first contracts, and generated docs.

## Security, tenancy, and governance

- **[Security and multi-tenancy](./security-and-tenancy/)** — `SecurityContext`, PDP/PEP authorization, `AccessScope`, and the tenant tree.
- **[Licensing, usage, and quotas](./licensing-usage-quotas/)** — entitlement checks and usage metering.
- **[Compliance and FIPS](./compliance-and-fips/)** — the security baseline and validated crypto.
- **[Secure data path](./secure-data-path/)** — the end-to-end defense-in-depth layering.

## Evolvability

- **[Error model](./error-model/)** — the canonical 16-category model rendered as RFC-9457 problems.
- **[Type system (GTS)](./type-system-gts/)** — versioned, schema-validated type identity.
- **[Plugins and extension points](./plugins-and-extension-points/)** — extending the domain model without forking.

## Deployment and testing

- **[Deployment shapes](./deployment-shapes/)** — one codebase, three shapes, chosen by configuration.
- **[Testing model](./testing-model/)** — the layered testing strategy.

When you're ready to apply them, the [first-gear walkthrough](../build-with-gears/your-first-gear/) ties everything together against real code.

---
status: accepted
date: 2026-04-17
decision-makers: Constructor Fabric Steering Committee
---

# Adopt Single-Root Tree Topology for the Tenant Model

## Context and Problem Statement

Gears model its tenants hierarchically. Gears rely on that hierarchy for ownership scoping, subtree queries in the Secure ORM, barrier enforcement, and resolution of "act as" semantics in service-to-service (S2S) flows. Before committing to a particular shape we must choose the canonical topology and record the reasoning.

Three shapes were considered:

1. **Single-root tree** — Every tenant descends from exactly one shared root; the root is the only tenant with `parent_id = NULL`.
2. **Multi-root forest** — Several independent trees coexist; each tree has its own root tenant with `parent_id = NULL`.
3. **DAG** — A tenant may have more than one parent, expressing shared ownership across sub-trees.

The choice affects several concrete engineering concerns:

- **S2S tenant-scoped OAuth credentials.** Some services do not expose tenant-less objects — every object they manage has a tenant owner — yet they still need to perform calls under the top-level tenant while obeying the standard tenant-scoped authorization machinery. Such calls are authenticated via an OAuth client registered at the vendor's IdP. The number of roots determines the number of OAuth clients that must be provisioned and routed between. (Operations on tenant-less objects use a different flow and are outside the scope of this decision.)
- **Deployment shape.** The platform must work both for multi-tenant vendor deployments and for single-user consumer products built on Gears. The same tenant topology must be interpretable in both.
- **Reasoning and tooling cost.** Closure tables, barriers, and `get_ancestors` / `get_descendants` semantics are substantially simpler for a tree than for a DAG.

## Decision Drivers

- **S2S tenant-scoped OAuth client management** — Minimize the number of OAuth clients required at the vendor IdP for flows that need to act as the top-level tenant.
- **Unambiguous "act as root" semantics** — There must be exactly one tenant the platform can address as "the root".
- **Operator mental model** — The topology should be easy to reason about for both platform operators and gear authors.
- **Avoid DAG complexity** — Closure-table maintenance, conflicting barriers, and ancestry queries on a DAG add substantial complexity without a concrete use case demanding them today.
- **Autonomy of business sub-trees** — Multi-tenant deployments must be able to host independent organizations with strong isolation.
- **Support consumer / single-user deployments** — Products built on Gears for a single end user must not be forced to invent an artificial second tenant just to satisfy topology rules.

## Considered Options

- **Option A**: Single-root tree (one shared root, every other tenant has a parent)
- **Option B**: Multi-root forest (several independent trees, each with its own root)
- **Option C**: DAG (a tenant may have multiple parents)

## Decision Outcome

Chosen option: **Option A — Single-root tree**, because it gives us a single canonical "act as root" identity for S2S tenant-scoped flows, keeps closure-table and barrier reasoning tree-shaped, and cleanly covers both multi-tenant and single-user deployment shapes without requiring a dedicated "system root" field on the tenant record.

**Interpretation by deployment shape:**

- **Multi-tenant deployment.** Independent organizations are modelled as *sub-roots* directly under the root. The root itself holds no business objects; it acts as a structural anchor.
- **Single-user / consumer deployment of a Gears -based product.** The root *is* the tenant that owns all business objects. No sub-roots are created.

Both shapes satisfy the same topology invariant: the hierarchy has exactly one tenant with `parent_id = NULL`. What differs is whether business objects live on the root or only below it.

**Root identification is by convention only.** The root is the unique tenant with `parent_id = NULL`. No `is_system` / `is_root` flag and no reserved `tenant_type` value are introduced. The root is not referred to as a "system tenant".

### Consequences

**Good:**

- **One OAuth client is sufficient** for S2S tenant-scoped flows that need to act as the root — no per-root credential fan-out.
- **Unambiguous root** — `get_root_tenant(ctx)` is a well-defined operation with exactly one answer.
- **Tree-shaped reasoning** — Closure-table rows, barriers, `get_ancestors`, and `get_descendants` are all defined on a tree, which keeps their semantics simple and local.
- **Deployment-neutral** — The same topology supports multi-tenant vendor deployments and single-user consumer deployments.
- **Minimal schema surface** — `parent_id: UUID?` suffices to express the hierarchy; the invariant "exactly one tenant has `NULL` parent" is enforced at the plugin layer rather than via extra columns or flags.

**Bad:**

- **No native shared-ownership expression.** A tenant cannot belong to two sub-trees simultaneously. Cross-tree sharing must be modelled via explicit resource-group / policy mechanics, not via tenant parentage.

## Pros and Cons of the Options

### Option A: Single-Root Tree (CHOSEN)

One shared root; every other tenant has exactly one parent.

- Good, because **one OAuth client covers root-level S2S tenant-scoped flows** — no fan-out at the IdP.
- Good, because **root identity is unambiguous** — platform-level tenant-scoped operations always address the same tenant.
- Good, because **tree reasoning is simple** — closure-table rows, barrier semantics, and ancestry queries are straightforward.
- Good, because **the same topology serves both multi-tenant and consumer deployments** without special-casing.
- Good, because **minimal schema surface** — `parent_id: Option<TenantId>` on the tenant record is enough; no `is_root` / `is_system` flag or reserved `tenant_type` value is needed.
- Bad, because **the invariant must be enforced at plugin load time** — zero-root and multi-root configurations must be rejected.

### Option B: Multi-Root Forest

Several independent trees coexist; each has its own root.

- Good, because supports multiple vendors/organizations as first-class independent trees with no shared anchor.
- Good, because each tree can in principle carry its own policies and configurations — though in practice barriers already give us this under a single root.
- Bad, because **requires one OAuth client per root** for S2S tenant-scoped flows that need to act as a top-level tenant — and a mechanism to decide which root a given flow should use.
- Bad, because **"the root" is ambiguous** for platform-level tenant-scoped operations — the caller must know and choose a root.
- Bad, because **no expressive power gained over single-root** — everything a forest can model, a tree with sub-roots can also model (and the forest is simply the sub-tree of a conceptual parent).

### Option C: DAG

A tenant may have more than one parent, allowing shared ownership across sub-trees.

- Good, because can natively express shared ownership ("tenant X belongs to both org A and org B").
- Bad, because **closure-table maintenance becomes substantially more complex** — a descendant may be reachable via several paths, each with different barrier compositions.
- Bad, because **barrier semantics become non-local** — "is there a barrier between ancestor and descendant?" is no longer a single yes/no when multiple paths exist.
- Bad, because **ancestry queries lose a canonical ordering** — "parent chain to the root" is not unique.
- Bad, because **no real-world use case currently justifies the additional complexity** — shared ownership can be modelled at the resource-group / policy layer without touching tenant parentage.

## More Information

**Related Documentation:**

- [TENANT_MODEL.md](../TENANT_MODEL.md) — Tenant topology, barriers, closure tables
- [DESIGN.md](../DESIGN.md) — Authorization design (tenant context, subtree queries)
- [RESOURCE_GROUP_MODEL.md](../RESOURCE_GROUP_MODEL.md) — Resource group topology and its relationship to the tenant model
- [ADR 0001: PDP/PEP Authorization Model](./0001-pdp-pep-authorization-model.md)
- [ADR 0002: Split AuthN and AuthZ Resolvers](./0002-split-authn-authz-resolvers.md)
- [ADR 0003: AuthN Resolver Minimalist Interface](./0003-authn-resolver-minimalist-interface.md)

**Implementation Notes:**

- Tenant-resolver plugins validate the single-root invariant at load time and return a configuration error if the source of truth exposes zero or more than one root.
- The public trait `TenantResolverClient` and the plugin trait `TenantResolverPluginClient` expose `get_root_tenant(ctx) -> TenantInfo` so consumers can obtain the root without knowing its id up front.
- The hierarchy APIs (`get_ancestors`, `get_descendants`, `is_ancestor`) operate naturally on a single-root tree: ancestry is a single chain to the root, and descendants form a single subtree per starting node.

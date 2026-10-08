---
title: Secure data path
description: The end-to-end, layered defense-in-depth path from static checks through authentication, authorization, database scoping, and egress.
sidebar:
  label: Secure data path
  order: 10
---

Security in Gears is a **layered path with no unscoped shortcut**. Each layer is enforced by the platform, so a gear cannot accidentally bypass it.

```text
1. Static checks         custom Dylints at build time
2. Authentication        gateway validates tokens → SecurityContext
3. Authorization         PolicyEnforcer → PDP → AccessScope
4. Database scoping      SecureConn applies AccessScope as WHERE clauses
5. Credentials & egress  secrets via credstore; outbound HTTP via OAGW
```

## The five layers

1. **Static checks** — custom Dylints enforce architecture rules (DTO placement, domain isolation, no raw SQL outside migrations, versioned paths, mandatory `OperationBuilder` metadata) at build time, before anything runs.
2. **Authentication** — tokens are validated at the API Gateway; a `SecurityContext` is injected. Gears never parse tokens.
3. **Authorization** — the `PolicyEnforcer` (PEP) asks the AuthZ resolver (PDP), which returns a decision plus row-level constraints compiled into an `AccessScope`. Authorization is fail-closed.
4. **Database scoping** — gears query through `SecureConn`, which applies the `AccessScope` as automatic `WHERE` clauses for tenant isolation and ABAC. There is intentionally no unscoped escape hatch.
5. **Credentials and egress** — secrets are resolved through the credentials-store gear; outbound HTTP goes through the Outbound API Gateway, so credential handling and egress are centralized and auditable.

## Why defense-in-depth

No single layer is trusted to be sufficient. A missing authorization call is caught by the scoped-query requirement; a raw-SQL attempt is caught by static lints; a leaked token cannot widen a tenant boundary because scoping is applied at the query layer. This is what makes "secure by default" a structural property rather than a convention.

## See also

- [Security and multi-tenancy](../security-and-tenancy/) — the authorization model and tenant tree in depth.
- [Add authorization](../../build-with-gears/add-authorization/) and [Add a database](../../build-with-gears/add-a-database/) — the flow against real code.
- [Compliance and FIPS](../compliance-and-fips/) — the crypto and supply-chain baseline.

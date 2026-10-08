---
title: Error model
description: One canonical, 16-category error taxonomy aligned with gRPC status codes, rendered at the REST boundary as RFC-9457 problem documents.
sidebar:
  label: Error model
  order: 11
---

All errors in Gears use the **16 canonical gRPC status codes** aligned with Google's definitions in [`google/rpc/code.proto`](https://github.com/googleapis/googleapis/blob/master/google/rpc/code.proto), each with a fixed HTTP mapping and a stable type identity:

`cancelled`, `unknown`, `invalid_argument`, `deadline_exceeded`, `not_found`, `already_exists`, `permission_denied`, `resource_exhausted`, `failed_precondition`, `aborted`, `out_of_range`, `unimplemented`, `internal`, `unavailable`, `data_loss`, `unauthenticated`.

Internally you work with a typed `CanonicalError`; at the REST boundary it renders as an **RFC-9457** problem document (`application/problem+json`), so clients get consistent, machine-readable errors across every gear regardless of which one produced them.

## Mapping domain errors

Domain errors map to the canonical model at the API boundary:

```rust
impl From<DomainError> for CanonicalError {
    fn from(e: DomainError) -> Self {
        match &e {
            DomainError::UserNotFound { id } =>
                UserResourceError::not_found(format!("User {id} not found"))
                    .with_resource(id.to_string()).create(),
            DomainError::Forbidden =>
                UserResourceError::permission_denied().with_reason("ACCESS_DENIED").create(),
            _ => CanonicalError::internal("An internal error occurred").create(),
        }
    }
}
```

A single `From<CanonicalError> for Problem` at the REST layer handles all 16 categories, so gears never hand-roll a wire format. The error type carries a [GTS](https://github.com/GlobalTypeSystem/gts-rust) identifier (see [Type system (GTS)](../type-system-gts/)), which makes errors routable and lets clients branch on a stable machine code rather than a string.

## Why one model

Because every gear speaks the same error taxonomy, a consumer handles errors from any gear the same way, error categories map deterministically to HTTP and gRPC status, and new gears cannot invent incompatible error shapes.

## See also

- [Type system (GTS)](../type-system-gts/) — the identity carried by errors.
- [API Gateway and OpenAPI](../api-gateway-openapi/) — where errors are declared per route.

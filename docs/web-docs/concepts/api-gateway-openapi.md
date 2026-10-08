---
title: API Gateway and OpenAPI
description: The public ingress, code-first contracts declared with OperationBuilder, and OpenAPI generated in sync with the code.
sidebar:
  label: API Gateway and OpenAPI
  order: 6
---

The **API Gateway** is the public entry point for external clients. It optionally terminates TLS, validates JWTs into a `SecurityContext`, applies rate limiting and body limits, exposes health endpoints, and routes to the target gear.

## Code-first contracts

Contracts in Gears are **code-first**. Route metadata is declared next to the handler with `OperationBuilder` — method, path, auth posture, request/response schemas, errors, and license posture:

```rust
OperationBuilder::get("/users-info/v1/users")
    .operation_id("users_info.list_users")
    .authenticated()
    .handler(handlers::list_users)
    .json_response_with_schema::<Page<UserDto>>(openapi, StatusCode::OK, "Paginated list")
    .with_odata_filter::<UserFilterField>()
    .error_400(openapi).error_500(openapi)
    .register(router, openapi);
```

## OpenAPI in sync by construction

An `OpenApiRegistry` collects every `OperationBuilder` declaration and generates the OpenAPI document from it. Because the spec is derived from the same metadata that wires the route, it stays in sync with the code by construction — and REST clients can be generated from the same contract. Custom lints enforce versioned paths (`/{service}/v{N}/…`) and mandatory operation metadata at build time.

The generated Swagger UI is served under the configured prefix (e.g. `/cf/docs`) and the raw document at `/cf/openapi.json` when `enable_docs` is set. See [Install and run](../../build-with-gears/).

## See also

- [Request path](../request-path/) — where the gateway sits in the flow.
- [Add pagination and filtering](../../build-with-gears/add-pagination-odata/) — OData on routes.
- [Error model](../error-model/) — how errors render at the boundary.

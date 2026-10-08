---
title: SDK contracts and ClientHub
description: The facade-trait SDK pattern and how gears resolve each other through the typed ClientHub, in-process or over gRPC.
sidebar:
  label: SDK contracts and ClientHub
  order: 3
---

Every gear's public API lives in a dedicated **SDK crate** (`<name>-sdk`) that contains only the interface: a transport-agnostic trait, models, and error types. The implementation depends on the SDK, never the reverse. This is the boundary that lets gears be composed, replaced, and moved across process boundaries without breaking callers.

## The facade trait

```rust
// users-info-sdk/src/client.rs — the public facade trait (abridged)
#[async_trait]
pub trait UsersInfoClientV1: Send + Sync {
    async fn get_user(&self, ctx: SecurityContext, id: Uuid) -> Result<User, UsersInfoError>;
    async fn create_user(&self, ctx: SecurityContext, new_user: NewUser) -> Result<User, UsersInfoError>;
    async fn delete_user(&self, ctx: SecurityContext, id: Uuid) -> Result<(), UsersInfoError>;
    // …additional methods omitted (the real trait also exposes streaming sub-clients)
}
```

The first parameter of every method is a `SecurityContext` — identity and tenancy flow as explicit data, not thread-local magic. Error types are transport-agnostic (`CanonicalError`), so the same contract renders correctly over REST or gRPC.

## Facade + backend

Behind the trait the runtime can wire different **backends**:

- an **in-process adapter** that calls the gear's domain service directly, or
- a generated **gRPC client** that talks to the gear in another process.

Consumers call the trait and never know which backend they got. Which one is registered is a configuration decision — see [Run a gear out-of-process](../../build-with-gears/out-of-process/).

## ClientHub: how gears find each other

Gears resolve each other's SDK traits through the typed **ClientHub**. A gear registers its implementation during `init`, and consumers look it up by trait:

```rust
// Provider side — register the local adapter under the SDK trait
ctx.client_hub().register::<dyn UsersInfoClientV1>(Arc::new(local_client));

// Consumer side — resolve and call it
let users = ctx.client_hub().get::<dyn UsersInfoClientV1>()?;
let user = users.get_user(ctx, id).await?;
```

Whether the registered implementation is a local adapter (single process) or a gRPC client (out-of-process) is decided by configuration — the calling code is identical. This is what lets one codebase run in any [deployment shape](../deployment-shapes/).

## See also

- [Gears and composition](../gears-and-composition/) — where SDKs fit in the tier model.
- [Plugins and extension points](../plugins-and-extension-points/) — multiple backends behind one facade.
- [Use existing gears](../../build-with-gears/use-existing-gears/) — consuming a gear's SDK in your app.

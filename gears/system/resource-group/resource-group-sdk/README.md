# Resource Group SDK

SDK crate for the Resource Group gear, providing public API contracts for hierarchical resource group management with the GTS type system in Gears.

## Overview

This crate defines the transport-agnostic interface for the Resource Group gear:

- **`ResourceGroupClient`** — Async trait for full type/group/membership lifecycle
- **`ResourceGroupReadHierarchy`** — Narrow read-only trait for in-process plugin consumers (e.g. AuthZ resolver, tenant-resolver RG plugin) that need ancestor/descendant walks plus flat OData listing
- Models: `ResourceGroupType`, `ResourceGroup`, `ResourceGroupMembership`, `ResourceGroupWithDepth`, `GroupHierarchy`, etc.
- **`ResourceGroupError`** — Error type for all operations
- OData filter field definitions (behind the `odata` feature)

## Usage

### Getting the Client

Consumers obtain the client from `ClientHub`:

```rust
use resource_group_sdk::ResourceGroupClient;

let rg = hub.get::<dyn ResourceGroupClient>()?;
```

### Type Lifecycle

```rust
use resource_group_sdk::CreateTypeRequest;

let rg_type = rg.create_type(&ctx, CreateTypeRequest { /* ... */ }).await?;
let fetched = rg.get_type(&ctx, &rg_type.code).await?;
```

### Group Lifecycle

```rust
use resource_group_sdk::CreateGroupRequest;

let group = rg.create_group(&ctx, CreateGroupRequest { /* ... */ }).await?;
let same = rg.get_group(&ctx, group.id).await?;
```

### Hierarchy Traversal

```rust
use toolkit_odata::ODataQuery;

let descendants = rg.get_group_descendants(&ctx, group.id, &ODataQuery::default()).await?;
let ancestors   = rg.get_group_ancestors(&ctx, group.id, &ODataQuery::default()).await?;
```

### Memberships

```rust
rg.add_membership(&ctx, group.id, "tenant", &tenant_id.to_string()).await?;
rg.remove_membership(&ctx, group.id, "tenant", &tenant_id.to_string()).await?;
```

## Read-Only Hierarchy Trait

Plugin consumers that need only read access can depend on the narrower
`ResourceGroupReadHierarchy` trait. It exposes ancestor/descendant walks and
OData-filtered listing — enough to support batch lookups like
`id in (id1, id2, …)` without pulling in the full client surface.

```rust
use resource_group_sdk::ResourceGroupReadHierarchy;

let read = hub.get::<dyn ResourceGroupReadHierarchy>()?;
let page = read.list_groups(&ctx, &query).await?;
```

## Error Handling

```rust
use resource_group_sdk::ResourceGroupError;

match rg.get_group(&ctx, id).await {
    Ok(group) => { /* ... */ }
    Err(ResourceGroupError::GroupNotFound { .. }) => { /* ... */ }
    Err(e) => return Err(e.into()),
}
```

## Features

- `odata` (default) — enables OData filter field definitions and typed query
  helpers (depends on `toolkit-odata-macros` and `toolkit-sdk`).

## License

Apache-2.0

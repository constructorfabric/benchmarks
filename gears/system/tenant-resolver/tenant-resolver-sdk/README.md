# Tenant Resolver SDK

SDK crate for the Tenant Resolver gear, providing public API contracts for multi-tenant hierarchy resolution in Gears.

## Overview

This crate defines the transport-agnostic interface for the Tenant Resolver gear:

- **`TenantResolverClient`** — Async trait for consumers (get tenants, traverse hierarchy, check ancestry)
- **`TenantResolverPluginClient`** — Async trait for plugin implementations
- **`TenantInfo`** / **`TenantRef`** — Tenant data models (full info vs. lightweight reference)
- **`TenantResolverError`** — Error types for all operations
- **`TenantResolverPluginSpecV1`** — GTS schema for plugin registration

## Usage

### Getting the Client

Consumers obtain the client from `ClientHub`:

```rust
use tenant_resolver_sdk::TenantResolverClient;

let resolver = hub.get::<dyn TenantResolverClient>()?;
```

### Get Tenant

```rust
let tenant = resolver.get_tenant(&ctx, tenant_id).await?;
println!("Name: {}, Status: {:?}", tenant.name, tenant.status);
```

### Get Root Tenant

```rust
// The unique tenant with no parent (single-root tree).
let root = resolver.get_root_tenant(&ctx).await?;
println!("Root: {} ({})", root.id, root.name);
```

### Batch Get

```rust
use tenant_resolver_sdk::{GetTenantsOptions, TenantStatus};

// All statuses
let tenants = resolver.get_tenants(&ctx, &[id1, id2], &GetTenantsOptions::default()).await?;

// Active only
let opts = GetTenantsOptions { status: vec![TenantStatus::Active] };
let active = resolver.get_tenants(&ctx, &[id1, id2], &opts).await?;
```

### Hierarchy Traversal

```rust
use tenant_resolver_sdk::{BarrierMode, GetAncestorsOptions, GetDescendantsOptions};

// Get ancestor chain (parent → root)
let response = resolver.get_ancestors(&ctx, tenant_id, &GetAncestorsOptions::default()).await?;
for ancestor in &response.ancestors {
    println!("Ancestor: {}", ancestor.id);
}

// Get descendants (children, grandchildren, ...)
let opts = GetDescendantsOptions {
    max_depth: Some(2),
    barrier_mode: BarrierMode::Ignore,
    ..Default::default()
};
let response = resolver.get_descendants(&ctx, tenant_id, &opts).await?;
```

### Ancestry Check

```rust
let is_anc = resolver.is_ancestor(&ctx, parent_id, child_id, &IsAncestorOptions::default()).await?;
```

## Models

### TenantInfo

Full tenant information returned by `get_tenant` and `get_tenants`:

```rust
pub struct TenantInfo {
    pub id: TenantId,              // Unique identifier (UUID)
    pub name: String,              // Human-readable name
    pub status: TenantStatus,      // Active, Suspended, or Deleted
    pub tenant_type: Option<String>, // Classification
    pub parent_id: Option<TenantId>, // None for the root tenant (single-root tree)
    pub self_managed: bool,        // Barrier flag
}
```

### TenantRef

Lightweight reference (without name) used by hierarchy operations:

```rust
pub struct TenantRef {
    pub id: TenantId,
    pub status: TenantStatus,
    pub tenant_type: Option<String>,
    pub parent_id: Option<TenantId>,
    pub self_managed: bool,
}
```

### BarrierMode

Controls traversal through self-managed (barrier) tenants:

- `BarrierMode::Respect` (default) — Stop at barrier boundaries
- `BarrierMode::Ignore` — Traverse through barriers

## Error Handling

```rust
use tenant_resolver_sdk::TenantResolverError;

match resolver.get_tenant(&ctx, id).await {
    Ok(tenant) => println!("Found: {}", tenant.name),
    Err(TenantResolverError::TenantNotFound { tenant_id }) => println!("Not found: {tenant_id}"),
    Err(TenantResolverError::NoPluginAvailable) => println!("No plugin registered"),
    Err(e) => println!("Error: {e}"),
}
```

## Implementing a Plugin

Implement `TenantResolverPluginClient` and register with a GTS instance ID:

```rust
use async_trait::async_trait;
use tenant_resolver_sdk::{TenantResolverPluginClient, TenantInfo, TenantResolverError};

struct MyPlugin { /* ... */ }

#[async_trait]
impl TenantResolverPluginClient for MyPlugin {
    async fn get_tenant(&self, ctx: &SecurityContext, id: TenantId)
        -> Result<TenantInfo, TenantResolverError> {
        // Your implementation
    }
    // ... other methods
}
```

## License

Apache-2.0

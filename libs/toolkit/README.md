# ToolKit

Declarative gear system and common runtime utilities used across CF/Gears.

## Overview

The `cf-gears-toolkit` crate provides:

- Gear registration and lifecycle (inventory-based discovery)
- `ClientHub` for typed in-process clients
- REST/OpenAPI helpers (`OperationBuilder`, `OpenApiRegistry`, RFC-9457 `Problem`)
- Runtime helpers (gear registry/manager, lifecycle helpers)

## Features

- **`db` (default)**: Enables DB integration (depends on `cf-gears-toolkit-db`), including:
  - `DatabaseCapability` (migrations contract)
  - `DbOptions::Manager` (runtime DB manager support)
  - DB handle resolution in `GearCtx` / `GearContextBuilder`

### Build without DB

To build `cf-gears-toolkit` without pulling in `cf-gears-toolkit-db` and its transitive dependencies:

```bash
cargo build -p cf-gears-toolkit --no-default-features
```

## License

Licensed under Apache-2.0.

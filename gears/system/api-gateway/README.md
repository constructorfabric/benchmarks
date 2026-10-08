# API Gateway Gear

HTTP gateway gear that owns the Axum router and collects typed operation specs to emit a single OpenAPI document.

## Overview

The `cf-gears-api-gateway` crate provides:

- HTTP server host for REST APIs
- Operation registration via `toolkit::api::OperationBuilder`
- OpenAPI document aggregation

## Configuration

```yaml
gears:
  api_gateway:
    config:
      bind_addr: "127.0.0.1:8086"
      enable_docs: true
      cors_enabled: false
      auth_disabled: false
```

## License

Licensed under Apache-2.0.

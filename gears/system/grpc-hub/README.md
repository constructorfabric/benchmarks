# gRPC Hub Gear

This gear builds and hosts the single `tonic::Server` instance for the process.

## Overview

The `cf-gears-grpc-hub` crate implements the `grpc_hub` gear and is responsible for:

- Hosting the gRPC server
- Installing gRPC services collected from other gears

## Configuration

```yaml
gears:
  grpc_hub:
    config:
      # TCP example: "0.0.0.0:50051"
      # Unix example (unix only): "uds:///tmp/cf-gears.sock"
      # Windows named pipe example (windows only): "pipe://\\\\.\\pipe\\cf-gears"
      listen_addr: "0.0.0.0:50051"
```

## License

Licensed under Apache-2.0.

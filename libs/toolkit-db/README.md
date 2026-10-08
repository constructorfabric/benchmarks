# ToolKit DB

Database abstractions for Gears / ToolKit with optional SeaORM integration.

## Overview

The `cf-gears-toolkit-db` crate provides:

- Typed database configuration / connection options
- SQLx backend support (SQLite / Postgres / MySQL via features)
- SeaORM integration
- Secure-by-default ORM wrapper (see `secure` gear)
- Per-gear migration runner (see `migration_runner` gear)

## Features

- `pg`, `mysql`, `sqlite`: enable SQLx backends

## Security Model

Gears cannot access raw database connections. All database operations go through
the `SecureConn` API which enforces tenant isolation at compile time. Migrations
are provided as definitions and executed by the runtime with a privileged connection.

## License

Licensed under Apache-2.0.

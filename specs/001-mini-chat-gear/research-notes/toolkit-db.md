# toolkit-db / toolkit-db-macros / toolkit-security: research for the `mini-chat` gear

All paths are relative to `/app`. Versions: `sea-orm 2.0.2`, `sea-query 1.0.2`, `sqlx 0.9.0` (Cargo.lock).
Crate names: `toolkit_db` (package `cf-gears-toolkit-db`), `toolkit_db_macros`, `toolkit_security`.
`gears/mini-chat/mini-chat/Cargo.toml` already depends on `toolkit-db = { workspace = true, features = ["sqlite", "pg"] }`,
`toolkit-db-macros`, `toolkit-security`, `sea-orm`, `sea-orm-migration`, `authz-resolver-sdk`, and `kube`/`k8s-openapi` (optional, under the `k8s` feature).

Reference gears that already use these libraries:
- **credstore**: tenant-scoped entity, raw-SQL migration with partial unique indexes, PG+SQLite. `gears/credstore/credstore/src/infra/storage/...`
- **types-registry**: complete **outbox** integration (leased handler, enqueue in tx, prefix migrations, start/stop), CAS updates with `rows_affected`, test harness. `gears/system/types-registry/types-registry/src/...`

---------------------------------------------------------------------------------------------------

## 1. Secure ORM

### 1.1 Public surface (`libs/toolkit-db/src/secure/mod.rs:126-169`)

```rust
// toolkit_db::secure::*
pub use entity_traits::ScopableEntity;
pub use error::{ScopeError, is_foreign_key_violation, is_unique_violation};
pub use toolkit_security::{AccessScope, EqScopeFilter, InScopeFilter, ScopeConstraint, ScopeFilter, ScopeValue, pep_properties};
pub use secure_conn::{SecureConn, SecureTx};          // legacy API
#[doc(hidden)] pub use runner::DBRunner;              // sealed marker trait, implemented by DbConn/DbTx
pub use db::{DEFAULT_TX_RETRY_ATTEMPTS, Db, DbConn, DbTx};
pub use tx_error::{InfraError, TxError};
pub use tx_config::{TxAccessMode, TxConfig, TxIsolationLevel};
pub use select::{Scoped, SecureEntityExt, SecureFindRelatedExt, SecureSelect, SecureSelectTwo, SecureSelectTwoMany, Unscoped};
pub use cte::{RecursiveCte, RecursiveDedup, SecureCteSelect};
pub use db_ops::{SecureDeleteExt, SecureDeleteMany, SecureInsertExt, SecureInsertMany, SecureInsertManyExt,
    SecureInsertOne, SecureOnConflict, SecureUpdateExt, SecureUpdateMany, max_bind_params_for,
    secure_insert, secure_insert_from_select, secure_insert_many, secure_update_with_scope, validate_tenant_in_scope};
pub use provider::{SimpleTenantFilter, TenantFilterProvider};
pub use toolkit_db_macros::Scopable;
```
Crate root re-exports (`libs/toolkit-db/src/lib.rs:105-118`): `toolkit_db::{Db, DbConn, DbTx, DBProvider, DbError, DbManager, DbConnConfig, GlobalDatabaseConfig, ConnectOpts, connect_db, build_db, DbLockError, DbLockGuard, LockConfig, sea_orm_migration}`;
modules `toolkit_db::{secure, outbox, migration_runner, contention, advisory_locks, odata, config, manager}`.

### 1.2 `#[derive(Scopable)]` (`libs/toolkit-db-macros/src/lib.rs:1-135`, `libs/toolkit-db/src/secure/entity_traits.rs`)

Attribute `#[secure(...)]` on the SeaORM `Model`. **All four dimensions must be declared explicitly**, each exactly once:
- `tenant_col = "col"` OR `no_tenant`
- `resource_col = "col"` OR `no_resource`
- `owner_col = "col"` OR `no_owner`
- `type_col = "col"` OR `no_type`
- `unrestricted` (alone; forbids all other attributes; sets `IS_UNRESTRICTED = true`, all cols `None`)
- `pep_prop(property_name = "column_name")` (repeatable) for custom ABAC properties

Column names are snake_case strings converted to `Column::UpperCamel` (`heck::ToUpperCamelCase`, `scopable.rs:1,148`).
The macro generates `resolve_property()`:
`tenant_col -> "owner_tenant_id"`, `resource_col -> "id"`, `owner_col -> "owner_id"`, `pep_prop(x = "c") -> "x"`. `type_col` is NOT addressable by any property.

Real example (credstore, `gears/credstore/credstore/src/infra/storage/entity/secrets.rs:1-47`):
```rust
use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db::secure::Scopable;     // (types-registry uses `use toolkit_db_macros::Scopable;` — both work)
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "credstore_secrets")]
#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub reference: String,
    pub sharing: i16,
    pub owner_id: Uuid,          // NOTE: has an owner column but declared no_owner
    pub status: i16,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub version: i64,
    pub secret_type_uuid: Uuid,
    pub expires_at: Option<OffsetDateTime>,
    pub value_fp: Option<Vec<u8>>,
    pub fp_key_id: Option<i16>,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
```
Owner-scoped variant for mini-chat chats: `#[secure(tenant_col = "tenant_id", resource_col = "id", owner_col = "user_id", no_type)]`.
Child tables without owner (messages etc.): `#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]` and query them with `scope.tenant_only()` AFTER checking the parent chat with the full scope (see 1.3).

Unrestricted example: `gears/system/types-registry/types-registry/src/infra/storage/entity/operation.rs:35-60` (`#[secure(unrestricted)]`).
CAUTION: for an `unrestricted` entity, a *constrained* scope compiles to deny-all on SELECT/UPDATE/DELETE (no property resolves → constraint dropped → `WHERE false`, `libs/toolkit-db/src/secure/cond.rs:239-330`). Use `AccessScope::allow_all()` with unrestricted entities (types-registry does exactly that, `registry_service.rs:356`). Insert validation is skipped for `IS_UNRESTRICTED` (`db_ops.rs` `validate_insert_scope`).

Manual impl alternative (trait): `libs/toolkit-db/src/secure/entity_traits.rs:103-177` — methods `tenant_col/resource_col/owner_col/type_col/resolve_property/scope_columns`, const `IS_UNRESTRICTED`.

### 1.3 `AccessScope` (`libs/toolkit-security/src/access_scope.rs:787-1100`)

Import: `toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties}` (also re-exported as `toolkit_db::secure::AccessScope`).
```rust
pub struct AccessScope { constraints: Vec<ScopeConstraint>, unconstrained: bool }   // Default = deny_all
AccessScope::from_constraints(Vec<ScopeConstraint>) / ::single(ScopeConstraint)
AccessScope::allow_all()      // unconstrained: NO row filter (use for background jobs / system work)
AccessScope::deny_all()       // WHERE false
AccessScope::for_tenants(Vec<Uuid>) / ::for_tenant(Uuid)        // filter owner_tenant_id IN (...)
AccessScope::for_resources(Vec<Uuid>) / ::for_resource(Uuid)    // filter id IN (...)
.constraints(), .is_unconstrained(), .is_deny_all(), .has_property(&str),
.all_values_for(&str), .all_uuid_values_for(&str), .contains_uuid(&str, Uuid)  // not an authz decision
.tenant_only()        // keep only owner_tenant_id filters (WIDENS; for child entities w/o owner/id cols). allow_all -> deny_all!
.tenant_and_owner()   // keep owner_tenant_id + owner_id filters. allow_all -> deny_all!
.ensure_owner(owner_id: Uuid)  // inject/intersect owner_id = X (defence in depth); allow_all -> owner_id = X only
pep_properties::{OWNER_TENANT_ID = "owner_tenant_id", RESOURCE_ID = "id", OWNER_ID = "owner_id"}
ScopeConstraint::new(Vec<ScopeFilter>) ; ScopeFilter::eq(prop, v) / ::in_uuids(prop, Vec<Uuid>) / ::r#in(prop, Vec<ScopeValue>)
```
Semantics: constraints are OR-ed, filters inside a constraint AND-ed; a constraint naming a property the entity can't resolve is dropped (fail-closed); all dropped → deny-all.

Manual scope with tenant+owner (credstore test style, `repo_tests.rs:21`):
```rust
let scope = AccessScope::single(ScopeConstraint::new(vec![
    ScopeFilter::in_uuids(pep_properties::OWNER_TENANT_ID, vec![tenant_id]),
    ScopeFilter::eq(pep_properties::OWNER_ID, user_id),
]));
```

From the PDP (`gears/system/authz-resolver/authz-resolver-sdk/src/pep/enforcer.rs:292-610`, usage doc in `authz-resolver-sdk/src/lib.rs:13-41`):
```rust
use authz_resolver_sdk::{AuthZResolverApi, pep::{AccessRequest, PolicyEnforcer, ResourceType}};
const CHAT: ResourceType = ResourceType::from_static(gts_id!("..."), &["owner_tenant_id", "id", "owner_id"]);
let authz = hub.get::<dyn AuthZResolverApi>()?;
let enforcer = PolicyEnforcer::new(authz);                       // or PolicyEnforcer::from_hub(hub)
// signature:
pub async fn access_scope(&self, ctx: &SecurityContext, resource: &ResourceType, action: &str,
                          resource_id: Option<Uuid>) -> Result<AccessScope, EnforcerError>;
pub async fn access_scope_with(&self, ctx: &SecurityContext, resource: &ResourceType, action: &str,
                               resource_id: Option<Uuid>, request: &AccessRequest) -> Result<AccessScope, EnforcerError>;
let scope = enforcer.access_scope(&ctx, &CHAT, "get", Some(chat_id)).await?;
let scope = enforcer.access_scope_with(&ctx, &CHAT, "create", None,
    &AccessRequest::new().context_tenant_id(tid).resource_property(pep_properties::OWNER_TENANT_ID, tid)).await?;
```
`EnforcerError::{Denied{deny_reason}, EvaluationFailed(CanonicalError), CompileFailed(ConstraintCompileError)}`. With `require_constraints(false)` an empty PDP answer becomes `allow_all()`.

### 1.4 Getting a DB in a gear

`libs/toolkit/src/context.rs:244-269` (feature `db`):
```rust
pub fn db(&self) -> Option<toolkit_db::DBProvider<toolkit_db::DbError>>;
pub fn db_required(&self) -> anyhow::Result<toolkit_db::DBProvider<toolkit_db::DbError>>;
```
Gear declaration needs `db` capability + `impl DatabaseCapability` (`libs/toolkit/src/contracts.rs:47-49`):
```rust
#[toolkit::gear(name = "credstore", deps = [authz_resolver, ...], capabilities = [system, db, rest, stateful],
                lifecycle(entry = "serve", stop_timeout = "30s", await_ready))]   // credstore/src/gear.rs:37-42
```
Lifecycle order (`libs/toolkit/src/runtime/host_runtime.rs:1-16`): pre_init → **DB migrations** → init → post_init → REST → start/stop. So schema (incl. outbox tables) exists when `init` runs.

`DBProvider<E>` (`libs/toolkit-db/src/db_provider.rs`), `E: From<DbError> + Send + 'static`, Clone (Arc inside):
```rust
pub fn new(db: Db) -> Self;
pub fn db(&self) -> Db;                         // clone of inner Db (needed for Outbox::builder)
pub fn conn(&self) -> Result<DbConn<'_>, E>;    // fails with DbError::ConnRequestedInsideTx inside a tx closure
pub async fn transaction<T, F>(&self, f: F) -> Result<T, E>
  where T: Send + 'static,
        F: for<'a> FnOnce(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>> + Send;
pub async fn transaction_with_config<T, F>(&self, tx_config: TxConfig, f: F) -> Result<T, E>;
```
Re-typing the error: `DBProvider::<MyDomainError>::new(ctx_db.db())` (types-registry `registry_service.rs:331`).

`Db` (`libs/toolkit-db/src/secure/db.rs:347-1060`), Clone:
```rust
pub fn conn(&self) -> Result<DbConn<'_>, DbError>;
pub fn backend(&self) -> sea_orm::DbBackend;
pub fn db_engine(&self) -> &'static str;     // "postgres" | "mysql" | "sqlite"
pub async fn lock(&self, gear: &str, key: &str) -> crate::Result<DbLockGuard>;
pub async fn try_lock(&self, gear: &str, key: &str, config: LockConfig) -> crate::Result<Option<DbLockGuard>>;
pub async fn transaction_ref<F, T>(&self, f: F) -> Result<T, DbError>;
pub async fn transaction_ref_mapped<F, T, E: From<DbError>>(&self, f: F) -> Result<T, E>;
pub async fn transaction_ref_mapped_with_config<F, T, E>(&self, tx_config: TxConfig, f: F) -> Result<T, E>;
pub async fn transaction_with_retry<T, E, X, F>(&self, tx_config: TxConfig, extract_db_err: X, body: F) -> Result<T, E>
   where X: Fn(&E) -> Option<&sea_orm::DbErr> + Send,
         F: for<'a> FnMut(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>> + Send; // retries 3x (DEFAULT_TX_RETRY_ATTEMPTS) on PG 40001/deadlock, SQLite BUSY
pub async fn transaction_with_retry_max<...>(&self, tx_config, max_attempts: u32, extract_db_err, body);
pub async fn transaction<F, T>(self, f: F) -> (Self, anyhow::Result<T>);            // consuming variants
pub async fn in_transaction<T, E, F>(self, f: F) -> (Self, Result<T, TxError<E>>);
```
`DbConn<'a>` / `DbTx<'a>` are opaque (no `ConnectionTrait` exposed); they implement the sealed `DBRunner`. Repositories take `runner: &impl DBRunner`.
`TxConfig { isolation: Option<TxIsolationLevel>, access_mode: Option<TxAccessMode> }`, helpers `TxConfig::serializable()`, `::read_only()`, `::with_isolation(..)` (`secure/tx_config.rs`). types-registry passes `TxConfig::default()` on SQLite and explicit isolation only on PG/MySQL (`domain/ports/mod.rs:44-75`).

Transaction usage (types-registry `domain/admission/acceptance.rs:520-590`) — clone captured data into the closure; it must be `'static`-friendly:
```rust
let tx_scope = scope.clone();
let res = db.transaction(|tx| {
    let tx_scope = tx_scope.clone();
    Box::pin(async move {
        repo.insert_x(tx, &tx_scope, ...).await?;
        let wake = enqueuer.enqueue(tx, ...).await?;   // enqueue LAST
        Ok((value, wake))
    })
}).await;
match res { Ok((v, wake)) => { wake.fire(); Ok(v) }   // fire only after commit
            Err(MyErr::Storage(e)) if e.is_unique_violation() => { /* re-read winner */ }
            Err(e) => Err(e) }
```

### 1.5 Secure query APIs (`secure/select.rs`, `secure/db_ops.rs`)

SELECT — `.secure()` on `sea_orm::Select<E>` (put `.filter/.order_by/.limit` before `.secure()` or after `scope_with`):
```rust
use sea_orm::{ColumnTrait, Condition, EntityTrait, Order, QueryFilter, QueryOrder, QuerySelect};
use toolkit_db::secure::{SecureEntityExt, DBRunner, ScopeError};
// SecureSelect<E, Scoped>: all(&impl DBRunner) -> Result<Vec<Model>, ScopeError>; one(..) -> Option<Model>; count(..) -> u64;
//   filter(Condition), order_by(col, Order), limit(u64), offset(u64), and_id(Uuid)?, project_all(runner, |q| q.select_only()...into_model::<T>()),
//   scope_via_exists::<J>(&scope), and_scope_for::<J>(&scope), find_also_related, find_with_related, into_inner()
operation::Entity::find()
    .filter(operation::Column::Id.eq(id))
    .secure()
    .scope_with(scope)                 // returns SecureSelect<E, Scoped> (NOT a Result; old docs show `?`)
    .one(runner).await?;
```
UPDATE (CAS / guarded update with `rows_affected`) — real code, types-registry `infra/storage/repo/operation_repo.rs:228-251`:
```rust
use sea_orm::sea_query::{Expr, Func};
use toolkit_db::secure::{SecureUpdateExt, ...};
let result = operation::Entity::update_many()
    .secure()                                                    // SecureUpdateMany<E, Unscoped>
    .col_expr(operation::Column::Status, Expr::value(OperationStatus::Running))
    .col_expr(operation::Column::StartedAt, Expr::value(now))
    .filter(Condition::all()
        .add(operation::Column::Id.eq(id))
        .add(operation::Column::Status.eq(OperationStatus::Pending)))   // the CAS guard: WHERE state = 'running' etc.
    .scope_with(scope)                                           // SecureUpdateMany<E, Scoped>
    .exec(runner).await?;                                        // -> sea_orm::UpdateResult
Ok(result.rows_affected == 1)
```
Also `exec_with_returning(runner) -> Vec<Model>` (PG + SQLite). `col_expr` on the tenant column → `ScopeError::Denied("tenant_id is immutable")` at exec. Expressions like `Expr::col(C::X).add(1)` or `Func::coalesce` are allowed in `col_expr` (atomic counters for quota).
DELETE: `Entity::delete_many().filter(..).secure().scope_with(&scope).exec(runner).await?.rows_affected` (`SecureDeleteExt`).
INSERT:
```rust
use toolkit_db::secure::{secure_insert, secure_insert_many, SecureInsertExt, SecureOnConflict};
// validates every scope constraint against the ActiveModel; tenant col must be Set
pub async fn secure_insert<E>(am: E::ActiveModel, scope: &AccessScope, runner: &impl DBRunner) -> Result<E::Model, ScopeError>;
pub async fn secure_insert_many<E>(models: Vec<E::ActiveModel>, scope: &AccessScope, runner: &impl DBRunner) -> Result<(), ScopeError>;
// builder form (supports ON CONFLICT):
Entity::insert(am.clone()).secure().scope_with_model(&scope, &am)?    // or .scope_unchecked(&scope)?
    .on_conflict(SecureOnConflict::<Entity>::columns([C::TenantId, C::UserId]).update_columns([C::Theme])?)
    .exec(runner).await?;                                              // or exec_with_returning
pub async fn secure_update_with_scope<E>(am, scope, id: Uuid, runner) -> Result<E::Model, ScopeError>; // find-in-scope then update
pub fn validate_tenant_in_scope(tenant_id: Uuid, scope: &AccessScope) -> Result<(), ScopeError>;
```
Batch-insert bind limits: `max_bind_params_for(runner)`; SQLite 999-ish (types-registry chunks 66 rows × 15 cols).

### 1.6 Raw SQL from gear code?

**Not possible through the public API.** `DBRunner` is sealed and method-free; `DbConn`/`DbTx` never expose `ConnectionTrait`/`DatabaseConnection` (`secure/runner.rs:1-110`, `lib.rs` "Gears cannot access raw database connections"). `into_inner()` gives back the SeaORM builder but nothing to execute it on. Clippy `disallowed-methods` also bans `Select::all/one/count`, `UpdateMany::exec`, `DeleteMany::exec` (`clippy.toml:16-27`).
Consequences / how to do the "complex" things:
- CAS `UPDATE ... WHERE state='running'` → `update_many().secure().col_expr(..).filter(..).scope_with(..).exec(..)` and check `rows_affected` (above). For background jobs use `AccessScope::allow_all()`.
- Aggregates / custom projections → `SecureSelect::project_all(runner, |q| q.select_only().column(..).column_as(Expr::col(..).sum(), "s").group_by(..).into_model::<T>())` (`select.rs:367-411`).
- No `SELECT ... FOR UPDATE` helper exists in secure select; use guarded updates (PG takes row locks on UPDATE; SQLite has a single writer lock).
- Raw SQL is only available inside **migrations** (`SchemaManager::get_connection()` → `execute_unprepared` / `execute_raw`) and inside an outbox **transactional** handler (`&DatabaseExecutor`).
- UUID bound through SeaORM on SQLite is sent as a 16-byte BLOB automatically, so ORM queries match BLOB-stored UUIDs.

---------------------------------------------------------------------------------------------------

## 2. Migrations

### 2.1 How a gear provides them
`impl DatabaseCapability for MyGear { fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> }`; the runtime runs them with a privileged connection before `init`, recording history in a per-gear table `toolkit_migrations__<prefix>__<hash8>` (`libs/toolkit-db/src/migration_runner.rs:1-20, 291-299`).

types-registry (with outbox) — `gears/system/types-registry/types-registry/src/gear.rs:278-296`:
```rust
impl DatabaseCapability for TypesRegistryGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        let mut migrations = crate::infra::storage::Migrator::migrations();
        let outbox = match toolkit_db::outbox::outbox_migrations_with_prefix(OUTBOX_TABLE_PREFIX) {
            Ok(outbox) => outbox,
            Err(e) => panic!("types-registry outbox migration prefix '{OUTBOX_TABLE_PREFIX}' is invalid: {e}"),
        };
        migrations.extend(outbox);
        migrations
    }
}
```
Migrator (`infra/storage/migrations/mod.rs`, credstore `infra/storage/migrations.rs`):
```rust
use sea_orm_migration::prelude::*;
pub mod m0001_initial_schema;
pub struct Migrator;
#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> { vec![Box::new(m0001_initial_schema::Migration)] }
}
```

### 2.2 PG + SQLite migration with partial unique indexes (credstore, complete)
`gears/credstore/credstore/src/infra/storage/migrations/m0001_initial_schema.rs:1-120`:
```rust
use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::ConnectionTrait;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();
        let table_sql = match backend {
            sea_orm::DatabaseBackend::Postgres => r"
CREATE TABLE IF NOT EXISTS credstore_secrets (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    reference TEXT NOT NULL CHECK (length(reference) BETWEEN 1 AND 255),
    sharing SMALLINT NOT NULL CHECK (sharing IN (1, 2, 3)),
    owner_id UUID NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TIMESTAMPTZ NULL,
    value_fp BYTEA NULL, ...
);".to_owned(),
            sea_orm::DatabaseBackend::Sqlite => r"
CREATE TABLE IF NOT EXISTS credstore_secrets (
    id BLOB PRIMARY KEY NOT NULL,
    tenant_id BLOB NOT NULL,
    reference TEXT NOT NULL CHECK (length(reference) BETWEEN 1 AND 255),
    sharing SMALLINT NOT NULL CHECK (sharing IN (1, 2, 3)),
    owner_id BLOB NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at TEXT NULL,
    value_fp BLOB NULL, ...
);".to_owned(),
            _ => return Err(DbErr::Custom("MySQL is not supported".to_owned())),
        };
        let statements = [
            table_sql.as_str(),
            // partial unique indexes — same syntax on PG and SQLite:
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_credstore_nonprivate ON credstore_secrets (tenant_id, reference) WHERE sharing <> 1;",
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_credstore_private ON credstore_secrets (tenant_id, reference, owner_id) WHERE sharing = 1;",
            "CREATE INDEX IF NOT EXISTS idx_credstore_expiry ON credstore_secrets (expires_at) WHERE expires_at IS NOT NULL AND status = 2;",
        ];
        for sql in statements { conn.execute_unprepared(sql).await?; }
        Ok(())
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared("DROP TABLE IF EXISTS credstore_secrets;").await?;
        Ok(())
    }
}
```
UUID DEFAULT literal on SQLite: `secret_type_uuid BLOB NOT NULL DEFAULT (x'<32 hex>')` (same file, `generic_uuid_hex = UUID_STR.replace('-', "")`).

types-registry style (const statement arrays + dispatch), `m20260817_000001_initial.rs:1011-1060`:
```rust
fn up_statements(backend: sea_orm::DatabaseBackend) -> Result<&'static [&'static str], DbErr> {
    match backend {
        sea_orm::DatabaseBackend::Postgres => Ok(PG_UP_STATEMENTS),
        sea_orm::DatabaseBackend::Sqlite => Ok(SQLITE_UP_STATEMENTS),
        sea_orm::DatabaseBackend::MySql => Ok(MYSQL_UP_STATEMENTS),
        other => Err(DbErr::Migration(format!("unsupported database backend {other:?}"))),
    }
}
#[allow(elided_lifetimes_in_paths)]          // needed: crate denies rust_2018_idioms; `SchemaManager<'_>` breaks async_trait (E0195)
#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let backend = manager.get_database_backend();
        let conn = manager.get_connection();
        for sql in up_statements(backend)? {
            conn.execute_raw(Statement::from_string(backend, (*sql).to_owned())).await?;
        }
        Ok(())
    }
    ...
}
```

### 2.3 Type mapping on SQLite (what the driver actually writes)
From the types-registry migration header (`m20260817_000001_initial.rs:23-60`) and sqlx/sea-query sources:
| Rust / PG | SQLite storage | Notes |
|---|---|---|
| `Uuid` / `uuid` | 16-byte **BLOB** | sqlx-sqlite 0.9 `Uuid::encode` pushes `Blob(as_bytes())` (`sqlx-sqlite-0.9.0/src/types/uuid.rs:25`). Declaring the column `TEXT` (as mini-chat DESIGN §2045 says) still stores a BLOB (TEXT affinity does not convert blobs); types-registry declares `BLOB` + `CHECK (length(col) = 16)`. Raw SQL literals must be `x'…'`. |
| `OffsetDateTime` / `timestamptz` | TEXT, RFC 3339 (`OffsetDateTime::format(&Rfc3339)`, `sqlx-sqlite/src/types/time.rs:59`) | Always write UTC so TEXT ordering/comparison is chronological. `DEFAULT CURRENT_TIMESTAMP` writes `YYYY-MM-DD HH:MM:SS` (different format!) — avoid comparing it against bound values. |
| `bool` / `boolean` | INTEGER 0/1 | types-registry adds `CHECK (col IN (0, 1))`. |
| `Vec<u8>` / `bytea` | BLOB | |
| `serde_json::Value` / `jsonb` | TEXT | entity: `#[sea_orm(column_type = "Json", nullable)] pub payload: Option<serde_json::Value>` (`libs/toolkit-db/tests/db_generic.rs:348-349`). sea-query schema builder renders SQLite `json_text`/`jsonb_text`/`uuid_text`/`timestamp_with_timezone_text` (`sea-query-1.0.2/src/backend/sqlite/table.rs:149-180`) if you use `ColumnDef::new(..).uuid()/.json()` instead of raw SQL. PG `json` (not jsonb) has no equality operator → breaks `SELECT DISTINCT`. |
| `i64` autoinc | `INTEGER PRIMARY KEY AUTOINCREMENT` | PG `bigint GENERATED BY DEFAULT AS IDENTITY` |

---------------------------------------------------------------------------------------------------

## 3. Outbox (`libs/toolkit-db/src/outbox/`, README at `libs/toolkit-db/src/outbox/README.md`)

### 3.1 Exports (`outbox/mod.rs:283-300`)
```rust
use toolkit_db::outbox::{
    Batch, HandlerResult, LeasedHandler, LeasedMessageHandler, MessageResult, OutboxMessage,
    TransactionalHandler, TransactionalMessageHandler, PerMessageAdapter,
    Outbox, OutboxBuilder, OutboxHandle, QueueBuilder, LeasedQueueBuilder,
    outbox_migrations, outbox_migrations_with_prefix,
    Record, RecordBuilder, Records, RecordsBuilder, Wake, in_transaction,
    LeaseConfig, OutboxError, OutboxMessageId, OutboxProfile, Partitions, WorkerTuning,
    DeadLetterFilter, DeadLetterMessage, DeadLetterScope, DeadLetterStatus,
    TraceSubscription, TraceWatch, TraceOutcome, TraceState, TraceStatus,
};
```

### 3.2 Tables / migrations (`outbox/tables.rs`, `outbox/migrations.rs:856-873`)
```rust
pub fn outbox_migrations() -> Vec<Box<dyn MigrationTrait>>;                       // prefix "toolkit_outbox"
pub fn outbox_migrations_with_prefix(prefix: impl Into<String>) -> Result<Vec<Box<dyn MigrationTrait>>, OutboxError>;
```
Default prefix `toolkit_outbox`; migration name `m001_create_toolkit_outbox_schema` (custom prefix: `m001_create_toolkit_outbox_schema__<prefix>`). Tables = `<prefix>_<suffix>`:
`_body` (payload BYTEA/BLOB, payload_type, created_at, trace), `_partitions` (queue, partition, sequence; UNIQUE(queue,partition)), `_incoming`, `_outgoing`, `_dead_letters` (partition_id, seq, payload, payload_type, created_at, failed_at, last_error, attempts, status, completed_at, deadline, trace), `_processor` (cursor + lease), `_vacuum_counter`, `_trace`, MySQL-only `_body_id_sequence`, `_incoming_id_sequence`.
So default tables are `toolkit_outbox_body`, `toolkit_outbox_partitions`, `toolkit_outbox_incoming`, `toolkit_outbox_outgoing`, `toolkit_outbox_dead_letters`, `toolkit_outbox_processor`, `toolkit_outbox_vacuum_counter`, `toolkit_outbox_trace`.
Prefix rules: ASCII letter first, `[A-Za-z0-9_]`, max 36 chars, no schema qualification. DDL is `CREATE TABLE IF NOT EXISTS` (idempotent). types-registry uses `TABLE_PREFIX = "types_registry__outbox"`. Each gear has its own DB file in SQLite deployments, but on a shared PG database a distinct prefix (e.g. `mini_chat__outbox`) avoids sharing tables with other gears. Runtime and migrations MUST use the same prefix (`.table_prefix(PREFIX)?`).

### 3.3 Enqueue API (`outbox/core.rs:452-520`, `outbox/record.rs`)
```rust
impl Outbox {
    pub fn builder(db: Db) -> OutboxBuilder;
    pub async fn enqueue(&self, db: &(impl DBRunner + Sync + ?Sized), msg: Record<'_>) -> Result<Wake, OutboxError>;
    pub async fn enqueue_batch(&self, db: &(impl DBRunner + Sync + ?Sized), batch: Records<'_>) -> Result<Wake, OutboxError>;
}
// single (validated at build(): queue name, payload_type printable ASCII 1..1024, payload <= 64 KiB, trace <= 256)
let record = Record::to(QUEUE, partition /* u32 in 0..partitions */)
    .payload(bytes /* Vec<u8> */, "mini_chat.usage.v1" /* payload_type */)
    .trace("optional-unique-id")          // optional
    .build()?;                            // Err(OutboxError::PayloadTooLarge { size, max }) when > MAX_PAYLOAD_SIZE (64*1024)
let wake = outbox.enqueue(tx /* &DbTx */, record).await?;   // inside the business tx
// batch (all-or-nothing):
let wake = outbox.enqueue_batch(tx, Records::to(QUEUE).payload_type("application/json")
    .push(0, a).push_with_type(1, b, "other.type").build()?).await?;
let mut w = Wake::empty(); w += outbox.enqueue(tx, r1).await?;   // combine
```
The payload-size limit constant lives in `pub(crate) mod validation` (`outbox/validation.rs:23`: `pub const MAX_PAYLOAD_SIZE: usize = 64 * KIB_LEN;`) — **not reachable from gears**; detect via `OutboxError::PayloadTooLarge { size, max }` (mini-chat ADR-0004 maps this to 400 `invalid_argument` on chat delete).
`Wake` is `#[must_use]`: call `wake.fire()` **after commit** (or `wake.discard()`/drop on rollback). Or let the lib do it:
```rust
pub async fn in_transaction<F, T, E>(db: &Db, f: F) -> Result<T, E>   // outbox/transaction.rs:32
  where E: From<DbError> + Send + 'static,
        F: for<'a> FnOnce(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<(T, Wake), E>> + Send + 'a>> + Send;
```
`OutboxError` (`outbox/types.rs`, `#[non_exhaustive]`): `QueueNotRegistered(String)`, `PartitionOutOfRange{queue,partition,max}`, `PayloadTooLarge{size,max}`, `PartitionCountMismatch{queue,expected,found}`, `EmptyTracedBatch`, `TracedBatchTooLarge{entities}`, `InvalidQueueName{reason}`, `InvalidPayloadType{reason}`, `InvalidTrace{reason}`, `TraceTooLong{size,max}`, `InvalidTablePrefix{reason}`, `Database(#[from] sea_orm::DbErr)`.
Queue name rule: `[a-zA-Z0-9._-]{1,1024}`, starts/ends alphanumeric (`mini-chat.audit`, `mini-chat.usage_snapshot` are valid). Partition count: `Partitions::of(n)` — power of 2 in 1..=64, panics otherwise (const-assert). Enqueue before the pipeline is started works only if the queue was registered (registration happens in `start()`), otherwise `QueueNotRegistered`.

### 3.4 Pipeline start / stop (`outbox/manager.rs`, `outbox/builder.rs`)
```rust
let handle: OutboxHandle = Outbox::builder(db /* toolkit_db::Db, e.g. ctx.db_required()?.db() */)
    .table_prefix(TABLE_PREFIX)?                         // Result<Self, OutboxError>
    .profile(OutboxProfile::low_latency())               // default_profile() / low_latency() / high_throughput()
    .processor_tuning(WorkerTuning::processor_low_latency().batch_size(1))   // optional
    .queue("mini-chat.usage_snapshot", Partitions::of(8))
        .leased(UsageHandler { .. })                     // LeasedHandler or LeasedMessageHandler
    .queue("mini-chat.thread_summary", Partitions::of(4))
        .leased(SummaryHandler { .. })
        .lease(LeaseConfig { duration: Duration::from_secs(120), headroom: Duration::from_secs(5) })  // per-queue; default 30s/2s; panics if headroom >= duration
    .queue("mini-chat.audit", Partitions::of(4))
        .leased(AuditHandler { .. })
        .lease(LeaseConfig { duration: Duration::from_secs(60), headroom: Duration::from_secs(2) })
    .start().await?;                                      // registers queues (idempotent; PartitionCountMismatch if count changed), spawns workers
let outbox: &Arc<Outbox> = handle.outbox();
handle.stop().await;                                       // cancels + joins workers; dropping the handle also cancels
```
Other builder knobs: `.processors(n)`, `.maintenance(guaranteed, shared)`, `.partition_batch_limit(n)`, `.sequencer_tuning(..)`, `.vacuum_tuning(..)`, `.reconciler_tuning(..)`, `.stats_interval(d)`, `.transactional(h)` / `.batch_transactional(h)` instead of `.leased`.
WorkerTuning processor presets (`types.rs:446-495`): `processor_default()` batch 10, retry 1s→60s; `processor_low_latency()` batch 10, retry 100ms→10s; `processor_high_throughput()` batch 100. Builder methods `.batch_size/.retry_base/.retry_max/.degradation_threshold/.lease_duration/.stop_grace(..)`. `stop_grace` default 5s.

### 3.5 Handlers (`outbox/handler.rs`)
```rust
pub struct OutboxMessage { pub partition_id: i64, pub seq: i64, pub payload: Vec<u8>, pub payload_type: String,
                           pub created_at: chrono::DateTime<chrono::Utc>, pub attempts: i16 /* retries so far, 0 on first */ }
pub enum MessageResult { Ok, Retry, Reject(String) }                // per-message
pub enum HandlerResult { Success, Retry { reason: String }, Reject { reason: String } }   // batch
#[async_trait::async_trait] pub trait LeasedMessageHandler: Send + Sync { async fn handle(&self, msg: &OutboxMessage) -> MessageResult; }
#[async_trait::async_trait] pub trait LeasedHandler: Send + Sync { async fn handle(&self, batch: &mut Batch<'_>) -> HandlerResult; }
#[async_trait::async_trait] pub trait TransactionalMessageHandler: Send + Sync {
    async fn handle(&self, txn: &sea_orm::DatabaseExecutor<'_>, msg: &OutboxMessage) -> HandlerResult; }
```
Blanket `impl<H: LeasedMessageHandler> LeasedHandler for H` (loop: Ok→`batch.ack()`, Reject→`batch.reject(reason)` = dead-letter that one and continue, Retry→whole remainder retried with exponential backoff, attempts++). `Batch` API: `next_msg()`, `next_chunk(n)`, `ack()`, `ack_chunk(n)`, `reject(String)`, `remaining() -> Duration` (until cancel point), `should_stop()`, `len()`, `is_empty()`, `processed()`.
Leased = at-least-once; handler future is dropped at `lease.duration - headroom` or `stop_grace` after stop → **handlers must be idempotent**.
**Attempts caveat** (types-registry `infra/outbox.rs:150-165`): attempts are counted per partition/batch, not per message; to use `attempts` as a per-message budget set processor `batch_size(1)`.
Bounded retry pattern (e.g. mini-chat audit "dead-letter on 120th attempt"): `if msg.attempts >= MAX { return MessageResult::Reject(reason) } else { MessageResult::Retry }`.

Minimal handler:
```rust
struct AuditHandler { gw: Arc<AuditGateway> }
#[async_trait::async_trait]
impl LeasedMessageHandler for AuditHandler {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        let ev: AuditEvent = match serde_json::from_slice(&msg.payload) {
            Ok(e) => e, Err(e) => return MessageResult::Reject(format!("bad payload: {e}")) };
        match self.gw.emit(&ev).await {
            Ok(()) => MessageResult::Ok,
            Err(_) if msg.attempts >= 119 => MessageResult::Reject("retry budget exhausted".into()),
            Err(_) => MessageResult::Retry,
        }
    }
}
```

### 3.6 Complete real usage: types-registry (`gears/system/types-registry/types-registry/src/infra/outbox.rs`, 597 lines)
- `pub const TABLE_PREFIX: &str = "types_registry__outbox"; pub const QUEUE: &str = "admission"; const PARTITIONS: u16 = 8;`
- partition routing: `u32::from(u16::from_be_bytes([bytes[14], bytes[15]]) % PARTITIONS)` from a UUID (stable across processes).
- Enqueuer bound late via `OnceLock<Weak<Outbox>>` (services exist before the pipeline starts):
```rust
async fn enqueue(&self, tx: &DbTx<'_>, operation_id: Uuid) -> Result<Wake, OutboxError> {
    let outbox = self.outbox.get().and_then(Weak::upgrade).ok_or(OutboxError::NotRunning)?;  // NotRunning is the gear's own error
    let record = Record::to(QUEUE, partition(operation_id)).payload(payload(operation_id), PAYLOAD_TYPE).build()?;
    Ok(outbox.enqueue(tx, record).await?)
}
pub async fn start(db: Db, registry: &Arc<RegistryService>, dispatch: &Arc<OutboxDispatch>) -> Result<OutboxHandle, OutboxError> {
    let handle = Outbox::builder(db)
        .table_prefix(TABLE_PREFIX)?
        .profile(OutboxProfile::low_latency())
        .processor_tuning(AdmissionHandler::required_tuning())   // WorkerTuning::processor_low_latency().batch_size(1)
        .queue(QUEUE, Partitions::of(PARTITIONS))
        .leased(AdmissionHandler::new(Arc::clone(registry), registry.max_delivery_attempts()))
        .lease(lease_config(registry.operation_timeout()))
        .start().await?;
    dispatch.bind(handle.outbox())?;
    Ok(handle)
}
```
- `LeasedHandler` impl looping `batch.next_msg()` with `batch.remaining()` used as deadline (`outbox.rs:520-550`).
- Gear keeps `outbox: parking_lot::Mutex<Option<OutboxHandle>>`; stop: `let h = self.outbox.lock().take(); if let Some(h) = h { h.stop().await }` (`gear.rs:96-114`).
- A minimal doc-style example: `libs/toolkit-db/examples/outbox_gear_integration.rs` (enqueuer port trait + `Wake` trait + noop for tests), plus `outbox_retry_reject.rs`, `outbox_dead_letters.rs`, `outbox_batch_multi_queue.rs`, `outbox_custom_prefix.rs`.

### 3.7 Dead letters (`outbox/core.rs:657-745`, `outbox/dead_letter.rs`)
```rust
outbox.dead_letter_list(&conn, &DeadLetterFilter::default().queue("mini-chat.audit").status(DeadLetterStatus::Pending).limit(50)).await?;  // Vec<DeadLetterMessage>
outbox.dead_letter_count(&conn, &filter).await?;                 // u64
outbox.dead_letter_replay(&conn, &DeadLetterScope::default().payload_type("x"), Duration::from_secs(60)).await?; // claims -> reprocessing
outbox.dead_letter_resolve(&conn, &[id]).await?;                  // -> resolved
outbox.dead_letter_reject(&conn, &[id], "reason").await?;        // -> pending, attempts++
outbox.dead_letter_discard(&conn, &scope).await?;                 // pending -> discarded
outbox.dead_letter_cleanup(&conn, &scope).await?;                 // delete resolved+discarded
// DeadLetterMessage { id, partition_id, seq, payload, payload_type, created_at, failed_at, last_error, attempts, status, completed_at, deadline }
// DeadLetterStatus { Pending, Reprocessing, Resolved, Discarded }; DeadLetterScope { partition_id, queue, payload_type, limit } + builder methods
```
`conn` is any `&impl DBRunner` (`db.conn()?`).

### 3.8 SQLite notes
Outbox on SQLite is single-process only (no row locks). It spawns background tasks that use separate pool connections → in tests use a **shared-cache** in-memory DB with >1 connection: `sqlite:file:<unique>?mode=memory&cache=shared`, `max_conns: Some(4)` (types-registry `tests/common/mod.rs:117-140`). Plain `sqlite::memory:` gives every pooled connection its own empty DB.

---------------------------------------------------------------------------------------------------

## 4. Database configuration

YAML (`config/mini-chat.yaml:12-23, 145-148`):
```yaml
server:
  home_dir: "~/.cf-gears"
database:
  servers:
    sqlite_mini_chat:
      engine: "sqlite"
      params: { WAL: "true", synchronous: "NORMAL", busy_timeout: "5000" }
      pool: { max_conns: 5, acquire_timeout: "30s" }
gears:
  mini-chat:
    database:
      server: "sqlite_mini_chat"
      file: "mini_chat.db"
    config: { ... }
```
Resolution (`libs/toolkit-db/src/manager.rs:27-270`): `DbManager::from_figment(figment, home_dir)` reads top-level `database` → `GlobalDatabaseConfig { servers: HashMap<String, DbConnConfig>, auto_provision: Option<bool> }`; `DbManager::get(gear)` reads `gears.<gear>.database` → `DbConnConfig`, merges the referenced `server` (gear fields win; `params` merged; pool: gear overrides; `file`/`path` gear-only), then `file` → `<home_dir>/<gear-name>/<file>` (dirs auto-created unless `auto_provision: false`), then `build_db_handle` → `Db` (cached per gear) → `GearCtx::db()` returns `DBProvider<DbError>`.
`DbConnConfig` fields (`config.rs`): `engine: Option<DbEngineCfg{Postgres,Mysql,Sqlite}>`, `dsn`, `host`, `port`, `user`, `password` (`${ENV}` expansion), `dbname`, `params` (SQLite PRAGMA whitelist / PG runtime params like `statement_timeout`), `file`, `path`, `pool: PoolCfg{max_conns,min_conns,acquire_timeout,idle_timeout,max_lifetime,..}`, `lock_keepalive`, `server`. A gear whose `database` key is missing gets `ctx.db() == None`.
PG example: `servers: { pg: { dsn: "postgres://user:${DB_PASSWORD}@host:5432/db" } }` + `gears.mini-chat.database: { server: pg }`.

Programmatic / tests (`lib.rs:120-140`):
```rust
pub async fn connect_db(dsn: &str, opts: ConnectOpts) -> Result<Db>;     // toolkit_db::connect_db
pub async fn build_db(cfg: DbConnConfig, global: Option<&GlobalDatabaseConfig>) -> Result<Db>;
pub struct ConnectOpts { max_conns: Option<u32> /*10*/, min_conns, acquire_timeout /*30s*/, idle_timeout, max_lifetime,
                         test_before_acquire: bool, create_sqlite_dirs: bool /*true*/, lock_keepalive }  // Default impl
```
DSNs: `sqlite::memory:`, `sqlite:file:name?mode=memory&cache=shared`, `sqlite://path.db?mode=rwc&journal_mode=wal`, `postgres://...`. In-memory SQLite defaults to journal DELETE, file DBs to WAL, busy_timeout 5000 ms (`lib.rs:330-420`).

Test helper pattern (credstore `src/infra/storage/repo_impl/repo_tests.rs:32-50`):
```rust
use sea_orm_migration::MigratorTrait;
use toolkit_db::migration_runner::run_migrations_for_testing;   // pub async fn (db: &Db, Vec<Box<dyn MigrationTrait>>) -> Result<MigrationResult, MigrationError>
use toolkit_db::{ConnectOpts, DBProvider, connect_db};
let dsn = format!("sqlite:file:mini_chat_{}?mode=memory&cache=shared", Uuid::new_v4());
let db = connect_db(&dsn, ConnectOpts { max_conns: Some(1), min_conns: Some(1), ..Default::default() }).await.expect("connect");
let mut migs = Migrator::migrations();
migs.extend(toolkit_db::outbox::outbox_migrations_with_prefix(OUTBOX_PREFIX).unwrap());
run_migrations_for_testing(&db, migs).await.expect("migrations");
let provider = Arc::new(DBProvider::<DomainError>::new(db));
```
(types-registry harness with variants for file/WAL/outbox: `gears/system/types-registry/types-registry/tests/common/mod.rs:63-140`.) Use `max_conns: Some(1)` with plain `sqlite::memory:`; use the shared-cache form + 4 conns when the outbox runs. Also `run_migrations_for_gear(&db, "mini-chat", migs)` exists.

---------------------------------------------------------------------------------------------------

## 5. Errors & unique-violation detection

- `toolkit_db::DbError` (`lib.rs:180-260`): `UnknownDsn`, `FeatureDisabled`, `InvalidConfig`, `ConfigConflict`, `InvalidSqlitePragma{key,message}`, `UnknownSqlitePragma`, `InvalidParameter`, `SqlitePragma`, `EnvVar{name,source}`, `UrlParse`, `Sqlx(sqlx::Error)`, `Sea(sea_orm::DbErr)`, `Io`, `Lock(DbLockError)`, `Other(anyhow::Error)`, `ConnRequestedInsideTx`. `From<ScopeError> for DbError` → `Other`. Domain errors used with `DBProvider<E>` need `impl From<DbError> for E`.
- `toolkit_db::secure::ScopeError` (`secure/error.rs:14-60`, `#[non_exhaustive]`): `Db(#[from] sea_orm::DbErr)`, `Invalid(&'static str)`, `TenantNotInScope{tenant_id}`, `Denied(&'static str)`, `UnresolvedScopeProperty{..}`, `GraphSyntax(String)`. Methods: `.is_unique_violation()`, `.is_foreign_key_violation()`.
- Free fns (`secure/error.rs:93-140`), re-exported from `toolkit_db::secure`:
```rust
pub fn is_unique_violation(err: &sea_orm::DbErr) -> bool;      // SqlErr::UniqueConstraintViolation, or msg contains "unique constraint"/"duplicate key"/"unique_violation"/"duplicate entry"
pub fn is_foreign_key_violation(err: &sea_orm::DbErr) -> bool;
```
  Usage: `Err(MyErr::Storage(e)) if e.is_unique_violation() => /* conflict / re-read */` (types-registry `acceptance.rs:587`); credstore's own copy `infra/error_conv.rs:26` (+ `is_check_violation`).
- `toolkit_db::contention::is_retryable_contention(backend: DbBackend, err: &DbErr) -> bool` (`contention.rs:106`) — PG serialization/deadlock, MySQL deadlock, SQLite BUSY; used by `Db::transaction_with_retry`.
- `TxError<E> { Domain(E), Infra(InfraError) }` + `.into_domain(map_infra)` for the consuming `in_transaction` API.
- `OutboxError` (see 3.3), `MigrationError` (`migration_runner.rs:34`), `DbLockError` (`advisory_locks.rs:1798`).

---------------------------------------------------------------------------------------------------

## 6. Leader election

**No leader-election helper exists in `libs/`** (grep for leader/elector/Lease/coordination over `libs/**/*.rs` finds nothing relevant; `toolkit-k8s-auth` has no Lease code). mini-chat DESIGN/ADR-0010 (`gears/mini-chat/docs/ADR/0010-...md:16,37`, `DESIGN.md:595-604, 7137`) specify a **gear-local** elector: Kubernetes Lease via `kube` when built with the `k8s` feature (Leases `mini-chat-orphan-watchdog`, `mini-chat-upload-reaper`, duration 15 s, renew 2 s, needs `POD_NAMESPACE`/`POD_NAME`, create Lease if missing), else a no-op elector (always leader); correctness relies on CAS guards. `kube`/`k8s-openapi` are already optional deps in `gears/mini-chat/mini-chat/Cargo.toml`.

Closest library primitive: DB advisory locks (`libs/toolkit-db/src/advisory_locks.rs`):
```rust
let guard: Option<DbLockGuard> = db.try_lock("mini-chat", "orphan-watchdog",
    LockConfig { max_wait: Some(Duration::from_millis(0)), max_retries: Some(0), ..Default::default() }).await?;
// PG: session pg_try_advisory_lock on a dedicated 1-conn pool; MySQL GET_LOCK; SQLite/fallback: file marker lock under OS cache dir
guard.release().await?;   // DbLockGuard::release(self) -> Result<(), DbLockError>; Drop is best-effort
```
`LockConfig { max_wait: Option<Duration> /*30s*/, max_retries: Option<u32>, backoff_base_ms, backoff_factor, max_backoff, jitter }`.
Also, outbox partitions are leased per processor, so outbox handler work is already single-consumer per partition without election.

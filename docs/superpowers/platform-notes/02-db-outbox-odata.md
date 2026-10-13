# Platform note 02: DB (Secure ORM), migrations, outbox, OData pagination

This note is for engineers building the `mini-chat` gear with SeaORM on SQLite and Postgres.
Everything below comes from the source as of this commit. Paths are relative to `/app`.
"(real)" marks a snippet copied from repository code. "(composed)" marks a snippet assembled
from real APIs that no gear uses in exactly this form. Read it instead of re-reading the
platform crates.

Crates and Rust names:

| Cargo dep (workspace key) | Rust crate | Purpose |
|---|---|---|
| `toolkit-db` (`features = ["sqlite", "pg"]`) | `toolkit_db` | `Db`, `DBProvider`, Secure ORM, migrations runner, outbox, OData-for-SeaORM, advisory locks. Re-exports `toolkit_db::sea_orm_migration`. |
| `toolkit-db-macros` | `toolkit_db_macros` | `#[derive(Scopable)]`. Also re-exported as `toolkit_db::secure::Scopable`. |
| `toolkit-security` | `toolkit_security` | `AccessScope`, `ScopeFilter`, `ScopeConstraint`, `pep_properties`, `SecurityContext`. |
| `toolkit-odata` (`features=["with-utoipa"]`) | `toolkit_odata` | `ODataQuery`, `Page`, `PageInfo`, `CursorV1`, `Error`, `filter::{FilterField, FieldKind, ...}`. |
| `toolkit-odata-macros` | `toolkit_odata_macros` | `#[derive(ODataFilterable)]`. **Not** currently in `gears/mini-chat/mini-chat/Cargo.toml`. Add `toolkit-odata-macros = { workspace = true }` or hand-write `FilterField`. |
| `toolkit` | `toolkit` | `GearCtx::db_required()`, `contracts::DatabaseCapability`, `api::odata::OData` extractor. |
| `sea-orm` / `sea-orm-migration` 2.0.2 | | Workspace features: `with-uuid, with-chrono, with-time, with-json, with-rust_decimal, macros, sqlite-use-returning-for-3_35`. `RETURNING` works on SQLite. |

`mini-chat` already depends on `toolkit-db` (sqlite+pg), `toolkit-db-macros`, `toolkit-security`,
`toolkit-odata` (with-utoipa), `sea-orm`, `sea-orm-migration`, `time`, `uuid`, and
`toolkit-canonical-errors` (feature `axum`). `chrono` is a dev-dependency only.

---

## 1. libs/toolkit-db

### 1.1 DB handle types a gear receives

```rust
// libs/toolkit/src/context.rs
pub fn db(&self) -> Option<toolkit_db::DBProvider<toolkit_db::DbError>>;
pub fn db_required(&self) -> anyhow::Result<toolkit_db::DBProvider<toolkit_db::DbError>>;
```

| Type | Path | Notes |
|---|---|---|
| `DBProvider<E>` | `toolkit_db::DBProvider` (`libs/toolkit-db/src/db_provider.rs`) | Cheap `Clone` (holds `Arc<Db>`). `E: From<DbError> + Send + 'static`. Methods: `new(db: Db)`, `db() -> Db`, `conn() -> Result<DbConn<'_>, E>`, `transaction(f)`, `transaction_with_config(cfg, f)`. |
| `Db` | `toolkit_db::Db` (= `secure::Db`) | `Clone`. `conn()`, `backend() -> sea_orm::DbBackend`, `db_engine() -> &'static str` ("postgres"/"sqlite"/...), `lock`, `try_lock`, many `transaction*` variants. Pass it to `Outbox::builder`. |
| `DbConn<'a>` | `toolkit_db::DbConn` | Non-transactional runner. Not Clone. `Db::conn()` returns `Err(DbError::ConnRequestedInsideTx)` when called inside a transaction closure (task-local guard). |
| `DbTx<'a>` | `toolkit_db::DbTx` | Transactional runner. Exists only inside the closure. |
| `DBRunner` | `toolkit_db::secure::DBRunner` (`#[doc(hidden)]`, sealed) | Implemented by `DbConn`, `DbTx`, `SecureConn`, `SecureTx`. **Repositories take `&impl DBRunner`** so one function serves both conn and tx. |
| `DbError` | `toolkit_db::DbError` | Variants include `Sea(sea_orm::DbErr)`, `Lock(DbLockError)`, `Other(anyhow::Error)` (a `ScopeError` converts to `Other`), `ConnRequestedInsideTx`, and config/connection variants. |

The wiring pattern re-types the provider with the gear's domain error (real, `gears/credstore/credstore/src/gear.rs`):

```rust
#[toolkit::gear(
    name = "credstore",
    deps = [authz_resolver, tenant_resolver, types_registry],
    capabilities = [system, db, rest, stateful],
    lifecycle(entry = "serve", stop_timeout = "30s", await_ready)
)]
pub struct CredStoreGear { service: OnceLock<Arc<Service>> }
// in Gear::init:
let db_raw = ctx.db_required()?;
let db: Arc<DBProvider<crate::domain::error::DomainError>> =
    Arc::new(DBProvider::new(db_raw.db()));
```

`DomainError` must implement `From<toolkit_db::DbError>`. Credstore's version (real, `gears/credstore/credstore/src/infra/canonical_mapping.rs`):

```rust
impl From<DbError> for DomainError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::Sea(db) => classify_db_err_to_domain(db),
            other => Self::Internal { diagnostic: "database error".to_owned(), cause: Some(Box::new(other)) },
        }
    }
}
```

Gear config (real, `config/mini-chat.yaml`): `gears.mini-chat.database: { server: "sqlite_mini_chat", file: "mini_chat.db" }`.
The server is `engine: "sqlite"` with params `WAL`, `synchronous`, `busy_timeout`. SQLite
defaults when not overridden (`libs/toolkit-db/src/lib.rs`): file DBs use WAL, `synchronous=NORMAL`,
and `busy_timeout` 5000 ms. sqlx turns `foreign_keys=ON` on by default. `ConnectOpts::default()` has `max_conns = 10`.

### 1.2 `#[derive(Scopable)]`

Source: `libs/toolkit-db-macros/src/scopable.rs`. It emits `impl ::toolkit_db::secure::ScopableEntity for Entity`.
Put it on the SeaORM `Model` next to `DeriveEntityModel`. **Each of the four dimensions needs exactly one explicit decision**, or the code fails to compile:

| Dimension | Set it | Opt out | PEP property the column answers |
|---|---|---|---|
| tenant | `tenant_col = "tenant_id"` | `no_tenant` | `"owner_tenant_id"` (`pep_properties::OWNER_TENANT_ID`) |
| resource | `resource_col = "id"` | `no_resource` | `"id"` (`pep_properties::RESOURCE_ID`) |
| owner | `owner_col = "user_id"` | `no_owner` | `"owner_id"` (`pep_properties::OWNER_ID`) |
| type | `type_col = "kind"` | `no_type` | (no property resolves to it) |
| custom | `pep_prop(prop_name = "column_name")` (repeatable) | | `"prop_name"` |
| global | `unrestricted` (no other attribute allowed) | | `IS_UNRESTRICTED = true`, all `None` |

The macro turns column names into `Column::UpperCamel` (`"tenant_id"` becomes `Column::TenantId`).
These are compile errors: missing decision, both `x_col` and `no_x`, duplicate attrs, `pep_prop` using a reserved name
(`owner_tenant_id`, `id`, `owner_id`), and empty names.

Real entity (`gears/credstore/credstore/src/infra/storage/entity/secrets.rs`):

```rust
use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db::secure::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "credstore_secrets")]
#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub reference: String,
    pub sharing: i16,            // SMALLINT, mapped to an enum in the repo layer
    pub owner_id: Uuid,          // NOT declared as owner_col on purpose
    pub status: i16,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub version: i64,
    pub secret_type_uuid: Uuid,
    pub expires_at: Option<OffsetDateTime>,
    pub value_fp: Option<Vec<u8>>,   // BLOB / BYTEA
    pub fp_key_id: Option<i16>,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
```

Global table (real, `gears/system/types-registry/.../entity/operation.rs`): `use toolkit_db_macros::Scopable;` plus `#[secure(unrestricted)]`.
SMALLINT enums (real, `.../entity/enums.rs`):

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i16", db_type = "SmallInteger")]
pub enum EntityKind { #[sea_orm(num_value = 1)] TypeSchema, #[sea_orm(num_value = 2)] Instance }
```

For a mini-chat user-owned table:
`#[secure(tenant_col = "tenant_id", resource_col = "id", owner_col = "user_id", no_type)]`.

### 1.3 `AccessScope`

`toolkit_security::AccessScope` is re-exported as `toolkit_db::secure::AccessScope` (`libs/toolkit-security/src/access_scope.rs`).
It is an OR of `ScopeConstraint`s. Each constraint is an AND of `ScopeFilter`s. `Default` is deny-all.

```rust
AccessScope::deny_all() / allow_all()              // allow_all = unconstrained: no WHERE added
AccessScope::for_tenant(id) / for_tenants(vec![..]) // IN on "owner_tenant_id"
AccessScope::for_resource(id) / for_resources(vec![..])
AccessScope::from_constraints(vec![ScopeConstraint::new(vec![
    ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant_id),
    ScopeFilter::eq(pep_properties::OWNER_ID, user_id),
])])                                               // ScopeConstraint::new panics on empty; try_new -> Result
AccessScope::single(constraint)
// Filters: ScopeFilter::eq(prop, v) | r#in(prop, Vec<ScopeValue>) | in_uuids(prop, Vec<Uuid>)
//          | in_group / in_group_subtree / in_tenant_subtree (subquery filters)
// Queries: is_deny_all(), is_unconstrained(), constraints(), has_property(p),
//          all_uuid_values_for(p), contains_uuid(p, id)  (contains_uuid is NOT an authz decision)
```

Narrowing helpers:

- `scope.ensure_owner(user_id) -> AccessScope` adds an owner predicate, with intersection semantics.
  Unconstrained becomes `[owner_id = user]`. Deny-all stays deny-all. A constraint without `owner_id` gets `owner_id = user` injected.
  A constraint whose `owner_id` filter contains `user` is narrowed to `Eq`. A constraint whose filter does not contain `user` is dropped.
- `scope.tenant_only()` keeps only `owner_tenant_id` filters. This **widens** the scope (it drops other AND terms), and an unconstrained scope becomes deny-all.
  Use it only to re-target a scope at a child entity that has no owner/resource columns, after the parent was checked with the full scope.
- `scope.tenant_and_owner()` keeps `owner_tenant_id` and `owner_id`.

Building a scope from authz (real, `gears/credstore/credstore/src/domain/authz.rs`):

```rust
use authz_resolver_sdk::pep::{AccessRequest, EnforcerError, PolicyEnforcer, ResourceType};
use toolkit_security::{AccessScope, SecurityContext, pep_properties};

pub fn secret_type_resource(gts_id: &str) -> ResourceType {
    ResourceType::new(gts_id.to_owned(), &[pep_properties::OWNER_TENANT_ID])
}   // const variant: ResourceType::from_static("gts...~", &[...])

pub async fn scope_for(enforcer: &PolicyEnforcer, ctx: &SecurityContext,
                       resource: &ResourceType, action: &str) -> Result<AccessScope, DomainError> {
    let tenant = ctx.subject_tenant_id();
    let request = AccessRequest::new()
        .resource_property(pep_properties::OWNER_TENANT_ID, tenant)
        .require_constraints(true);
    enforcer.access_scope_with(ctx, resource, action, None, &request).await.map_err(map_enforcer_err)
}
// map_enforcer_err: Denied | CompileFailed -> 403 AccessDenied; EvaluationFailed -> 503.
```

The `PolicyEnforcer` API (`gears/system/authz-resolver/authz-resolver-sdk/src/pep/enforcer.rs`):
- `PolicyEnforcer::new(Arc<dyn AuthZResolverApi>)`
- `access_scope(&ctx, &ResourceType, action, Option<Uuid>)`, which always sets `require_constraints=true`
- `access_scope_with(.., &AccessRequest)`

To also restrict rows to the caller: `let scope = enforcer.access_scope(..).await?.ensure_owner(ctx.subject_id());`
(this needs the entity to declare `owner_col`).

How a scope compiles to SQL (`libs/toolkit-db/src/secure/cond.rs`, `build_scope_condition`):

- Deny-all compiles to `WHERE false`. Unconstrained adds no filter.
- Constraints are ORed. Filters inside a constraint are ANDed.
- A property the entity cannot resolve **fails that constraint closed**. Example: the scope carries `owner_id` but the entity has `no_owner`.
  If every constraint fails, the result is deny-all. That is why `tenant_only()` exists.

### 1.4 Secure SELECT

Trait imports: `use toolkit_db::secure::{SecureEntityExt, SecureInsertExt, SecureUpdateExt, SecureDeleteExt};`.
File: `libs/toolkit-db/src/secure/select.rs`.

```rust
Entity::find()                                  // any sea_orm::Select<E>: filters, order_by, lock_* can go BEFORE .secure()
    .filter(Column::ChatId.eq(chat_id))
    .secure()                                   // SecureSelect<E, Unscoped>
    .scope_with(&scope)                         // -> SecureSelect<E, Scoped>  (NOT a Result; some doc comments wrongly show `?`)
    .and_id(id)?                                // Result<Self, ScopeError>; needs resource_col
    .filter(Condition::all().add(..))           // more filters on Scoped
    .order_by(Column::CreatedAt, Order::Asc).limit(n).offset(k)
    .one(&conn).await?;                         // .all / .one / .count -> Result<_, ScopeError>
// scope_with_arc(Arc<AccessScope>), and_scope_for::<J>(&scope), scope_via_exists::<J>(&scope),
// find_also_related(R) / find_with_related(R), model.secure_find_related(R, &scope) (SecureFindRelatedExt),
// project_all(&conn, |q| q.select_only().column(..).column_as(..).group_by(..).into_model::<T>()),
// into_inner() -> sea_orm::Select<E> (escape hatch).
```

Workspace clippy (`clippy.toml`) disallows `sea_orm::Select::{all,one,count}`, `UpdateMany::exec`, and `DeleteMany::exec`.
Always execute through the secure wrappers.

### 1.5 Secure INSERT / UPDATE / DELETE

File: `libs/toolkit-db/src/secure/db_ops.rs`.

```rust
// Validated single insert. Returns the Model. Tenant-scoped entities need tenant_id Set
// (ScopeError::Invalid("tenant_id is required") otherwise). All scope constraints are checked
// in memory against the ActiveModel: ScopeError::Denied / TenantNotInScope.
pub async fn secure_insert<E>(am: E::ActiveModel, scope: &AccessScope, runner: &impl DBRunner)
    -> Result<E::Model, ScopeError>;
// Batch insert. Every model is validated first, then the batch is split by bind-param limits.
// A split batch is not atomic; wrap it in a tx.
pub async fn secure_insert_many<E>(models: Vec<E::ActiveModel>, scope: &AccessScope, runner: &impl DBRunner)
    -> Result<(), ScopeError>;
// Load-in-scope then update by id. tenant_id is immutable (ScopeError::Denied("tenant_id is immutable")).
pub async fn secure_update_with_scope<E>(am: E::ActiveModel, scope: &AccessScope, id: Uuid, runner: &impl DBRunner)
    -> Result<E::Model, ScopeError>;
pub fn validate_tenant_in_scope(tenant_id: Uuid, scope: &AccessScope) -> Result<(), ScopeError>;
pub fn max_bind_params_for(runner: &impl DBRunner) -> usize;
pub async fn secure_insert_from_select<E, C>(columns: C, source: SelectStatement, scope: &AccessScope, runner) -> Result<u64, ScopeError>;
//   ^ only for entities with no scope columns, and only with an unconstrained scope
```

Builder forms:

```rust
Entity::insert(am.clone()).secure().scope_with_model(&scope, &am)?    // validates
      .on_conflict(SecureOnConflict::<Entity>::columns([Column::A, Column::B]).update_columns([Column::C])?)
      .exec(&conn).await?;            // -> InsertResult;  .exec_with_returning(&conn) -> Model
Entity::insert(am).secure().scope_unchecked(&scope)?.exec(&conn).await?;   // no validation (credstore does this)
Entity::insert_many(rows).secure().scope_unchecked(&scope)?.exec(&conn).await?;
Entity::update_many()
    .col_expr(Column::Status, Expr::value(2i16))     // setting tenant col -> Denied at exec
    .filter(Condition::all().add(Column::Id.eq(id)).add(Column::Version.eq(v)))
    .secure().scope_with(&scope)
    .exec(&conn).await?.rows_affected;               // .exec_with_returning(&conn) -> Vec<Model>
Entity::delete_many().filter(cond).secure().scope_with(&scope).exec(&conn).await?.rows_affected;
```

Insert validation semantics (`validate_insert_scope`):
- Constraints are ORed and filters ANDed.
- A filter on a column the ActiveModel leaves `NotSet` is skipped.
- An unresolvable property fails the constraint.
- `allow_all` passes. Deny-all rejects.

For child rows without tenant/owner columns (e.g. messages scoped through their chat), authorize the parent first.
Then insert with `scope_unchecked` or `AccessScope::allow_all()`. Types-registry does the same for `operation_item`.

Real write (`gears/credstore/credstore/src/infra/storage/repo_impl/writes.rs`):

```rust
pub(super) async fn mark_active(repo: &SecretRepoImpl, scope: &AccessScope, id: Uuid) -> Result<(), DomainError> {
    let conn = repo.db.conn()?;                 // DBProvider<DomainError>::conn
    let now = OffsetDateTime::now_utc();
    let rows_affected = entity::secrets::Entity::update_many()
        .col_expr(entity::secrets::Column::Status, Expr::value(SecretStatus::Active.as_smallint()))
        .col_expr(entity::secrets::Column::UpdatedAt, Expr::value(now))
        .filter(Condition::all()
            .add(entity::secrets::Column::Id.eq(id))
            .add(entity::secrets::Column::Status.eq(SecretStatus::Provisioning.as_smallint())))
        .secure()
        .scope_with(scope)
        .exec(&conn)
        .await
        .map_err(map_scope_err)?
        .rows_affected;
    if rows_affected == 0 { return Err(DomainError::Conflict); }
    Ok(())
}
```

Real repo shape (types-registry `infra/storage/repo/operation_repo.rs`): free functions over `&impl DBRunner`, returning `ScopeError`:

```rust
pub async fn insert(runner: &impl DBRunner, scope: &AccessScope, new: NewOperation) -> Result<OperationRow, ScopeError> {
    let am = operation::ActiveModel { id: Set(new.id), /* ... */ status: Set(OperationStatus::Pending),
                                       created_at: Set(new.now), started_at: Set(None), completed_at: Set(None) };
    operation_row(secure_insert::<operation::Entity>(am, scope, runner).await?)
}
pub async fn find_by_id(runner: &impl DBRunner, scope: &AccessScope, id: Uuid) -> Result<Option<OperationRow>, ScopeError> {
    operation::Entity::find().filter(operation::Column::Id.eq(id))
        .secure().scope_with(scope).one(runner).await?.map(operation_row).transpose()
}
```

### 1.6 Background jobs / "unscoped" access / raw SQL

- **Gear code has no raw-SQL API.** `DBRunner` is sealed, and `Db`/`DBProvider` never expose `DatabaseConnection`.
  Raw SQL (`execute_unprepared`) is only reachable inside migrations (`SchemaManager::get_connection()`).
- System jobs (reapers, watchdogs, outbox handlers) use the same secure builders with `AccessScope::allow_all()`, which adds no WHERE clause.
  Real (`credstore .../repo_impl/reads.rs`):

```rust
let rows: Vec<InventoryRow> = entity::secrets::Entity::find()
    .secure()
    .scope_with(&AccessScope::allow_all())
    .project_all(&conn, |q| q.select_only()
        .column(entity::secrets::Column::Sharing).column(entity::secrets::Column::Status)
        .column_as(entity::secrets::Column::Id.count(), "c")
        .group_by(entity::secrets::Column::Sharing).group_by(entity::secrets::Column::Status)
        .into_model::<InventoryRow>())
    .await.map_err(scope_err_to_domain)?;
```

- For SQL the builders cannot express, there is `SecureCteSelect` (`Entity::find().secure().scope_with(&s).with_ctes()`, `.cte::<J>(..)`, `.recursive_cte(..)`, `.all_as::<T>()`).
  It lives in `secure/cte.rs`. The last fallback is `into_inner()` plus sea-orm types (clippy-disallowed execution).

### 1.7 Transactions

`DBProvider<E>::transaction` (preferred, `libs/toolkit-db/src/db_provider.rs`):

```rust
pub async fn transaction<T, F>(&self, f: F) -> Result<T, E>
where T: Send + 'static,
      F: for<'a> FnOnce(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>> + Send;
pub async fn transaction_with_config<T, F>(&self, tx_config: TxConfig, f: F) -> Result<T, E>;
```

- `Ok` commits and `Err` rolls back. A begin/commit `DbErr` is mapped through `E: From<DbError>` (`DbError::Sea`).
- There is no manual begin/commit API. Pass `tx` (a `&DbTx`) into repo functions taking `&impl DBRunner`.
- Inside the closure, `db.conn()` fails with `DbError::ConnRequestedInsideTx`.
- The future cannot borrow shorter-lived locals, so clone `Arc`s/scopes into it.

Real (`gears/system/types-registry/.../domain/admission/acceptance.rs`), including outbox enqueue and the unique-violation race:

```rust
let tx_scope = scope.clone();
let tx_dispatch = Arc::clone(dispatch);
let tx_stores = Arc::clone(stores);
let insert = db                                   // &DBProvider<AcceptanceError>
    .transaction(|tx| {
        let validated = Arc::clone(&validated);
        let tx_scope = tx_scope.clone();
        let tx_dispatch = Arc::clone(&tx_dispatch);
        let tx_stores = Arc::clone(&tx_stores);
        Box::pin(async move {
            let parent = tx_stores.insert_operation(tx, &tx_scope, NewOperation { /* ... */ }).await?;
            tx_stores.insert_items(tx, &tx_scope, &parent, &validated.items).await?;
            // Enqueue last, so any earlier failure rolls back before a wake exists.
            let wake = tx_dispatch.enqueue(tx, parent.id).await
                .map_err(|e| AcceptanceError::Dispatch(e.into()))?;
            Ok((Accepted { operation_id: parent.id, replayed: false, status: parent.status }, wake))
        })
    })
    .await;
match insert {
    Ok((accepted, wake)) => { wake.fire(); Ok(accepted) }             // fire AFTER commit
    Err(AcceptanceError::Storage(e)) if e.is_unique_violation() => { /* re-read winner outside the tx */ }
    Err(e) => Err(e),
}
```

Other `Db` variants (`libs/toolkit-db/src/secure/db.rs`):
- `transaction_ref(f) -> Result<T, DbError>`
- `transaction_ref_mapped(f) -> Result<T, E>`. This is what `DBProvider` uses.
- `transaction(self, f) -> (Self, anyhow::Result<T>)`
- `in_transaction(self, f) -> (Self, Result<T, TxError<E>>)`. `TxError::{Domain(E), Infra(InfraError)}`.
- `transaction_with_config(self, TxConfig, f)`

Retry on contention:

```rust
pub async fn transaction_with_retry<T, E, X, F>(&self, tx_config: TxConfig, extract_db_err: X, body: F) -> Result<T, E>
where E: From<DbError> + Send + 'static, T: Send + 'static,
      X: Fn(&E) -> Option<&sea_orm::DbErr> + Send,
      F: for<'a> FnMut(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>> + Send;
// DEFAULT_TX_RETRY_ATTEMPTS = 3; transaction_with_retry_max(cfg, n, ..). Body must be idempotent.
```

It retries only when `toolkit_db::contention::is_retryable_contention(backend, &DbErr)` is true.
That covers PG 40001/40P01 ("could not serialize access", "deadlock detected") and SQLite BUSY `(code: 5)`, BUSY_SNAPSHOT `(code: 517)`, and "database is locked".
Call it via `provider.db().transaction_with_retry(..)`, since `DBProvider` does not wrap it.

`TxConfig { isolation: Option<TxIsolationLevel>, access_mode: Option<TxAccessMode> }`:
- Helpers: `TxConfig::serializable()`, `TxConfig::read_only()`, `TxConfig::with_isolation(..)`.
- `TxIsolationLevel::{ReadUncommitted, ReadCommitted, RepeatableRead, Serializable}`
- `TxAccessMode::{ReadOnly, ReadWrite}`
- SQLite is always serializable.

### 1.8 DB error classification

From `libs/toolkit-db/src/secure/error.rs`, re-exported in `toolkit_db::secure`:

```rust
#[non_exhaustive] pub enum ScopeError {
    Db(sea_orm::DbErr), Invalid(&'static str), TenantNotInScope { tenant_id: Uuid }, Denied(&'static str),
    UnresolvedScopeProperty { .. }, GraphSyntax(String),
}
impl ScopeError { pub fn is_unique_violation(&self) -> bool; pub fn is_foreign_key_violation(&self) -> bool; }
pub fn is_unique_violation(err: &sea_orm::DbErr) -> bool;      // SqlErr::UniqueConstraintViolation, then message fallback:
    // "unique constraint" | "duplicate key" | "unique_violation" | "duplicate entry" | "unique constraint failed"
    // PG 23505, SQLite 2067 "UNIQUE constraint failed", MySQL 1062
pub fn is_foreign_key_violation(err: &sea_orm::DbErr) -> bool; // PG 23503, SQLite 787 "FOREIGN KEY constraint failed"
```

There is no library CHECK-violation helper. Credstore has its own (`gears/credstore/credstore/src/infra/error_conv.rs`).
It matches lowercase message text: "check constraint", "check_violation", "sqlite_constraint_check", SQLSTATE 23514.
`toolkit-canonical-errors` (feature `sea-orm`) has `From<sea_orm::DbErr> for CanonicalError`, which maps to 500 Internal.

Credstore's ladder (real): unique becomes `Conflict`. Retryable contention becomes 503 `ServiceUnavailable`. CHECK and anything else become `Internal`.
Its `map_scope_err` maps `TenantNotInScope` to `AccessDenied` and `Invalid`/`Denied`/other to `Internal`.
Keep a wildcard arm because `ScopeError` is `#[non_exhaustive]`.

On Postgres, a constraint violation aborts the current transaction. Re-read the winner **outside** the failed tx, as types-registry does.

### 1.9 SQLite vs Postgres differences

- **Backend detection**: `db.backend()` / `db.db_engine()`. In migrations use `manager.get_database_backend()`. Repos rarely branch.
- **UUID**: sqlx encodes `Uuid` as **16-byte BLOB** on SQLite and native `uuid` on PG. SQLite `uuid_text`/`TEXT` columns still receive blobs.
  Declare `BLOB` in SQLite DDL. Types-registry adds `CHECK (length(x) = 16)`. Literal defaults use `x'<32 hex>'`.
  Sort order is byte-wise on both backends.
- **TIMESTAMPTZ (`time::OffsetDateTime`)**: on SQLite it is **TEXT** in RFC 3339 via `time` (`sqlx-sqlite/src/types/time.rs`).
  - UTC renders as `...Z`. Subseconds are **trimmed** (`.5Z`, `.123Z`, or none when zero). A non-UTC offset keeps `+hh:mm`.
  - Lexicographic order is therefore wrong across different fractional lengths (`...00.1Z` > `...00.15Z`). Normalize before writing: `.to_offset(UtcOffset::UTC)` plus a fixed precision, e.g. `replace_nanosecond(0)`.
  - PG `TIMESTAMPTZ` stores microseconds, so a value with nanoseconds does not round-trip exactly. Truncating to whole seconds or to micros on write keeps both backends identical.
  - `DEFAULT CURRENT_TIMESTAMP` on SQLite yields `YYYY-MM-DD HH:MM:SS`, a different text format. Always set timestamps from Rust.
- **DATE (`time::Date`)**: SQLite TEXT `YYYY-MM-DD`, PG `DATE`.
- **chrono vs time**: OData filters bind `chrono::DateTime<Utc>` (`ODataValue::DateTime` becomes `Value::ChronoDateTimeUtc`). On SQLite that renders `...+00:00` (not `Z`).
  So same-second comparisons against `time`-written values are unreliable on SQLite. Cross-second ranges are fine.
- **Locking**: `FOR UPDATE` renders on PG. The SQLite builder renders nothing (sea-query `prepare_select_lock` is a no-op). See §5.
- **Collation**: SQLite TEXT is `BINARY`. PG uses the DB collation. Ordering by `title` differs unless you set `COLLATE "C"` on PG.
- **Bools/JSON**: SQLite has no bool type (use INTEGER + `CHECK (x IN (0,1))`) and stores JSON as TEXT. PG uses BOOLEAN/JSONB.
- **Outbox on SQLite** works within a single process only.

---

## 2. Migrations

### 2.1 How a gear provides migrations

`toolkit::contracts::DatabaseCapability` (`libs/toolkit/src/contracts.rs`):

```rust
pub trait DatabaseCapability: Send + Sync {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>>;
}
```

Declare `capabilities = [db, ...]` in `#[toolkit::gear(..)]` and implement the trait.
Real (`gears/credstore/credstore/src/infra/storage/migrations.rs` + `gear.rs`):

```rust
use sea_orm_migration::prelude::*;
pub mod m0001_initial_schema;
pub struct Migrator;
#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> { vec![Box::new(m0001_initial_schema::Migration)] }
}

impl DatabaseCapability for CredStoreGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        crate::infra::storage::migrations::Migrator::migrations()
    }
}
```

Runner semantics (`libs/toolkit-db/src/migration_runner.rs`):
- Each gear gets its own history table `toolkit_migrations__<prefix>__<hash8>`.
- Migrations are **sorted by `name()`** before applying, not by vec order. Use sortable names such as `m20261004_000001_initial` or `m0001_...`.
- Each `up()` plus its history insert runs in one transaction on a best-effort basis. PG DDL is transactional, so do not use `CREATE INDEX CONCURRENTLY`.
- Duplicate names are an error.
- They run in the runtime's DB-migration phase, before `RestApiCapability` registration.
- For tests: `toolkit_db::migration_runner::run_migrations_for_testing(&db, migrations).await -> Result<MigrationResult, MigrationError>`.

### 2.2 Writing a dual-backend migration (real pattern, credstore `m0001_initial_schema.rs`)

Per-backend raw SQL preserves CHECKs and partial indexes verbatim. MySQL is rejected.

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
        let generic_uuid_hex = GENERIC_TYPE_UUID_STR.replace('-', "");
        let table_sql = match backend {
            sea_orm::DatabaseBackend::Postgres => format!(r"
CREATE TABLE IF NOT EXISTS credstore_secrets (
    id UUID PRIMARY KEY,
    tenant_id UUID NOT NULL,
    reference TEXT NOT NULL CHECK (length(reference) BETWEEN 1 AND 255),
    sharing SMALLINT NOT NULL CHECK (sharing IN (1, 2, 3)),
    owner_id UUID NOT NULL,
    status SMALLINT NOT NULL CHECK (status IN (1, 2, 3)),
    created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1,
    secret_type_uuid UUID NOT NULL DEFAULT '{GENERIC_TYPE_UUID_STR}',
    expires_at TIMESTAMPTZ NULL,
    value_fp BYTEA NULL,
    fp_key_id SMALLINT NULL,
    CHECK ((value_fp IS NULL) = (fp_key_id IS NULL))
);"),
            sea_orm::DatabaseBackend::Sqlite => format!(r"
CREATE TABLE IF NOT EXISTS credstore_secrets (
    id BLOB PRIMARY KEY NOT NULL,
    tenant_id BLOB NOT NULL,
    reference TEXT NOT NULL CHECK (length(reference) BETWEEN 1 AND 255),
    sharing SMALLINT NOT NULL CHECK (sharing IN (1, 2, 3)),
    owner_id BLOB NOT NULL,
    status SMALLINT NOT NULL CHECK (status IN (1, 2, 3)),
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    version BIGINT NOT NULL DEFAULT 1,
    secret_type_uuid BLOB NOT NULL DEFAULT (x'{generic_uuid_hex}'),
    expires_at TEXT NULL,
    value_fp BLOB NULL,
    fp_key_id SMALLINT NULL,
    CHECK ((value_fp IS NULL) = (fp_key_id IS NULL))
);"),
            _ => return Err(DbErr::Custom(MYSQL_NOT_SUPPORTED.to_owned())),
        };
        let statements = [
            table_sql.as_str(),
            // partial UNIQUE indexes: identical syntax on PG and SQLite
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_credstore_nonprivate ON credstore_secrets (tenant_id, reference) WHERE sharing <> 1;",
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_credstore_private ON credstore_secrets (tenant_id, reference, owner_id) WHERE sharing = 1;",
            "CREATE INDEX IF NOT EXISTS idx_credstore_lookup ON credstore_secrets (reference, tenant_id, status);",
            "CREATE INDEX IF NOT EXISTS idx_credstore_pending ON credstore_secrets (updated_at) WHERE status <> 2;",
            "CREATE INDEX IF NOT EXISTS idx_credstore_expiry ON credstore_secrets (expires_at) WHERE expires_at IS NOT NULL AND status = 2;",
        ];
        for sql in statements { conn.execute_unprepared(sql).await?; }   // one statement per call
        Ok(())
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared("DROP TABLE IF EXISTS credstore_secrets;").await?;
        Ok(())
    }
}
```

The type lowering table used by types-registry (`.../migrations/m20260817_000001_initial.rs`, header doc) is the house standard:

| Postgres | SQLite | Notes |
|---|---|---|
| `uuid` | `BLOB` + `CHECK (length(col) = 16)` | sqlx writes 16 raw bytes. A `CHAR(36)` would silently hold a blob. |
| `bigint GENERATED BY DEFAULT AS IDENTITY` | `INTEGER PRIMARY KEY AUTOINCREMENT` | |
| `varchar(n) COLLATE "C"` | `TEXT COLLATE BINARY` | Use for exact/prefix-ordered identifiers. |
| `bytea` | `BLOB` | |
| `boolean` | `INTEGER` + `CHECK (col IN (0, 1))` | |
| `timestamptz` | `TEXT` | See the §1.9 format caveats. |
| `date` | `TEXT` | |
| `smallint` + `CHECK (x IN (..))` | same | Enums are SMALLINT with an enumerated CHECK on every backend. Numbering is append-only. |

FKs: write them inline at CREATE time, e.g. `CONSTRAINT fk_x FOREIGN KEY (chat_id) REFERENCES chats (id) ON DELETE CASCADE`, or composite `(id, chat_id)`. Both backends accept the same syntax.
SQLite cannot `ALTER TABLE ADD CONSTRAINT` (no FK/CHECK added later without a table rebuild), so put every constraint in the initial CREATE.
SQLite enforces FKs because sqlx sets `foreign_keys=ON`. A composite FK target needs a UNIQUE/PK on exactly those columns, e.g. `UNIQUE (id, chat_id)`.
The schema builder (`manager.create_table(Table::create()..col(ColumnDef::new(..).uuid()))`) also works.
On SQLite it emits `uuid_text`/`timestamp_with_timezone_text` declared types and cannot express partial indexes. Raw SQL per backend is the norm.

---

## 3. Transactional outbox (`libs/toolkit-db/src/outbox`)

Public items, all `toolkit_db::outbox::*`:
`Outbox`, `OutboxBuilder`, `OutboxHandle`, `QueueBuilder`, `LeasedQueueBuilder`, `Partitions`, `LeaseConfig`, `WorkerTuning`, `OutboxProfile`,
`Record`, `Records`, `Wake`, `in_transaction`, `Batch`, `OutboxMessage`, `HandlerResult`, `MessageResult`, `LeasedHandler`, `LeasedMessageHandler`,
`TransactionalHandler`, `TransactionalMessageHandler`, `PerMessageAdapter`, `DeadLetterScope`, `DeadLetterFilter`, `DeadLetterMessage`, `DeadLetterStatus`,
`OutboxError`, `OutboxMessageId`, `outbox_migrations`, `outbox_migrations_with_prefix`, and trace types (`TraceSubscription`, `TraceOutcome`, ...).

The pipeline runs incoming, then sequencer, then outgoing, then processor, with vacuum GC in the background.
- PG uses `FOR UPDATE SKIP LOCKED`.
- SQLite runs single-process (single-writer model).

### 3.1 Migrations (tables)

```rust
pub fn outbox_migrations() -> Vec<Box<dyn MigrationTrait>>;   // prefix "toolkit_outbox", name "m001_create_toolkit_outbox_schema"
pub fn outbox_migrations_with_prefix(prefix: impl Into<String>) -> Result<Vec<Box<dyn MigrationTrait>>, OutboxError>;
//   name "m001_create_toolkit_outbox_schema__<prefix>"; prefix: 1-36 bytes, starts with ASCII letter, [A-Za-z0-9_]
//   (else OutboxError::InvalidTablePrefix); derived names must fit 63 bytes.
```

Append them to the gear's list. Real (`gears/system/types-registry/.../gear.rs`):

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
// infra/outbox.rs: pub const TABLE_PREFIX: &str = "types_registry__outbox";
```

The runner sorts by name, and `m001_create_toolkit_outbox_schema__...` sorts before `m2026...`, so the outbox schema is created first. It has no dependency on gear tables.
Use the **same prefix** in `Outbox::builder(db).table_prefix(prefix)?`. For mini-chat: `"mini_chat__outbox"` (17 bytes).

Tables are `<prefix>_<suffix>`. Columns shown are the SQLite DDL from `migrations.rs`. PG uses BIGINT identity / BYTEA / TIMESTAMPTZ / VARCHAR equivalents.

| Table | Columns |
|---|---|
| `_body` | `id INTEGER PK AUTOINCREMENT, payload BLOB NOT NULL, payload_type TEXT NOT NULL, created_at TEXT DEFAULT datetime('now'), trace TEXT NULL` (PG: `payload BYTEA`, `payload_type VARCHAR(1024)`, `trace VARCHAR(256)`) |
| `_partitions` | `id, queue TEXT, partition INTEGER, sequence INTEGER DEFAULT 0, UNIQUE(queue, partition)` |
| `_incoming` | `id, partition_id -> partitions(id), body_id -> body(id)` |
| `_outgoing` | `id, partition_id, body_id, seq, sequenced_at` |
| `_processor` | `partition_id PK -> partitions, processed_seq DEFAULT 0, attempts DEFAULT 0, last_error, locked_by, locked_until` (one row per partition: cursor plus lease) |
| `_dead_letters` | `id, partition_id, seq, payload BLOB, payload_type, created_at, failed_at, last_error, attempts, status TEXT DEFAULT 'pending', completed_at, deadline, trace` |
| `_vacuum_counter` | `partition_id PK, counter` |
| `_trace` | `id, trace, owner_instance, queue, entities, pending, failures, attempts, last_error, retrying_since, created_at, completed_at, notified_at` |
| `_body_id_sequence`, `_incoming_id_sequence` | MySQL only |

### 3.2 Records and enqueue

The payload is opaque `Vec<u8>` (JSON is your choice, e.g. `serde_json::to_vec`).
`payload_type` is 1-1024 printable ASCII (0x20..=0x7E). The queue name must match `[A-Za-z0-9._-]{1,1024}` and start and end alphanumeric.
**Max payload is `64 KiB`** (`validation::MAX_PAYLOAD_SIZE = 64 * KIB_LEN`). Exceeding it fails at `.build()` with `OutboxError::PayloadTooLarge { size, max }`, before any SQL runs.
The optional trace is 1-256 printable ASCII; longer gives `TraceTooLong`.

```rust
// single (record.rs)
let record = Record::to(QUEUE, partition /* u32, < partition count */)
    .payload(bytes /* Vec<u8> */, PAYLOAD_TYPE /* &str */)
    // .trace("unique-per-batch")   optional
    .build()?;                                   // Result<Record<'_>, OutboxError>
let wake: Wake = outbox.enqueue(tx, record).await?;   // db: &(impl DBRunner + Sync + ?Sized): DbTx or DbConn

// batch (all-or-nothing validation; one queue)
let batch = Records::to(QUEUE).payload_type("mini_chat.x.v1")
    .push(0, bytes_a).push_with_type(1, bytes_b, "other.v1").build()?;
let wake = outbox.enqueue_batch(tx, batch).await?;
```

Enqueue errors at runtime: `QueueNotRegistered(String)`, `PartitionOutOfRange { queue, partition, max }`, `Database(DbErr)`.

The partition key is a caller-chosen `u32` in `[0, partitions)`. Route by a stable key so one entity's messages stay ordered.
Real routing (types-registry `infra/outbox.rs`):

```rust
const PARTITIONS: u16 = 8;
fn partition(operation_id: Uuid) -> u32 {
    let bytes = operation_id.as_bytes();
    u32::from(u16::from_be_bytes([bytes[14], bytes[15]]) % PARTITIONS)
}
```

**`Wake` contract** (`wake.rs`). Enqueue does not wake the sequencer.
- Call `wake.fire()` only **after commit**. On rollback, call `wake.discard()` or just drop it.
- Dropping an un-fired, non-empty wake logs a warning, and delivery then waits for the cold reconciler (`reconciler` idle ~1 min, 30 s in `low_latency`).
- Combine several wakes with `+` / `+=`, starting from `Wake::empty()`.
- Doc comments elsewhere mention `Wake::flush`. The real method is `fire`.

Helper that owns the contract:

```rust
pub async fn in_transaction<F, T, E>(db: &Db, f: F) -> Result<T, E>
where E: From<DbError> + Send + 'static, T: Send + 'static,
      F: for<'a> FnOnce(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<(T, Wake), E>> + Send + 'a>> + Send;
// usage: toolkit_db::outbox::in_transaction(&provider.db(), |tx| Box::pin(async move {
//     repo::insert_x(tx, &scope, ..).await?;                 // business writes first
//     let wake = outbox.enqueue(tx, record).await?;          // enqueue LAST
//     Ok((value, wake))
// })).await?;
```

Real enqueue port (types-registry):

```rust
async fn enqueue(&self, tx: &DbTx<'_>, operation_id: Uuid) -> Result<Wake, OutboxError> {
    let outbox = self.outbox.get().and_then(Weak::upgrade).ok_or(OutboxError::NotRunning)?;   // gear-local error
    let record = Record::to(QUEUE, partition(operation_id))
        .payload(payload(operation_id), PAYLOAD_TYPE)
        .build()?;
    Ok(outbox.enqueue(tx, record).await?)
}
```

`OutboxDispatch` holds a `OnceLock<Weak<Outbox>>` that is bound after `start()`. This avoids an ownership cycle between the service and the handler.

### 3.3 Builder, queues, handlers, start/stop

```rust
let handle: OutboxHandle = Outbox::builder(db /* toolkit_db::Db, e.g. provider.db() */)
    .table_prefix(TABLE_PREFIX)?                         // Result<Self, OutboxError>
    .profile(OutboxProfile::low_latency())               // default_profile() | low_latency() | high_throughput() | relaxed()
    .processor_tuning(WorkerTuning::processor_low_latency().batch_size(1))   // optional overrides
    .queue(QUEUE, Partitions::of(8))                     // const fn; panics unless power of two in 1..=64
        .leased(handler)                                 // -> LeasedQueueBuilder
        .lease(LeaseConfig { duration: Duration::from_secs(60), headroom: Duration::from_secs(2) })  // panics if headroom >= duration
    // .queue("other", Partitions::of(4)).transactional(tx_handler)   // -> OutboxBuilder
    .start()
    .await?;                                             // registers queues (DB), spawns workers
let outbox: &Arc<Outbox> = handle.outbox();
// shutdown (e.g. in lifecycle serve after cancel):
handle.stop().await;                                     // cancels workers and joins; Drop also cancels
```

Other builder knobs:
- `.processors(n)`: default 4.
- `.maintenance(guaranteed, shared)`: default 2/1. Budget `max_connections >= processors + guaranteed + shared + producer headroom`. Mini-chat's SQLite pool is `max_conns: 5`, so lower these.
- `.sequencer_tuning`, `.vacuum_tuning`, `.reconciler_tuning`, `.stats_interval`, `.partition_batch_limit`, `.max_inner_iterations`.

The partition count is persisted per queue. A different count on restart gives `OutboxError::PartitionCountMismatch` (registration is idempotent when it matches).
`LeaseConfig::default()` is 30 s duration with 2 s headroom. The handler budget is `duration - headroom`, after which its future is dropped.

Handler traits (`handler.rs`):

```rust
pub struct OutboxMessage {
    pub partition_id: i64, pub seq: i64, pub payload: Vec<u8>, pub payload_type: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub attempts: i16,   // retries already taken: 0 on first attempt
}
pub enum HandlerResult { Success, Retry { reason: String }, Reject { reason: String } }
pub enum MessageResult { Ok, Retry, Reject(String) }

#[async_trait::async_trait] pub trait LeasedHandler: Send + Sync {
    async fn handle(&self, batch: &mut Batch<'_>) -> HandlerResult; }
#[async_trait::async_trait] pub trait LeasedMessageHandler: Send + Sync {          // blanket impl -> LeasedHandler
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult; }
#[async_trait::async_trait] pub trait TransactionalMessageHandler: Send + Sync {
    async fn handle(&self, txn: &sea_orm::DatabaseExecutor<'_>, msg: &OutboxMessage) -> HandlerResult; }
#[async_trait::async_trait] pub trait TransactionalHandler: Send + Sync {          // batch_transactional(..)
    async fn handle(&self, txn: &DatabaseExecutor<'_>, msgs: &[OutboxMessage]) -> HandlerResult; }
// Batch: next_msg(), next_chunk(n), ack(), ack_chunk(n), reject(reason), remaining() -> Duration,
//        should_stop() (lease spent or shutdown), len(), is_empty(), processed()
```

Delivery modes:
- **Leased**: at-least-once. The handler runs outside any tx, so it must be idempotent.
- **Transactional**: the handler runs inside the tx that holds the partition lock, and its writes commit atomically with the ack.
  It gets a raw `DatabaseExecutor`, not a `DbTx`, so the Secure ORM wrappers are not usable there.

Outcomes:
- `Success` advances the cursor.
- `Retry` does not advance the cursor. The same batch is redelivered with exponential backoff (`WorkerTuning.retry_base`..`retry_max`) and `attempts` is incremented.
- `Reject` (batch) dead-letters the whole batch and advances.
- With per-message `MessageResult::Reject(reason)`, only that message is dead-lettered and processing continues. `MessageResult::Retry` stops and retries the rest.

There is **no built-in max attempts**. The handler decides from `msg.attempts` when to return Reject.
The attempt counter is **per partition, not per message**. It is reset when the batch completes.
Types-registry pins `batch_size(1)` so that `attempts` acts as a per-message budget:
"Raising the batch size ... would silently turn that budget into a counter shared by every message in the batch".

Real leased handler (types-registry, abridged):

```rust
#[async_trait::async_trait]
impl LeasedHandler for AdmissionHandler {
    async fn handle(&self, batch: &mut Batch<'_>) -> HandlerResult {
        loop {
            let lease = batch.remaining();
            let Some(msg) = batch.next_msg() else { break };
            match self.handle_message(msg, lease).await {
                MessageResult::Ok => batch.ack(),
                MessageResult::Retry => return HandlerResult::Retry { reason: "types_registry admission asked for redelivery".to_owned() },
                MessageResult::Reject(reason) => batch.reject(reason),
            }
            if batch.should_stop() { break; }
        }
        HandlerResult::Success
    }
}
```

### 3.4 Dead letters

Methods on `Outbox`. `db` is `&(impl DBRunner + Sync)`.

```rust
outbox.dead_letter_list(&conn, &DeadLetterFilter::from_scope(DeadLetterScope::default().queue(QUEUE).limit(50))).await?; // Vec<DeadLetterMessage>
outbox.dead_letter_count(&conn, &filter).await?;                    // u64; filter defaults status = Pending; .status(..) / .any_status()
outbox.dead_letter_replay(&conn, &scope, Duration::from_secs(60)).await?;  // claim -> Reprocessing w/ deadline, returns msgs
outbox.dead_letter_resolve(&conn, &[id]).await?;                    // -> Resolved
outbox.dead_letter_reject(&conn, &[id], "reason").await?;           // -> back to Pending, attempts++
outbox.dead_letter_discard(&conn, &scope).await?;                   // Pending -> Discarded
outbox.dead_letter_cleanup(&conn, &scope).await?;                   // delete Resolved + Discarded
// DeadLetterScope { partition_id, queue, payload_type, limit } builders: .partition(i64) .queue(..) .payload_type(..) .limit(u32)
// DeadLetterMessage { id, partition_id, seq, payload, payload_type, created_at, failed_at, last_error, attempts, status, completed_at, deadline }
// DeadLetterStatus::{Pending, Reprocessing, Resolved, Discarded}
```

Dead letters are never auto-replayed. The application owns that policy.

### 3.5 `OutboxError`

`#[non_exhaustive]`:
- `QueueNotRegistered(String)`
- `PartitionOutOfRange { queue, partition: u32, max: u32 }`
- `PayloadTooLarge { size: usize, max: usize }`
- `PartitionCountMismatch { queue, expected: u16, found: usize }`
- `EmptyTracedBatch`
- `TracedBatchTooLarge { entities }`
- `InvalidQueueName { reason }`
- `InvalidPayloadType { reason }`
- `InvalidTrace { reason }`
- `TraceTooLong { size, max }`
- `InvalidTablePrefix { reason }`
- `Database(#[from] sea_orm::DbErr)`

---

## 4. OData and cursor pagination

### 4.1 The axum extractor: `toolkit::api::odata::OData`

```rust
pub struct OData(pub ODataQuery);       // FromRequestParts, Rejection = CanonicalError; Deref<Target = ODataQuery>
pub async fn extract_odata_query<S>(parts: &mut Parts, state: &S) -> Result<ODataQuery, CanonicalError>;
pub struct ODataParams {               // wire binding
    #[serde(rename = "$filter")] filter, #[serde(rename = "$orderby")] orderby, #[serde(rename = "$select")] select,
    #[serde(alias = "$top")] limit: Option<u64>,  #[serde(alias = "$skiptoken")] cursor: Option<String>,
}
pub const ACCEPTED_SYSTEM_QUERY_OPTIONS = ["$filter", "$orderby", "$select", "$top", "$skiptoken"];
pub const MAX_FILTER_LEN = 8*1024; MAX_NODES = 2000; MAX_ORDERBY_LEN = 1024; MAX_ORDER_FIELDS = 10;
pub const MAX_SELECT_LEN = 2048; MAX_SELECT_FIELDS = 100;
```

Extractor behaviour, in order (`libs/toolkit/src/api/odata.rs`):

1. Any `$`-key outside the accepted list fails with 400 `UNSUPPORTED_QUERY_PARAM`, one violation per key with the key as `field` (e.g. `$skip`, `$count`, `$Top`).
   Non-`$` keys are ignored by the extractor. Policing them is the gear's job (types-registry has a guard).
2. A query string that does not deserialize gives `INVALID_QUERY_PARAMS` (field `query`). Sending both `limit` and `$top` also lands here, as a duplicate field.
3. `$filter`: if trimmed length > 8 KiB, `FILTER_TOO_LONG`. A parse error gives `INVALID_FILTER`. More than 2000 nodes gives `FILTER_TOO_COMPLEX`. On success it sets `query.filter` and `query.filter_hash = short_filter_hash(..)` (FNV-1a 64, 16 hex chars).
4. `cursor` together with `$orderby` gives `ORDER_WITH_CURSOR` (two violations: `$orderby` and `cursor`).
5. `cursor`: `CursorV1::decode` failure gives `INVALID_CURSOR`. When a cursor is present, `query.order` is left empty because the order comes from the cursor.
6. `$orderby`: `"f1 [asc|desc], f2 ..."`. A bad clause, more than 10 fields, or too long gives `INVALID_ORDERBY_FIELD`.
   Unknown field names are **not** checked here. Pagination checks them.
7. `limit == 0` gives `INVALID_LIMIT` (field `$top`). Other values pass through unclamped.
8. `$select`: empty, too long, too many fields, or duplicates give `INVALID_SELECT`. Fields are lowercased into `query.select`.

```rust
pub struct ODataQuery {           // toolkit_odata (also re-exported as toolkit::api::odata::ODataQuery)
    pub filter: Option<Box<ast::Expr>>, pub order: ODataOrderBy, pub limit: Option<u64>,
    pub cursor: Option<CursorV1>, pub filter_hash: Option<String>, pub select: Option<Vec<String>>,
}  // builders: with_filter/with_order/with_limit/with_cursor/with_filter_hash/with_select; has_filter(), selected_fields()
pub struct ODataOrderBy(pub Vec<OrderKey>);  pub struct OrderKey { pub field: String, pub dir: SortDir }
pub enum SortDir { Asc, Desc }               // reverse(); ODataOrderBy::{empty, is_empty, ensure_tiebreaker, to_signed_tokens}
```

### 4.2 Error type and canonical mapping

`toolkit_odata::Error` (`libs/toolkit-odata/src/lib.rs`):
- Filter/order/limit: `InvalidFilter(String)`, `InvalidOrderByField(String)`, `InvalidLimit`
- Cursor/query mismatch: `OrderMismatch`, `FilterMismatch`, `InvalidCursor`, `OrderWithCursor`
- Cursor decode details: `CursorInvalidBase64`, `CursorInvalidJson`, `CursorInvalidVersion`, `CursorInvalidKeys`, `CursorInvalidFields`, `CursorInvalidDirection`
- Internal: `Db(String)`, `ParsingUnavailable(&'static str)`

`impl From<toolkit_odata::Error> for CanonicalError` (`libs/toolkit-odata/src/problem_mapping.rs`). Every arm except the last two maps to
`OdataError::invalid_argument()` (400). `OdataError` is `#[resource_error(gts_id!("cf.core.odata.query.v1~"))]`, so
`context.resource_type = "gts.cf.core.odata.query.v1~"`.

| Error | `field_violations[].field` | `reason` |
|---|---|---|
| `InvalidFilter` | `$filter` | `INVALID_FILTER` |
| `InvalidOrderByField` | `$orderby` | `INVALID_ORDERBY_FIELD` |
| `InvalidCursor`, `CursorInvalid*` | `cursor` | `INVALID_CURSOR` |
| `OrderMismatch` | `cursor` | `ORDER_MISMATCH` |
| `FilterMismatch` | `cursor` | `FILTER_MISMATCH` |
| `InvalidLimit` | `$top` | `INVALID_LIMIT` |
| `OrderWithCursor` | `$orderby` + `cursor` | `ORDER_WITH_CURSOR` |
| `Db(msg)`, `ParsingUnavailable` | | `CanonicalError::internal(..)` (500) |

Extractor-only reasons use the same resource type: `FILTER_TOO_LONG`, `FILTER_TOO_COMPLEX`, `INVALID_SELECT`, `UNSUPPORTED_QUERY_PARAM`, `INVALID_QUERY_PARAMS`.
A handler can just `?` a `toolkit_odata::Error` into `CanonicalError`.

### 4.3 Page types

From `libs/toolkit-odata/src/page.rs`:

```rust
#[cfg_attr(feature = "with-utoipa", derive(utoipa::ToSchema))]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PageInfo { pub next_cursor: Option<String>, pub prev_cursor: Option<String>, pub limit: u64 }
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page<T> { pub items: Vec<T>, pub page_info: PageInfo }   // manual ToSchema: name "Page_<T>"
impl<T> Page<T> { fn new(items, page_info); fn empty(limit); fn map_items<U>(self, f) -> Page<U> }
```

`None` cursors serialize as JSON `null`. There is no `skip_serializing_if`.

### 4.4 Cursor encoding

`CursorV1 { k: Vec<String>, o: SortDir, s: String, f: Option<String>, d: String }`:
- `k` holds the key values of the boundary row.
- `s` is the signed order tokens, e.g. `"-updated_at,-id"`.
- `f` is the filter hash.
- `d` is `"fwd"` or `"bwd"`.

On the wire it is base64url (no padding) of JSON `{"v":1,"k":[..],"o":"asc|desc","s":"..","f":"..","d":"fwd"}`.
It is **not signed**. `ODataLimits::with_signed_cursors` exists, but nothing applies it.

Cursor values are encoded per `FieldKind` (`toolkit_db::odata::encode_cursor_value`):

| FieldKind | sea_orm::Value accepted on encode |
|---|---|
| `String` | `String(Some)` |
| `I64` | `BigInt`, `Int`, `SmallInt` |
| `Uuid` | `Uuid(Some)` |
| `DateTimeUtc` | `ChronoDateTimeUtc` **or** `TimeDateTimeWithTimeZone` (formatted `...:SS.nnnnnnnnnZ`, 9 digits) |
| `Date` | `ChronoDate` only (**not** `TimeDate`) |
| `Bool` | `Bool` |
| `F64` | `Double` |
| `Decimal` | `Decimal` |

**A `None` (NULL) value in an ordered column fails** with `InvalidCursor`, which the client sees as a 400.
Do not make nullable columns orderable (e.g. a nullable chat `title`) unless every row has a value.
On decode, `DateTimeUtc` parses as `time::OffsetDateTime` first.

### 4.5 Declaring filterable/orderable fields

Derive (`toolkit_odata_macros::ODataFilterable`). It generates `pub enum <Struct>FilterField` (variants in UpperCamel) implementing `toolkit_odata::filter::FilterField`.
Each field needs `#[odata(filter(kind = "<FieldKind variant>"))]`. Fields without the attribute are excluded, and at least one field must have it.
The derive cannot express `nullable`. Hand-write the impl for that.

```rust
#[derive(toolkit_odata_macros::ODataFilterable)]
pub struct ChatQuery {                              // (composed)
    #[odata(filter(kind = "Uuid"))]        pub id: uuid::Uuid,
    #[odata(filter(kind = "String"))]      pub title: String,
    #[odata(filter(kind = "DateTimeUtc"))] pub updated_at: time::OffsetDateTime,
}   // => pub enum ChatQueryFilterField { Id, Title, UpdatedAt }
```

Hand-written equivalent (real, `libs/toolkit-db/tests/odata_null_like.rs`; `name()` there comes from `strum::IntoStaticStr`):

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Field { Id, Name, Parent }
impl FilterField for Field {                       // toolkit_odata::filter::{FilterField, FieldKind}
    const FIELDS: &'static [Self] = &[Self::Id, Self::Name, Self::Parent];
    fn name(&self) -> &'static str { match self { Self::Id => "id", Self::Name => "name", Self::Parent => "parent" } }
    fn kind(&self) -> FieldKind { match self { Self::Id => FieldKind::I64, Self::Name => FieldKind::String, Self::Parent => FieldKind::Uuid } }
    fn nullable(&self) -> bool { matches!(self, Self::Parent) }   // allows `parent eq null` / `ne null`
}
```

`from_name` matches case-insensitively. Operators allowed per kind (`FieldKind::allows`):

| Kind | Operators |
|---|---|
| String | `eq ne in contains startswith endswith` |
| Uuid | `eq ne in` |
| Bool | `eq ne` |
| Numeric / DateTimeUtc / Date / Time | `eq ne gt ge lt le in` |

The parser rejects any other operator.

Mapping to columns (`toolkit_db::odata::{FieldToColumn, ODataFieldMapping}`, `libs/toolkit-db/src/odata/sea_orm_filter.rs`):

```rust
struct Mapper;
impl FieldToColumn<Field> for Mapper {
    type Column = ent::Column;
    fn map_field(field: Field) -> ent::Column { match field { Field::Id => ent::Column::Id, Field::Name => ent::Column::Name, Field::Parent => ent::Column::Parent } }
    // optional: fn map_value(field, op: FilterOp, v: &ODataValue) -> Result<ODataValue, String>  (wire->storage, e.g. "user" -> 1)
    // optional: fn is_orderable(field) -> bool   (false => $orderby on it = INVALID_ORDERBY_FIELD)
}
impl ODataFieldMapping<Field> for Mapper {
    type Entity = ent::Entity;
    fn extract_cursor_value(model: &ent::Model, field: Field) -> sea_orm::Value {
        match field {
            Field::Id => sea_orm::Value::BigInt(Some(model.id)),
            Field::Name => sea_orm::Value::String(Some(model.name.clone())),
            Field::Parent => sea_orm::Value::Uuid(model.parent),
        }
    }
    // optional: fn cursor_kind(field) -> FieldKind  (e.g. I64 for a SMALLINT enum exposed as String)
}
```

For a `time::OffsetDateTime` column, return `sea_orm::Value::TimeDateTimeWithTimeZone(Some(m.updated_at))`. For a UUID column, return `sea_orm::Value::Uuid(Some(m.id))`.

There is also a legacy string-keyed API: `toolkit_db::odata::FieldMap::<E>::new().insert("name", Column::Name, FieldKind::String)` with `OPager::<E,_>::new(&scope, &conn, &fmap).tiebreaker(..).limits(d, m).fetch(&q, map)`.
Its defaults are tiebreaker `("id", Desc)` and limits 25/1000. Prefer the typed API for new code.

OpenAPI documentation: `use toolkit::api::operation_builder::OperationBuilderODataExt;`.
Call `.with_odata_filter::<ChatQueryFilterField>()`, `.with_odata_orderby::<..>()`, and `.with_odata_select()` on the `OperationBuilder`. They add params plus `x-odata-filter`/`x-odata-orderby`.

### 4.6 Pagination helper over a scoped select

```rust
pub struct LimitCfg { pub default: u64, pub max: u64 }       // toolkit_db::odata::LimitCfg
pub async fn paginate_odata<F, M, E, D, Mapper, C>(
    select: SecureSelect<E, Scoped>,           // must already be .secure().scope_with(..): scope cannot be skipped
    conn: &C,                                   // C: DBRunner (DbConn or DbTx)
    query: &toolkit_odata::ODataQuery,
    tiebreaker: (&str, SortDir),               // field NAME as in FilterField::name()
    limit_cfg: LimitCfg,
    model_to_domain: Mapper,                   // Fn(E::Model) -> D
) -> Result<Page<D>, toolkit_odata::Error>
where F: FilterField, M: ODataFieldMapping<F, Entity = E>, E: EntityTrait, Mapper: Fn(E::Model) -> D, C: DBRunner;
pub async fn paginate_odata_try<..>(..., Mapper: Fn(E::Model) -> Result<D, MapErr>)
    -> Result<Page<D>, PaginateOdataTryError<MapErr>>;   // enum { OData(toolkit_odata::Error), MapError(MapErr) }
```

Semantics (`paginate_odata_collect`):

- `limit = clamp(query.limit.unwrap_or(default))`. 0 becomes 1 (the extractor already rejected 0) and anything above `max` becomes `max` **silently**.
  `PageInfo.limit` is the clamped value. It fetches `limit + 1` rows to detect a next page and issues no COUNT.
- Effective order:
  - With a cursor, it is `from_signed_tokens(cursor.s)`; a decode failure gives `InvalidCursor`.
  - Otherwise it is `query.order.ensure_tiebreaker(tiebreaker)`. **If `query.order` is empty, the effective order is the tiebreaker alone.**
- Every order key must resolve via `F::from_name` and pass `M::is_orderable`, or the result is `InvalidOrderByField(field)`.
- A cursor with `f` must match `query.filter_hash`, or the result is `FilterMismatch`. ORDER_MISMATCH only arises through `toolkit_odata::validate_cursor_against`, which this helper does not call.
- The filter goes through `convert_expr_to_filter_node::<F>`; an unknown field or bad type gives `InvalidFilter`. It is ANDed onto the scoped select.
  The cursor predicate is a lexicographic OR-of-ANDs that respects each key's direction. `bwd` reverses it.
- DB errors become `Error::Db(String)` (500).
- `next_cursor` is set when more rows exist (or on a backward page). `prev_cursor` is set when a cursor was supplied.

### 4.7 End-to-end list endpoint (composed)

No gear uses `paginate_odata` end-to-end yet. Types-registry hand-rolls keyset paging in `gears/system/types-registry/.../api/rest/{params,cursor}.rs` on top of `extract_odata_query`.
The closest real usage is `libs/toolkit-db/tests/odata_null_like.rs` (`paginate_odata`) and `tests/secure_odata_sqlite.rs` (`OPager`).
The sketch below covers mini-chat `GET /v1/chats` (default `updated_at desc`, `id` tiebreaker, limit 20/100).

```rust
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use toolkit::api::odata::OData;
use toolkit_canonical_errors::CanonicalError;
use toolkit_db::odata::{paginate_odata, FieldToColumn, LimitCfg, ODataFieldMapping};
use toolkit_db::secure::{DBRunner, SecureEntityExt};
use toolkit_odata::{ODataOrderBy, ODataQuery, OrderKey, Page, SortDir};

pub const CHAT_LIMITS: LimitCfg = LimitCfg { default: 20, max: 100 };

pub async fn list_chats(conn: &impl DBRunner, scope: &AccessScope, mut q: ODataQuery)
    -> Result<Page<ChatRow>, toolkit_odata::Error>
{
    // Default primary order only when the client sent neither $orderby nor cursor.
    // (With a cursor the order comes from cursor.s; the extractor forbids both together.)
    if q.cursor.is_none() && q.order.is_empty() {
        q.order = ODataOrderBy(vec![OrderKey { field: "updated_at".into(), dir: SortDir::Desc }]);
    }
    let select = chat::Entity::find()
        .filter(chat::Column::DeletedAt.is_null())        // business filter before .secure()
        .secure()
        .scope_with(scope);                               // PDP scope (+ .ensure_owner(..) applied by caller)
    paginate_odata::<ChatQueryFilterField, ChatMapper, chat::Entity, ChatRow, _, _>(
        select, conn, &q, ("id", SortDir::Desc), CHAT_LIMITS, ChatRow::from,
    ).await                                               // effective order: updated_at desc, id desc
}

// handler: OData errors convert into CanonicalError via `?` (resource_type gts.cf.core.odata.query.v1~)
pub async fn get_chats(Extension(ctx): Extension<SecurityContext>, Extension(state): Extension<AppState>,
                       OData(query): OData) -> Result<Json<Page<ChatDetailDto>>, CanonicalError> {
    let scope = state.authz.chat_scope(&ctx, "list").await?;        // gear-specific
    let conn = state.db.conn().map_err(/* domain -> canonical */)?;
    let page = list_chats(&conn, &scope, query).await?;             // toolkit_odata::Error -> CanonicalError
    Ok(Json(page.map_items(ChatDetailDto::from)))
}
```

For messages (`created_at asc`, `id` tiebreaker asc), apply `filter(message::Column::ChatId.eq(chat_id))` after the chat itself has been authorized.
Then use `tenant_only()` or a scope that resolves on the message entity, plus `("id", SortDir::Asc)`.
`$select` is validated by the extractor. A handler that ignores `query.select` simply ignores it.

---

## 5. Advisory locks and SELECT FOR UPDATE

Advisory locks (`libs/toolkit-db/src/advisory_locks.rs`), reached through `Db`. `DBProvider` does not forward them, so call `provider.db().lock(..)`.

```rust
pub async fn lock(&self, gear: &str, key: &str) -> toolkit_db::Result<DbLockGuard>;                 // ONE non-blocking attempt
pub async fn try_lock(&self, gear: &str, key: &str, config: LockConfig) -> toolkit_db::Result<Option<DbLockGuard>>; // retries; Ok(None) when exhausted
// DbLockGuard: key() -> &str; async release(self) -> Result<(), DbLockError>  (prefer explicit release; Drop is best-effort async)
pub struct LockConfig { pub max_wait: Option<Duration> /*30s*/, pub max_retries: Option<u32> /*None*/,
    pub backoff_base_ms: u64 /*2*/, pub backoff_factor: u64 /*25*/, pub max_backoff: Duration /*5s*/, pub jitter: bool /*true*/ }
pub enum DbLockError { Io(..), Database(sqlx::Error) /*pg|mysql*/, AlreadyHeld { lock_name }, NotHeld,
    UnexpectedDatabaseResult { message }, InvalidConfig { message } }   // DbError::Lock(..) via `?`
```

Backends:
- **PG**: session-level `pg_try_advisory_lock` on one dedicated, lazily opened connection per process (a pgbouncer in transaction mode breaks it).
  An in-process held-key registry prevents re-entrant double holds.
- **SQLite**: exclusive file-marker locks under the OS cache dir. A crash can leave a stale marker.
- Re-acquiring a key right after a `Drop` may fail until the unlock drains. Await `release()`.

`SELECT ... FOR UPDATE`: no Secure ORM method exists, and no gear uses one.
Because `.secure()` wraps any `sea_orm::Select<E>`, you can call `sea_orm::QuerySelect::{lock_exclusive(), lock_shared(), lock(LockType::Update), lock_with_behavior(..)}` **before** `.secure()`:
`Entity::find().filter(..).lock_exclusive().secure().scope_with(&s).one(tx)`.
Use it inside a transaction (`DbTx`). PG renders `FOR UPDATE`. SQLite renders nothing, and its writer lock (BEGIN plus first write, `busy_timeout`) serializes writers instead.
House alternatives for race-safety:
- guarded `UPDATE ... WHERE status = expected/version = v` and checking `rows_affected` (credstore)
- unique constraints treated as the serialization point (types-registry)
- `TxConfig::serializable()` with `transaction_with_retry`

---

## 6. Gotchas checklist for mini-chat

1. Repos take `&impl DBRunner` plus `&AccessScope` and return `ScopeError`. Services own transaction boundaries via `DBProvider::transaction`. Never call `conn()` inside the closure.
2. Every entity needs `#[derive(Scopable)]` with four explicit decisions. A scope property the entity cannot resolve gives silent deny-all (empty result), not an error.
3. UUIDs are `BLOB` on SQLite. Timestamps are TEXT RFC 3339 with trimmed subseconds. Normalize precision on write, or ordering and cursors drift on SQLite.
4. Put every CHECK, FK, and partial unique index in the initial CREATE (SQLite cannot add them later). Run one statement per `execute_unprepared`. Name migrations so they sort.
5. Outbox: same prefix in `outbox_migrations_with_prefix` and `.table_prefix()`. Enqueue last in the tx and `fire()` the `Wake` after commit. Payload is 64 KiB max. The partition count is fixed once deployed. `attempts` is per partition, so use `batch_size(1)` for per-message budgets. Handlers must be idempotent (leased).
6. OData: inject the default `$orderby` when there is no order and no cursor. The tiebreaker must be a declared `FilterField` name. Do not make nullable columns orderable. Clamping above max is silent. `limit=0` is rejected by the extractor. Every query-layer error carries `resource_type = gts.cf.core.odata.query.v1~`.
7. Detect conflicts with `ScopeError::is_unique_violation()` / `toolkit_db::secure::is_unique_violation(&DbErr)`. On PG, re-read outside the aborted transaction.

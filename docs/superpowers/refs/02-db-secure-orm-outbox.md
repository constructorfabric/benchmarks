# 02 — DB access, Secure ORM, migrations, transactions and the transactional outbox

Reference for implementing the `mini-chat` gear (`gears/mini-chat/mini-chat`, crate `cf-gears-mini-chat`, lib `mini_chat`).
Everything below was read from the repo at `/app` (sea-orm **2.0.2**, sea-query **1.0.2**, `cf-gears-toolkit-db` 0.16.2).
The mini-chat `Cargo.toml` already declares `toolkit-db = { workspace = true, features = ["sqlite", "pg"] }`,
`toolkit-db-macros`, `toolkit-security`, `sea-orm`, `sea-orm-migration`, `uuid`, `time`, `async-trait`, `thiserror`.
It does **not** yet depend on `chrono` at runtime (only dev). `OutboxMessage.created_at` is a `chrono::DateTime<Utc>`, so you only need `chrono` if you read that field.

Key source files:

| Topic | File |
|---|---|
| Crate root, `DbError`, `connect_db`, `ConnectOpts`, `DbEngine` | `libs/toolkit-db/src/lib.rs` |
| `Db`, `DbConn`, `DbTx`, transactions | `libs/toolkit-db/src/secure/db.rs` |
| `DBProvider<E>` | `libs/toolkit-db/src/db_provider.rs` |
| Secure re-exports | `libs/toolkit-db/src/secure/mod.rs` |
| `SecureSelect` | `libs/toolkit-db/src/secure/select.rs` |
| Insert/update/delete wrappers | `libs/toolkit-db/src/secure/db_ops.rs` |
| `ScopeError`, `is_unique_violation` | `libs/toolkit-db/src/secure/error.rs` |
| `TxConfig` | `libs/toolkit-db/src/secure/tx_config.rs` |
| Contention / retry detection | `libs/toolkit-db/src/contention.rs` |
| `ScopableEntity` trait | `libs/toolkit-db/src/secure/entity_traits.rs` |
| `#[derive(Scopable)]` | `libs/toolkit-db-macros/src/lib.rs`, `scopable.rs` |
| `AccessScope`, `ScopeFilter`, `pep_properties` | `libs/toolkit-security/src/access_scope.rs` |
| `SecurityContext` | `libs/toolkit-security/src/context.rs` |
| PDP to AccessScope (`PolicyEnforcer`) | `gears/system/authz-resolver/authz-resolver-sdk/src/pep/enforcer.rs` |
| Migration runner | `libs/toolkit-db/src/migration_runner.rs` |
| Outbox (whole subsystem) | `libs/toolkit-db/src/outbox/*.rs` (+ `examples/outbox_*.rs`) |
| Real outbox user (leased) | `gears/system/types-registry/types-registry/src/infra/outbox.rs`, `src/gear.rs` |
| Real tenant-scoped gear | `gears/credstore/credstore/src/...` |

---

## 1. Secure ORM

### 1.1 `#[derive(Scopable)]` attributes

Import it from `toolkit_db::secure::Scopable` (re-export) or from `toolkit_db_macros::Scopable`. Both are used in the repo.
The derive expands to `impl ::toolkit_db::secure::ScopableEntity for Entity`, so the crate must depend on `toolkit-db` under the name `toolkit_db`.

**All four dimensions must be declared explicitly. There are no defaults. Declaring both `x_col` and `no_x` is a compile error.**

| Dimension | Attribute | PEP property it answers (`resolve_property`) |
|---|---|---|
| Tenant | `tenant_col = "col"` or `no_tenant` | `"owner_tenant_id"` (`pep_properties::OWNER_TENANT_ID`) |
| Resource | `resource_col = "col"` or `no_resource` | `"id"` (`pep_properties::RESOURCE_ID`) |
| Owner | `owner_col = "col"` or `no_owner` | `"owner_id"` (`pep_properties::OWNER_ID`) |
| Type | `type_col = "col"` or `no_type` | none (no property resolves to it) |
| Custom | `pep_prop(name = "col")` (repeatable) | `"name"` |
| Global | `unrestricted` (must be the only attribute) | none; `IS_UNRESTRICTED = true` |

Column names are snake_case and become `Column::UpperCamel`. There is **no** `user_col`. The owner dimension is `owner_col`, and it can point at any column. For mini-chat's owner-only content this is `owner_col = "user_id"`. The PDP's `owner_id` predicate then compiles to `WHERE user_id = ...`.

Tenant + owner scoped entity (shape for `chats`):

```rust
use sea_orm::entity::prelude::*;
use time::OffsetDateTime;
use toolkit_db::secure::Scopable;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel, Scopable)]
#[sea_orm(table_name = "chats")]
#[secure(tenant_col = "tenant_id", resource_col = "id", owner_col = "user_id", no_type)]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub user_id: Uuid,
    pub title: Option<String>,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub deleted_at: Option<OffsetDateTime>,
}
#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
```

Real examples:
- Tenant only: `gears/credstore/credstore/src/infra/storage/entity/secrets.rs` uses `#[secure(tenant_col = "tenant_id", resource_col = "id", no_owner, no_type)]`.
- Global/system: every types-registry entity uses `#[secure(unrestricted)]`, e.g. `gears/system/types-registry/types-registry/src/infra/storage/entity/operation.rs`.
- Owner column on a `user_id` field, hand-written impl: `libs/toolkit-db/src/secure/db_ops.rs` around line 1656, `mod owner_entity`, which maps `OWNER_ID => Column::UserId`.

Child tables such as `messages`, `chat_turns`, `attachments` and `reactions` usually have `tenant_id` but no `user_id`.
Declare them `tenant_col = "tenant_id", resource_col = "id", no_owner, no_type`, and scope them with `scope.tenant_only()` **after** the parent chat has been checked against the full scope (see 1.5).

**Fail-closed rule.** The scope condition builder (`libs/toolkit-db/src/secure/cond.rs`, `build_scope_condition`) handles properties as follows:
- If a constraint names a property the entity cannot resolve, for example `owner_id` on a `no_owner` entity, that whole constraint is dropped.
- If every constraint is dropped, the result is deny-all (`WHERE false`).
- Constraints are OR-ed, and the filters inside one constraint are AND-ed.
- The default `AccessScope` is deny-all. `AccessScope::allow_all()` applies no filtering.

### 1.2 Scoped reads: `SecureSelect`

Imports: `use toolkit_db::secure::{AccessScope, SecureEntityExt, ScopeError};` plus `sea_orm::{EntityTrait, QueryFilter, ColumnTrait, Condition, Order}`.

`.secure()` returns `SecureSelect<E, Unscoped>`, which cannot execute.
`.scope_with(&scope)` returns `SecureSelect<E, Scoped>`. It is infallible: it returns the scoped select directly, **not** a `Result`, even though some doc comments show `?`.
Methods on `SecureSelect<E, Scoped>` (`select.rs`):
- `all(&runner) -> Result<Vec<Model>, ScopeError>`
- `one(&runner) -> Result<Option<Model>, ScopeError>`
- `count(&runner) -> Result<u64, ScopeError>`
- `and_id(uuid) -> Result<Self, ScopeError>`. It uses `resource_col`, which is why it returns a `Result`.
- `filter(Condition)`, `order_by(col, Order)`, `limit(u64)`, `offset(u64)`
- `and_scope_for::<J>(&scope)`, `scope_via_exists::<J>(&scope)`
- `find_also_related(R)` / `find_with_related(R)`. These auto-scope the related entity.
- `project_all(&runner, |select| select.select_only()...into_model::<T>())`
- `into_inner()`, which returns the raw `Select<E>`, for example for `paginate_odata`.

`runner` is anything that implements the sealed `DBRunner` trait: `DbConn<'_>`, `DbTx<'_>`, `SecureConn`, `SecureTx<'_>`.

```rust
let chat = chats::Entity::find()
    .filter(chats::Column::DeletedAt.is_null())          // sea_orm QueryFilter (before .secure())
    .secure()
    .scope_with(&scope)
    .and_id(chat_id)?                                      // ScopeError::Invalid if no resource_col
    .one(&conn)                                            // or `tx` inside a transaction
    .await?;
```

### 1.3 Scoped writes

**Insert.** There are three ways:
1. `secure_insert::<E>(am, &scope, &runner) -> Result<E::Model, ScopeError>`. It requires `tenant_id` to be `Set` on tenant-scoped entities, otherwise `ScopeError::Invalid("tenant_id is required")`. It validates the ActiveModel against the scope: constraints OR-ed, filters AND-ed, `NotSet` columns skipped. Failure gives `ScopeError::Denied` or `TenantNotInScope`. It returns the inserted model.
2. `E::insert(am).secure().scope_with_model(&scope, &am)?` or `.scope_unchecked(&scope)?`, then optionally `.on_conflict(SecureOnConflict<E>)` or `.on_conflict_raw(OnConflict)`, then `.exec(&runner)` or `.exec_with_returning(&runner)`. Credstore uses `scope_unchecked` (`gears/credstore/credstore/src/infra/storage/repo_impl/writes.rs`, `insert_provisioning`).
3. Batch: `secure_insert_many::<E>(Vec<ActiveModel>, &scope, &runner)` or `E::insert_many(..).secure().scope_unchecked(&scope)?.exec(&runner)` (trait `SecureInsertManyExt`). Watch out: a column that is `NotSet` in one row but `Set` in another gets NULL, not the DB default. Use `max_bind_params_for(&runner)` for chunking. SQLite allows 999 bind parameters, so types-registry chunks at 66 rows × 15 columns.

`SecureOnConflict::<E>::columns([..]).update_columns([..])?` refuses to update the tenant column. `.value(col, expr)?` is also available.

**Update.** `E::update_many().col_expr(Column::X, Expr::value(v)).filter(cond).secure().scope_with(&scope).exec(&runner)` returns `sea_orm::UpdateResult { rows_affected }`.
- `.secure()` can come before or after `col_expr` and `filter`, because both are available in every typestate.
- `exec_with_returning(&runner) -> Vec<Model>` works on PG and SQLite.
- Setting the tenant column fails at exec time with `ScopeError::Denied("tenant_id is immutable")`.
- Single-row ActiveModel update: `secure_update_with_scope::<E>(am, &scope, id, &runner)`. It reads the row inside the scope first. If the row is not there you get `Denied("entity not found or not accessible ...")`.

**Delete.** `E::delete_many().filter(..).secure().scope_with(&scope).exec(&runner)` returns `DeleteResult { rows_affected }`.

### 1.4 Conditional update (CAS) and `rows_affected`

This is the pattern for the turn finalization CAS (`WHERE state = 'running'`). It is copied from `gears/system/types-registry/types-registry/src/infra/storage/repo/operation_repo.rs` (`mark_running` / `mark_system_failed`) and credstore `writes.rs::mark_active`:

```rust
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, Condition, EntityTrait};
use toolkit_db::secure::{AccessScope, DBRunner, ScopeError, SecureUpdateExt};

pub async fn finalize_turn(
    runner: &impl DBRunner, scope: &AccessScope, turn_id: Uuid, now: OffsetDateTime,
) -> Result<bool, ScopeError> {
    let res = chat_turns::Entity::update_many()
        .secure()
        .col_expr(chat_turns::Column::State, Expr::value("done"))
        .col_expr(chat_turns::Column::CompletedAt, Expr::value(now))
        .filter(
            Condition::all()
                .add(chat_turns::Column::Id.eq(turn_id))
                .add(chat_turns::Column::State.eq("running")),
        )
        .scope_with(scope)
        .exec(runner)
        .await?;
    Ok(res.rows_affected == 1)   // the CAS winner token
}
```

`DBRunner` is `#[doc(hidden)]` but public, and repos use it as their generic runner bound (`toolkit_db::secure::DBRunner`).
Expressions such as `Expr::col(c).add(1)` and `Func::coalesce([...])` work with `col_expr`; see `mark_system_failed`.

### 1.5 Building `AccessScope`

`AccessScope` lives in `toolkit_security`. It is re-exported as `toolkit_db::secure::{AccessScope, ScopeConstraint, ScopeFilter, ScopeValue, pep_properties}`.

**From the PDP.** This is the normal request path. Use `authz_resolver_sdk::pep::{PolicyEnforcer, ResourceType, AccessRequest, EnforcerError}`:

```rust
const CHAT: ResourceType = ResourceType::from_static(
    "gts.cf.core.mini_chat.chat.v1~",   // use the gear's GTS id
    &[pep_properties::OWNER_TENANT_ID, pep_properties::OWNER_ID, pep_properties::RESOURCE_ID],
);
let enforcer = PolicyEnforcer::new(authz_client);            // Arc<dyn AuthZResolverApi> from ClientHub
let scope = enforcer.access_scope(&ctx, &CHAT, "read", Some(chat_id)).await?;   // -> AccessScope
// or with request overrides:
let req = AccessRequest::new()
    .resource_property(pep_properties::OWNER_TENANT_ID, ctx.subject_tenant_id())
    .require_constraints(true);
let scope = enforcer.access_scope_with(&ctx, &CHAT, "create", None, &req).await?;
```

Errors are `EnforcerError::{Denied{..}, CompileFailed(_), EvaluationFailed(CanonicalError)}`. Credstore maps them as follows (`gears/credstore/credstore/src/domain/authz.rs::map_enforcer_err`):
- `Denied` and `CompileFailed` become 403.
- `EvaluationFailed` becomes 503.

**Manual construction.** Use this for tests, system work, or defence in depth:

```rust
use toolkit_security::{AccessScope, ScopeConstraint, ScopeFilter, pep_properties};
let s = AccessScope::for_tenant(tenant_id);                 // owner_tenant_id IN (t)
let s = AccessScope::for_tenants(vec![t1, t2]);
let s = AccessScope::for_resource(id);                      // id IN (id)
// tenant AND owner (one constraint = AND):
let s = AccessScope::single(ScopeConstraint::new(vec![
    ScopeFilter::eq(pep_properties::OWNER_TENANT_ID, tenant_id),
    ScopeFilter::eq(pep_properties::OWNER_ID, user_id),
]));
let s = AccessScope::from_constraints(vec![c1, c2]);       // OR of constraints
// ScopeConstraint::try_new(filters) -> Err(EmptyScopeConstraint) for empty input.
AccessScope::allow_all();   // unconstrained (no WHERE). Used for system/background work.
AccessScope::deny_all();    // == Default
```

Derivations (all fail closed when unconstrained, i.e. they become deny-all, except `ensure_owner`):
- `scope.tenant_only()` keeps only `owner_tenant_id` filters. It is meant for re-targeting to a child entity declared `no_owner`, after the parent has been authorized.
- `scope.tenant_and_owner()` keeps the tenant and owner filters.
- `scope.ensure_owner(user_id)` guarantees `owner_id = user_id` in every constraint. Unconstrained input becomes `[owner_id = user]`. Use it as a defence-in-depth owner guard.

Queries: `is_unconstrained()`, `is_deny_all()`, `has_property(p)`, `all_uuid_values_for(p)`, and `contains_uuid(p, id)`. `contains_uuid` is not an authorization decision.
`toolkit_db::secure::validate_tenant_in_scope(tenant_id, &scope)` checks a tenant id against the scope.

`SecurityContext` (`toolkit_security::SecurityContext`) has `subject_id()`, `subject_tenant_id()`, `subject_type()` and `token_scopes()`. Test builder: `SecurityContext::builder().subject_id(u).subject_tenant_id(t).build()?`.

### 1.6 System / unscoped access for background jobs

There is **no** "skip security" switch. You have two options:
- Pass `AccessScope::allow_all()` to the same secure API. Types-registry does this for its background worker: `fn scope() -> AccessScope { AccessScope::allow_all() }` in `src/domain/registry_service.rs`. The orphan watchdog and upload reaper can use `allow_all()` for their cross-tenant sweeps. On insert, `validate_insert_scope` short-circuits for unconstrained scopes.
- Mark the entity `#[secure(unrestricted)]`. This is only for truly global tables, because it also removes PDP filtering for user paths.

### 1.7 Getting connections and running transactions

Types, all from `toolkit_db`:
- `Db`: a cheap clone of `Arc<DbHandle>`.
- `DbConn<'a>`: a non-transactional runner.
- `DbTx<'a>`: a transactional runner.
- `DBProvider<E>`: a typed façade over `Arc<Db>`.
- `DbError`, `TxConfig`, `TxIsolationLevel`, `TxAccessMode` (the last three via `toolkit_db::secure`).

Gears never see `sea_orm::DatabaseConnection`. There is **no public raw-SQL execution API for gear code** (`execute_raw` / `Statement` only appear in migrations and inside toolkit-db). Everything goes through the Secure ORM builders. `SecureConn` / `SecureTx` exist in `secure_conn.rs` but are not used by current gears. Use `DbConn` / `DbTx`.

```rust
// DBProvider<E> where E: From<DbError> + Send + 'static
pub fn conn(&self) -> Result<DbConn<'_>, E>;           // errors with DbError::ConnRequestedInsideTx if called inside a tx
pub fn db(&self) -> Db;
pub async fn transaction<T, F>(&self, f: F) -> Result<T, E>
  where T: Send + 'static,
        F: for<'a> FnOnce(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>> + Send;
pub async fn transaction_with_config<T, F>(&self, cfg: TxConfig, f: F) -> Result<T, E>;

// Db (same closure shape)
pub fn conn(&self) -> Result<DbConn<'_>, DbError>;
pub fn backend(&self) -> sea_orm::DbBackend;           // Postgres | Sqlite | MySql — use for dialect branches
pub fn db_engine(&self) -> &'static str;               // "postgres" | "sqlite" | "mysql"
pub async fn transaction_ref<F,T>(&self, f: F) -> Result<T, DbError>;
pub async fn transaction_ref_mapped<F,T,E: From<DbError>>(&self, f: F) -> Result<T, E>;
pub async fn transaction_ref_mapped_with_config<F,T,E>(&self, cfg: TxConfig, f: F) -> Result<T, E>;
pub async fn transaction_with_retry<T,E,X,F>(&self, cfg: TxConfig, extract_db_err: X, body: F) -> Result<T, E>
  where X: Fn(&E) -> Option<&sea_orm::DbErr> + Send,
        F: for<'a> FnMut(&'a DbTx<'a>) -> Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>> + Send;
pub async fn transaction_with_retry_max(...max_attempts: u32...);   // DEFAULT_TX_RETRY_ATTEMPTS = 3
pub async fn lock(&self, gear: &str, key: &str) -> Result<DbLockGuard>;           // advisory lock
pub async fn try_lock(&self, gear: &str, key: &str, cfg: LockConfig) -> Result<Option<DbLockGuard>>;
```

`transaction_with_retry` retries the **whole** closure on PG `40001`/`40P01` and SQLite `SQLITE_BUSY`/`BUSY_SNAPSHOT`, with jittered backoff of 100 ms max (`contention::is_retryable_contention`).
`TxConfig` constructors: `TxConfig::serializable()`, `TxConfig::read_only()`, `TxConfig::with_isolation(TxIsolationLevel::RepeatableRead)`, or `TxConfig::default()`.

Transaction example. The closure must own its captures, so clone `Arc`s and `Copy` ids into it. Return a `Box::pin(async move {...})`:

```rust
let db: Arc<DBProvider<DomainError>> = ...;  // DomainError: From<DbError>
let scope = scope.clone();
let won = db.transaction(move |tx| {
    Box::pin(async move {
        let won = finalize_turn(tx, &scope, turn_id, now).await.map_err(map_scope_err)?;
        if won {
            secure_insert::<messages::Entity>(am, &scope, tx).await.map_err(map_scope_err)?;
        }
        Ok(won)
    })
}).await?;
```

Inside the closure you **must** use `tx`. Calling `db.conn()` there returns `DbError::ConnRequestedInsideTx`, because a task-local guard is set.
If the closure returns `Err`, the transaction rolls back. If it returns `Ok`, it commits, and a commit failure is mapped through `E::from(DbError)`.

`impl From<ScopeError> for DbError` exists (it wraps into `DbError::Other`). So with `E = DbError`, `?` works on `ScopeError` directly. With a domain error type you write your own `map_scope_err`, as credstore does in `repo_impl/helpers.rs`.

### 1.8 `SELECT ... FOR UPDATE`

There is no dedicated API, but it works by composing sea-orm's `QuerySelect` lock methods **before** `.secure()`:

```rust
use sea_orm::QuerySelect;
let rows = quota_usage::Entity::find()
    .filter(...)
    .lock_exclusive()                      // FOR UPDATE ; also .lock(LockType::Update), .lock_with_behavior(..)
    .secure().scope_with(&scope)
    .all(tx).await?;                       // must run on a DbTx
```

Verified in `sea-query-1.0.2/src/backend/sqlite/query.rs`: `prepare_select_lock` is a **no-op** for SQLite ("SQLite doesn't supports row locking"). So the same code emits `FOR UPDATE` on PG and nothing on SQLite.
On SQLite, correctness comes from the single-writer lock. Because sqlx uses deferred `BEGIN`, the write lock is only taken at the first write statement. Concurrent read-then-write transactions can therefore hit `SQLITE_BUSY` / `BUSY_SNAPSHOT`, so wrap them in `transaction_with_retry`.
No gear in the repo currently uses `lock_exclusive`. The portable alternative, used everywhere in the repo, is the conditional `UPDATE ... WHERE <expected state>` plus a `rows_affected` check (1.4), or a partial unique index plus unique-violation detection (section 6).

### 1.9 Unique-violation-safe "insert or conflict"

Use `SecureOnConflict` with `.on_conflict(...)`. Alternatively, insert and catch `ScopeError::Db(e)` where `is_unique_violation(&e)`; see section 6.

---

## 2. Obtaining the DB in `init`; backends; UUID storage

### 2.1 Gear wiring

From `libs/toolkit/src/context.rs`:

```rust
pub fn db(&self) -> Option<toolkit_db::DBProvider<toolkit_db::DbError>>;
pub fn db_required(&self) -> anyhow::Result<toolkit_db::DBProvider<toolkit_db::DbError>>;
```

Credstore's pattern (`gears/credstore/credstore/src/gear.rs` lines 104-106) re-types the provider with the gear's own error:

```rust
let db_raw = ctx.db_required()?;
let db: Arc<DBProvider<DomainError>> = Arc::new(DBProvider::new(db_raw.db()));
// the raw `Db` for Outbox::builder:  db_raw.db()
```

The gear declares the capability with `#[toolkit::gear(name = "mini-chat", deps = [...], capabilities = [db, rest, stateful], ...)]` and implements `toolkit::contracts::DatabaseCapability`.
The runtime runs migrations **before** `init`/REST, using the per-gear table `toolkit_migrations__<sanitized>__<hash8>`.
For background tasks (outbox start/stop), types-registry uses `lifecycle(entry = "serve", stop_timeout = "30s", await_ready)` with a `serve(self: Arc<Self>, cancel, ready)` method. The alternative is implementing `toolkit::contracts::RunnableCapability { async fn start(&self, CancellationToken); async fn stop(&self, deadline: CancellationToken) }` (`libs/toolkit/src/contracts.rs`), which matches the mini-chat DESIGN "start/stop" steps.

### 2.2 Backends

The backend is detected from the DSN prefix (`lib.rs::DbHandle::detect`): `postgres://` / `postgresql://` → PG, `sqlite:` → SQLite, `mysql://` → MySQL (mini-chat does not need it).
For runtime dialect branches use `db.backend()` (`sea_orm::DbBackend`) or `db.db_engine()`. In migrations use `manager.get_database_backend()`.

SQLite DSN parameters are a whitelist (`journal_mode`, `wal`, `synchronous`, `busy_timeout`).
- File DBs default to WAL with `busy_timeout = 5000` ms.
- `:memory:` uses the DELETE journal and gets no busy_timeout.
- sqlx enables `PRAGMA foreign_keys` by default.

### 2.3 UUIDs and timestamps in SQLite

- sqlx encodes a Rust `Uuid` as **16 raw bytes** on SQLite and sea-query binds `Value::Uuid` straight through. Entities keep `pub id: Uuid` and no helper or custom type is needed. The rationale is in the header of `gears/system/types-registry/types-registry/src/infra/storage/migrations/m20260817_000001_initial.rs`.
- The declared column type can be `BLOB` (credstore, types-registry) or `TEXT` (the mini-chat DESIGN wording). SQLite is dynamically typed, so the **stored value is a BLOB either way**. Types-registry adds `CHECK (length(col) = 16)` on SQLite so raw text cannot sneak in.
- Raw-SQL literals in migrations must use the blob form. Credstore uses `DEFAULT (x'{hex}')`, where the hex is the uuid string with the `-` removed, while PG uses `DEFAULT '{uuid}'`. Never compare to a `'xxxxxxxx-...'` text literal in SQLite.
- There is **no UUID helper in toolkit-db**. When building `Expr::value(uuid)` through the ORM, the bytes are bound for you.
- `time::OffsetDateTime` is stored as TEXT in SQLite (`TIMESTAMPTZ` in PG). Credstore and types-registry declare `TEXT` on SQLite. Write timestamps from Rust in UTC rather than mixing them with SQLite `CURRENT_TIMESTAMP` defaults, because those use a different text format and compare lexicographically.
- Booleans are `INTEGER` on SQLite; types-registry adds `CHECK (x IN (0,1))`. Byte arrays are `BYTEA` on PG and `BLOB` on SQLite.

---

## 3. Migrations

### 3.1 How a gear exposes them

```rust
// infra/storage/migrations.rs (credstore style)
use sea_orm_migration::prelude::*;
pub mod m0001_initial;
pub struct Migrator;
#[async_trait::async_trait]                        // credstore uses async_trait on MigratorTrait; types-registry does not
impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> { vec![Box::new(m0001_initial::Migration)] }
}

// gear.rs (types-registry style: own migrations + outbox migrations)
impl toolkit::contracts::DatabaseCapability for MiniChatGear {
    fn migrations(&self) -> Vec<Box<dyn sea_orm_migration::MigrationTrait>> {
        use sea_orm_migration::MigratorTrait;
        let mut m = crate::infra::storage::migrations::Migrator::migrations();
        m.extend(
            toolkit_db::outbox::outbox_migrations_with_prefix(OUTBOX_TABLE_PREFIX)
                .unwrap_or_else(|e| panic!("invalid outbox prefix: {e}")),  // migrations() can't return errors
        );
        m
    }
}
```

Credstore: `gears/credstore/credstore/src/gear.rs:180`. Types-registry: `src/gear.rs:278-296`.
`toolkit_db::sea_orm_migration` is re-exported, but mini-chat already depends on `sea-orm-migration` directly.

### 3.2 Writing a dialect-aware migration

Credstore (`gears/credstore/credstore/src/infra/storage/migrations/m0001_initial_schema.rs`) uses per-backend `CREATE TABLE` plus **shared** partial-index SQL, because SQLite supports `CREATE [UNIQUE] INDEX ... WHERE`:

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
            sea_orm::DatabaseBackend::Postgres => r"CREATE TABLE IF NOT EXISTS chat_turns (
                id UUID PRIMARY KEY, tenant_id UUID NOT NULL, chat_id UUID NOT NULL,
                request_id UUID NOT NULL, state SMALLINT NOT NULL,
                started_at TIMESTAMPTZ NOT NULL, deleted_at TIMESTAMPTZ NULL)",
            sea_orm::DatabaseBackend::Sqlite => r"CREATE TABLE IF NOT EXISTS chat_turns (
                id BLOB PRIMARY KEY NOT NULL, tenant_id BLOB NOT NULL, chat_id BLOB NOT NULL,
                request_id BLOB NOT NULL, state SMALLINT NOT NULL,
                started_at TEXT NOT NULL, deleted_at TEXT NULL)",
            _ => return Err(DbErr::Custom("mini-chat: MySQL not supported".into())),
        };
        for sql in [
            table_sql,
            // identical on PG and SQLite:
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_turns_running ON chat_turns (chat_id) WHERE state = 1 AND deleted_at IS NULL;",
            "CREATE UNIQUE INDEX IF NOT EXISTS uq_chat_turns_req ON chat_turns (chat_id, request_id);",
        ] {
            conn.execute_unprepared(sql).await?;
        }
        Ok(())
    }
    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.get_connection().execute_unprepared("DROP TABLE IF EXISTS chat_turns;").await?;
        Ok(())
    }
}
```

Credstore's real partial indexes, verbatim, which run on both backends:

```sql
CREATE UNIQUE INDEX IF NOT EXISTS uq_credstore_nonprivate ON credstore_secrets (tenant_id, reference) WHERE sharing <> 1;
CREATE UNIQUE INDEX IF NOT EXISTS uq_credstore_private ON credstore_secrets (tenant_id, reference, owner_id) WHERE sharing = 1;
CREATE INDEX IF NOT EXISTS idx_credstore_expiry ON credstore_secrets (expires_at) WHERE expires_at IS NOT NULL AND status = 2;
```

Types-registry uses another form: `fn up_statements(backend) -> Result<&'static [&'static str], DbErr>` with `PG_UP_STATEMENTS` / `SQLITE_UP_STATEMENTS` arrays executed via `conn.execute_raw(Statement::from_string(backend, sql.to_owned()))`, and an explicit error arm for unsupported backends (`DatabaseBackend` is `#[non_exhaustive]`).
Its per-backend type table (PG → SQLite):

| PG | SQLite |
|---|---|
| `bigint GENERATED BY DEFAULT AS IDENTITY` | `INTEGER PRIMARY KEY AUTOINCREMENT` |
| `uuid` | `BLOB` + `length(x)=16` CHECK |
| `bytea` | `BLOB` |
| `boolean` | `INTEGER` + `IN (0,1)` |
| `timestamptz` | `TEXT` |
| `varchar COLLATE "C"` | `TEXT COLLATE BINARY` |

Gotcha: the `#[async_trait]` impl must elide the `SchemaManager` lifetime. Types-registry adds `#[allow(elided_lifetimes_in_paths)]` because the crate denies `rust_2018_idioms`.
The SchemaManager builder (`manager.create_table(Table::create()...)`) also works; see `libs/toolkit-db/tests/sqlite/transaction.rs`. But it cannot express partial indexes or CHECKs portably, which is why gears use raw SQL.

---

## 4. Transactional outbox (`toolkit_db::outbox`)

### 4.1 Public API surface (`libs/toolkit-db/src/outbox/mod.rs`)

```rust
pub use toolkit_db::outbox::{
    Outbox, OutboxBuilder, OutboxHandle, QueueBuilder, LeasedQueueBuilder,
    LeasedHandler, LeasedMessageHandler, TransactionalHandler, TransactionalMessageHandler, PerMessageAdapter,
    Batch, OutboxMessage, HandlerResult, MessageResult,
    Record, RecordBuilder, RecordTarget, Records, RecordsBuilder, RecordsTarget,
    Wake, in_transaction,
    LeaseConfig, OutboxError, OutboxMessageId, OutboxProfile, Partitions, WorkerTuning,
    DeadLetterFilter, DeadLetterMessage, DeadLetterScope, DeadLetterStatus,
    outbox_migrations, outbox_migrations_with_prefix,
    TraceSubscription, TraceWatch, TraceOutcome, TraceState, TraceStatus,
};
```

The pipeline has these stages: enqueue (incoming + body rows) → sequencer (assigns per-partition seq into outgoing) → one processor task per partition → vacuum.
- Ordering is **per partition**.
- Leased mode is **at-least-once**, so handlers must be idempotent.
- On PG, partitions are claimed with `FOR UPDATE SKIP LOCKED`. SQLite is single-process only.

### 4.2 Migrations for outbox tables

```rust
pub fn outbox_migrations() -> Vec<Box<dyn MigrationTrait>>;                  // prefix "toolkit_outbox"
pub fn outbox_migrations_with_prefix(prefix: impl Into<String>)
    -> Result<Vec<Box<dyn MigrationTrait>>, OutboxError>;                    // OutboxError::InvalidTablePrefix
```

Prefix rules (`outbox/tables.rs`): 1-36 bytes, must start with an ASCII letter, and may contain only `[A-Za-z0-9_]`. **No `-`**, so use `mini_chat_outbox` or `mini_chat__outbox`.
The migration name is `m001_create_toolkit_outbox_schema__<prefix>`. The **same prefix** must be passed to `Outbox::builder(db).table_prefix(prefix)?`.
Types-registry uses `TABLE_PREFIX = "types_registry__outbox"`.

### 4.3 Builder / startup

```rust
impl Outbox { pub fn builder(db: Db) -> OutboxBuilder; }
impl OutboxBuilder {
    pub fn table_prefix(self, p: impl Into<String>) -> Result<Self, OutboxError>;
    pub fn profile(self, p: OutboxProfile) -> Self;            // default_profile() | low_latency() | high_throughput() | relaxed()
    pub fn processor_tuning(self, t: WorkerTuning) -> Self;     // overrides the profile; also sequencer_/vacuum_/reconciler_tuning
    pub fn processors(self, n: usize) -> Self;
    pub fn queue(self, name: &str, partitions: Partitions) -> QueueBuilder;
    pub async fn start(self) -> Result<OutboxHandle, OutboxError>;
}
impl QueueBuilder {
    pub fn leased(self, h: impl LeasedHandler + 'static) -> LeasedQueueBuilder;   // LeasedMessageHandler works via blanket impl
    pub fn transactional(self, h: impl TransactionalMessageHandler + 'static) -> OutboxBuilder;
    pub fn batch_transactional(self, h: impl TransactionalHandler + 'static) -> OutboxBuilder;
}
impl LeasedQueueBuilder {
    pub fn lease(self, cfg: LeaseConfig) -> Self;              // panics if headroom >= duration
    pub fn queue(self, name: &str, p: Partitions) -> QueueBuilder;   // chain next queue
    pub fn done(self) -> OutboxBuilder;
    pub async fn start(self) -> Result<OutboxHandle, OutboxError>;
}
pub struct LeaseConfig { pub duration: Duration, pub headroom: Duration }   // default 30s / 2s
impl OutboxHandle { pub fn outbox(&self) -> &Arc<Outbox>; pub async fn stop(self); }  // Drop also cancels workers
```

- `Partitions::of(n)` requires n to be a power of two in 1..=64, and **panics** otherwise. It is a `const fn`.
- The partition count is persisted per queue. Restarting with a different count fails `start()` with `OutboxError::PartitionCountMismatch`.
- Queue names must match `[A-Za-z0-9._-]{1,1024}` and start and end with an alphanumeric, so `mini-chat.usage_snapshot` is valid.
- The handler budget is `duration - headroom`. At that point the handler future is dropped and the result becomes `Retry { reason: "lease expired" }`.
- `WorkerTuning::processor_default()` uses `batch_size: 10`, `retry_base: 1s`, `retry_max: 1min`, `stop_grace: 5s`. The builders are `.batch_size(n)`, `.idle_interval(d)`, `.retry_base(d)`, `.retry_max(d)`, `.stop_grace(d)`, and so on.
- To get a retry cap of about 30 s, use `.processor_tuning(WorkerTuning::processor_low_latency().retry_max(Duration::from_secs(30)))`.

**`attempts` caveat** (types-registry `AdmissionHandler::required_tuning`): the retry counter lives on the **partition's processor row**. It is not per message, and it resets to 0 on progress. If you need "give up after N attempts" per message, use `batch_size(1)`, as types-registry does with `WorkerTuning::processor_low_latency().batch_size(1)`.

Five queues with leased handlers (mini-chat shape):

```rust
use std::time::Duration;
use toolkit_db::outbox::{LeaseConfig, Outbox, OutboxError, OutboxHandle, OutboxProfile, Partitions, WorkerTuning};

pub const OUTBOX_TABLE_PREFIX: &str = "mini_chat_outbox";

pub async fn start_outbox(db: toolkit_db::Db, h: Handlers, cfg: &OutboxCfg) -> Result<OutboxHandle, OutboxError> {
    Outbox::builder(db)
        .table_prefix(OUTBOX_TABLE_PREFIX)?
        .profile(OutboxProfile::low_latency())
        .processor_tuning(WorkerTuning::processor_low_latency().batch_size(1).retry_max(Duration::from_secs(30)))
        .queue(&cfg.usage_queue, Partitions::of(8)).leased(h.usage)                 // default lease 30s/2s
        .queue(&cfg.attachment_cleanup_queue, Partitions::of(4)).leased(h.attachment_cleanup)
        .queue(&cfg.chat_cleanup_queue, Partitions::of(4)).leased(h.chat_cleanup)
        .queue(&cfg.thread_summary_queue, Partitions::of(4)).leased(h.thread_summary)
            .lease(LeaseConfig { duration: Duration::from_secs(cfg.claim_timeout_secs), headroom: Duration::from_secs(2) })
        .queue(&cfg.audit_queue, Partitions::of(4)).leased(h.audit)
            .lease(LeaseConfig { duration: Duration::from_secs(60), headroom: Duration::from_secs(2) })
        .start()
        .await
}
```

Note that `QueueBuilder::queue` borrows `&str`, which is copied into a `String`. `.leased(...)` returns a `LeasedQueueBuilder`, whose `.queue(...)` finalizes the previous queue.

### 4.4 Handler traits and message

```rust
pub struct OutboxMessage {            // all fields pub → constructible in unit tests
    pub partition_id: i64,            // DB PK of the partition row, NOT the 0..n partition number
    pub seq: i64,
    pub payload: Vec<u8>,
    pub payload_type: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub attempts: i16,                // 0 on first attempt (per-partition counter, see caveat)
}

#[async_trait::async_trait]
pub trait LeasedMessageHandler: Send + Sync {          // per-message (common case)
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult;
}
pub enum MessageResult { Ok, Retry, Reject(String) }   // Reject → dead letter, continue with the rest

#[async_trait::async_trait]
pub trait LeasedHandler: Send + Sync {                  // batch form; blanket impl for every LeasedMessageHandler
    async fn handle(&self, batch: &mut Batch<'_>) -> HandlerResult;
}
pub enum HandlerResult { Success, Retry { reason: String }, Reject { reason: String } }

// Batch<'a>: next_msg() -> Option<&OutboxMessage>, next_chunk(n), ack(), ack_chunk(n), reject(String),
//            remaining() -> Duration (until lease cancel point), should_stop() -> bool, len(), processed()
// Batch::new is pub(crate): unit-test handlers through LeasedMessageHandler::handle(&msg) directly.

#[async_trait::async_trait]
pub trait TransactionalMessageHandler: Send + Sync {    // runs inside the ack tx (exactly-once in-DB)
    async fn handle(&self, txn: &sea_orm::DatabaseExecutor<'_>, msg: &OutboxMessage) -> HandlerResult;
}
```

Semantics of the blanket per-message impl (`handler.rs`):
- `Ok` calls `batch.ack()`.
- `Reject(r)` calls `batch.reject(r)` and continues.
- `Retry` returns `HandlerResult::Retry` immediately.
- It stops early when `should_stop()` is true.

With a leased `Retry`, the processed prefix (acks and rejects) **is** committed: the cursor advances past it and only the tail is retried (`strategy.rs` around line 713). Retry uses exponential backoff from `retry_base` up to `retry_max`.
With a batch-level `HandlerResult::Reject`, every message in the batch is dead-lettered.

Types-registry's custom `LeasedHandler` reads `batch.remaining()` before each `next_msg()` to bound its work (`src/infra/outbox.rs` around line 503):

```rust
#[async_trait::async_trait]
impl LeasedHandler for AdmissionHandler {
    async fn handle(&self, batch: &mut Batch<'_>) -> HandlerResult {
        loop {
            let lease = batch.remaining();
            let Some(msg) = batch.next_msg() else { break };
            match self.handle_message(msg, lease).await {
                MessageResult::Ok => batch.ack(),
                MessageResult::Retry => return HandlerResult::Retry { reason: "asked for redelivery".to_owned() },
                MessageResult::Reject(reason) => batch.reject(reason),
            }
            if batch.should_stop() { break; }
        }
        HandlerResult::Success
    }
}
```

### 4.5 Enqueue inside a DB transaction

```rust
impl Outbox {
    pub async fn enqueue(&self, db: &(impl DBRunner + Sync + ?Sized), msg: Record<'_>) -> Result<Wake, OutboxError>;
    pub async fn enqueue_batch(&self, db: &(impl DBRunner + Sync + ?Sized), batch: Records<'_>) -> Result<Wake, OutboxError>;
}
// single
let rec = Record::to(QUEUE, partition)                      // partition: u32 in 0..n  (explicit, NOT a key)
    .payload(bytes, "mini-chat.usage.v1")                   // payload_type: 1-1024 printable ASCII
    .trace("optional-unique-id")                            // optional completion tracking
    .build()?;                                              // validates; no DB I/O
let wake = outbox.enqueue(tx, rec).await?;
// batch (one queue, default type, per-item override)
let recs = Records::to(QUEUE).payload_type("t.v1")
    .push(p0, bytes0).push_with_type(p1, bytes1, "t2.v1").build()?;
let wake = outbox.enqueue_batch(tx, recs).await?;
```

**Partition key.** There is no hashing helper, so the caller maps its key to `0..n` itself. Types-registry uses the UUID tail bytes:

```rust
fn partition(id: Uuid) -> u32 {
    let b = id.as_bytes();
    u32::from(u16::from_be_bytes([b[14], b[15]]) % PARTITIONS)   // PARTITIONS: u16
}
```

Use a stable key, for example `chat_id` for per-chat ordering or `tenant_id` for usage.
Enqueue errors include:
- `OutboxError::QueueNotRegistered(q)`, before `start()` has registered the queue.
- `PartitionOutOfRange{..}`.
- `Database(DbErr)`.

**Wake contract.** Enqueue does **not** wake the sequencer.
- Call `wake.fire()` **after** the transaction commits.
- Call `wake.discard()` or drop it on rollback. Dropping an unfired, non-empty wake logs a warning, and the rows are still delivered by the cold reconciler, just later (idle interval 1 min, or 30 s for `low_latency`).
- Combine several wakes with `let mut w = Wake::empty(); w += outbox.enqueue(..).await?;`.

There are two ways to honour this:

```rust
// (a) helper: commit-then-fire in one place; closure returns (T, Wake). Enqueue LAST.
let value = toolkit_db::outbox::in_transaction(&db /* &Db */, move |tx| Box::pin(async move {
    // ... business writes on tx ...
    let wake = outbox.enqueue(tx, rec).await.map_err(MyErr::from)?;
    Ok::<_, MyErr>((value, wake))
})).await?;

// (b) own transaction (types-registry / example style)
let wake = db.transaction(move |tx| Box::pin(async move {
    /* writes */ Ok(outbox.enqueue(tx, rec).await.map_err(MyErr::from)?)
})).await?;
wake.fire();
```

`outbox.enqueue(&conn, rec)` with a plain `DbConn` is allowed (non-atomic), as in `examples/outbox_retry_reject.rs`.

**Payload size limit.** `MAX_PAYLOAD_SIZE = 64 KiB` (`64 * 1024`, `outbox/validation.rs`). `Record...build()` fails with `OutboxError::PayloadTooLarge { size, max }`. Other build errors: `InvalidQueueName{reason}`, `InvalidPayloadType{reason}`, `InvalidTrace{reason}`, `TraceTooLong{size,max}` (trace max 256 bytes), and `EmptyTracedBatch`.
`OutboxError` is `#[non_exhaustive]` and `thiserror`-based, with `Database(#[from] sea_orm::DbErr)`.

### 4.6 Dead letters

When a handler rejects, the message is copied into the dead-letter table with its payload, partition, seq, reason and attempts. Nothing is replayed automatically.

```rust
impl Outbox {
    pub async fn dead_letter_list(&self, db: &(impl DBRunner + Sync), f: &DeadLetterFilter) -> Result<Vec<DeadLetterMessage>, OutboxError>;
    pub async fn dead_letter_count(&self, db, f: &DeadLetterFilter) -> Result<u64, OutboxError>;
    pub async fn dead_letter_replay(&self, db, s: &DeadLetterScope, timeout: Duration) -> Result<Vec<DeadLetterMessage>, OutboxError>; // claim → reprocessing
    pub async fn dead_letter_resolve(&self, db, ids: &[i64]) -> Result<u64, OutboxError>;     // → resolved
    pub async fn dead_letter_reject(&self, db, ids: &[i64], reason: &str) -> Result<u64, OutboxError>; // → pending, attempts++
    pub async fn dead_letter_discard(&self, db, s: &DeadLetterScope) -> Result<u64, OutboxError>;      // pending → discarded
    pub async fn dead_letter_cleanup(&self, db, s: &DeadLetterScope) -> Result<u64, OutboxError>;      // delete resolved+discarded
}
// DeadLetterScope::default().queue("mini-chat.audit").payload_type("..").partition(id).limit(n)
// DeadLetterFilter::default() (status = Pending) / ::from_scope(s).status(DeadLetterStatus::Resolved).any_status()
// DeadLetterMessage { id, partition_id, seq, payload, payload_type, created_at, failed_at, last_error,
//                     attempts, status: DeadLetterStatus, completed_at, deadline }
```

### 4.7 Shutdown

Call `handle.stop().await` (consumes the handle). It cancels the workers and joins them. An in-flight handler gets `WorkerTuning::stop_grace` (5 s) plus lease headroom before it is dropped.
Types-registry keeps `Mutex<Option<OutboxHandle>>`, then `take()`s it after the cancel token fires, releasing the mutex before `stop().await` (`gear.rs::serve`).
For a gear with an explicit `RunnableCapability::stop(deadline)`, combine it with the deadline: `tokio::select! { _ = handle.stop() => {}, _ = deadline.cancelled() => {} }`.

### 4.8 Minimal end-to-end example (SQLite in-memory, leased)

Condensed from `libs/toolkit-db/examples/outbox_retry_reject.rs` and `outbox_gear_integration.rs`:

```rust
use std::time::Duration;
use toolkit_db::outbox::{
    DeadLetterFilter, LeasedMessageHandler, MessageResult, Outbox, OutboxMessage, Partitions, Record,
    WorkerTuning, outbox_migrations_with_prefix,
};
use toolkit_db::{ConnectOpts, DbError, connect_db, migration_runner::run_migrations_for_testing};

struct Usage;
#[async_trait::async_trait]
impl LeasedMessageHandler for Usage {
    async fn handle(&self, msg: &OutboxMessage) -> MessageResult {
        match serde_json::from_slice::<serde_json::Value>(&msg.payload) {
            Err(e) => MessageResult::Reject(format!("corrupt payload: {e}")),
            Ok(_) if msg.attempts >= 120 => MessageResult::Reject("retry budget exhausted".into()),
            Ok(_v) => MessageResult::Ok,            // do idempotent work here
        }
    }
}

#[tokio::test]
async fn outbox_roundtrip() -> anyhow::Result<()> {
    let db = connect_db("sqlite:file:mc_outbox_t1?mode=memory&cache=shared",
                        ConnectOpts { max_conns: Some(1), ..Default::default() }).await?;
    run_migrations_for_testing(&db, outbox_migrations_with_prefix("mini_chat_outbox")?).await?;

    let handle = Outbox::builder(db.clone())
        .table_prefix("mini_chat_outbox")?
        .processor_tuning(WorkerTuning::processor_default().idle_interval(Duration::from_millis(50)))
        .sequencer_tuning(WorkerTuning::sequencer_default().idle_interval(Duration::from_millis(50)))
        .queue("mini-chat.usage_snapshot", Partitions::of(4)).leased(Usage)
        .start().await?;
    let outbox = handle.outbox().clone();

    let wake = db.transaction_ref(move |tx| Box::pin(async move {
        // business writes on tx ...
        let rec = Record::to("mini-chat.usage_snapshot", 0)
            .payload(br#"{"x":1}"#.to_vec(), "mini-chat.usage.v1").build()
            .map_err(|e| DbError::Other(e.into()))?;
        outbox.enqueue(tx, rec).await.map_err(|e| DbError::Other(e.into()))
    })).await?;
    wake.fire();

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(handle.outbox().dead_letter_count(&db.conn()?, &DeadLetterFilter::default()).await?, 0);
    handle.stop().await;
    Ok(())
}
```

---

## 5. Tests without network

### 5.1 SQLite in-memory

Credstore (`gears/credstore/credstore/src/infra/storage/repo_impl/repo_tests.rs`):

```rust
let dsn = format!("sqlite:file:credstore_repo_{}?mode=memory&cache=shared", Uuid::new_v4());
let db = connect_db(&dsn, ConnectOpts { max_conns: Some(1), min_conns: Some(1), ..Default::default() }).await?;
run_migrations_for_testing(&db, Migrator::migrations()).await?;   // migration table "_test"
let repo = SecretRepoImpl::new(Arc::new(DBProvider::<DomainError>::new(db)));
```

Types-registry (`gears/system/types-registry/types-registry/tests/common/mod.rs`) has several helpers:
- `test_db()` uses `"sqlite::memory:"` with `max_conns = 1`. This is **mandatory**: a bare `:memory:` gives each pooled connection its own empty DB.
- `test_db_file(path)` uses `sqlite://{path}?mode=rwc` with 4 connections. `test_db_file_wal` adds `&journal_mode=wal`.
- `test_db_with_outbox()` uses `sqlite:file:tr-outbox-{uuid}?mode=memory&cache=shared` with `max_conns = 4`, running the gear's migrations plus `outbox_migrations_with_prefix(TABLE_PREFIX)`.
- `await_delivery(what, || async { Option<T> })` polls with backoff (10 ms → 100 ms) up to 2 s.
- A `TestDir` guard deletes temp dirs. Declare it before the provider so the provider drops first.

Rules:
- Use a unique name per test in `file:<name>?mode=memory&cache=shared` so parallel tests stay isolated.
- With `max_conns = 1`, never hold a `DbConn` borrowed while calling `transaction(...)`, and never call `db.conn()` inside a transaction closure. The first fails, the second is rejected.

### 5.2 Outbox in tests

Test a handler **without** the pipeline by building an `OutboxMessage { partition_id: 1, seq: 1, payload, payload_type: "...".into(), created_at: chrono::Utc::now(), attempts: 0 }` and calling the handler's `handle(&msg)`. Types-registry calls `AdmissionHandler::admit_payload(bytes, attempts)` this way.
For full-pipeline tests, start the real pipeline on a shared-cache in-memory DB (4.8) and poll for the side effect. `idle_interval(50ms)` keeps tests fast.
`toolkit_db::outbox::Wake::empty()` lets a no-op enqueuer port return a wake (`examples/outbox_gear_integration.rs`, `NoopEnqueuer`).

### 5.3 Postgres via `libs/test-containers` (needs Docker; keep it behind a feature)

Types-registry gates these with `#![cfg(feature = "integration")]` and `integration = ["toolkit-db/pg", ...]`. Its dev-dependencies are `testcontainers = { workspace = true }` and `test-containers = { workspace = true }`:

```rust
use testcontainers::{ImageExt, runners::AsyncRunner};
let c = test_containers::postgres()
    .with_env_var("POSTGRES_PASSWORD", "pass").with_env_var("POSTGRES_USER", "user")
    .with_env_var("POSTGRES_DB", "app")
    .start().await?;
let port = c.get_host_port_ipv4(5432).await?;
let host = c.get_host().await?.to_string();
let db = provider_for_with_outbox(&format!("postgres://user:pass@{host}:{port}/app"), 4).await;
```

(`tests/outbox_backends_test.rs` around line 311.) The image tag is pinned by `test_containers::postgres_tag()`.

---

## 6. Unique-violation detection and error mapping

### 6.1 Helpers (`toolkit_db::secure`)

```rust
pub fn is_unique_violation(err: &sea_orm::DbErr) -> bool;       // SQLSTATE 23505 / SQLite 2067 / MySQL 1062 + message fallback
pub fn is_foreign_key_violation(err: &sea_orm::DbErr) -> bool;
impl ScopeError { pub fn is_unique_violation(&self) -> bool; pub fn is_foreign_key_violation(&self) -> bool; }
// toolkit_db::contention
pub fn is_retryable_contention(backend: sea_orm::DbBackend, err: &sea_orm::DbErr) -> bool; // 40001/40P01, SQLITE_BUSY(5)/517
```

`ScopeError` (`#[non_exhaustive]`) has these variants:
- `Db(sea_orm::DbErr)`
- `Invalid(&'static str)`
- `TenantNotInScope { tenant_id }`
- `Denied(&'static str)`
- `UnresolvedScopeProperty{..}`
- `GraphSyntax(String)`

`DbError` (crate root, `#[derive(thiserror::Error)]`) includes `Sea(#[from] DbErr)`, `Sqlx(..)`, `Other(#[from] anyhow::Error)`, `ConnRequestedInsideTx`, `Lock(..)`, and config variants.

Mapping a running-turn partial-unique-index race to 409:

```rust
match secure_insert::<chat_turns::Entity>(am, &scope, tx).await {
    Err(e) if e.is_unique_violation() => Err(DomainError::GenerationInProgress),   // → 409 aborted
    Err(e) => Err(map_scope_err(e)),
    Ok(m) => Ok(m),
}
```

Caution: inside a **PG** transaction, a failed INSERT aborts the transaction, so later statements fail with `25P02`. Return the error and let the tx roll back, or use `ON CONFLICT DO NOTHING` via `SecureOnConflict` / `on_conflict_raw` and check the result instead. SQLite does not have this problem.

### 6.2 Mapping to canonical errors (credstore pattern)

- `toolkit_canonical_errors` has `impl From<sea_orm::DbErr> for CanonicalError` behind feature `sea-orm`. It maps **everything to Internal**, so classify first.
- Credstore classifies in three layers:
  - `gears/credstore/credstore/src/infra/error_conv.rs`: `is_unique_violation`, `is_check_violation`, and `is_serialization_failure`, which uses `is_retryable_contention` for both PG and SQLite.
  - `gears/credstore/credstore/src/infra/canonical_mapping.rs`: `classify_db_err_to_domain(DbErr) -> DomainError`. Unique becomes `Conflict`, serialization becomes `ServiceUnavailable`, anything else becomes `Internal`. It also has `impl From<DbError> for DomainError`, which is required by `DBProvider<DomainError>`.
  - `repo_impl/helpers.rs::map_scope_err`: `Db` → classify, `TenantNotInScope` → `AccessDenied`, `Invalid`/`Denied`/other → `Internal`.
- `gears/credstore/credstore/src/infra/sdk_error_mapping.rs` converts `DomainError` to `CanonicalError` with a resource marker:

```rust
use toolkit_canonical_errors::{CanonicalError, resource_error};
#[resource_error(gts_id!("cf.core.credstore.secret.v1~"))]
pub(crate) struct SecretResource;
// DomainError::Conflict        => SecretResource::already_exists("secret already exists").with_resource("secret").create()
// DomainError::VersionConflict => SecretResource::aborted("...").with_reason("OPTIMISTIC_LOCK_FAILURE").create()   // 409
// DomainError::AccessDenied    => SecretResource::permission_denied().with_reason("ACCESS_DENIED").create()
// DomainError::NotFound        => SecretResource::not_found("secret not found").with_resource("secret").create()
// ServiceUnavailable           => CanonicalError::service_unavailable().with_detail(d).with_retry_after_seconds(s).create()
// Internal                     => CanonicalError::internal(diagnostic).create()
```

For mini-chat, `GENERATION_IN_PROGRESS` on a duplicate running turn should become `XResource::aborted(..).with_reason("GENERATION_IN_PROGRESS")`, matching DESIGN §"Atomicity invariant".

---

## 7. Gotchas checklist

1. `.scope_with(&scope)` on select, update and delete is infallible. `and_id()`, insert `scope_with_model` and `scope_unchecked` return `Result`.
2. Deny-all is the default. An `owner_id` constraint on a `no_owner` entity makes that constraint deny. Use `tenant_only()` for children, but only after authorizing the parent.
3. Inside a transaction closure use only `tx`. `db.conn()` there returns `ConnRequestedInsideTx`.
4. There is no raw SQL for gear runtime code. Migrations get `manager.get_connection().execute_unprepared(..)`.
5. `lock_exclusive()` gives `FOR UPDATE` on PG and nothing on SQLite. Prefer CAS updates with `rows_affected`.
6. SQLite UUIDs are 16-byte blobs whatever the declared type. Use the `x'..'` literal form in raw DDL defaults.
7. Outbox: the prefix must be `[A-Za-z][A-Za-z0-9_]{0,35}` and identical in migrations and builder. Partitions are a power of two ≤ 64 and persisted. Payloads are ≤ 64 KiB. Partition numbers are explicit `u32` values, so hash your key. Fire the `Wake` after commit. `attempts` is per partition, so use `batch_size(1)` for per-message budgets. Leased delivery is at-least-once, so make handlers idempotent.
8. Tests: use `sqlite::memory:` with `max_conns = 1`, or `file:<unique>?mode=memory&cache=shared` for multiple connections or the outbox. Use `run_migrations_for_testing(&db, migrations)`.

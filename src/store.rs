//! The `jinja_templates` table: loading stored templates and invalidating
//! every backend's cache of them when the table changes.
//!
//! Invalidation piggybacks on Postgres' relcache invalidation. A statement
//! trigger on the table queues a relcache invalidation for it. Postgres
//! delivers that to the current backend at the next command and to all other
//! backends when the transaction commits. On rollback it is replayed locally,
//! so a backend that read uncommitted template edits throws them away again.
//! Each backend's callback bumps a generation counter that the stored-template
//! engines check before rendering. Renders therefore never poll the table for
//! freshness: the only queries are the ones that load templates.

use std::cell::{Cell, RefCell};

use minijinja::{Error, ErrorKind};
use pgrx::prelude::*;

extension_sql!(
    r#"
CREATE TABLE jinja_templates (
    name   text PRIMARY KEY,
    source text NOT NULL CONSTRAINT jinja_templates_source_valid CHECK (jinja_is_valid(source))
);
COMMENT ON TABLE jinja_templates IS
    'Named Jinja2 templates for jinja_render_named(); they can include, extend and import each other by name.';

-- Templates are user data: have pg_dump include the rows.
SELECT pg_catalog.pg_extension_config_dump('jinja_templates', '');

CREATE TRIGGER jinja_templates_invalidate
    AFTER INSERT OR UPDATE OR DELETE OR TRUNCATE ON jinja_templates
    FOR EACH STATEMENT EXECUTE FUNCTION jinja_templates_invalidate();
-- Also fire when changes arrive through logical replication (which runs with
-- session_replication_role = replica); otherwise subscribers would keep
-- rendering stale templates.
ALTER TABLE jinja_templates ENABLE ALWAYS TRIGGER jinja_templates_invalidate;
"#,
    name = "jinja_templates_table",
    requires = [jinja_is_valid, jinja_templates_invalidate],
);

/// Statement trigger that asks every backend to drop its cached templates.
#[pg_trigger]
fn jinja_templates_invalidate<'a>(
    trigger: &'a PgTrigger<'a>,
) -> Result<Option<PgHeapTuple<'a, AllocatedByPostgres>>, PgTriggerError> {
    let relid = trigger.relid()?;
    // SAFETY: plain call into Postgres' invalidation queue with a valid OID.
    unsafe { pg_sys::CacheInvalidateRelcacheByRelid(relid) };
    Ok(None)
}

thread_local! {
    /// Bumped whenever this backend learns that `jinja_templates` changed.
    static GENERATION: Cell<u64> = const { Cell::new(0) };
    /// OID of `jinja_templates`, once resolved. The invalidation callback uses
    /// it to ignore the relcache traffic of unrelated tables.
    static TABLE_OID: Cell<pg_sys::Oid> = const { Cell::new(pg_sys::InvalidOid) };
    /// Schema-qualified, quoted table name, resolved under `TABLE_GENERATION`.
    static TABLE_NAME: RefCell<Option<String>> = const { RefCell::new(None) };
    static TABLE_GENERATION: Cell<u64> = const { Cell::new(0) };
}

pub fn generation() -> u64 {
    GENERATION.get()
}

/// Registers the relcache callback. Must be called from `_PG_init`.
pub fn register_invalidation_callback() {
    // SAFETY: called once at library load; the callback is a static function.
    unsafe {
        pg_sys::CacheRegisterRelcacheCallback(Some(on_relcache_invalidate), pg_sys::Datum::from(0))
    };
}

/// Runs inside Postgres' invalidation processing. That can happen mid-render,
/// for example while the loader's own query takes a lock, when the engines
/// are already borrowed. So this must not touch them, and must not raise an
/// error. It only bumps a counter the engines check before their next render.
unsafe extern "C-unwind" fn on_relcache_invalidate(_arg: pg_sys::Datum, relid: pg_sys::Oid) {
    // InvalidOid means "all relations", sent after a cache reset such as an
    // invalidation queue overflow.
    if relid == pg_sys::InvalidOid || relid == TABLE_OID.get() {
        GENERATION.set(GENERATION.get().wrapping_add(1));
    }
}

/// MiniJinja loader for the stored-template environments.
///
/// Runs with the caller's privileges, so rendering stored templates requires
/// SELECT on `jinja_templates`.
pub fn load(name: &str) -> Result<Option<String>, Error> {
    let table = table_name()?;
    let query = format!("SELECT source FROM {table} WHERE name = $1");
    select_first(&query, &[name.into()], |row| row.get::<String>(1))
        .map(Option::flatten)
        .map_err(|e| {
            Error::new(
                ErrorKind::InvalidOperation,
                format!("could not load template {name:?}: {e}"),
            )
        })
}

/// Resolves the table through the extension's schema rather than the
/// search_path, so a same-named table elsewhere can never be picked up.
/// Re-resolved after any invalidation, which also covers the extension
/// being dropped and recreated.
fn table_name() -> Result<String, Error> {
    let generation = generation();
    if TABLE_GENERATION.get() == generation {
        if let Some(name) = TABLE_NAME.with_borrow(|n| n.clone()) {
            return Ok(name);
        }
    }
    let row = select_first(
        "SELECT c.oid, format('%I.%I', n.nspname, c.relname)
           FROM pg_catalog.pg_extension e
           JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace
           JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relname = 'jinja_templates'
          WHERE e.extname = 'pgjinja'",
        &[],
        |row| Ok((row.get::<pg_sys::Oid>(1)?, row.get::<String>(2)?)),
    )
    .map_err(|e| {
        Error::new(
            ErrorKind::InvalidOperation,
            format!("could not locate jinja_templates: {e}"),
        )
    })?;
    let Some((Some(oid), Some(name))) = row else {
        return Err(Error::new(
            ErrorKind::InvalidOperation,
            "could not locate jinja_templates: is the pgjinja extension installed?",
        ));
    };
    TABLE_OID.set(oid);
    TABLE_NAME.set(Some(name.clone()));
    TABLE_GENERATION.set(generation);
    Ok(name)
}

/// Runs a read-only query and maps its first row, if any.
///
/// Read-only matters: these queries run inside renders, which may be part of
/// a parallel query, and read-write SPI would try to advance the command
/// counter there and fail. pgrx's `Spi::get_*` helpers all go through the
/// read-write path, hence this wrapper.
fn select_first<T>(
    query: &str,
    args: &[pgrx::datum::DatumWithOid],
    map: impl FnOnce(pgrx::spi::SpiHeapTupleData) -> pgrx::spi::SpiResult<T>,
) -> pgrx::spi::SpiResult<Option<T>> {
    Spi::connect(|client| {
        let mut rows = client.select(query, Some(1), args)?;
        rows.next().map(map).transpose()
    })
}

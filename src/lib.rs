//! pgjinja: Jinja2 templates in Postgres, rendered by MiniJinja.

use pgrx::guc::{GucContext, GucFlags, GucRegistry, GucSetting};
use pgrx::prelude::*;

mod engine;
mod functions;
#[cfg(any(test, feature = "pg_test"))]
mod pg_tests;
mod store;

::pgrx::pg_module_magic!(name, version);

/// Maximum fuel (roughly, VM instructions) a single render may consume.
/// Guards against templates such as `{% for i in range(10**9) %}` that would
/// otherwise pin a backend: a render never returns to Postgres to check for
/// interrupts, so statement_timeout and pg_cancel_backend cannot stop it.
/// 0 disables the limit.
static FUEL: GucSetting<i32> = GucSetting::<i32>::new(10_000_000);

#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    GucRegistry::define_int_guc(
        c"pgjinja.fuel",
        c"Maximum fuel a single template render may consume (0 = unlimited).",
        c"Each executed template instruction consumes fuel; a render that runs out is aborted with an error.",
        &FUEL,
        0,
        i32::MAX,
        GucContext::Userset,
        GucFlags::default(),
    );
    store::register_invalidation_callback();
}

fn fuel() -> Option<u64> {
    match FUEL.get() {
        0 => None,
        n => Some(n as u64),
    }
}

/// Required by `cargo pgrx test`; must be visible at the crate root.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}

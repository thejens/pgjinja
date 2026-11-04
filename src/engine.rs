//! Per-backend MiniJinja environments and their template caches.
//!
//! Postgres backends are single-threaded processes, so `thread_local!` gives
//! each connection its own environments without any locking. Compiled
//! templates live as long as the connection, so a template is parsed once per
//! backend rather than once per row.
//!
//! Inline templates (passed as SQL arguments) and stored templates (rows of
//! `jinja_templates`) render in separate environments. The inline environment
//! has no loader, so `{% include %}` and friends cannot reach the table. That
//! is what keeps `jinja_render` a pure function and lets it be IMMUTABLE.

use std::cell::RefCell;
use std::num::NonZeroUsize;

use lru::LruCache;
use minijinja::value::Kwargs;
use minijinja::{Environment, Error, State, UndefinedBehavior, Value};

use crate::store;

/// Upper bound on distinct inline template sources kept compiled per backend.
/// Hitting it only costs a re-parse; it exists so that workloads generating a
/// unique template per row cannot grow backend memory without bound.
const INLINE_CACHE_CAPACITY: usize = 512;

/// Rendering options shared by all entry points.
#[derive(Clone, Copy)]
pub struct Options {
    /// Treat references to undefined variables as errors rather than empty
    /// strings (Jinja2's default).
    pub strict: bool,
    /// Instruction budget for one render; `None` means unlimited.
    pub fuel: Option<u64>,
}

/// Cache key for inline templates. The autoescape mode is part of the key
/// because MiniJinja decides escaping when a template is compiled, so the
/// same source must compile separately for each mode.
#[derive(Hash, PartialEq, Eq)]
struct InlineKey {
    source: String,
    autoescape: bool,
}

struct InlineEngine {
    env: Environment<'static>,
    /// Maps template source to the name it is registered under in `env`.
    /// Keyed by the full source rather than a hash, so a hash collision can
    /// never render the wrong template.
    cache: LruCache<InlineKey, String>,
    next_id: u64,
}

impl InlineEngine {
    fn new(strict: bool) -> Self {
        Self {
            env: base_environment(strict),
            cache: LruCache::new(NonZeroUsize::new(INLINE_CACHE_CAPACITY).expect("non-zero")),
            next_id: 0,
        }
    }

    fn template_name(&mut self, source: &str, autoescape: bool) -> Result<String, Error> {
        let key = InlineKey {
            source: source.to_owned(),
            autoescape,
        };
        if let Some(name) = self.cache.get(&key) {
            return Ok(name.clone());
        }
        self.next_id += 1;
        // The `.html` suffix is what turns on HTML escaping: MiniJinja's
        // default autoescape callback selects the escaping mode from the
        // template name's extension.
        let name = if autoescape {
            format!("inline#{}.html", self.next_id)
        } else {
            format!("inline#{}", self.next_id)
        };
        // Compile before caching so a syntax error leaves no entry behind.
        self.env
            .add_template_owned(name.clone(), source.to_owned())?;
        if let Some((_, evicted)) = self.cache.push(key, name.clone()) {
            self.env.remove_template(&evicted);
        }
        Ok(name)
    }

    fn render(
        &mut self,
        source: &str,
        autoescape: bool,
        ctx: Value,
        opts: Options,
    ) -> Result<String, Error> {
        let name = self.template_name(source, autoescape)?;
        // Fuel comes from a GUC that can change between calls, so it is set
        // per render. It only aborts runaway templates and never changes the
        // output of a render that completes.
        self.env.set_fuel(opts.fuel);
        self.env.get_template(&name)?.render(ctx)
    }
}

struct StoredEngine {
    env: Environment<'static>,
    /// The `store::generation()` the cached templates were loaded under.
    generation: u64,
}

impl StoredEngine {
    fn new(strict: bool) -> Self {
        let mut env = base_environment(strict);
        env.set_loader(store::load);
        Self {
            env,
            generation: store::generation(),
        }
    }

    fn render(&mut self, name: &str, ctx: Value, opts: Options) -> Result<String, Error> {
        // The generation moves whenever `jinja_templates` changes in any
        // backend, so dropping everything here makes the next lookup of each
        // template (and of whatever it includes) re-read the table.
        let current = store::generation();
        if current != self.generation {
            self.env.clear_templates();
            self.generation = current;
        }
        self.env.set_fuel(opts.fuel);
        self.env.get_template(name)?.render(ctx)
    }
}

fn base_environment(strict: bool) -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(if strict {
        UndefinedBehavior::Strict
    } else {
        UndefinedBehavior::Lenient
    });
    // Python-style methods (`name.upper()`, `d.items()`), which Jinja2 users
    // expect to work.
    env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
    register_contrib(&mut env);
    env
}

/// Registers the deterministic subset of minijinja-contrib.
///
/// `now()` and the `rand` helpers are left out on purpose: `jinja_render` is
/// declared IMMUTABLE so Postgres can use it in generated columns and indexes,
/// which is only sound if the same input always renders the same output.
fn register_contrib(env: &mut Environment<'static>) {
    use minijinja_contrib::{filters, globals};
    env.add_filter("pluralize", filters::pluralize);
    env.add_filter("filesizeformat", filters::filesizeformat);
    env.add_filter("truncate", truncate);
    env.add_filter("striptags", filters::striptags);
    env.add_filter("datetimeformat", filters::datetimeformat);
    env.add_filter("dateformat", filters::dateformat);
    env.add_filter("timeformat", filters::timeformat);
    env.add_function("cycler", globals::cycler);
    env.add_function("joiner", globals::joiner);
}

thread_local! {
    static INLINE_LENIENT: RefCell<InlineEngine> = RefCell::new(InlineEngine::new(false));
    static INLINE_STRICT: RefCell<InlineEngine> = RefCell::new(InlineEngine::new(true));
    static STORED_LENIENT: RefCell<StoredEngine> = RefCell::new(StoredEngine::new(false));
    static STORED_STRICT: RefCell<StoredEngine> = RefCell::new(StoredEngine::new(true));
}

/// Renders an inline template source.
pub fn render_inline(
    source: &str,
    autoescape: bool,
    ctx: Value,
    opts: Options,
) -> Result<String, Error> {
    let engine = if opts.strict {
        &INLINE_STRICT
    } else {
        &INLINE_LENIENT
    };
    engine.with_borrow_mut(|e| e.render(source, autoescape, ctx, opts))
}

/// Renders the template stored under `name` in `jinja_templates`.
pub fn render_stored(name: &str, ctx: Value, opts: Options) -> Result<String, Error> {
    let engine = if opts.strict {
        &STORED_STRICT
    } else {
        &STORED_LENIENT
    };
    engine.with_borrow_mut(|e| e.render(name, ctx, opts))
}

/// Parses `source` without rendering it and returns the syntax error, if any.
pub fn validate(source: &str) -> Result<(), Error> {
    // A throwaway environment, so that validating many one-off strings (for
    // example in a CHECK constraint during a bulk load) does not evict
    // templates that are actually being rendered.
    Environment::new().template_from_str(source).map(|_| ())
}

/// Jinja2's `truncate(s, length=255, killwords=False, end='...', leeway=None)`.
///
/// minijinja-contrib's version takes keyword arguments only, so the common
/// Jinja2 spelling `truncate(80)` would fail. This adapter folds positional
/// arguments into keywords and delegates.
fn truncate(
    state: &mut State,
    value: &Value,
    positional: minijinja::value::Rest<Value>,
    kwargs: Kwargs,
) -> Result<Value, Error> {
    const NAMES: [&str; 4] = ["length", "killwords", "end", "leeway"];
    if positional.len() > NAMES.len() {
        return Err(Error::from(minijinja::ErrorKind::TooManyArguments));
    }
    let mut merged: Vec<(String, Value)> = NAMES
        .iter()
        .zip(positional.iter())
        .map(|(name, v)| (name.to_string(), v.clone()))
        .collect();
    for name in kwargs.args() {
        if merged.iter().any(|(n, _)| n == name) {
            return Err(Error::new(
                minijinja::ErrorKind::InvalidOperation,
                format!("truncate() got multiple values for argument {name:?}"),
            ));
        }
        merged.push((name.to_string(), kwargs.get::<Value>(name)?));
    }
    kwargs.assert_all_used()?;
    minijinja_contrib::filters::truncate(state, value, merged.into_iter().collect())
}

//! The SQL-callable functions.

use pgrx::prelude::*;
use pgrx::JsonB;

use crate::engine::{self, Options};

/// Renders a Jinja2 template using a JSON object as context.
///
/// Row fields: `jinja_render(tpl, to_jsonb(t))`.
/// Query parameters: `jinja_render(tpl, jsonb_build_object('name', $1))`.
///
/// IMMUTABLE because rendering is a pure function of the arguments: the
/// inline environment has no loader (so it cannot read `jinja_templates`)
/// and registers no clock- or random-based helpers.
#[pg_extern(immutable, parallel_safe)]
fn jinja_render(
    template: Option<&str>,
    context: default!(Option<JsonB>, "'{}'::jsonb"),
    strict: default!(bool, false),
    autoescape: default!(bool, false),
) -> Option<String> {
    let template = template?;
    let ctx = context_value(context, "jinja_render");
    let opts = options(strict);
    match engine::render_inline(template, autoescape, ctx, opts) {
        Ok(s) => Some(s),
        Err(e) => raise("template render failed", &e),
    }
}

/// Renders the template stored under `name` in `jinja_templates`. It may
/// `{% include %}`, `{% extends %}` and `{% import %}` other stored templates.
/// Names ending in `.html`/`.htm`/`.xml` are HTML-escaped, and names ending
/// in `.json`/`.json5`/`.js`/`.yaml`/`.yml` are JSON-escaped, following Jinja2's
/// convention.
///
/// STABLE, not IMMUTABLE: the output depends on the table's contents.
#[pg_extern(stable, parallel_safe)]
fn jinja_render_named(
    name: Option<&str>,
    context: default!(Option<JsonB>, "'{}'::jsonb"),
    strict: default!(bool, false),
) -> Option<String> {
    let name = name?;
    let ctx = context_value(context, "jinja_render_named");
    match engine::render_stored(name, ctx, options(strict)) {
        Ok(s) => Some(s),
        Err(e) => raise(&format!("rendering template {name:?} failed"), &e),
    }
}

/// Returns true if `template` parses. Intended for CHECK constraints on
/// columns that store templates.
#[pg_extern(immutable, parallel_safe)]
pub fn jinja_is_valid(template: &str) -> bool {
    engine::validate(template).is_ok()
}

/// Returns the syntax error for `template`, or NULL if it parses.
#[pg_extern(immutable, parallel_safe)]
fn jinja_syntax_error(template: &str) -> Option<String> {
    engine::validate(template).err().map(|e| e.to_string())
}

fn options(strict: bool) -> Options {
    Options {
        strict,
        fuel: crate::fuel(),
    }
}

fn context_value(context: Option<JsonB>, function: &str) -> minijinja::Value {
    match context.map(|j| j.0) {
        None | Some(serde_json::Value::Null) => minijinja::Value::from(()),
        Some(v @ serde_json::Value::Object(_)) => {
            minijinja::Value::from(minijinja::value::Serde(v))
        }
        Some(other) => error!(
            "{function} context must be a JSON object, got {}",
            json_type(&other)
        ),
    }
}

fn raise(message: &str, err: &minijinja::Error) -> ! {
    // `display_debug_info` includes the offending source line with a caret,
    // which is the most useful thing to show someone debugging a template.
    let detail = format!("{}", err.display_debug_info());
    ereport!(
        PgLogLevel::ERROR,
        PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
        format!("{message}: {err}"),
        detail
    );
    unreachable!()
}

fn json_type(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

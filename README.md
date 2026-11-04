# pgjinja: Jinja2 templates in PostgreSQL

[![CI](https://github.com/thejens/pgjinja/actions/workflows/ci.yml/badge.svg)](https://github.com/thejens/pgjinja/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![PostgreSQL 14–18](https://img.shields.io/badge/PostgreSQL-14%E2%80%9318-336791?logo=postgresql&logoColor=white)

**pgjinja is a PostgreSQL extension for rendering Jinja2 templates directly in SQL.**
Generate emails, HTML, Markdown, JSON, prompts or reports from table rows and query parameters, without leaving the database.
It is written in Rust with [pgrx](https://github.com/pgcentralfoundation/pgrx) and uses [MiniJinja](https://github.com/mitsuhiko/minijinja), the Jinja2-compatible template engine by the creator of Jinja2.

Looking for SQLite? See [sqlite-jinja](https://github.com/thejens/sqlite-jinja), which has the same functions as a loadable SQLite extension.

```sql
SELECT jinja_render(
  'Hi {{ name }}, you have {{ n }} new {{ "message" if n == 1 else "messages" }}',
  jsonb_build_object('name', 'Ada', 'n', 3)
);
-- Hi Ada, you have 3 new messages
```

- **Jinja2 syntax you already know:** `{{ }}`, `{% for %}`, `{% if %}`, filters, macros, `{% extends %}`, `{% include %}`, and Python-style methods such as `name.title()`.
- **Render from row data or query parameters:** pass a `jsonb` context built with `to_jsonb(row)` or `jsonb_build_object(...)`.
- **Store templates in a table:** stored templates can extend, include and import each other. Edits reach every connection immediately and transactionally.
- **Fast:** templates are compiled once per connection and cached, at about 2–3 µs per rendered row.
- **Safe by design:** HTML/JSON autoescaping, a per-render instruction budget against runaway loops, and no file system or network access.
- **IMMUTABLE and PARALLEL SAFE**, so it can be used in generated columns, expression indexes and parallel queries.

## Contents

- [Installation](#installation)
- [Usage](#usage)
  - [Render a template with row data](#render-a-template-with-row-data)
  - [Render a template with query parameters](#render-a-template-with-query-parameters)
  - [Stored templates with inheritance and includes](#stored-templates-with-inheritance-and-includes)
  - [HTML and JSON autoescaping](#html-and-json-autoescaping)
  - [Validate templates with a CHECK constraint](#validate-templates-with-a-check-constraint)
  - [Generated columns](#generated-columns)
- [Function reference](#function-reference)
- [Configuration](#configuration)
- [Template language](#template-language)
- [How caching and invalidation work](#how-caching-and-invalidation-work)
- [Performance](#performance)
- [FAQ](#faq)
- [Development](#development)

## Installation

pgjinja supports PostgreSQL 14, 15, 16, 17 and 18 on Linux and macOS. It is built from source with [cargo-pgrx](https://github.com/pgcentralfoundation/pgrx):

```sh
# Prerequisites: a Rust toolchain (https://rustup.rs) and PostgreSQL with its development headers
cargo install cargo-pgrx --version 0.19.3 --locked
cargo pgrx init --pg16 $(which pg_config)   # use the flag that matches your major version

git clone https://github.com/thejens/pgjinja.git
cd pgjinja
cargo pgrx install --release --pg-config $(which pg_config)
```

Then, in each database:

```sql
CREATE EXTENSION pgjinja;
```

## Usage

### Render a template with row data

`to_jsonb(row)` turns a whole row into the template context, so every column is available as a variable:

```sql
CREATE TABLE orders (id int, customer text, total numeric, items text[]);
INSERT INTO orders VALUES (1, 'Ada Lovelace', 42.5, ARRAY['notebook', 'pen']);

SELECT jinja_render(
  'Order #{{ id }} for {{ customer }}: {{ items | length }} items, {{ "%.2f" | format(total) }} EUR',
  to_jsonb(o)
) FROM orders o;
-- Order #1 for Ada Lovelace: 2 items, 42.50 EUR
```

Templates can also come from a column, so each row can carry its own template:

```sql
SELECT jinja_render(n.template, to_jsonb(u))
FROM notifications n JOIN users u ON u.id = n.user_id;
```

### Render a template with query parameters

Build the context from bind parameters with `jsonb_build_object`. This works with prepared statements and any client driver:

```sql
PREPARE welcome(text, text[]) AS
  SELECT jinja_render(
    E'Welcome {{ name }}!\n{% for f in features %}- {{ f | upper }}\n{% endfor %}',
    jsonb_build_object('name', $1, 'features', $2)
  );

EXECUTE welcome('Grace', ARRAY['search', 'export']);
-- Welcome Grace!
-- - SEARCH
-- - EXPORT
```

Nested JSON works as you would expect: `{{ user.address.city }}`, `{% for k, v in settings.items() %}`.

### Stored templates with inheritance and includes

Templates saved in the `jinja_templates` table are rendered by name with `jinja_render_named`. They can `{% extends %}`, `{% include %}` and `{% import %}` each other, exactly like files in a Jinja2 template directory:

```sql
INSERT INTO jinja_templates (name, source) VALUES
  ('layout.html', '<html><body><h1>{% block title %}{% endblock %}</h1>{% block body %}{% endblock %}{% include "footer.html" %}</body></html>'),
  ('footer.html', '<footer>{{ company }}</footer>'),
  ('macros.html', '{% macro badge(text) %}<span class="badge">{{ text }}</span>{% endmacro %}'),
  ('invoice.html', '{% extends "layout.html" %}
{% from "macros.html" import badge %}
{% block title %}Invoice {{ number }}{% endblock %}
{% block body %}{{ badge(status) }}{% endblock %}');

SELECT jinja_render_named('invoice.html',
  jsonb_build_object('number', 'INV-7', 'status', 'paid', 'company', 'Acme & Co'));
```

Changes to `jinja_templates` reach every session immediately, including partials pulled in with `include` or `extends`. See [How caching and invalidation work](#how-caching-and-invalidation-work).

The table is dumped by `pg_dump` along with your data.

### HTML and JSON autoescaping

Inline templates are not escaped by default. Turn escaping on with `autoescape => true`:

```sql
SELECT jinja_render('<p>{{ comment }}</p>', jsonb_build_object('comment', '<script>alert(1)</script>'),
                    autoescape => true);
-- <p>&lt;script&gt;alert(1)&lt;&#x2f;script&gt;</p>
```

Stored templates follow the Jinja2 convention of choosing the escaping mode from the template name:

| Name ends in | Escaping |
|---|---|
| `.html`, `.htm`, `.xml` | HTML |
| `.json`, `.json5`, `.js`, `.yaml`, `.yml` | JSON (values are serialized as JSON) |
| anything else | none |

Use `{{ value | safe }}` to opt out for trusted markup.

### Validate templates with a CHECK constraint

```sql
CREATE TABLE email_templates (
  name text PRIMARY KEY,
  body text NOT NULL CHECK (jinja_is_valid(body))
);

SELECT jinja_syntax_error('{% for x in %}');
-- syntax error: unexpected end of block (in <string>:1)
```

`jinja_templates` already has this constraint.

### Generated columns

`jinja_render` is `IMMUTABLE`, so it can be used in generated columns and expression indexes. Note that `to_jsonb` and `jsonb_build_object` are only `STABLE` in PostgreSQL. Build the context from a `jsonb` column or with `jsonb_object(text[])` instead:

```sql
CREATE TABLE contacts (
  data    jsonb,
  display text GENERATED ALWAYS AS (jinja_render('{{ last | upper }}, {{ first }}', data)) STORED
);
```

## Function reference

| Function | Volatility | Description |
|---|---|---|
| `jinja_render(template text, context jsonb DEFAULT '{}', strict bool DEFAULT false, autoescape bool DEFAULT false) → text` | IMMUTABLE | Renders an inline template. |
| `jinja_render_named(name text, context jsonb DEFAULT '{}', strict bool DEFAULT false) → text` | STABLE | Renders a template from `jinja_templates`, resolving `extends`, `include` and `import` from the same table. |
| `jinja_is_valid(template text) → bool` | IMMUTABLE | Whether the template parses. |
| `jinja_syntax_error(template text) → text` | IMMUTABLE | The parse error, or `NULL` if the template is valid. |

All functions are `PARALLEL SAFE`. Arguments:

- `context` must be a JSON object, or `NULL` for an empty context.
- A `NULL` template or name returns `NULL`.
- With `strict => true`, referencing an undefined variable raises an error instead of rendering an empty string (Jinja2's `StrictUndefined`).

Errors carry the template line and a caret pointing at the problem in the `DETAIL` field:

```
ERROR:  template render failed: syntax error: unexpected `}`, expected end of variable block (in inline#3:1)
DETAIL:
   1 > {{ x }
     i      ^ syntax error
```

## Configuration

| Setting | Default | Description |
|---|---|---|
| `pgjinja.fuel` | `10000000` | Instruction budget for a single render; `0` disables it. A render that exceeds it fails with `engine ran out of fuel`. Settable per session or transaction. |

The fuel limit exists because a render doesn't return control to PostgreSQL until it finishes. `statement_timeout` and `pg_cancel_backend()` can't stop a template like `{% for i in range(10**9) %}`, but fuel can.

## Template language

pgjinja uses MiniJinja, which implements nearly all of Jinja2:
- variables, filters and tests
- `if`, `for` (including `loop.*`, `break`, `continue`) and `set`
- macros, `call` blocks, and template inheritance with `extends`, `block`, `include`, `import` and `from`
- whitespace control and expressions

See the [MiniJinja syntax documentation](https://docs.rs/minijinja/latest/minijinja/syntax/index.html) and its [list of differences from Jinja2](https://github.com/mitsuhiko/minijinja/blob/main/COMPATIBILITY.md).

Available on top of the built-in filters:
- Python-style methods on strings, lists and dicts (`.upper()`, `.split()`, `.items()`, `.get()`, …)
- `tojson`, `urlencode`, `pluralize`, `truncate` (Jinja2-compatible positional arguments), `striptags`, `filesizeformat`
- `datetimeformat`, `dateformat` and `timeformat`
- `cycler([...])` and `joiner()`

Deliberately not available, to keep rendering deterministic: `now()` and random-number helpers. Pass `now()` from SQL in the context if you need it.

**JSON numbers and precision:** `jsonb` numbers become 64-bit integers or floats inside templates. Cast large or exact `numeric` values to `text` before passing them in if every digit matters.

## How caching and invalidation work

- **Inline templates** are compiled on first use and kept in a per-connection LRU cache of 512 entries. The cache is keyed by the full template source, so a template is parsed once per connection rather than once per row.
- **Stored templates** are loaded on demand with a read-only query and cached per connection. Rendering a cached template runs no SQL at all.

When `jinja_templates` changes, a statement-level trigger queues a PostgreSQL **relcache invalidation** for the table. PostgreSQL then delivers it:
- to your own session at the next command, so you see your own uncommitted edits;
- to every other session when the transaction commits;
- not at all if the transaction rolls back. The local cache is reset again, so an aborted edit is never served.

The trigger is `ENABLE ALWAYS`, so changes applied by logical replication invalidate caches on subscribers too. Hot standbys replay invalidations from WAL.

`jinja_render` cannot see `jinja_templates`, so `{% include %}` in an inline template fails with "template not found". That separation is what makes `jinja_render` truly `IMMUTABLE`. Use `jinja_render_named` for templates with dependencies.

## Performance

Measured on an Apple M-series laptop with PostgreSQL 16, release build, 200,000 rows, one worker:

| Query | Time | Per row |
|---|---|---|
| `format('<p>#%s %s</p>', id, customer)` baseline | 41 ms | 0.2 µs |
| `jinja_render(...)` with a 2-key `jsonb_build_object` | 315 ms | 1.6 µs |
| `jinja_render_named(...)` with `extends` and `block` | 362 ms | 1.8 µs |
| `jinja_render(...)` with `to_jsonb(row)` (4 columns incl. an array) | 626 ms | 3.1 µs |

Building `jsonb` is a large part of the cost. Passing only the fields a template needs with `jsonb_build_object` is the cheapest option. Parallel query spreads rendering across workers.

## FAQ

### Is there a SQLite version?

Yes: [sqlite-jinja](https://github.com/thejens/sqlite-jinja), with the same functions and template behaviour. It is a loadable extension for the `sqlite3` CLI, Python, Node.js, Datasette and Rust.

### How do I render a Jinja2 template in PostgreSQL?

Install pgjinja and call `jinja_render(template, context)`. Pass row data with `to_jsonb(row)` or query parameters with `jsonb_build_object(...)`.

### Why not PL/Python with the jinja2 package?

`plpython3u` is an *untrusted* language. Many managed providers don't offer it, it needs Python and Jinja2 installed on the database server, and functions written in it can access the file system. pgjinja is a self-contained native extension with no I/O and per-connection template caching.

### Can I use it on Amazon RDS, Google Cloud SQL, Azure, Supabase or Neon?

Only where the provider lets you install custom native extensions. pgjinja is a compiled C-ABI extension like PostGIS or pgvector, so it needs the provider to ship it, or a self-managed PostgreSQL.

### Is it safe to render user-supplied templates?

Templates can't touch the file system, the network or other tables. `jinja_render_named` reads only `jinja_templates`, using the caller's privileges. The fuel limit bounds CPU per render. Fuel does not bound memory: a single expression such as `'x' * 10**9` can still allocate a lot. Treat untrusted templates with the same care as untrusted SQL expressions. Enable autoescaping whenever the output is HTML.

### Does it work with Jinja2 templates from my Python or Ansible project?

Mostly, yes. MiniJinja covers the Jinja2 language. Python-specific extensions, custom filters and Ansible-specific filters are not available.

## Development

```sh
cargo pgrx test pg16      # unit tests inside a temporary PostgreSQL
cargo pgrx regress pg16   # pg_regress tests (tests/pg_regress)
cargo pgrx run pg16       # psql session with the extension installed
```

Code layout:

| File | Responsibility |
|---|---|
| `src/functions.rs` | SQL-callable functions |
| `src/engine.rs` | MiniJinja environments and template caches |
| `src/store.rs` | `jinja_templates`, the loader and cache invalidation |
| `src/pg_tests.rs` | In-database tests |

## License

MIT. See [LICENSE](LICENSE).

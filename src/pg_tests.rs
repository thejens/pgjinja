//! In-database tests, run with `cargo pgrx test`.

use pgrx::prelude::*;

#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    fn q(sql: &str) -> Option<String> {
        Spi::get_one::<String>(sql).expect("query failed")
    }

    fn run(sql: &str) {
        Spi::run(sql).expect("statement failed");
    }

    /// Runs `sql` in a subtransaction and returns its error message, or None
    /// if it succeeded. Lets a test assert on several errors without
    /// aborting its own transaction.
    fn err(sql: &str) -> Option<String> {
        run(
            "CREATE FUNCTION pg_temp.err(q text) RETURNS text LANGUAGE plpgsql AS $$
             BEGIN EXECUTE q; RETURN NULL;
             EXCEPTION WHEN others THEN RETURN SQLERRM; END $$",
        );
        let literal = sql.replace('\'', "''");
        let msg = q(&format!("SELECT pg_temp.err('{literal}')"));
        run("DROP FUNCTION pg_temp.err(text)");
        msg
    }

    fn assert_err(sql: &str, needle: &str) {
        match err(sql) {
            Some(msg) => assert!(msg.contains(needle), "error {msg:?} lacks {needle:?}"),
            None => panic!("expected an error containing {needle:?} from: {sql}"),
        }
    }

    // ----- inline templates -----

    #[pg_test]
    fn renders_query_parameters() {
        assert_eq!(
            q("SELECT jinja_render('Hello {{ name | upper }}!', jsonb_build_object('name', 'world'))"),
            Some("Hello WORLD!".into())
        );
    }

    #[pg_test]
    fn renders_row_fields() {
        run("CREATE TABLE users (id int, name text, tags text[]);
             INSERT INTO users VALUES (1, 'Ada', ARRAY['math','poetry']);");
        assert_eq!(
            q("SELECT jinja_render('#{{ id }} {{ name }}: {{ tags | join(\", \") }}', to_jsonb(u)) FROM users u"),
            Some("#1 Ada: math, poetry".into())
        );
    }

    #[pg_test]
    fn template_from_a_column() {
        run("CREATE TABLE tpl (body text CHECK (jinja_is_valid(body)));
             INSERT INTO tpl VALUES ('{% for x in xs %}{{ loop.index }}={{ x }}{% if not loop.last %},{% endif %}{% endfor %}');");
        assert_eq!(
            q("SELECT jinja_render(body, '{\"xs\": [\"a\", \"b\"]}') FROM tpl"),
            Some("1=a,2=b".into())
        );
        assert_err(
            "INSERT INTO tpl VALUES ('{% if %}')",
            "violates check constraint",
        );
    }

    #[pg_test]
    fn lenient_by_default_strict_on_request() {
        assert_eq!(
            q("SELECT jinja_render('[{{ missing }}]')"),
            Some("[]".into())
        );
        assert_err(
            "SELECT jinja_render('{{ missing }}', '{}', strict => true)",
            "undefined value",
        );
    }

    #[pg_test]
    fn null_handling() {
        assert_eq!(q("SELECT jinja_render(NULL, '{}')"), None);
        assert_eq!(q("SELECT jinja_render('ok', NULL)"), Some("ok".into()));
    }

    #[pg_test]
    fn pycompat_methods() {
        assert_eq!(
            q("SELECT jinja_render('{{ s.title() }}', '{\"s\": \"hello there\"}')"),
            Some("Hello There".into())
        );
    }

    #[pg_test]
    fn truncate_accepts_jinja2_positional_arguments() {
        let t = |tpl: &str| q(&format!("SELECT jinja_render($t${tpl}$t$)"));
        assert_eq!(
            t(r#"{{ "hello wonderful world" | truncate(9) }}"#),
            Some("hello...".into())
        );
        assert_eq!(
            t(r#"{{ "hello wonderful world" | truncate(9, true, "~", 0) }}"#),
            Some("hello wo~".into())
        );
        assert_eq!(
            t(r#"{{ "hello wonderful world" | truncate(length=9, end="!") }}"#),
            Some("hello!".into())
        );
        assert_err(
            r#"SELECT jinja_render('{{ "x" | truncate(5, length=3) }}')"#,
            "multiple values for argument",
        );
        assert_err(
            r#"SELECT jinja_render('{{ "x" | truncate(5, true, "~", 0, 1) }}')"#,
            "too many arguments",
        );
    }

    #[pg_test]
    fn advertised_helpers_are_available() {
        assert_eq!(
            q(
                r#"SELECT jinja_render($t${% for i in range(9) %}{% if i == 3 %}{% break %}{% endif %}{{ i }}{% endfor %}|{{ {"a": [1]} | tojson }}|{{ "a b&c" | urlencode }}|{{ 2 | pluralize }}|{{ 2048 | filesizeformat }}|{{ "<b>x</b>" | striptags }}|{{ "2026-11-04" | dateformat }}$t$)"#
            ),
            Some(r#"012|{"a": [1]}|a%20b%26c|s|2.0 kB|x|Nov 4 2026"#.into())
        );
        assert_eq!(
            q(
                r#"SELECT jinja_render($t${% set c = cycler(["odd", "even"]) %}{{ c.next() }}{{ c.next() }}{% set j = joiner(",") %}{{ j() }}a{{ j() }}b$t$)"#
            ),
            Some("oddevena,b".into())
        );
    }

    #[pg_test]
    fn syntax_error_reporting() {
        assert_eq!(
            Spi::get_one::<bool>("SELECT jinja_is_valid('{{ oops')"),
            Ok(Some(false))
        );
        assert!(q("SELECT jinja_syntax_error('{% if %}')").is_some());
        assert_eq!(q("SELECT jinja_syntax_error('{{ fine }}')"), None);
        assert_err("SELECT jinja_render('{{ x }')", "syntax error");
    }

    #[pg_test]
    fn fuel_limits_runaway_templates() {
        run("SET LOCAL pgjinja.fuel = 1000");
        assert_err(
            "SELECT jinja_render('{% for i in range(100000) %}x{% endfor %}')",
            "ran out of fuel",
        );
    }

    #[pg_test]
    fn rejects_non_object_context() {
        assert_err(
            "SELECT jinja_render('x', '[1,2]')",
            "context must be a JSON object, got array",
        );
    }

    #[pg_test]
    fn autoescape_is_opt_in_and_cached_per_mode() {
        let ctx = r#"'{"s": "<b>&</b>"}'"#;
        // Same source in both modes: they must not share a compiled template.
        assert_eq!(
            q(&format!("SELECT jinja_render('{{{{ s }}}}', {ctx})")),
            Some("<b>&</b>".into())
        );
        assert_eq!(
            q(&format!(
                "SELECT jinja_render('{{{{ s }}}}', {ctx}, autoescape => true)"
            )),
            Some("&lt;b&gt;&amp;&lt;&#x2f;b&gt;".into())
        );
        assert_eq!(
            q(&format!(
                "SELECT jinja_render('{{{{ s | safe }}}}', {ctx}, autoescape => true)"
            )),
            Some("<b>&</b>".into())
        );
    }

    #[pg_test]
    fn inline_templates_cannot_reach_stored_ones() {
        run("INSERT INTO jinja_templates VALUES ('footer', 'bye')");
        // Keeping the table out of reach is what makes jinja_render IMMUTABLE.
        assert_err(
            "SELECT jinja_render('{% include \"footer\" %}')",
            "template not found",
        );
    }

    // ----- stored templates -----

    #[pg_test]
    fn stored_templates_extend_include_and_import() {
        run(r#"INSERT INTO jinja_templates VALUES
            ('layout.html', '<h1>{% block title %}{% endblock %}</h1>{% block body %}{% endblock %}{% include "footer.html" %}'),
            ('footer.html', '<footer>{{ company }}</footer>'),
            ('macros.html', '{% macro badge(t) %}<i>{{ t }}</i>{% endmacro %}'),
            ('page.html',   '{% extends "layout.html" %}{% from "macros.html" import badge %}{% block title %}{{ title }}{% endblock %}{% block body %}{{ badge(tag) }}{% endblock %}')"#);
        assert_eq!(
            q(
                r#"SELECT jinja_render_named('page.html', '{"title": "Hi <you>", "tag": "new", "company": "A&B"}')"#
            ),
            Some("<h1>Hi &lt;you&gt;</h1><i>new</i><footer>A&amp;B</footer>".into())
        );
    }

    #[pg_test]
    fn stored_template_escaping_follows_the_name() {
        run(r#"INSERT INTO jinja_templates VALUES
            ('plain', '{{ s }}'), ('doc.html', '{{ s }}'), ('data.json', '{"s": {{ s }}}')"#);
        let ctx = r#"'{"s": "<a \"q\">"}'"#;
        assert_eq!(
            q(&format!("SELECT jinja_render_named('plain', {ctx})")),
            Some("<a \"q\">".into())
        );
        assert_eq!(
            q(&format!("SELECT jinja_render_named('doc.html', {ctx})")),
            Some("&lt;a &quot;q&quot;&gt;".into())
        );
        assert_eq!(
            q(&format!("SELECT jinja_render_named('data.json', {ctx})")),
            Some(r#"{"s": "<a \"q\">"}"#.into())
        );
    }

    #[pg_test]
    fn edits_are_visible_to_the_next_command() {
        run("INSERT INTO jinja_templates VALUES ('greet', 'v1'), ('wrap', '[{% include \"greet\" %}]')");
        assert_eq!(q("SELECT jinja_render_named('wrap')"), Some("[v1]".into()));
        // Changing only the included template must invalidate the cached
        // parent too, since the parent caches nothing but its own source.
        run("UPDATE jinja_templates SET source = 'v2' WHERE name = 'greet'");
        assert_eq!(q("SELECT jinja_render_named('wrap')"), Some("[v2]".into()));
        run("DELETE FROM jinja_templates WHERE name = 'greet'");
        assert_err("SELECT jinja_render_named('wrap')", "template not found");
    }

    #[pg_test]
    fn rolled_back_edits_are_forgotten() {
        run("INSERT INTO jinja_templates VALUES ('t', 'committed')");
        assert_eq!(
            q("SELECT jinja_render_named('t')"),
            Some("committed".into())
        );
        // Read an uncommitted edit inside a subtransaction that then aborts.
        // Postgres replays the invalidation on abort, so the cached copy of
        // the edit must not survive.
        run("DO $$ BEGIN
               UPDATE jinja_templates SET source = 'uncommitted' WHERE name = 't';
               IF jinja_render_named('t') <> 'uncommitted' THEN RAISE 'edit not visible'; END IF;
               RAISE EXCEPTION 'roll back';
             EXCEPTION WHEN raise_exception THEN
               IF SQLERRM <> 'roll back' THEN RAISE; END IF;
             END $$");
        assert_eq!(
            q("SELECT jinja_render_named('t')"),
            Some("committed".into())
        );
    }

    #[pg_test]
    fn truncate_invalidates() {
        run("INSERT INTO jinja_templates VALUES ('t', 'x')");
        assert_eq!(q("SELECT jinja_render_named('t')"), Some("x".into()));
        run("TRUNCATE jinja_templates");
        assert_err("SELECT jinja_render_named('t')", "template not found");
    }

    #[pg_test]
    fn replicated_changes_invalidate() {
        run("INSERT INTO jinja_templates VALUES ('t', 'before')");
        assert_eq!(q("SELECT jinja_render_named('t')"), Some("before".into()));
        // Logical replication applies changes in replica mode, where ordinary
        // triggers stay silent.
        run("SET LOCAL session_replication_role = replica;
             UPDATE jinja_templates SET source = 'after' WHERE name = 't';");
        assert_eq!(q("SELECT jinja_render_named('t')"), Some("after".into()));
    }

    #[pg_test]
    fn stored_template_errors() {
        assert_err("SELECT jinja_render_named('nope')", "template not found");
        assert_err(
            "INSERT INTO jinja_templates VALUES ('bad', '{% for %}')",
            "jinja_templates_source_valid",
        );
        run("INSERT INTO jinja_templates VALUES ('strict', '{{ missing }}')");
        assert_eq!(q("SELECT jinja_render_named('strict')"), Some("".into()));
        assert_err(
            "SELECT jinja_render_named('strict', strict => true)",
            "undefined value",
        );
    }

    #[pg_test]
    fn ignores_same_named_table_on_search_path() {
        run("CREATE SCHEMA decoy;
             CREATE TABLE decoy.jinja_templates (name text, source text);
             INSERT INTO decoy.jinja_templates VALUES ('t', 'decoy');
             INSERT INTO jinja_templates VALUES ('t', 'real');
             SET LOCAL search_path = decoy, public;");
        assert_eq!(
            q("SELECT public.jinja_render_named('t')"),
            Some("real".into())
        );
    }
}

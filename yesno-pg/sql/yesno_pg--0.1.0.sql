/* yesno_pg 0.1.0 — extension installation script.
 *
 * Hand-written, not generated. pgrx normally produces this with `cargo pgrx
 * schema`, which builds a helper binary that *executes* the extension to dump
 * its entity graph — three moving parts we would have to reproduce under Bazel
 * to gain nothing here. `#[pg_extern] fn foo` exports the C symbol
 * `foo_wrapper` plus its `pg_finfo_foo_wrapper` record, which is the ordinary
 * PG_FUNCTION_INFO_V1 protocol, so binding to it by hand is exactly what every
 * C extension does.
 *
 * The symbol name in the second AS argument must stay `<fn>_wrapper`. Get it
 * wrong and `CREATE EXTENSION` succeeds while the first call fails with "could
 * not find function", because PostgreSQL resolves it lazily at call time.
 */

\echo Use "CREATE EXTENSION yesno_pg" to load this file. \quit

CREATE FUNCTION yesno_pg_version() RETURNS text
    AS 'MODULE_PATHNAME', 'yesno_pg_version_wrapper'
    LANGUAGE C STRICT IMMUTABLE PARALLEL SAFE;

COMMENT ON FUNCTION yesno_pg_version() IS
    'Version of the yesno_pg extension library actually loaded.';

/* The foreign data wrapper.
 *
 * `fdw_handler` and `fdw_validator` are pseudo-types with no Rust
 * equivalent, so both entry points are hand-written `#[no_mangle] extern "C"`
 * functions rather than `#[pg_extern]` — which is why the symbol names here have
 * no `_wrapper` suffix, unlike `yesno_pg_version` above.
 *
 * The validator is what makes a misspelled option an error at `CREATE SERVER`
 * time instead of a default that quietly takes effect on the first query.
 */

CREATE FUNCTION yesno_fdw_handler() RETURNS fdw_handler
    AS 'MODULE_PATHNAME', 'yesno_fdw_handler'
    LANGUAGE C STRICT;

CREATE FUNCTION yesno_fdw_validator(text[], oid) RETURNS void
    AS 'MODULE_PATHNAME', 'yesno_fdw_validator'
    LANGUAGE C STRICT;

CREATE FOREIGN DATA WRAPPER yesno_fdw
    HANDLER yesno_fdw_handler
    VALIDATOR yesno_fdw_validator;

COMMENT ON FOREIGN DATA WRAPPER yesno_fdw IS
    'Exposes one yesno key as a foreign table of a single bigint ordinal column.';

/* The index access method.
 *
 * `CREATE ACCESS METHOD` needs a handler returning `index_am_handler`, and
 * an operator class per indexable type. One strategy — equality — and no
 * support functions: the key is a hash of the value's text form, not a
 * per-type support procedure, which is why any type with an output function
 * works and why the opclasses below are near-identical.
 *
 * The key is namespaced by the index's OID, so two indexes never share a
 * posting list. See `src/iam/mod.rs`.
 */

CREATE FUNCTION yesno_iam_handler( internal ) RETURNS index_am_handler
    AS 'MODULE_PATHNAME', 'yesno_iam_handler'
    LANGUAGE C STRICT;

CREATE ACCESS METHOD yesno TYPE INDEX HANDLER yesno_iam_handler;

COMMENT ON ACCESS METHOD yesno IS
    'Inverted index over yesno posting lists. Bitmap scans only; exact selectivity.';

CREATE OPERATOR CLASS yesno_text_ops DEFAULT FOR TYPE text USING yesno AS
    OPERATOR 1 =;

CREATE OPERATOR CLASS yesno_int4_ops DEFAULT FOR TYPE int4 USING yesno AS
    OPERATOR 1 =;

CREATE OPERATOR CLASS yesno_int8_ops DEFAULT FOR TYPE int8 USING yesno AS
    OPERATOR 1 =;

/* The table access method.
 *
 * **Not a general heap.** A yesno table is a single-column `bigint` set, and
 * two restrictions follow from what yesno stores rather than from effort:
 *
 *   1. Visibility does not come from stored xids, so a yesno table and a heap
 *      table in one query can **tear**. See `tam-mvcc` in TODO.md. Isolation
 *      *within* yesno is honoured: REPEATABLE READ pins a version for the
 *      transaction; READ COMMITTED pins one for each statement and releases it
 *      before the next.
 *   2. The tuple *is* its TID, so the value domain caps near 2^42.
 *
 * UPDATE, SELECT … FOR UPDATE, ON CONFLICT, CLUSTER and TABLESAMPLE all
 * error with the reason rather than doing something approximate.
 */

CREATE FUNCTION yesno_tam_handler( internal ) RETURNS table_am_handler
    AS 'MODULE_PATHNAME', 'yesno_tam_handler'
    LANGUAGE C STRICT;

CREATE ACCESS METHOD yesno_table TYPE TABLE HANDLER yesno_tam_handler;

COMMENT ON ACCESS METHOD yesno_table IS
    'A single-column bigint set stored as a yesno posting list. Not MVCC-consistent with heap tables.';

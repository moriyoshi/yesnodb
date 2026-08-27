//! `IMPORT FOREIGN SCHEMA`: turning names into foreign tables.
//!
//! # Why a dictionary exists at all
//!
//! A yesno key is a `u64`. There is no server-side notion of what a key *means*,
//! and deliberately so — the key space is an address space, not a catalogue. So
//! the mapping from a human name to a key lives in an ordinary PostgreSQL table
//! that the user owns, named by the server's `dictionary` option. This callback
//! reads it and emits one `CREATE FOREIGN TABLE` per row.
//!
//! Until 2026-08-30 the option was **parsed and validated but never read**.
//! That is a worse failure than a missing option: from the outside an inert
//! option that validates is indistinguishable from a wired one, and the error
//! surfaces as "IMPORT FOREIGN SCHEMA found nothing" rather than as a rejection.
//!
//! # Without a dictionary
//!
//! Not an error. With no `dictionary` option the populated keys are
//! enumerated from the server and each becomes `k<key>`. That is the
//! dictionary-free fallback the key-enumeration work unlocked, and it is useful
//! precisely because it needs no catalogue the user has to maintain.
//!
//! # Keys are `bigint`, and that is the same trap as ordinals
//!
//! The dictionary's key column is a `bigint`, so a key at or above `2^63`
//! is stored **negative** — the identical bit reinterpretation the ordinal
//! column uses. Reading it as if it were unsigned, or rejecting a negative,
//! would make half the key space unnameable.

use pgrx::prelude::*;
use pgrx::spi::Spi;

use crate::options::Dictionary;
use crate::transport::Transport;

/// Build the `CREATE FOREIGN TABLE` statements for an `IMPORT FOREIGN SCHEMA`.
///
/// # Safety
///
/// Called by the DDL layer with valid pointers.
#[pg_guard]
pub unsafe extern "C-unwind" fn import_foreign_schema(
    stmt: *mut pg_sys::ImportForeignSchemaStmt,
    server_oid: pg_sys::Oid,
) -> *mut pg_sys::List {
    let opts = match unsafe { server_options(server_oid) } {
        Ok(o) => o,
        Err(e) => error!("yesno_fdw: {e}"),
    };

    let local_schema = unsafe { cstr((*stmt).local_schema) };
    let server_name = unsafe { cstr((*stmt).server_name) };

    // `LIMIT TO` / `EXCEPT` filtering, applied to the *local* table names.
    let listed: Vec<String> = unsafe { table_list_names((*stmt).table_list) };
    let list_type = unsafe { (*stmt).list_type };

    let entries = match &opts.dictionary {
        Some(d) => match read_dictionary(d) {
            Ok(v) => v,
            Err(e) => error!("yesno_fdw: {e}"),
        },
        None => match enumerate_keys(&opts) {
            Ok(v) => v,
            Err(e) => error!("yesno_fdw: {e}"),
        },
    };

    let mut out: *mut pg_sys::List = core::ptr::null_mut();
    for (name, key) in entries {
        let wanted = match list_type {
            pg_sys::ImportForeignSchemaType::FDW_IMPORT_SCHEMA_LIMIT_TO => {
                listed.iter().any(|n| n == &name)
            }
            pg_sys::ImportForeignSchemaType::FDW_IMPORT_SCHEMA_EXCEPT => {
                !listed.iter().any(|n| n == &name)
            }
            _ => true,
        };
        if !wanted {
            continue;
        }

        // Every identifier goes through `quote_identifier`, which is what
        // makes a dictionary row with a capital letter, a space, or a quote in
        // it produce a valid statement rather than a syntax error — and what
        // stops a crafted term from ending the statement early. The key needs
        // no quoting beyond the literal: it is rendered from a `u64`.
        let sql = format!(
            "CREATE FOREIGN TABLE {}.{} ( ordinal bigint ) SERVER {} OPTIONS ( key '{}' )",
            quote_ident(&local_schema),
            quote_ident(&name),
            quote_ident(&server_name),
            key
        );
        let cs = std::ffi::CString::new(sql).expect("no interior nul");
        unsafe {
            out = pg_sys::lappend(out, pg_sys::pstrdup(cs.as_ptr()) as *mut core::ffi::c_void);
        }
    }
    out
}

/// `( name, key )` pairs from the user's dictionary table.
fn read_dictionary(d: &Dictionary) -> Result<Vec<(String, u64)>, String> {
    // Identifiers are quoted here too. The relation may be schema-qualified,
    // so it is passed through as written rather than quoted as a single
    // identifier — quoting `public.terms` whole would look for a relation
    // literally named `public.terms`.
    let sql = format!(
        "SELECT {}::text, {}::bigint FROM {} ORDER BY 1",
        quote_ident(&d.term_column),
        quote_ident(&d.key_column),
        d.relation
    );

    Spi::connect(|client| {
        let table = client
            .select(sql.as_str(), None, &[])
            .map_err(|e| format!("reading dictionary {}: {e}", d.relation))?;
        let mut out = Vec::new();
        for row in table {
            let name: Option<String> = row.get(1).map_err(|e| e.to_string())?;
            let key: Option<i64> = row.get(2).map_err(|e| e.to_string())?;
            let (Some(name), Some(key)) = (name, key) else {
                // Skipped rather than failing the whole import: one
                // incomplete dictionary row should not make every other table
                // unimportable.
                continue;
            };
            // The bit reinterpretation, in the same direction the ordinal
            // column uses. See this module's header.
            out.push((name, key as u64));
        }
        Ok(out)
    })
}

/// Every populated key, named `k<key>`.
fn enumerate_keys(opts: &crate::options::ServerOptions) -> Result<Vec<(String, u64)>, String> {
    let mut transport = super::scan::connect_pub(opts)?;
    let keys = transport.keys().map_err(|e| e.to_string())?;
    Ok(keys.into_iter().map(|k| (format!("k{k}"), k)).collect())
}

/// Read and parse a foreign server's options.
///
/// # Safety
/// `oid` must name a foreign server.
unsafe fn server_options(oid: pg_sys::Oid) -> Result<crate::options::ServerOptions, String> {
    let raw = unsafe {
        let server = pg_sys::GetForeignServer(oid);
        if server.is_null() {
            return Err("no such foreign server".into());
        }
        super::relation_options((*server).options)
    };
    crate::options::ServerOptions::parse(&raw).map_err(|e| e.to_string())
}

fn quote_ident(s: &str) -> String {
    let cs = std::ffi::CString::new(s).unwrap_or_default();
    unsafe {
        let q = pg_sys::quote_identifier(cs.as_ptr());
        cstr(q as *mut core::ffi::c_char)
    }
}

/// # Safety
/// `p` must be a valid NUL-terminated string or null.
unsafe fn cstr(p: *mut core::ffi::c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    unsafe { core::ffi::CStr::from_ptr(p) }
        .to_string_lossy()
        .into_owned()
}

/// The `relname`s in a `LIMIT TO` / `EXCEPT` list.
///
/// # Safety
/// `list` must be a valid `List` of `RangeVar*` or null.
unsafe fn table_list_names(list: *mut pg_sys::List) -> Vec<String> {
    let mut out = Vec::new();
    if list.is_null() {
        return out;
    }
    let len = unsafe { (*list).length } as usize;
    for i in 0..len {
        let cell = unsafe { (*list).elements.add(i) };
        let rv = unsafe { (*cell).ptr_value } as *mut pg_sys::RangeVar;
        if rv.is_null() {
            continue;
        }
        out.push(unsafe { cstr((*rv).relname) });
    }
    out
}

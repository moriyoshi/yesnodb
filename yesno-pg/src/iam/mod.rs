//! The index access method: `CREATE INDEX … USING yesno ( col )`.
//!
//! # Where a posting list lives
//!
//! The indexed table is an **ordinary heap**, not a foreign table, so there
//! is no `SERVER` to read options from. The endpoint comes from a GUC —
//! `yesno_pg.endpoint` — set in `postgresql.conf` or per session.
//!
//! A per-index reloption ( `WITH ( server = … )` ) would be more flexible and is
//! the obvious next step; it needs `amoptions` to build a real `bytea` through
//! `build_reloptions`, which is a larger piece of `pg_sys` than the GUC and buys
//! nothing until someone wants two servers from one database.
//!
//! # Keys are namespaced by index OID
//!
//! The key is **not** `hash( value )`. Two indexes — on different tables, or
//! on different columns of one table — would then share posting lists, and each
//! would see the other's TIDs. `hash( index_oid ‖ value )` keeps them disjoint
//! without any catalogue of its own.

pub mod cost;
pub mod handler;
pub mod scan;
pub mod tid;
pub mod vacuum;
pub mod write;

use pgrx::prelude::*;

use crate::transport::flight::FlightTransport;
use crate::transport::TransportError;

/// The Flight endpoint every yesno index writes to.
///
/// A GUC rather than a reloption, and a `SUSET` one: it names a network
/// service, so letting any user repoint it would let them redirect another
/// user's index writes.
pub static ENDPOINT: pgrx::guc::GucSetting<Option<std::ffi::CString>> =
    pgrx::guc::GucSetting::<Option<std::ffi::CString>>::new(None);

/// Register the GUC. Called from `_PG_init`.
pub fn init_guc() {
    pgrx::guc::GucRegistry::define_string_guc(
        c"yesno_pg.endpoint",
        c"Flight endpoint for yesno indexes",
        c"grpc://host:port of the yesnod a `USING yesno` index stores its \
          posting lists in. Required before CREATE INDEX.",
        &ENDPOINT,
        pgrx::guc::GucContext::Suset,
        pgrx::guc::GucFlags::default(),
    );
}

/// The endpoint and this index's key namespace.
///
/// # Safety
///
/// `index` must be a valid index `Relation`.
pub unsafe fn index_target(index: pg_sys::Relation) -> Option<(String, u64)> {
    let endpoint = ENDPOINT.get()?.into_string().ok()?;
    let oid = unsafe { (*index).rd_id }.to_u32() as u64;
    Some((endpoint, oid))
}

/// A transport for this index's endpoint.
///
/// # Safety
///
/// `index` must be a valid index `Relation`.
pub unsafe fn open_transport_for_index(
    index: pg_sys::Relation,
) -> Result<FlightTransport, TransportError> {
    let Some((endpoint, _)) = (unsafe { index_target(index) }) else {
        return Err(TransportError::Connect {
            endpoint: "<unset>".into(),
            why: "yesno_pg.endpoint is not set; a yesno index needs one".into(),
        });
    };
    FlightTransport::new(&endpoint)
}

/// The key a value maps to within this index.
///
/// # Safety
///
/// `index` must be a valid index `Relation` and `datum` a non-null value of its
/// first indexed column's type.
pub unsafe fn index_key_for_datum(index: pg_sys::Relation, datum: pg_sys::Datum) -> Option<u64> {
    let (_, ns) = unsafe { index_target(index) }?;
    let atttype = unsafe {
        let desc = (*index).rd_att;
        if desc.is_null() || (*desc).natts < 1 {
            return None;
        }
        crate::pg_compat::first_attribute_type(desc)
    };
    let text = unsafe { datum_text(datum, atttype) }?;
    Some(key_for(ns, text.as_bytes()))
}

/// `hash( index_oid ‖ value )`. See this module's header for why the OID is in
/// the hash.
fn key_for(namespace: u64, value: &[u8]) -> u64 {
    let mut buf = Vec::with_capacity(8 + value.len());
    buf.extend_from_slice(&namespace.to_le_bytes());
    buf.extend_from_slice(value);
    tid::hash_datum_bytes(&buf)
}

/// A datum's canonical text, via its type's output function.
///
/// The text form rather than the raw bytes: it is stable across a type's
/// internal representation and it is what `yesno-datafusion`'s `HashEncoder`
/// hashes, so a term has the same key from either direction.
///
/// # Safety
///
/// `datum` must be a valid non-null value of type `atttype`.
unsafe fn datum_text(datum: pg_sys::Datum, atttype: pg_sys::Oid) -> Option<String> {
    unsafe {
        let mut out_fn = pg_sys::Oid::INVALID;
        let mut is_varlena = false;
        pg_sys::getTypeOutputInfo(atttype, &mut out_fn, &mut is_varlena);
        if out_fn == pg_sys::Oid::INVALID {
            return None;
        }
        let s = pg_sys::OidOutputFunctionCall(out_fn, datum);
        if s.is_null() {
            return None;
        }
        let owned = core::ffi::CStr::from_ptr(s).to_string_lossy().into_owned();
        pg_sys::pfree(s.cast());
        Some(owned)
    }
}

/// The key an index scan is looking for.
///
/// Only strategy 1 ( equality ) qualifies. A scan key of any other
/// strategy must yield `None` rather than being treated as equality: an index
/// that answered `col > x` with `col = x`'s posting list would return a small
/// wrong answer that looks plausible.
///
/// # Safety
///
/// `scan` must be a valid `IndexScanDesc`.
pub unsafe fn index_key_for_scan(scan: pg_sys::IndexScanDesc) -> Option<u64> {
    unsafe {
        if (*scan).numberOfKeys < 1 || (*scan).keyData.is_null() {
            return None;
        }
        let k = &*(*scan).keyData;
        if k.sk_strategy != 1 {
            return None;
        }
        // A NULL scan key matches nothing: `amsearchnulls` is false, so
        // `col = NULL` cannot be answered here and must not be hashed as if the
        // datum were a value.
        if k.sk_flags & pg_sys::SK_ISNULL as i32 != 0 {
            return None;
        }
        let index = (*scan).indexRelation;
        index_key_for_datum(index, k.sk_argument)
    }
}

/// The key an index *path* will look for, for cost estimation.
///
/// # Safety
///
/// `path` must be a valid `IndexPath`.
pub unsafe fn index_key_for_path(path: *mut pg_sys::IndexPath) -> Option<u64> {
    unsafe {
        let clauses = crate::fdw::scan::list_nodes_pub((*path).indexclauses);
        for c in clauses {
            if c.is_null() {
                continue;
            }
            let ic = c as *mut pg_sys::IndexClause;
            for q in crate::fdw::scan::list_nodes_pub((*ic).indexquals) {
                if q.is_null() || (*q).type_ != pg_sys::NodeTag::T_RestrictInfo {
                    continue;
                }
                let clause = (*(q as *mut pg_sys::RestrictInfo)).clause;
                if clause.is_null() || (*clause).type_ != pg_sys::NodeTag::T_OpExpr {
                    continue;
                }
                let op = clause as *mut pg_sys::OpExpr;
                let args = crate::fdw::scan::list_nodes_pub((*op).args);
                let [_lhs, rhs] = args.as_slice() else {
                    continue;
                };
                if rhs.is_null() || (**rhs).type_ != pg_sys::NodeTag::T_Const {
                    continue;
                }
                let konst = *rhs as *mut pg_sys::Const;
                if (*konst).constisnull {
                    continue;
                }
                let index = pg_sys::RelationIdGetRelation((*(*path).indexinfo).indexoid);
                if index.is_null() {
                    continue;
                }
                let key = index_key_for_datum(index, (*konst).constvalue);
                pg_sys::RelationClose(index);
                if key.is_some() {
                    return key;
                }
            }
        }
        None
    }
}

/// The endpoint and key namespace for a **table** stored by the yesno table AM.
///
/// Shares the GUC and the OID-namespacing rule with an index, so a table and
/// an index never collide. `super::tam` calls it rather than defining a second
/// one; two answers to "which key is this relation's" is exactly the kind of
/// duplication that drifts.
///
/// # Safety
///
/// `rel` must be a valid `Relation`.
pub unsafe fn index_target_for_table(rel: pg_sys::Relation) -> Option<(String, u64)> {
    let endpoint = ENDPOINT.get()?.into_string().ok()?;
    let oid = unsafe { (*rel).rd_id }.to_u32() as u64;
    // The relation's own OID is the key: a yesno table is one posting list.
    Some((endpoint, tid::hash_datum_bytes(&oid.to_le_bytes())))
}

/// A transport for a yesno table's endpoint.
///
/// # Safety
///
/// `rel` must be a valid `Relation`.
pub unsafe fn open_transport_for_table(
    rel: pg_sys::Relation,
) -> Result<FlightTransport, TransportError> {
    let Some((endpoint, _)) = (unsafe { index_target_for_table(rel) }) else {
        return Err(TransportError::Connect {
            endpoint: "<unset>".into(),
            why: "yesno_pg.endpoint is not set; a yesno table needs one".into(),
        });
    };
    FlightTransport::new(&endpoint)
}

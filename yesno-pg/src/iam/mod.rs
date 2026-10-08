//! The index access method: `CREATE INDEX … USING yesno ( col )`.
//!
//! # Where a posting list lives
//!
//! The indexed table is an **ordinary heap**, not a foreign table, so there
//! is no `SERVER` to read options from. The server comes from a GUC — either
//! `yesno_pg.endpoint` for a Flight deployment or `yesno_pg.channel_socket`
//! for a plugin-channel one — set in `postgresql.conf` or per session.
//! Exactly one of the two, because they are two transports rather than two
//! spellings of one; both set is reported instead of resolved by precedence.
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

use crate::transport::{Transport, TransportError};

/// The Flight endpoint every yesno index writes to.
///
/// A GUC rather than a reloption, and a `SUSET` one: it names a network
/// service, so letting any user repoint it would let them redirect another
/// user's index writes.
pub static ENDPOINT: pgrx::guc::GucSetting<Option<std::ffi::CString>> =
    pgrx::guc::GucSetting::<Option<std::ffi::CString>>::new(None);

/// The plugin-channel socket, as an alternative to [`ENDPOINT`].
///
/// A second GUC rather than overloading the first, because the two are
/// different transports and a single string would have to be sniffed to tell
/// `grpc://host:port` from a path. Also `SUSET`, for the reason `ENDPOINT` is:
/// it names a service, and letting any user repoint it would let them redirect
/// another user's index writes.
pub static CHANNEL_SOCKET: pgrx::guc::GucSetting<Option<std::ffi::CString>> =
    pgrx::guc::GucSetting::<Option<std::ffi::CString>>::new(None);

/// Register the GUCs. Called from `_PG_init`.
pub fn init_guc() {
    pgrx::guc::GucRegistry::define_string_guc(
        c"yesno_pg.endpoint",
        c"Flight endpoint for yesno indexes",
        c"grpc://host:port of the yesnod a `USING yesno` index stores its \
          posting lists in. Required before CREATE INDEX unless \
          yesno_pg.channel_socket is set instead.",
        &ENDPOINT,
        pgrx::guc::GucContext::Suset,
        pgrx::guc::GucFlags::default(),
    );
    pgrx::guc::GucRegistry::define_string_guc(
        c"yesno_pg.channel_socket",
        c"Plugin-channel socket for yesno indexes",
        c"Unix socket of a running yesnod's plugin channel, as an alternative \
          to yesno_pg.endpoint. Exactly one of the two must be set. Reaching a \
          yesnod this way needs no gRPC listener and works where an \
          in-process open cannot, because PostgreSQL forks a backend per \
          connection and only one process may hold the database lock.",
        &CHANNEL_SOCKET,
        pgrx::guc::GucContext::Suset,
        pgrx::guc::GucFlags::default(),
    );
}

/// Which transport the GUCs name.
///
/// Three outcomes, not two. `Ok( None )` is "neither GUC is set", which a
/// caller may reasonably read as *this relation has no server yet*; `Err` is
/// **both set**, which it may not.
///
/// Both set is an error rather than a precedence rule. A precedence would let a
/// stale `yesno_pg.endpoint` silently win over the socket an operator had just
/// configured, and the failure would look like the socket being ignored.
fn configured_transport() -> Result<Option<crate::options::Transport>, TransportError> {
    use crate::options::Transport as TransportKind;
    let named = |guc: &pgrx::guc::GucSetting<Option<std::ffi::CString>>| {
        guc.get()
            .and_then(|value| value.into_string().ok())
            .filter(|value| !value.is_empty())
    };
    match (named(&ENDPOINT), named(&CHANNEL_SOCKET)) {
        (Some(endpoint), None) => Ok(Some(TransportKind::Flight { endpoint })),
        (None, Some(socket)) => Ok(Some(TransportKind::Channel { socket })),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(TransportError::Connect {
            endpoint: "<ambiguous>".into(),
            why: "both yesno_pg.endpoint and yesno_pg.channel_socket are set; \
                  they are two different transports and exactly one must be chosen"
                .into(),
        }),
    }
}

/// The identity a relation's writes buffer under, from whichever GUC is set.
///
/// Ambiguity is **reported here rather than returned as `None`**, because every
/// caller of [`index_target`] reads `None` as "no server configured" and
/// answers an empty set. A misconfigured cluster would then look like an empty
/// table, which is the one failure mode worse than an error.
fn configured_identity() -> Option<String> {
    match configured_transport() {
        Ok(kind) => kind?.buffer_key(),
        Err(e) => error!("yesno_pg: {e}"),
    }
}

/// Open whichever transport the GUCs name.
///
/// `noun` is what the caller needs it for -- "index" or "table" -- so that an
/// unset configuration says which operation it blocked.
///
/// The index and table paths have no server options to read a batch size from,
/// so the default page applies.
fn open_configured(noun: &str) -> Result<Box<dyn Transport>, TransportError> {
    let Some(kind) = configured_transport()? else {
        return Err(TransportError::Connect {
            endpoint: "<unset>".into(),
            why: format!(
                "neither yesno_pg.endpoint nor yesno_pg.channel_socket is set; \
                 a yesno {noun} needs one"
            ),
        });
    };
    crate::transport::open(&kind, crate::options::DEFAULT_BATCH_ROWS)
}

/// The server identity this index's writes buffer under, and its key namespace.
///
/// # Safety
///
/// `index` must be a valid index `Relation`.
pub unsafe fn index_target(index: pg_sys::Relation) -> Option<(String, u64)> {
    let identity = configured_identity()?;
    let oid = unsafe { (*index).rd_id }.to_u32() as u64;
    Some((identity, oid))
}

/// A transport for this index's configured server.
///
/// The relation is not read. It is still a parameter because the server is a
/// *per-relation* fact that a reloption would make per-relation for real --
/// see this module's header -- and the GUC is the stand-in. Dropping it would
/// have to be put back by every caller on the day that changes.
///
/// # Safety
///
/// `_index` must be a valid index `Relation`.
pub unsafe fn open_transport_for_index(
    _index: pg_sys::Relation,
) -> Result<Box<dyn Transport>, TransportError> {
    open_configured("index")
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
    let identity = configured_identity()?;
    let oid = unsafe { (*rel).rd_id }.to_u32() as u64;
    // The relation's own OID is the key: a yesno table is one posting list.
    Some((identity, tid::hash_datum_bytes(&oid.to_le_bytes())))
}

/// A transport for a yesno table's configured server.
///
/// The relation is not read, for the reason [`open_transport_for_index`]
/// gives.
///
/// # Safety
///
/// `_rel` must be a valid `Relation`.
pub unsafe fn open_transport_for_table(
    _rel: pg_sys::Relation,
) -> Result<Box<dyn Transport>, TransportError> {
    open_configured("table")
}

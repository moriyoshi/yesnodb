//! PostgreSQL-major ABI adapters.
//!
//! pgrx exposes the server headers almost verbatim, so a major-version change
//! can alter Rust signatures even when yesno's planner or executor semantics do
//! not change. Keep those differences here: callers state the invariant once,
//! and each branch supplies the ABI shape for its PostgreSQL major.

use pgrx::pg_sys;

/// Construct yesno's one base-relation foreign path.
///
/// # Safety
/// `root` and `rel` are valid planner pointers.
pub unsafe fn foreign_scan_path(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    rows: f64,
) -> *mut pg_sys::ForeignPath {
    #[cfg(feature = "pg17")]
    unsafe {
        pg_sys::create_foreignscan_path(
            root,
            rel,
            (*rel).reltarget,
            rows,
            0.0,
            rows,
            core::ptr::null_mut(), // pathkeys: u64 order is not int8 order
            core::ptr::null_mut(), // required_outer
            core::ptr::null_mut(), // fdw_outerpath
            core::ptr::null_mut(), // fdw_restrictinfo
            core::ptr::null_mut(), // fdw_private
        )
    }
    #[cfg(feature = "pg18")]
    unsafe {
        pg_sys::create_foreignscan_path(
            root,
            rel,
            (*rel).reltarget,
            rows,
            0, // disabled_nodes; this path is always available
            0.0,
            rows,
            core::ptr::null_mut(), // pathkeys: u64 order is not int8 order
            core::ptr::null_mut(), // required_outer
            core::ptr::null_mut(), // fdw_outerpath
            core::ptr::null_mut(), // fdw_restrictinfo
            core::ptr::null_mut(), // fdw_private
        )
    }
}

/// Construct a pushed set-operation join path.
///
/// # Safety
/// Planner pointers and `private` are valid for the duration of planning.
pub unsafe fn foreign_join_path(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    rows: f64,
    private: *mut pg_sys::List,
) -> *mut pg_sys::ForeignPath {
    #[cfg(feature = "pg17")]
    unsafe {
        pg_sys::create_foreign_join_path(
            root,
            rel,
            core::ptr::null_mut(), // default PathTarget
            rows,
            0.0,
            rows,
            core::ptr::null_mut(), // pathkeys
            core::ptr::null_mut(), // required_outer
            core::ptr::null_mut(), // fdw_outerpath
            core::ptr::null_mut(), // fdw_restrictinfo
            private,
        )
    }
    #[cfg(feature = "pg18")]
    unsafe {
        pg_sys::create_foreign_join_path(
            root,
            rel,
            core::ptr::null_mut(), // default PathTarget
            rows,
            0, // disabled_nodes; this path is always available
            0.0,
            rows,
            core::ptr::null_mut(), // pathkeys
            core::ptr::null_mut(), // required_outer
            core::ptr::null_mut(), // fdw_outerpath
            core::ptr::null_mut(), // fdw_restrictinfo
            private,
        )
    }
}

/// Construct the one-row, near-zero-cost pushed `count(*)` path.
///
/// # Safety
/// Planner pointers and `private` are valid for the duration of planning.
pub unsafe fn foreign_upper_path(
    root: *mut pg_sys::PlannerInfo,
    rel: *mut pg_sys::RelOptInfo,
    private: *mut pg_sys::List,
) -> *mut pg_sys::ForeignPath {
    #[cfg(feature = "pg17")]
    unsafe {
        pg_sys::create_foreign_upper_path(
            root,
            rel,
            (*rel).reltarget,
            1.0,
            0.0,
            1.0,
            core::ptr::null_mut(), // pathkeys
            core::ptr::null_mut(), // fdw_outerpath
            core::ptr::null_mut(), // fdw_restrictinfo
            private,
        )
    }
    #[cfg(feature = "pg18")]
    unsafe {
        pg_sys::create_foreign_upper_path(
            root,
            rel,
            (*rel).reltarget,
            1.0,
            0, // disabled_nodes; this path is always available
            0.0,
            1.0,
            core::ptr::null_mut(), // pathkeys
            core::ptr::null_mut(), // fdw_outerpath
            core::ptr::null_mut(), // fdw_restrictinfo
            private,
        )
    }
}

/// Read the first full attribute descriptor.
///
/// PostgreSQL 18 places compact attributes in the tuple descriptor's flexible
/// array and the full descriptors immediately after them. This is the
/// `TupleDescAttr(desc, 0)` address calculation from `tupdesc.h`; bindgen
/// cannot emit that static-inline accessor.
///
/// # Safety
/// `desc` is non-null and has at least one attribute.
pub unsafe fn first_attribute_type(desc: pg_sys::TupleDesc) -> pg_sys::Oid {
    #[cfg(feature = "pg17")]
    unsafe {
        (*(*desc).attrs.as_ptr()).atttypid
    }
    #[cfg(feature = "pg18")]
    unsafe {
        let attributes = (*desc)
            .compact_attrs
            .as_ptr()
            .add((*desc).natts as usize)
            .cast::<pg_sys::FormData_pg_attribute>();
        (*attributes).atttypid
    }
}

//! The example from README.md, compiled and run so the README cannot drift.
//!
//! A code block in a README is a claim about the API. Keeping it as an example
//! target means a rename breaks the build rather than silently leaving the
//! documentation wrong.

use std::sync::Arc;

use yesno_core::matrix::{BitMatrix, Layout, MatrixSink, Semiring};
use yesno_core::stream::ChunkStreamExt;
use yesno_core::{Db, Result};

fn main() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("yesno-readme-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let rust = 42u64;
    let database = 7u64;
    let durable = 9u64;
    let db = Db::open(&dir)?;

    db.insert_many(rust, &[1, 5, 9, 65_540])?;
    db.insert_many(database, &[5, 9, 13])?;
    db.insert_many(durable, &[1, 5])?;
    db.checkpoint()?; // now durable

    let snap = db.snapshot()?; // consistent across every shard
    assert!(snap.contains(rust, 65_540)?);

    // Lazy over stored posting lists: `rust AND (database OR durable)`.
    let a = Arc::new(snap.load(rust)?);
    let b = Arc::new(snap.load(database)?);
    let c = Arc::new(snap.load(durable)?);
    let n = a.stream().and(b.stream().or(c.stream())).cardinality()?;
    assert_eq!(n, 3, "{{1, 5, 9}}");

    // The non-materializing count agrees with the eager result.
    assert_eq!(n, a.and(&b.or(&c)).len());

    // The same set, read as a 4x4 bit matrix: element (r, c) is ordinal r*4 + c.
    let layout = Layout::dense(4, 4);
    let mut sink = MatrixSink::new(layout);
    sink.place(0, &BitMatrix::identity(4))?;
    let stored = sink.build();

    let m = stored.read_matrix(0, &layout).expect("addressable");
    assert!(m.is_identity());

    // A*B + C over GF(2), where addition cancels: I*I + I == 0.
    let zero = m.gemm(&m, &m, Semiring::Gf2).expect("shapes agree");
    assert!(zero.is_zero());

    // Over the boolean semiring it does not cancel.
    assert_eq!(m.gemm(&m, &m, Semiring::Boolean).unwrap().count_ones(), 4);

    println!(
        "readme example ok: cardinality = {n}, durable = {}, matrix trace = {}",
        db.is_durable(),
        m.trace()
    );
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

//! Expression equivalence regression for sources without metadata.

use std::sync::Arc;
use yesno_core::stream::{BoxedStream, ChunkSource, ChunkStreamExt, SetStream};
use yesno_core::{Expr, OrdSet};

/// `None` from a source's span means unknown, not empty. The planner used to
/// remove it from OR and turn an AND containing it into `Empty`, while both
/// `cardinality` and `collect_set` agreed on the same incorrect planned result.
/// The independent eager set below makes that mistake observable.
#[test]
fn an_opaque_source_is_not_rewritten_to_an_empty_operand() {
    #[derive(Debug)]
    struct Opaque(Arc<OrdSet>);

    impl ChunkSource for Opaque {
        fn open(&self) -> BoxedStream {
            Box::new(SetStream::new(self.0.clone()))
        }
    }

    let left = Arc::new(OrdSet::from_sorted_slice(&[7, 9, 65_537]));
    let right = Arc::new(OrdSet::from_sorted_slice(&[9, 10, 65_538]));
    let source = Expr::Source(Arc::new(Opaque(left.clone())));
    let resident = Expr::set(right.clone());

    let cases = [
        (source.clone().or(resident.clone()), left.or(&right)),
        (source.clone().and(resident.clone()), left.and(&right)),
        (source.clone().xor(resident.clone()), left.xor(&right)),
        (source.and_not(resident), left.and_not(&right)),
    ];
    for (expr, expected) in cases {
        assert_eq!(expr.collect_set().unwrap(), expected);
        assert_eq!(expr.cardinality().unwrap(), expected.len());
        assert_eq!(expr.open_planned().collect_set().unwrap(), expected);
    }
}

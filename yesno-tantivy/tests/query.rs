use std::sync::Arc;

use tantivy::collector::Count;
use tantivy::indexer::NoMergePolicy;
use tantivy::schema::{Schema, FAST, INDEXED};
use tantivy::{doc, DocSet, Index, IndexReader, IndexWriter, TantivyDocument, Term, TERMINATED};
use yesno_core::{Expr, OrdSet};
use yesno_tantivy::{
    FastFieldOrdinalResolver, MissingOrdinalPolicy, PrepareError, PreparedYesnoQuery, YesnoDocSet,
};

fn index_with_segments(
    segments: &[&[u64]],
) -> (
    Index,
    IndexReader,
    IndexWriter<TantivyDocument>,
    tantivy::schema::Field,
) {
    let mut schema = Schema::builder();
    let stable_id = schema.add_u64_field("stable_id", FAST | INDEXED);
    let index = Index::create_in_ram(schema.build());
    let reader = index.reader().unwrap();
    let mut writer = index.writer(50_000_000).unwrap();
    writer.set_merge_policy(Box::new(NoMergePolicy));
    for segment in segments {
        for &ordinal in *segment {
            writer.add_document(doc!(stable_id => ordinal)).unwrap();
        }
        writer.commit().unwrap();
    }
    reader.reload().unwrap();
    (index, reader, writer, stable_id)
}

#[test]
fn yesno_docset_obeys_advance_and_seek_contracts() {
    let set = Arc::new(OrdSet::from_sorted_slice(&[1, 3, 65_536, 70_000]));
    let mut docs = YesnoDocSet::try_new(set).unwrap();

    assert_eq!(docs.doc(), 1);
    assert_eq!(docs.seek(0), 1);
    assert_eq!(docs.seek(2), 3);
    assert_eq!(docs.seek(3), 3);
    assert_eq!(docs.advance(), 65_536);
    assert_eq!(docs.seek(69_999), 70_000);
    assert_eq!(docs.advance(), TERMINATED);
    assert_eq!(docs.advance(), TERMINATED);
    assert_eq!(docs.seek(1), TERMINATED);
}

#[test]
fn prepared_query_matches_across_segments_and_merge_invalidates_it() {
    let (_index, reader, mut writer, _stable_id) = index_with_segments(&[&[10, 20], &[30, 40]]);
    let searcher = reader.searcher();
    assert!(searcher.segment_readers().len() > 1);
    let resolver = FastFieldOrdinalResolver::build(&searcher, "stable_id").unwrap();
    let query = PreparedYesnoQuery::from_ordinals(
        &searcher,
        &resolver,
        [20, 30],
        MissingOrdinalPolicy::Error,
        Some(7),
    )
    .unwrap();

    assert_eq!(query.yesno_version(), Some(7));
    assert_eq!(query.matched_docs(), 2);
    assert_eq!(searcher.search(&query, &Count).unwrap(), 2);

    let segment_ids = searcher
        .segment_readers()
        .iter()
        .map(|reader| reader.segment_id())
        .collect::<Vec<_>>();
    writer.merge(&segment_ids).wait().unwrap();
    reader.reload().unwrap();
    let merged = reader.searcher();
    assert_eq!(merged.segment_readers().len(), 1);
    assert!(merged.search(&query, &Count).is_err());
}

#[test]
fn deletion_invalidates_prepared_query_and_old_resolver() {
    let (_index, reader, mut writer, stable_id) = index_with_segments(&[&[10, 20, 30]]);
    let searcher = reader.searcher();
    let resolver = FastFieldOrdinalResolver::build(&searcher, "stable_id").unwrap();
    let query = PreparedYesnoQuery::from_ordinals(
        &searcher,
        &resolver,
        [20],
        MissingOrdinalPolicy::Error,
        None,
    )
    .unwrap();

    writer.delete_term(Term::from_field_u64(stable_id, 20));
    writer.commit().unwrap();
    reader.reload().unwrap();
    let after_delete = reader.searcher();

    assert!(after_delete.search(&query, &Count).is_err());
    assert!(matches!(
        PreparedYesnoQuery::from_ordinals(
            &after_delete,
            &resolver,
            [20],
            MissingOrdinalPolicy::Error,
            None,
        ),
        Err(PrepareError::WrongSearcherGeneration)
    ));
}

#[test]
fn missing_ordinals_are_strict_unless_explicitly_ignored() {
    let (_index, reader, _writer, _stable_id) = index_with_segments(&[&[10, 20]]);
    let searcher = reader.searcher();
    let resolver = FastFieldOrdinalResolver::build(&searcher, "stable_id").unwrap();

    assert!(matches!(
        PreparedYesnoQuery::from_ordinals(
            &searcher,
            &resolver,
            [99],
            MissingOrdinalPolicy::Error,
            None,
        ),
        Err(PrepareError::MissingOrdinal { ordinal: 99 })
    ));

    let query = PreparedYesnoQuery::from_ordinals(
        &searcher,
        &resolver,
        [99],
        MissingOrdinalPolicy::Ignore,
        None,
    )
    .unwrap();
    assert_eq!(searcher.search(&query, &Count).unwrap(), 0);
}

#[test]
fn embedded_expression_is_evaluated_before_tantivy_search() {
    let (_index, reader, _writer, _stable_id) = index_with_segments(&[&[10, 20, 30]]);
    let searcher = reader.searcher();
    let resolver = FastFieldOrdinalResolver::build(&searcher, "stable_id").unwrap();
    let left = Arc::new(OrdSet::from_sorted_slice(&[10, 20]));
    let right = Arc::new(OrdSet::from_sorted_slice(&[20, 30]));
    let expression = Expr::set(left).and(Expr::set(right));

    let query = PreparedYesnoQuery::from_expr(
        &searcher,
        &resolver,
        &expression,
        MissingOrdinalPolicy::Error,
        Some(11),
    )
    .unwrap();

    assert_eq!(query.yesno_version(), Some(11));
    assert_eq!(searcher.search(&query, &Count).unwrap(), 1);
}

#[test]
fn resolver_rejects_non_unique_stable_ids() {
    let (_index, reader, _writer, _stable_id) = index_with_segments(&[&[10, 10]]);
    let searcher = reader.searcher();
    assert!(matches!(
        FastFieldOrdinalResolver::build(&searcher, "stable_id"),
        Err(PrepareError::DuplicateStableOrdinal { ordinal: 10 })
    ));
}

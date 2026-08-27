//! Tantivy query integration for yesno ordinal sets.
//!
//! Tantivy's query cursor is synchronous and infallible after construction.
//! Any embedded read, remote Flight request, or stable-ID translation therefore
//! finishes before a [`PreparedYesnoQuery`] is handed to Tantivy. The prepared
//! query is bound to one [`SearcherGeneration`]; executing it against another
//! generation is an error rather than an empty result.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use tantivy::index::{SegmentId, SegmentReader};
use tantivy::query::{ConstScorer, EnableScoring, Explanation, Query, Scorer, Weight};
use tantivy::{DocId, DocSet, Score, Searcher, SearcherGeneration, TantivyError, TERMINATED};
use thiserror::Error;
use yesno_core::OrdSet;

#[cfg(feature = "flight")]
pub mod flight;

/// How to handle a yesno ordinal absent from the bound Tantivy searcher.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MissingOrdinalPolicy {
    /// Refuse the query. This prevents a stale or mismatched index pair from
    /// silently losing results.
    #[default]
    Error,
    /// Ignore absent ordinals. Applications choosing eventual consistency must
    /// opt into this explicitly.
    Ignore,
}

/// A stable yesno ordinal resolved to one Tantivy segment-local document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolvedDoc {
    pub segment_id: SegmentId,
    pub doc_id: DocId,
}

/// Resolves stable yesno ordinals for exactly one Tantivy searcher generation.
pub trait OrdinalResolver: Send + Sync {
    fn generation(&self) -> &SearcherGeneration;
    fn resolve(&self, ordinal: u64) -> Option<ResolvedDoc>;
}

/// Errors detected before Tantivy starts consuming a prepared query.
#[derive(Debug, Error)]
pub enum PrepareError {
    #[error(transparent)]
    Tantivy(#[from] TantivyError),

    #[error(transparent)]
    Yesno(#[from] yesno_core::CodecError),

    #[error("the ordinal resolver belongs to a different Tantivy searcher generation")]
    WrongSearcherGeneration,

    #[error(
        "document {doc_id} in segment {segment_id} has no value in required fast field {field:?}"
    )]
    MissingFastFieldValue {
        field: String,
        segment_id: SegmentId,
        doc_id: DocId,
    },

    #[error(
        "document {doc_id} in segment {segment_id} has more than one value in fast field {field:?}"
    )]
    MultipleFastFieldValues {
        field: String,
        segment_id: SegmentId,
        doc_id: DocId,
    },

    #[error("stable ordinal {ordinal} occurs in more than one live Tantivy document")]
    DuplicateStableOrdinal { ordinal: u64 },

    #[error("yesno ordinal {ordinal} is absent from the bound Tantivy searcher")]
    MissingOrdinal { ordinal: u64 },

    #[error("ordinal {ordinal} resolved to unknown Tantivy segment {segment_id}")]
    UnknownResolvedSegment { ordinal: u64, segment_id: SegmentId },

    #[error(
        "ordinal {ordinal} resolved to document {doc_id} outside segment {segment_id}'s max_doc {max_doc}"
    )]
    ResolvedDocumentOutOfRange {
        ordinal: u64,
        segment_id: SegmentId,
        doc_id: DocId,
        max_doc: DocId,
    },

    #[error("more than one yesno ordinal resolved to document {doc_id} in segment {segment_id}")]
    DuplicateResolvedDocument {
        segment_id: SegmentId,
        doc_id: DocId,
    },

    #[error("yesno document ID {doc_id} collides with Tantivy's TERMINATED sentinel")]
    DocumentIdNotRepresentable { doc_id: u64 },

    #[error("constant query score must be finite, got {score}")]
    InvalidScore { score: Score },
}

/// A generation-bound resolver built from a unique, single-valued `u64` fast
/// field.
///
/// Only live documents are mapped. Deleted documents are therefore treated like
/// ordinals absent from this searcher rather than allowed to collide with a
/// replacement document carrying the same stable ID.
#[derive(Clone, Debug)]
pub struct FastFieldOrdinalResolver {
    generation: SearcherGeneration,
    docs: HashMap<u64, ResolvedDoc>,
}

impl FastFieldOrdinalResolver {
    pub fn build(searcher: &Searcher, field: impl Into<String>) -> Result<Self, PrepareError> {
        let field = field.into();
        let mut docs = HashMap::new();

        for reader in searcher.segment_readers() {
            let segment_id = reader.segment_id();
            let column = reader.fast_fields().u64(&field)?;
            for doc_id in reader.doc_ids_alive() {
                let mut values = column.values_for_doc(doc_id);
                let Some(ordinal) = values.next() else {
                    return Err(PrepareError::MissingFastFieldValue {
                        field: field.clone(),
                        segment_id,
                        doc_id,
                    });
                };
                if values.next().is_some() {
                    return Err(PrepareError::MultipleFastFieldValues {
                        field: field.clone(),
                        segment_id,
                        doc_id,
                    });
                }
                if docs
                    .insert(ordinal, ResolvedDoc { segment_id, doc_id })
                    .is_some()
                {
                    return Err(PrepareError::DuplicateStableOrdinal { ordinal });
                }
            }
        }

        Ok(Self {
            generation: searcher.generation().clone(),
            docs,
        })
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }
}

impl OrdinalResolver for FastFieldOrdinalResolver {
    fn generation(&self) -> &SearcherGeneration {
        &self.generation
    }

    fn resolve(&self, ordinal: u64) -> Option<ResolvedDoc> {
        self.docs.get(&ordinal).copied()
    }
}

/// A fully materialized, network-free Tantivy query.
///
/// The maps contain segment-local document IDs represented as yesno sets. Empty
/// segments are omitted from `matches` but remain present in `generation`, so
/// the weight can distinguish a valid empty segment from the wrong searcher.
#[derive(Clone, Debug)]
pub struct PreparedYesnoQuery {
    generation: SearcherGeneration,
    matches: Arc<BTreeMap<SegmentId, Arc<OrdSet>>>,
    yesno_version: Option<u64>,
    score: Score,
    matched_docs: u64,
}

impl PreparedYesnoQuery {
    pub fn from_ordinals<I, R>(
        searcher: &Searcher,
        resolver: &R,
        ordinals: I,
        missing: MissingOrdinalPolicy,
        yesno_version: Option<u64>,
    ) -> Result<Self, PrepareError>
    where
        I: IntoIterator<Item = u64>,
        R: OrdinalResolver + ?Sized,
    {
        if resolver.generation() != searcher.generation() {
            return Err(PrepareError::WrongSearcherGeneration);
        }

        let limits: BTreeMap<SegmentId, DocId> = searcher
            .segment_readers()
            .iter()
            .map(|reader| (reader.segment_id(), reader.max_doc()))
            .collect();
        let mut grouped: BTreeMap<SegmentId, Vec<u64>> = BTreeMap::new();

        for ordinal in ordinals {
            let Some(resolved) = resolver.resolve(ordinal) else {
                if missing == MissingOrdinalPolicy::Ignore {
                    continue;
                }
                return Err(PrepareError::MissingOrdinal { ordinal });
            };
            let Some(&max_doc) = limits.get(&resolved.segment_id) else {
                return Err(PrepareError::UnknownResolvedSegment {
                    ordinal,
                    segment_id: resolved.segment_id,
                });
            };
            if resolved.doc_id >= max_doc || resolved.doc_id >= TERMINATED {
                return Err(PrepareError::ResolvedDocumentOutOfRange {
                    ordinal,
                    segment_id: resolved.segment_id,
                    doc_id: resolved.doc_id,
                    max_doc,
                });
            }
            grouped
                .entry(resolved.segment_id)
                .or_default()
                .push(u64::from(resolved.doc_id));
        }

        let mut matches = BTreeMap::new();
        let mut matched_docs = 0u64;
        for (segment_id, mut docs) in grouped {
            docs.sort_unstable();
            if let Some(pair) = docs.windows(2).find(|pair| pair[0] == pair[1]) {
                return Err(PrepareError::DuplicateResolvedDocument {
                    segment_id,
                    doc_id: pair[0] as DocId,
                });
            }
            matched_docs += docs.len() as u64;
            matches.insert(segment_id, Arc::new(OrdSet::from_sorted_slice(&docs)));
        }

        Ok(Self {
            generation: searcher.generation().clone(),
            matches: Arc::new(matches),
            yesno_version,
            score: 0.0,
            matched_docs,
        })
    }

    pub fn from_set<R>(
        searcher: &Searcher,
        resolver: &R,
        set: &OrdSet,
        missing: MissingOrdinalPolicy,
        yesno_version: Option<u64>,
    ) -> Result<Self, PrepareError>
    where
        R: OrdinalResolver + ?Sized,
    {
        Self::from_ordinals(searcher, resolver, set.iter(), missing, yesno_version)
    }

    /// Evaluate an embedded yesno expression, then prepare its result.
    ///
    /// Evaluation remains fallible and completes before Tantivy creates a
    /// synchronous scorer. The remote counterpart is
    /// `flight::FlightQueryPreparer`.
    pub fn from_expr<R>(
        searcher: &Searcher,
        resolver: &R,
        expression: &yesno_core::Expr,
        missing: MissingOrdinalPolicy,
        yesno_version: Option<u64>,
    ) -> Result<Self, PrepareError>
    where
        R: OrdinalResolver + ?Sized,
    {
        let set = expression.collect_set()?;
        Self::from_set(searcher, resolver, &set, missing, yesno_version)
    }

    pub fn with_score(mut self, score: Score) -> Result<Self, PrepareError> {
        if !score.is_finite() {
            return Err(PrepareError::InvalidScore { score });
        }
        self.score = score;
        Ok(self)
    }

    pub fn generation(&self) -> &SearcherGeneration {
        &self.generation
    }

    pub fn yesno_version(&self) -> Option<u64> {
        self.yesno_version
    }

    pub fn matched_docs(&self) -> u64 {
        self.matched_docs
    }

    fn validate_searcher(&self, searcher: &Searcher) -> tantivy::Result<()> {
        if searcher.generation() != &self.generation {
            return Err(TantivyError::InvalidArgument(
                "prepared yesno query belongs to a different searcher generation".to_string(),
            ));
        }
        Ok(())
    }
}

impl Query for PreparedYesnoQuery {
    fn weight(&self, enable_scoring: EnableScoring<'_>) -> tantivy::Result<Box<dyn Weight>> {
        if let Some(searcher) = enable_scoring.searcher() {
            self.validate_searcher(searcher)?;
        }
        Ok(Box::new(PreparedYesnoWeight {
            generation: self.generation.clone(),
            matches: self.matches.clone(),
            score: self.score,
            empty: Arc::new(OrdSet::new()),
        }))
    }
}

struct PreparedYesnoWeight {
    generation: SearcherGeneration,
    matches: Arc<BTreeMap<SegmentId, Arc<OrdSet>>>,
    score: Score,
    empty: Arc<OrdSet>,
}

impl PreparedYesnoWeight {
    fn set_for(&self, reader: &SegmentReader) -> tantivy::Result<Arc<OrdSet>> {
        let segment_id = reader.segment_id();
        match self.generation.segments().get(&segment_id) {
            Some(delete_opstamp) if *delete_opstamp == reader.delete_opstamp() => Ok(self
                .matches
                .get(&segment_id)
                .cloned()
                .unwrap_or_else(|| self.empty.clone())),
            _ => Err(TantivyError::InvalidArgument(format!(
                "prepared yesno query does not belong to segment {segment_id}"
            ))),
        }
    }
}

impl Weight for PreparedYesnoWeight {
    fn scorer(&self, reader: &SegmentReader, boost: Score) -> tantivy::Result<Box<dyn Scorer>> {
        let docset = YesnoDocSet::try_new(self.set_for(reader)?)
            .map_err(|error| TantivyError::InvalidArgument(error.to_string()))?;
        Ok(Box::new(ConstScorer::new(docset, self.score * boost)))
    }

    fn explain(&self, reader: &SegmentReader, doc: DocId) -> tantivy::Result<Explanation> {
        let set = self.set_for(reader)?;
        if !set.contains(u64::from(doc)) {
            return Err(TantivyError::InvalidArgument(format!(
                "document {doc} does not match the prepared yesno query"
            )));
        }
        Ok(Explanation::new("yesno constant-score filter", self.score))
    }
}

/// Tantivy's cursor contract over one immutable yesno set of local document IDs.
pub struct YesnoDocSet {
    set: Arc<OrdSet>,
    chunk: usize,
    within: u32,
    doc: DocId,
}

impl YesnoDocSet {
    pub fn try_new(set: Arc<OrdSet>) -> Result<Self, PrepareError> {
        if let Some(max) = set.max() {
            if max >= u64::from(TERMINATED) {
                return Err(PrepareError::DocumentIdNotRepresentable { doc_id: max });
            }
        }
        let mut out = Self {
            set,
            chunk: 0,
            within: 0,
            doc: TERMINATED,
        };
        out.set_position(0, 0);
        Ok(out)
    }

    fn set_position(&mut self, mut chunk: usize, mut within: u32) {
        loop {
            let Some((prefix, container)) = self.set.chunk_at(chunk) else {
                self.chunk = self.set.chunk_count();
                self.within = 0;
                self.doc = TERMINATED;
                return;
            };
            if let Some(low) = container.select(within) {
                self.chunk = chunk;
                self.within = within;
                self.doc = ((prefix << 16) | u64::from(low)) as DocId;
                return;
            }
            chunk += 1;
            within = 0;
        }
    }
}

impl DocSet for YesnoDocSet {
    fn advance(&mut self) -> DocId {
        if self.doc != TERMINATED {
            self.set_position(self.chunk, self.within + 1);
        }
        self.doc
    }

    fn seek(&mut self, target: DocId) -> DocId {
        if self.doc == TERMINATED || target >= TERMINATED {
            self.set_position(self.set.chunk_count(), 0);
            return TERMINATED;
        }
        if target <= self.doc {
            return self.doc;
        }

        let prefix = u64::from(target) >> 16;
        let low = target as u16;
        let chunk = self
            .set
            .partition_point_in(self.chunk, self.set.chunk_count(), prefix)
            + self.chunk;
        let within = match self.set.chunk_at(chunk) {
            Some((found, container)) if found == prefix => container.rank(low),
            _ => 0,
        };
        self.set_position(chunk, within);
        self.doc
    }

    fn doc(&self) -> DocId {
        self.doc
    }

    fn size_hint(&self) -> u32 {
        self.set.len() as u32
    }

    fn cost(&self) -> u64 {
        self.set.len()
    }
}

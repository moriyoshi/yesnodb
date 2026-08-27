//! Remote preparation through yesno's Apache Flight client.
//!
//! The network is used only while constructing a [`PreparedYesnoQuery`]. A
//! Tantivy search never performs I/O and never observes a partially received
//! result.

use arrow_array::{Array, UInt64Array};
use arrow_flight::error::FlightError;
use futures::TryStreamExt;
use tantivy::Searcher;
use thiserror::Error;
use tonic::{transport::Channel, Code};
use yesno_flight::{SetExpr, YesnoClient};

use crate::{MissingOrdinalPolicy, OrdinalResolver, PrepareError, PreparedYesnoQuery};

/// Which yesno snapshot the remote preparation must use.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Consistency {
    /// Plan at the server's current snapshot. Recoverable stale-ticket errors
    /// may be replanned at a newer version.
    #[default]
    Current,
    /// Plan at exactly this database version. The server never falls forward.
    Pinned(u64),
}

/// Errors returned before the prepared query enters Tantivy.
#[derive(Debug, Error)]
pub enum RemotePrepareError {
    #[error(transparent)]
    Flight(#[from] FlightError),

    #[error(transparent)]
    Prepare(#[from] PrepareError),

    #[error("remote query promises {promised} matches, exceeding the configured limit {limit}")]
    TooManyMatches { promised: u64, limit: u64 },

    #[error("remote query returned no UInt64 `ordinal` column")]
    MissingOrdinalColumn,

    #[error("remote query returned a null ordinal")]
    NullOrdinal,

    #[error("remote ordinals are not strictly increasing: {previous} then {current}")]
    OrdinalsNotStrictlyIncreasing { previous: u64, current: u64 },

    #[error("remote query promised {promised} ordinals but returned {received}")]
    CardinalityMismatch { promised: u64, received: u64 },
}

/// A bounded, retry-aware remote yesno query plan.
#[derive(Clone, Debug)]
pub struct FlightQueryPreparer {
    expression: SetExpr,
    consistency: Consistency,
    max_matches: u64,
    missing: MissingOrdinalPolicy,
    score: f32,
    max_retries: u32,
}

impl FlightQueryPreparer {
    /// Create a current-snapshot plan with a 10-million-ordinal materialization
    /// ceiling, strict missing-ID handling, zero score, and one retry.
    pub fn new(expression: SetExpr) -> Self {
        Self {
            expression,
            consistency: Consistency::Current,
            max_matches: 10_000_000,
            missing: MissingOrdinalPolicy::Error,
            score: 0.0,
            max_retries: 1,
        }
    }

    pub fn consistency(mut self, consistency: Consistency) -> Self {
        self.consistency = consistency;
        self
    }

    pub fn pinned(self, version: u64) -> Self {
        self.consistency(Consistency::Pinned(version))
    }

    pub fn max_matches(mut self, max_matches: u64) -> Self {
        self.max_matches = max_matches;
        self
    }

    pub fn missing_ordinals(mut self, missing: MissingOrdinalPolicy) -> Self {
        self.missing = missing;
        self
    }

    pub fn score(mut self, score: f32) -> Self {
        self.score = score;
        self
    }

    /// Set the number of complete re-plans allowed after the initial attempt.
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    pub fn expression(&self) -> &SetExpr {
        &self.expression
    }

    pub fn consistency_mode(&self) -> Consistency {
        self.consistency
    }

    /// Fetch, validate, resolve, and bind the remote result to `searcher`.
    pub async fn prepare<R>(
        &self,
        client: &mut YesnoClient<Channel>,
        searcher: &Searcher,
        resolver: &R,
    ) -> Result<PreparedYesnoQuery, RemotePrepareError>
    where
        R: OrdinalResolver + ?Sized,
    {
        let mut retries = 0;
        loop {
            match self.prepare_once(client, searcher, resolver).await {
                Ok(query) => return Ok(query),
                Err(error) if retries < self.max_retries && self.retryable(&error) => {
                    retries += 1;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn prepare_once<R>(
        &self,
        client: &mut YesnoClient<Channel>,
        searcher: &Searcher,
        resolver: &R,
    ) -> Result<PreparedYesnoQuery, RemotePrepareError>
    where
        R: OrdinalResolver + ?Sized,
    {
        let info = match self.consistency {
            Consistency::Current => client.prepare_query(&self.expression).await?,
            Consistency::Pinned(version) => {
                client.prepare_query_at(&self.expression, version).await?
            }
        };
        let promised = info.total_records();
        if promised > self.max_matches || usize::try_from(promised).is_err() {
            return Err(RemotePrepareError::TooManyMatches {
                promised,
                limit: self.max_matches,
            });
        }

        let version = info.version();
        let mut batches = client.fetch(&info).await?;
        let mut ordinals = Vec::with_capacity(promised as usize);
        let mut previous = None;
        while let Some(batch) = batches.try_next().await? {
            let column = batch
                .column_by_name("ordinal")
                .and_then(|column| column.as_any().downcast_ref::<UInt64Array>())
                .ok_or(RemotePrepareError::MissingOrdinalColumn)?;
            if column.null_count() != 0 {
                return Err(RemotePrepareError::NullOrdinal);
            }
            for &ordinal in column.values() {
                if let Some(previous) = previous {
                    if ordinal <= previous {
                        return Err(RemotePrepareError::OrdinalsNotStrictlyIncreasing {
                            previous,
                            current: ordinal,
                        });
                    }
                }
                previous = Some(ordinal);
                ordinals.push(ordinal);
                if ordinals.len() as u64 > promised {
                    return Err(RemotePrepareError::CardinalityMismatch {
                        promised,
                        received: ordinals.len() as u64,
                    });
                }
            }
        }

        let received = ordinals.len() as u64;
        if received != promised {
            return Err(RemotePrepareError::CardinalityMismatch { promised, received });
        }

        PreparedYesnoQuery::from_ordinals(
            searcher,
            resolver,
            ordinals,
            self.missing,
            Some(version),
        )?
        .with_score(self.score)
        .map_err(RemotePrepareError::from)
    }

    fn retryable(&self, error: &RemotePrepareError) -> bool {
        let RemotePrepareError::Flight(FlightError::Tonic(status)) = error else {
            return false;
        };
        match status.code() {
            Code::Unavailable | Code::DeadlineExceeded => true,
            Code::Aborted | Code::FailedPrecondition => {
                matches!(self.consistency, Consistency::Current)
            }
            _ => false,
        }
    }
}

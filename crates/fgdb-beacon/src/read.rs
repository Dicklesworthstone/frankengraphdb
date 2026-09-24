//! Read requests shared by Beacon's database adapters. These are in-memory
//! plans, not durable index definitions, authority grants or certificates.
//!
//! Property/label keys stay generic so this search crate does not acquire a
//! storage dependency. The embedded adapter instantiates them with the native
//! PropertyKeyId/LabelId types. Coordinate ORDER is semantic and never sorted.

use fgdb_types::{CanonicalScalar, CommitSeq, VId};

use crate::{
    BeaconError, ExactHybridHit, ExactHybridQuery, IndexConfig, IndexDocument, IndexSnapshot,
    Neighbor, TextHit, TextMatch, VectorSearch, WorkControl,
};

/// Explicit scalar-property projection. Missing/null text omits that lane;
/// ANY missing/null coordinate omits the vector lane. A present wrong-typed
/// value refuses the read. Absent lanes are never filled with synthetic zeros.
///
/// Numeric coordinates must be finite and exactly representable as f32. This
/// is not an encoding for a vector-valued durable property and never silently
/// rounds a stored f64, integer or Decimal128. Repeated keys deliberately
/// repeat coordinates. A caller must mask keys BEFORE resolving their values.
#[derive(Clone, Debug)]
pub struct Projection<K> {
    pub text: Option<K>,
    pub vector: Vec<K>,
}

/// One allowance covers source traversal, scalar projection, construction,
/// search, and final return. Scratch entries count source admission events,
/// not RSS; staging rows count borrowed historical winners, not copied rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadPolicy {
    pub max_work_units: usize,
    pub max_source_scratch: usize,
    pub max_staging_rows: usize,
    pub max_result_rows: usize,
}

impl Default for ReadPolicy {
    fn default() -> Self {
        Self {
            max_work_units: 100_000_000,
            max_source_scratch: 100_000,
            max_staging_rows: 100_000,
            max_result_rows: 100_000,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ReadOptions<K, L> {
    pub as_of: Option<CommitSeq>,
    pub vertex_label: Option<L>,
    pub projection: Projection<K>,
    pub index: IndexConfig,
    pub policy: ReadPolicy,
}

impl<K, L> ReadOptions<K, L> {
    #[must_use]
    pub fn text(property: K) -> Self {
        Self {
            as_of: None,
            vertex_label: None,
            projection: Projection {
                text: Some(property),
                vector: Vec::new(),
            },
            index: IndexConfig::default(),
            policy: ReadPolicy::default(),
        }
    }

    /// Disable unused lanes before validation or source access. A text-only
    /// request must not observe invalid, expensive or unauthorized vectors.
    pub fn config_for(&self, query: Search<'_>) -> Result<IndexConfig, BeaconError> {
        let (vector, text) = query.lanes();
        let mut config = self.index.clone();
        if !vector {
            config.vector = None;
        }
        if !text {
            config.text = None;
        }
        config.validate()?;
        if text && (config.text.is_none() || self.projection.text.is_none()) {
            return Err(BeaconError::InvalidConfig(
                "text search needs a text property and lane",
            ));
        }
        if vector {
            let vector = config
                .vector
                .as_ref()
                .ok_or(BeaconError::Disabled("vector"))?;
            if self.projection.vector.len() != vector.dimensions {
                return Err(BeaconError::InvalidConfig(
                    "vector properties must match dimensions",
                ));
            }
        }
        if query.k() > self.policy.max_result_rows {
            return Err(BeaconError::ResourceLimit {
                resource: "result rows",
                limit: self.policy.max_result_rows,
            });
        }
        Ok(config)
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Search<'a> {
    Text {
        query: &'a str,
        k: usize,
        mode: TextMatch,
    },
    Vector {
        query: &'a [f32],
        k: usize,
        mode: VectorSearch,
    },
    /// Exact rational fusion of the requested candidate sets; NOT an
    /// exhaustive hybrid answer or an AnswerContract::Exact certificate.
    Hybrid(ExactHybridQuery<'a>),
}

impl Search<'_> {
    #[must_use]
    pub fn k(self) -> usize {
        match self {
            Self::Text { k, .. } | Self::Vector { k, .. } => k,
            Self::Hybrid(query) => query.k,
        }
    }

    #[must_use]
    pub fn lanes(self) -> (bool, bool) {
        match self {
            Self::Text { .. } => (false, true),
            Self::Vector { .. } => (true, false),
            Self::Hybrid(query) => (
                query.profile.vector_weight() != 0,
                query.profile.text_weight() != 0,
            ),
        }
    }

    /// Validate native query parameters before reading the corpus. No search
    /// mode changes here: exact/approximate selection remains caller-owned.
    pub fn validate(
        self,
        config: &IndexConfig,
        work: &mut dyn WorkControl,
    ) -> Result<(), BeaconError> {
        work.charge(1)?;
        let vector = |query: &[f32], mode, work: &mut dyn WorkControl| {
            config
                .vector
                .as_ref()
                .ok_or(BeaconError::Disabled("vector"))?
                .validate_vector(query, work)?;
            if matches!(mode, VectorSearch::Approximate { ef_search: 0 }) {
                return Err(BeaconError::InvalidQuery("ef_search must be positive"));
            }
            Ok(())
        };
        match self {
            Self::Vector { query, mode, .. } => vector(query, mode, work),
            Self::Text { .. } => Ok(()),
            Self::Hybrid(query) => {
                let (v, t) = self.lanes();
                let count = u64::from(query.vector_candidates) * u64::from(v)
                    + u64::from(query.text_candidates) * u64::from(t);
                if query.k as u128 > u128::from(count) {
                    return Err(BeaconError::InvalidQuery(
                        "RRF k exceeds active candidate depths",
                    ));
                }
                if v && query.vector_candidates != 0 {
                    vector(query.vector, query.vector_mode, work)?;
                }
                Ok(())
            }
        }
    }

    pub fn execute(
        self,
        snapshot: &IndexSnapshot,
        work: &mut dyn WorkControl,
    ) -> Result<Rows, BeaconError> {
        // This adapter accepts an already restricted corpus, not an ACL-shaped
        // output predicate. Every source lane sees the same entire domain.
        match self {
            Self::Text { query, k, mode } => snapshot
                .text_search(query, k, mode, |_| true, work)
                .map(Rows::Text),
            Self::Vector { query, k, mode } => snapshot
                .knn(query, k, mode, |_| true, work)
                .map(Rows::Vector),
            Self::Hybrid(query) => snapshot
                .hybrid_search_exact_fusion(query, |_| true, work)
                .map(Rows::Hybrid),
        }
    }
}

/// Only search hits leave an execution; private corpus statistics and index
/// topology are not returned. Raw scores retain their native numeric domains.
#[derive(Clone, Debug, PartialEq)]
pub enum Rows {
    Text(Vec<TextHit>),
    Vector(Vec<Neighbor>),
    Hybrid(Vec<ExactHybridHit>),
}

impl Rows {
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Text(rows) => rows.len(),
            Self::Vector(rows) => rows.len(),
            Self::Hybrid(rows) => rows.len(),
        }
    }
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Keep native read/frontier and interruption causes instead of flattening
/// them to an index failure. Warden adapters use their typed QueryError as C.
#[derive(Debug)]
pub enum ReadError<R, C> {
    Read(R),
    Interrupted(C),
    Index(BeaconError),
}
impl<R: core::fmt::Display, C: core::fmt::Display> core::fmt::Display for ReadError<R, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Read(e) => e.fmt(f),
            Self::Interrupted(e) => e.fmt(f),
            Self::Index(e) => e.fmt(f),
        }
    }
}
impl<R: core::error::Error + 'static, C: core::error::Error + 'static> core::error::Error
    for ReadError<R, C>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Read(e) => Some(e),
            Self::Interrupted(e) => Some(e),
            Self::Index(e) => Some(e),
        }
    }
}

impl<K: Copy> Projection<K> {
    /// Resolve one selected historical row, under an already narrowed key
    /// resolver. Copy at most one document at a time; the builder owns totals.
    pub fn project<'a>(
        &self,
        id: VId,
        config: &IndexConfig,
        mut property: impl FnMut(K) -> Option<&'a CanonicalScalar>,
        work: &mut dyn WorkControl,
    ) -> Result<IndexDocument, BeaconError> {
        work.charge(1)?;
        let text = if config.text.is_some() {
            match self.text.and_then(&mut property) {
                None | Some(CanonicalScalar::Null) => None,
                Some(CanonicalScalar::Text(text)) => {
                    let text = text.as_str();
                    if text.len() > config.max_text_bytes {
                        return Err(BeaconError::ResourceLimit {
                            resource: "staged text bytes",
                            limit: config.max_text_bytes,
                        });
                    }
                    work.charge(text.len())?;
                    let mut owned = String::new();
                    owned.try_reserve_exact(text.len()).map_err(|_| {
                        BeaconError::ResourceLimit {
                            resource: "text allocation",
                            limit: config.max_text_bytes,
                        }
                    })?;
                    owned.push_str(text);
                    Some(owned)
                }
                Some(_) => {
                    return Err(BeaconError::InvalidQuery(
                        "indexed text property must be text",
                    ));
                }
            }
        } else {
            None
        };
        let vector = if let Some(vector) = &config.vector {
            if self.vector.len() != vector.dimensions {
                return Err(BeaconError::InvalidConfig(
                    "vector properties must match dimensions",
                ));
            }
            if self.vector.len() > config.max_vector_values {
                return Err(BeaconError::ResourceLimit {
                    resource: "staged vector values",
                    limit: config.max_vector_values,
                });
            }
            // Establish completeness before validating any coordinate. An
            // absent (including capability-masked) lane is uniformly absent.
            let mut complete = true;
            for &key in &self.vector {
                work.charge(1)?;
                if matches!(property(key), None | Some(CanonicalScalar::Null)) {
                    complete = false;
                }
            }
            if complete {
                let mut values = Vec::new();
                values.try_reserve_exact(self.vector.len()).map_err(|_| {
                    BeaconError::ResourceLimit {
                        resource: "vector allocation",
                        limit: config.max_vector_values,
                    }
                })?;
                for &key in &self.vector {
                    work.charge(1)?;
                    let value = match property(key) {
                        Some(CanonicalScalar::Int(value)) => {
                            let narrowed = *value as f32;
                            if narrowed as i128 != i128::from(*value) {
                                return Err(BeaconError::InvalidQuery(
                                    "vector integer is not exactly representable as f32",
                                ));
                            }
                            narrowed
                        }
                        Some(CanonicalScalar::Float(value)) => {
                            let value = value.get();
                            let narrowed = value as f32;
                            if !narrowed.is_finite() || f64::from(narrowed) != value {
                                return Err(BeaconError::InvalidQuery(
                                    "vector float is not finite exact f32",
                                ));
                            }
                            narrowed
                        }
                        _ => {
                            return Err(BeaconError::InvalidQuery(
                                "vector coordinates must be integers or floats",
                            ));
                        }
                    };
                    values.push(value);
                }
                Some(values)
            } else {
                None
            }
        } else {
            None
        };
        work.charge(1)?;
        Ok(IndexDocument { id, vector, text })
    }
}

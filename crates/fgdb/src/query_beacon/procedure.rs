//! `CALL hybrid.search(name => value, ...) YIELD node, score, ...`: Beacon's
//! exact reciprocal-rank retrieval as the procedure source of a native read,
//! so GraphRAG retrieval composes with the rest of the statement
//! (`YIELD node, score RETURN node.name, score`) at the read's own snapshot.
//!
//! The corpus is the EXPLICIT projection the arguments name, exactly as the
//! CLI's `search` flags do: `text` over `text_property` (BM25, `text_match`
//! any/all/phrase/fuzzy1/fuzzy2/fuzzy1-all/fuzzy2-all, `max_expansions`),
//! `vector` over one `vector_properties` entry per coordinate (`metric`
//! l2/cosine/dot, exact unless `ann` gives the HNSW `ef_search`), restricted
//! to `label` when given. `seeds` with an explicit `max_hops` add the graph
//! lane (`relation`, `direction` out/in/both, `include_seeds`,
//! `graph_candidates`, `graph_weight`). Schema names resolve at prepare time
//! (`fgdb_gql` symbol arguments), so a prepared call never re-resolves.
//!
//! Every requested lane is fused by exact RRF (`fusion => 'RRF'`, the only
//! profile; `vector_weight`/`text_weight` default 1, rank constant 60), so
//! `score` always means the same thing: the fused Decimal score. Each lane's
//! own rank and score are separate outputs, null where that lane did not
//! return the vertex. Rows arrive best first, ties by ascending vertex.
//!
//! The index is built for this one execution from the read's snapshot. This
//! is exact fusion of the selected candidates, not an exhaustive hybrid
//! top-k; `ann` remains approximate. It is not a durable index definition.

use super::graph;
use super::{Meter, Options, Scan};
use crate::query::ProcedureError;
use crate::{GqlError, ReadError, Snapshot};
use fgdb_beacon::expansion::{ExpansionDirection, ExpansionLimits, ExpansionSpec};
use fgdb_beacon::read::{Projection, ReadError as BeaconReadError, ReadPolicy};
use fgdb_beacon::{
    BeaconError, DistanceMetric, EditDistance, ExactHybridQuery, ExactRrfProfile, GraphHybridHit,
    GraphHybridQuery, HnswConfig, IndexConfig, TextMatch, VectorSearch,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::{GraphValue, GraphValueRow};
use fgdb_gql::{
    GlaExecutionStats, GqlExecutionStats, GqlQueryError, GqlQueryExecution, GqlQueryPolicy,
    PreparedProcedureCall,
};
use fgdb_types::{CanonicalF64, CanonicalScalar, CommitSeq, QueryCx, VId};
use std::cell::{Cell, RefCell};

type Cancel = Box<asupersync::error::Error>;

/// The procedure's outputs, in the order a bare `YIELD *` would list them.
pub const HYBRID_SEARCH_OUTPUTS: [&str; 8] = [
    "node",
    "score",
    "vector_rank",
    "text_rank",
    "graph_rank",
    "vector_distance",
    "text_score",
    "graph_hops",
];

/// Vocabulary terms a fuzzy match may expand to unless `max_expansions`
/// says otherwise, as for the CLI. Exceeding it refuses, never truncates.
const DEFAULT_MAX_EXPANSIONS: usize = 64;

/// Why a `CALL hybrid.*` stage was refused before or during retrieval.
#[derive(Debug)]
pub enum HybridCallError {
    /// The namespace has exactly one procedure, `hybrid.search`.
    UnknownProcedure(String),
    /// `hybrid.search` takes named arguments only (`k => 10`).
    Positional,
    /// No such argument.
    UnknownArgument(String),
    /// An argument outside its domain.
    Argument {
        name: &'static str,
        expected: &'static str,
    },
    /// Arguments that need each other, or exclude each other.
    Combination(&'static str),
    /// A YIELD name the procedure does not output.
    Yield(String),
    /// The statement reads this output as a vertex; only `node` is one.
    NotVertex(String),
    /// Beacon refused the query, its projection or its index construction.
    Index(BeaconError),
}

impl core::fmt::Display for HybridCallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UnknownProcedure(name) => {
                write!(f, "no procedure hybrid.{name}; the namespace has search")
            }
            Self::Positional => f.write_str("hybrid.search takes named arguments (k => 10)"),
            Self::UnknownArgument(name) => write!(f, "hybrid.search has no argument {name:?}"),
            Self::Argument { name, expected } => {
                write!(f, "hybrid.search argument {name} must be {expected}")
            }
            Self::Combination(rule) => write!(f, "hybrid.search: {rule}"),
            Self::Yield(name) => write!(f, "hybrid.search does not output {name:?}"),
            Self::NotVertex(name) => write!(
                f,
                "hybrid.search output {name:?} is not a vertex; only node is"
            ),
            Self::Index(error) => write!(f, "hybrid.search: {error}"),
        }
    }
}

impl core::error::Error for HybridCallError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Index(error) => Some(error),
            _ => None,
        }
    }
}

/// Whether a CALL stage belongs to this host rather than Prism.
pub(crate) fn is_hybrid(call: &PreparedProcedureCall) -> bool {
    call.namespace() == "hybrid"
}

pub(crate) fn refuse<C>(error: HybridCallError) -> GqlQueryError<GqlError, C> {
    GqlQueryError::Source(GqlError::Procedure(ProcedureError::Search(error)))
}

/// A bound `hybrid.search`: the frozen corpus definition and query inputs.
pub(crate) struct HybridSearch {
    options: Options,
    text: String,
    text_mode: TextMatch,
    vector: Vec<f32>,
    vector_mode: VectorSearch,
    k: usize,
    candidates: u32,
    profile: ExactRrfProfile,
    seeds: Vec<VId>,
    relation: Option<RelationId>,
    direction: ExpansionDirection,
    max_hops: u32,
    include_seeds: bool,
    graph_candidates: u32,
    graph_weight: u16,
    /// For each YIELD column, its position in [`HYBRID_SEARCH_OUTPUTS`].
    outputs: Vec<usize>,
}

fn domain(name: &'static str, expected: &'static str) -> HybridCallError {
    HybridCallError::Argument { name, expected }
}

fn text_value<'v>(name: &'static str, value: &'v GraphValue) -> Result<&'v str, HybridCallError> {
    match value {
        GraphValue::Scalar(CanonicalScalar::Text(text)) => Ok(text.as_str()),
        _ => Err(domain(name, "text")),
    }
}

fn int_value(name: &'static str, value: &GraphValue) -> Result<i64, HybridCallError> {
    match value {
        GraphValue::Scalar(CanonicalScalar::Int(value)) => Ok(*value),
        _ => Err(domain(name, "an integer")),
    }
}

fn positive<T: TryFrom<i64>>(name: &'static str, value: &GraphValue) -> Result<T, HybridCallError> {
    let value = int_value(name, value)?;
    (value > 0)
        .then(|| T::try_from(value).ok())
        .flatten()
        .ok_or(domain(name, "a positive integer in range"))
}

/// A symbol argument arrives as the identity its name resolved to.
fn identity(name: &'static str, value: &GraphValue) -> Result<u64, HybridCallError> {
    u64::try_from(int_value(name, value)?).map_err(|_| domain(name, "a schema name"))
}

fn list<'v>(
    name: &'static str,
    value: &'v GraphValue,
) -> Result<&'v [GraphValue], HybridCallError> {
    match value {
        GraphValue::List(items) => Ok(items),
        _ => Err(domain(name, "a list")),
    }
}

impl HybridSearch {
    /// Bind the call's named arguments and YIELD list. `at` pins the read's
    /// snapshot; `remaining` caps Beacon's own work and staging allowances
    /// by what the read has left, so retrieval cannot outspend the statement.
    pub(crate) fn bind(
        call: &PreparedProcedureCall,
        arguments: &[GraphValue],
        at: CommitSeq,
        remaining: GqlQueryPolicy,
    ) -> Result<Self, HybridCallError> {
        if call.name() != "search" {
            return Err(HybridCallError::UnknownProcedure(call.name().to_owned()));
        }
        if call.argument_names().len() != arguments.len() {
            return Err(HybridCallError::Positional);
        }
        let mut text = None;
        let mut text_property = None;
        let mut text_match = None;
        let mut max_expansions = None;
        let mut vector = None;
        let mut vector_properties = None;
        let mut metric = None;
        let mut ann = None;
        let mut label = None;
        let mut k = None;
        let mut candidates = None;
        let mut fusion = false;
        let mut vector_weight = None;
        let mut text_weight = None;
        let mut seeds = None;
        let mut relation = None;
        let mut direction = None;
        let mut max_hops = None;
        let mut include_seeds = None;
        let mut graph_candidates = None;
        let mut graph_weight = None;
        for (name, value) in call.argument_names().iter().zip(arguments) {
            // A null argument is an absent one, so `vector => $q` with a
            // null $q runs the remaining lanes.
            if matches!(value, GraphValue::Scalar(CanonicalScalar::Null)) {
                continue;
            }
            match name.as_str() {
                "text" => text = Some(text_value("text", value)?.to_owned()),
                "text_property" => {
                    text_property = Some(PropertyKeyId(identity("text_property", value)?));
                }
                "text_match" => {
                    let fuzzy = |distance, require_all| TextMatch::Fuzzy {
                        distance,
                        require_all,
                        max_expansions: DEFAULT_MAX_EXPANSIONS,
                    };
                    text_match = Some(match text_value("text_match", value)? {
                        "any" => TextMatch::Any,
                        "all" => TextMatch::All,
                        "phrase" => TextMatch::Phrase,
                        "fuzzy1" => fuzzy(EditDistance::One, false),
                        "fuzzy2" => fuzzy(EditDistance::Two, false),
                        "fuzzy1-all" => fuzzy(EditDistance::One, true),
                        "fuzzy2-all" => fuzzy(EditDistance::Two, true),
                        _ => {
                            return Err(domain(
                                "text_match",
                                "any, all, phrase, fuzzy1, fuzzy2, fuzzy1-all or fuzzy2-all",
                            ));
                        }
                    });
                }
                "max_expansions" => {
                    max_expansions = Some(positive::<usize>("max_expansions", value)?);
                }
                "vector" => {
                    let mut coordinates = Vec::new();
                    for item in list("vector", value)? {
                        let coordinate = match item {
                            GraphValue::Scalar(CanonicalScalar::Float(x)) => x.get() as f32,
                            GraphValue::Scalar(CanonicalScalar::Int(x)) => *x as f32,
                            _ => return Err(domain("vector", "a list of finite numbers")),
                        };
                        if !coordinate.is_finite() {
                            return Err(domain("vector", "a list of finite numbers"));
                        }
                        coordinates.push(coordinate);
                    }
                    vector = Some(coordinates);
                }
                "vector_properties" => {
                    let mut keys = Vec::new();
                    for item in list("vector_properties", value)? {
                        let key = PropertyKeyId(identity("vector_properties", item)?);
                        if keys.contains(&key) {
                            return Err(domain("vector_properties", "distinct properties"));
                        }
                        keys.push(key);
                    }
                    vector_properties = Some(keys);
                }
                "metric" => {
                    metric = Some(match text_value("metric", value)? {
                        "l2" => DistanceMetric::SquaredEuclidean,
                        "cosine" => DistanceMetric::Cosine,
                        "dot" => DistanceMetric::NegativeDotProduct,
                        _ => return Err(domain("metric", "l2, cosine or dot")),
                    });
                }
                "ann" => ann = Some(positive::<usize>("ann", value)?),
                "label" => label = Some(LabelId(identity("label", value)?)),
                "k" => k = Some(positive::<usize>("k", value)?),
                "candidates" => candidates = Some(positive::<u32>("candidates", value)?),
                "fusion" => {
                    if !text_value("fusion", value)?.eq_ignore_ascii_case("RRF") {
                        return Err(domain("fusion", "'RRF'"));
                    }
                    fusion = true;
                }
                "vector_weight" => vector_weight = Some(positive::<u16>("vector_weight", value)?),
                "text_weight" => text_weight = Some(positive::<u16>("text_weight", value)?),
                "seeds" => {
                    let mut vertices = Vec::new();
                    for item in list("seeds", value)? {
                        vertices.push(match item {
                            GraphValue::Vertex(vertex) => *vertex,
                            GraphValue::Scalar(CanonicalScalar::Int(id)) if *id >= 0 => {
                                VId(*id as u128)
                            }
                            _ => return Err(domain("seeds", "a list of vertices")),
                        });
                    }
                    seeds = Some(vertices);
                }
                "relation" => relation = Some(RelationId(identity("relation", value)?)),
                "direction" => {
                    direction = Some(match text_value("direction", value)? {
                        "out" => ExpansionDirection::Outgoing,
                        "in" => ExpansionDirection::Incoming,
                        "both" => ExpansionDirection::Undirected,
                        _ => return Err(domain("direction", "out, in or both")),
                    });
                }
                "max_hops" => max_hops = Some(positive::<u32>("max_hops", value)?),
                "include_seeds" => {
                    include_seeds = Some(match value {
                        GraphValue::Scalar(CanonicalScalar::Bool(value)) => *value,
                        _ => return Err(domain("include_seeds", "a boolean")),
                    });
                }
                "graph_candidates" => {
                    graph_candidates = Some(positive::<u32>("graph_candidates", value)?);
                }
                "graph_weight" => graph_weight = Some(positive::<u16>("graph_weight", value)?),
                other => return Err(HybridCallError::UnknownArgument(other.to_owned())),
            }
        }
        let _ = fusion;
        let (text, text_property) = match (text, text_property) {
            (Some(text), Some(key)) => (Some(text), Some(key)),
            (None, None) => (None, None),
            _ => {
                return Err(HybridCallError::Combination(
                    "text and text_property are given together",
                ));
            }
        };
        let vector = match (vector, vector_properties) {
            (Some(coordinates), Some(keys)) => {
                if coordinates.len() != keys.len() || keys.is_empty() {
                    return Err(HybridCallError::Combination(
                        "vector needs exactly one vector_properties entry per coordinate",
                    ));
                }
                Some((coordinates, keys))
            }
            (None, None) => None,
            _ => {
                return Err(HybridCallError::Combination(
                    "vector and vector_properties are given together",
                ));
            }
        };
        if text.is_none() && vector.is_none() {
            return Err(HybridCallError::Combination(
                "a search needs text with text_property, vector with vector_properties, or both",
            ));
        }
        if text.is_none()
            && (text_match.is_some() || max_expansions.is_some() || text_weight.is_some())
        {
            return Err(HybridCallError::Combination(
                "text_match, max_expansions and text_weight need text",
            ));
        }
        if vector.is_none() && (metric.is_some() || ann.is_some() || vector_weight.is_some()) {
            return Err(HybridCallError::Combination(
                "metric, ann and vector_weight need vector",
            ));
        }
        let text_mode = match (text_match.unwrap_or(TextMatch::Any), max_expansions) {
            (
                TextMatch::Fuzzy {
                    distance,
                    require_all,
                    ..
                },
                Some(max_expansions),
            ) => TextMatch::Fuzzy {
                distance,
                require_all,
                max_expansions,
            },
            (mode, None) => mode,
            (_, Some(_)) => {
                return Err(HybridCallError::Combination(
                    "max_expansions applies only to a fuzzy text_match",
                ));
            }
        };
        let graph = seeds.is_some();
        if !graph
            && (max_hops.is_some()
                || relation.is_some()
                || direction.is_some()
                || include_seeds.is_some()
                || graph_candidates.is_some()
                || graph_weight.is_some())
        {
            return Err(HybridCallError::Combination(
                "max_hops, relation, direction, include_seeds, graph_candidates and graph_weight need seeds",
            ));
        }
        let max_hops = match (graph, max_hops) {
            (true, None) => {
                return Err(HybridCallError::Combination(
                    "seeds need an explicit max_hops",
                ));
            }
            (_, hops) => hops.unwrap_or(0),
        };
        let k = k.unwrap_or(10);
        let candidates = match candidates {
            Some(candidates) => candidates,
            None => u32::try_from(k).map_err(|_| domain("k", "a count within u32"))?,
        };
        let profile = ExactRrfProfile::new(
            60,
            if vector.is_some() {
                vector_weight.unwrap_or(1)
            } else {
                0
            },
            if text.is_some() {
                text_weight.unwrap_or(1)
            } else {
                0
            },
        )
        .map_err(HybridCallError::Index)?;
        let mut outputs = Vec::with_capacity(call.outputs().len());
        for output in call.outputs() {
            let position = HYBRID_SEARCH_OUTPUTS
                .iter()
                .position(|known| known == output)
                .ok_or_else(|| HybridCallError::Yield(output.clone()))?;
            outputs.push(position);
        }
        for &column in call.vertex_outputs() {
            if outputs.get(column) != Some(&0) {
                let name = call.outputs().get(column).cloned().unwrap_or_default();
                return Err(HybridCallError::NotVertex(name));
            }
        }
        let default = ReadPolicy::default();
        let records = remaining
            .rows
            .max_snapshot_records()
            .map_or(usize::MAX, |records| {
                usize::try_from(records).unwrap_or(usize::MAX)
            });
        let options = Options {
            as_of: Some(at),
            vertex_label: label,
            projection: Projection {
                text: text_property,
                vector: vector
                    .as_ref()
                    .map(|(_, keys)| keys.clone())
                    .unwrap_or_default(),
            },
            index: IndexConfig {
                vector: vector.as_ref().map(|(coordinates, _)| {
                    HnswConfig::new(
                        coordinates.len(),
                        metric.unwrap_or(DistanceMetric::SquaredEuclidean),
                    )
                }),
                ..IndexConfig::default()
            },
            policy: ReadPolicy {
                max_work_units: default
                    .max_work_units
                    .min(usize::try_from(remaining.evaluator.max_work_units).unwrap_or(usize::MAX)),
                max_staging_rows: default.max_staging_rows.min(records),
                ..default
            },
        };
        Ok(Self {
            options,
            text: text.unwrap_or_default(),
            text_mode,
            vector: vector
                .map(|(coordinates, _)| coordinates)
                .unwrap_or_default(),
            vector_mode: ann.map_or(VectorSearch::Exact, |ef_search| VectorSearch::Approximate {
                ef_search,
            }),
            k,
            candidates,
            profile,
            seeds: seeds.unwrap_or_default(),
            relation,
            direction: direction.unwrap_or(ExpansionDirection::Outgoing),
            max_hops,
            include_seeds: include_seeds.unwrap_or(false),
            graph_candidates: if graph {
                graph_candidates.unwrap_or(candidates)
            } else {
                0
            },
            graph_weight: if graph { graph_weight.unwrap_or(1) } else { 0 },
            outputs,
        })
    }

    pub(crate) fn options(&self) -> &Options {
        &self.options
    }

    /// The fused query. A lane that was not requested has weight zero and is
    /// neither validated nor searched; without seeds the graph lane is off.
    pub(crate) fn query(&self) -> GraphHybridQuery<'_> {
        GraphHybridQuery {
            retrieval: ExactHybridQuery {
                vector: &self.vector,
                text: &self.text,
                k: self.k,
                vector_candidates: if self.profile.vector_weight() == 0 {
                    0
                } else {
                    self.candidates
                },
                text_candidates: if self.profile.text_weight() == 0 {
                    0
                } else {
                    self.candidates
                },
                vector_mode: self.vector_mode,
                text_mode: self.text_mode,
                profile: self.profile,
            },
            graph_candidates: self.graph_candidates,
            graph_weight: self.graph_weight,
        }
    }

    pub(crate) fn expansion(&self) -> ExpansionSpec<'_, RelationId> {
        ExpansionSpec {
            seeds: &self.seeds,
            relation: self.relation,
            direction: self.direction,
            max_hops: self.max_hops,
            include_seeds: self.include_seeds,
            limits: ExpansionLimits::default(),
        }
    }

    /// Fused hits as rows of exactly the YIELD columns, best first.
    pub(crate) fn rows(&self, hits: Vec<GraphHybridHit>) -> Vec<GraphValueRow> {
        let rank = |rank: Option<core::num::NonZeroU32>| {
            rank.map_or(GraphValue::Scalar(CanonicalScalar::Null), |rank| {
                GraphValue::Scalar(CanonicalScalar::Int(i64::from(rank.get())))
            })
        };
        let float = |value: Option<f64>| {
            value.map_or(GraphValue::Scalar(CanonicalScalar::Null), |value| {
                GraphValue::Scalar(CanonicalScalar::Float(CanonicalF64::new(value)))
            })
        };
        hits.into_iter()
            .map(|hit| {
                let values = self
                    .outputs
                    .iter()
                    .map(|&output| match output {
                        0 => GraphValue::Vertex(hit.id),
                        1 => GraphValue::Scalar(CanonicalScalar::Decimal(hit.decimal_score)),
                        2 => rank(hit.vector_rank),
                        3 => rank(hit.text_rank),
                        4 => rank(hit.graph_rank),
                        5 => float(hit.vector_distance),
                        6 => float(hit.text_score),
                        _ => hit
                            .graph_hops
                            .map_or(GraphValue::Scalar(CanonicalScalar::Null), |hops| {
                                GraphValue::Scalar(CanonicalScalar::Int(i64::from(hops)))
                            }),
                    })
                    .collect();
                GraphValueRow::from_owned_values(values)
            })
            .collect()
    }
}

/// A failed retrieval: the caller's own interruption, or a typed refusal.
pub(crate) fn failure<C>(error: BeaconReadError<ReadError, C>) -> GqlQueryError<GqlError, C> {
    match error {
        BeaconReadError::Interrupted(cancel) => GqlQueryError::Interrupted(cancel),
        BeaconReadError::Index(error) => refuse(HybridCallError::Index(error)),
        BeaconReadError::Read(error) => GqlQueryError::Source(GqlError::Read(error)),
    }
}

/// The privileged host: the whole snapshot at `at`, metered against the
/// read's remaining allowance like every other privileged source.
pub(crate) fn privileged(
    snapshot: &Snapshot,
    at: CommitSeq,
    call: &PreparedProcedureCall,
    arguments: &[GraphValue],
    remaining: GqlQueryPolicy,
    cx: &QueryCx,
) -> Result<GqlQueryExecution<GraphValueRow>, GqlQueryError<GqlError, Cancel>> {
    let search = HybridSearch::bind(call, arguments, at, remaining).map_err(refuse)?;
    let spent = Cell::new(0_u64);
    let admitted = Cell::new(0_u64);
    let work = RefCell::new(Meter::new(search.options.policy.max_work_units, |units| {
        spent.set(spent.get().saturating_add(units as u64));
        cx.checkpoint()
    }));
    let result = graph::evaluate(
        snapshot,
        at,
        &search.options,
        search.query(),
        search.expansion(),
        &work,
        Scan::Metered,
        |_| {
            admitted.set(admitted.get() + 1);
            Ok(true)
        },
        |_| true,
        |_| true,
        |_| true,
    );
    let hits = work
        .into_inner()
        .finish::<ReadError, _>(result)
        .map_err(failure)?;
    let value = search.rows(hits);
    Ok(GqlQueryExecution {
        rows: GqlExecutionStats {
            snapshot_records: admitted.get(),
            result_rows: value.len() as u64,
        },
        evaluator: GlaExecutionStats {
            work_units: spent.get(),
            scratch_entries: value.len() as u64,
        },
        value,
    })
}

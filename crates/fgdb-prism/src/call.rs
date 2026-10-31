//! Frozen, typed CALL programs. Only implemented in-core signatures enter this
//! registry; none of these entries claims external-memory execution support.

use crate::{DijkstraComparison, DijkstraOptions};
use fgdb_crypto::{Digest, Hasher};
use fgdb_types::VId;
use std::collections::BTreeMap;

pub const FNX_SIGNATURE_REGISTRY_VERSION: u16 = 23;
pub const MAX_FNX_CALL_BYTES: usize = 16 * 1024;
pub const FNX_NUMERIC_PROFILE: &str = "fnx-f64-canonical-node-order-v1";
pub const FNX_DISCRETE_PROFILE: &str = "fgdb-exact-integer-canonical-vid-order-v1";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FnxArgument {
    Integer(i64),
    Float(f64),
    Boolean(bool),
    /// Full-width stable identity, never an ordinal or a floating-point value.
    Vertex(VId),
    Null,
}
pub type FnxParameters = BTreeMap<String, FnxArgument>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnxParameterType {
    FiniteFloat,
    PositiveInteger,
    NonNegativeInteger,
    Boolean,
    Vertex,
    OptionalNonNegativeInteger,
    OptionalNonNegativeFloat,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FnxParameterSpec {
    pub name: &'static str,
    pub value_type: FnxParameterType,
    /// Whether this parameter occupies a slot in the positional signature.
    /// Named-only controls do not change existing positional calls.
    pub positional: bool,
    /// A positional parameter without a default is required. A named-only
    /// control without a default is absent unless explicitly supplied.
    pub default: Option<FnxArgument>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FnxOutput {
    Vertex = 0,
    Score = 1,
    Distance = 2,
    Component = 3,
    Triangles = 4,
    /// A vertex's k-core number: an exact count.
    Core = 5,
    Hub = 6,
    Authority = 7,
    /// The other full-width vertex identity in a canonical undirected pair.
    Neighbor = 8,
}
impl FnxOutput {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Vertex => "vertex",
            Self::Score => "score",
            Self::Distance => "distance",
            Self::Component => "component",
            Self::Triangles => "triangles",
            Self::Core => "core",
            Self::Hub => "hub",
            Self::Authority => "authority",
            Self::Neighbor => "neighbor",
        }
    }
    /// Whether `YIELD <name>` selects this field: its registered name, or
    /// `node` for the vertex, the openCypher spelling the README's analytics
    /// statements use (fgdb-luq0b). The column keeps the spelling written, so
    /// `YIELD node` is a column named `node`, and a call spelled either way
    /// keeps its own digest.
    #[must_use]
    pub fn is_named(self, name: &str) -> bool {
        // ubs:ignore -- public YIELD field names, not secret material.
        self.name() == name || (matches!(self, Self::Vertex) && name == "node")
    }
    /// Fields that can bind a vertex in a later native query stage.
    #[must_use]
    pub const fn is_vertex_reference(self) -> bool {
        matches!(self, Self::Vertex | Self::Component | Self::Neighbor)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnxImplementationClass {
    InCoreDecodedCache,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnxGraphKind {
    Any,
    Directed,
    Undirected,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FnxSignature {
    pub name: &'static str,
    pub graph_input_arity: u8,
    pub parameters: &'static [FnxParameterSpec],
    pub outputs: &'static [FnxOutput],
    pub implementation: FnxImplementationClass,
    pub graph_kind: FnxGraphKind,
    pub numeric_profile: &'static str,
    pub rng_policy: &'static str,
    pub graph_laws: &'static str,
    pub complexity: &'static str,
    pub execution_kernel: &'static str,
}

const PAGERANK_PARAMETERS: &[FnxParameterSpec] = &[
    FnxParameterSpec {
        name: "alpha",
        value_type: FnxParameterType::FiniteFloat,
        positional: true,
        default: Some(FnxArgument::Float(0.85)),
    },
    FnxParameterSpec {
        name: "max_iter",
        value_type: FnxParameterType::PositiveInteger,
        positional: true,
        default: Some(FnxArgument::Integer(100)),
    },
    FnxParameterSpec {
        name: "tol",
        value_type: FnxParameterType::FiniteFloat,
        positional: true,
        default: Some(FnxArgument::Float(1e-6)),
    },
    FnxParameterSpec {
        name: "weighted",
        value_type: FnxParameterType::Boolean,
        positional: true,
        default: Some(FnxArgument::Boolean(true)),
    },
    FnxParameterSpec {
        name: "iterations",
        value_type: FnxParameterType::PositiveInteger,
        positional: false,
        default: None,
    },
    FnxParameterSpec {
        name: "l1_tol",
        value_type: FnxParameterType::FiniteFloat,
        positional: false,
        default: None,
    },
    FnxParameterSpec {
        name: "source",
        value_type: FnxParameterType::Vertex,
        positional: false,
        default: None,
    },
];
const BFS_PARAMETERS: &[FnxParameterSpec] = &[
    FnxParameterSpec {
        name: "source",
        value_type: FnxParameterType::Vertex,
        positional: true,
        default: None,
    },
    FnxParameterSpec {
        name: "cutoff",
        value_type: FnxParameterType::OptionalNonNegativeInteger,
        positional: true,
        default: Some(FnxArgument::Null),
    },
];
const DIJKSTRA_PARAMETERS: &[FnxParameterSpec] = &[
    FnxParameterSpec {
        name: "source",
        value_type: FnxParameterType::Vertex,
        positional: true,
        default: None,
    },
    FnxParameterSpec {
        name: "cutoff",
        value_type: FnxParameterType::OptionalNonNegativeFloat,
        positional: true,
        default: Some(FnxArgument::Null),
    },
    FnxParameterSpec {
        name: "strict",
        value_type: FnxParameterType::Boolean,
        positional: true,
        default: Some(FnxArgument::Boolean(false)),
    },
];
const CDLP_PARAMETERS: &[FnxParameterSpec] = &[FnxParameterSpec {
    name: "max_iter",
    value_type: FnxParameterType::NonNegativeInteger,
    positional: true,
    default: None,
}];
const SIGNATURES: &[FnxSignature] = &[
    FnxSignature {
        name: "fnx.pagerank",
        graph_input_arity: 1,
        parameters: PAGERANK_PARAMETERS,
        outputs: &[FnxOutput::Vertex, FnxOutput::Score],
        implementation: FnxImplementationClass::InCoreDecodedCache,
        graph_kind: FnxGraphKind::Any,
        numeric_profile: FNX_NUMERIC_PROFILE,
        rng_policy: "none",
        graph_laws: "explicit projection; finite nonnegative weights when weighted; isolates retained; uniform initial vector; optional named source concentrates teleportation and dangling redistribution at one admitted VId; default convergence uses L1 < N*tol; named l1_tol selects L1 < l1_tol and conflicts with tol/iterations; named iterations selects exact updates and conflicts with max_iter/tol/l1_tol",
        complexity: "O(k * (|V| + |E|))",
        execution_kernel: "fgdb-prism/pagerank-row-cursor-v1",
    },
    FnxSignature {
        name: "fnx.single_source_shortest_path_length",
        graph_input_arity: 1,
        parameters: BFS_PARAMETERS,
        outputs: &[FnxOutput::Vertex, FnxOutput::Distance],
        implementation: FnxImplementationClass::InCoreDecodedCache,
        graph_kind: FnxGraphKind::Any,
        numeric_profile: FNX_DISCRETE_PROFILE,
        rng_policy: "none",
        graph_laws: "explicit projection; unweighted outgoing reachability; inclusive hop cutoff; reachable rows only in VId order",
        complexity: "O(|V| + |E|)",
        execution_kernel: "fgdb-prism/bfs-bounded-pull-v1",
    },
    FnxSignature {
        name: "fnx.connected_components",
        graph_input_arity: 1,
        parameters: &[],
        outputs: &[FnxOutput::Vertex, FnxOutput::Component],
        implementation: FnxImplementationClass::InCoreDecodedCache,
        graph_kind: FnxGraphKind::Undirected,
        numeric_profile: FNX_DISCRETE_PROFILE,
        rng_policy: "none",
        graph_laws: "explicit undirected projection; unweighted; isolates retained; component label is minimum member VId",
        complexity: "O(|V| + |E|)",
        execution_kernel: "fgdb-prism/components-bounded-pull-v1",
    },
    FnxSignature {
        name: "fnx.weakly_connected_components",
        graph_input_arity: 1,
        parameters: &[],
        outputs: &[FnxOutput::Vertex, FnxOutput::Component],
        implementation: FnxImplementationClass::InCoreDecodedCache,
        graph_kind: FnxGraphKind::Directed,
        numeric_profile: FNX_DISCRETE_PROFILE,
        rng_policy: "none",
        graph_laws: "explicit directed projection; successors and predecessors; unweighted; component label is minimum member VId",
        complexity: "O(|V| + |E|)",
        execution_kernel: "fgdb-prism/weak-components-bounded-pull-v1",
    },
    FnxSignature {
        name: "fnx.strongly_connected_components",
        graph_input_arity: 1,
        parameters: &[],
        outputs: &[FnxOutput::Vertex, FnxOutput::Component],
        implementation: FnxImplementationClass::InCoreDecodedCache,
        graph_kind: FnxGraphKind::Directed,
        numeric_profile: FNX_DISCRETE_PROFILE,
        rng_policy: "none",
        graph_laws: "explicit directed projection; unweighted mutual reachability; component label is minimum member VId",
        complexity: "O(|V| + |E|)",
        execution_kernel: "fgdb-prism/kosaraju-row-cursor-v1",
    },
    FnxSignature {
        name: "fnx.single_source_dijkstra_path_length",
        graph_input_arity: 1,
        parameters: DIJKSTRA_PARAMETERS,
        outputs: &[FnxOutput::Vertex, FnxOutput::Distance],
        implementation: FnxImplementationClass::InCoreDecodedCache,
        graph_kind: FnxGraphKind::Any,
        numeric_profile: "dijkstra-f64-policy-selected-by-strict-v1",
        rng_policy: "none",
        graph_laws: "explicit projection; finite nonnegative weights; inclusive cost cutoff; FIFO ties; fnx 1e-12 relaxation unless strict; reachable rows in VId order",
        complexity: "O((|V| + |E|) * log(1 + |V|)); O(|V| + |E|) when final projected weights are all one and |V| <= 2^53",
        execution_kernel: "fgdb-prism/dijkstra-unit-or-indexed-heap-v2",
    },
    FnxSignature {
        name: "fnx.triangles",
        graph_input_arity: 1,
        parameters: &[],
        outputs: &[FnxOutput::Vertex, FnxOutput::Triangles],
        implementation: FnxImplementationClass::InCoreDecodedCache,
        graph_kind: FnxGraphKind::Undirected,
        numeric_profile: FNX_DISCRETE_PROFILE,
        rng_policy: "none",
        graph_laws: "explicit undirected simple projection; unweighted; self-loops ignored; exact per-vertex counts; isolates retained",
        complexity: "O(|V| + |E| + sum_edges min(deg(u), deg(v)))",
        execution_kernel: "fgdb-prism/triangles-degree-forward-csr-v1",
    },
    FnxSignature {
        name: "fnx.clustering_coefficient",
        graph_input_arity: 1,
        parameters: &[],
        outputs: &[FnxOutput::Vertex, FnxOutput::Score],
        implementation: FnxImplementationClass::InCoreDecodedCache,
        graph_kind: FnxGraphKind::Undirected,
        numeric_profile: "fgdb-unweighted-clustering-exact-count-f64-ratio-v1",
        rng_policy: "none",
        graph_laws: "explicit undirected simple projection; unweighted; self-loops ignored; 2*t/(d*(d-1)); degree below two yields zero",
        complexity: "O(|V| + |E| + sum_edges min(deg(u), deg(v)))",
        execution_kernel: "fgdb-prism/clustering-degree-forward-csr-v1",
    },
    foundation(
        "fnx.degree_centrality",
        FnxOutput::Score,
        "explicit undirected simple projection; unweighted; self-loops refused; degree / (|V| - 1); a single vertex scores 1",
        "O(|V| + |E|)",
        "fgdb-prism/degree-centrality-csr-v1",
    ),
    FnxSignature {
        graph_kind: FnxGraphKind::Any,
        ..foundation(
            "fnx.closeness_centrality",
            FnxOutput::Score,
            "unqualified whole-graph input defaults to undirected; explicit directed, reversed or undirected simple projection; unweighted; self-loops refused; incoming hop distances to each vertex, so a reversed projection measures outgoing distances in the stored graph; Wasserman-Faust reachable-set correction; isolates score zero",
            "O(|V| * (|V| + |E|))",
            "fgdb-prism/closeness-incoming-csr-batched-policy-v3",
        )
    },
    FnxSignature {
        graph_kind: FnxGraphKind::Any,
        ..foundation(
            "fnx.harmonic_centrality",
            FnxOutput::Score,
            "unqualified whole-graph input defaults to undirected; explicit directed, reversed or undirected simple projection; unweighted; self-loops refused; sum of reciprocal incoming hop distances from every other reachable vertex, so a reversed projection measures outgoing distances in the stored graph; isolates score zero",
            "O(|V| * (|V| + |E|))",
            "fgdb-prism/harmonic-incoming-csr-batched-policy-v3",
        )
    },
    FnxSignature {
        name: "fnx.betweenness_centrality",
        graph_input_arity: 1,
        parameters: &[],
        outputs: &[FnxOutput::Vertex, FnxOutput::Score],
        implementation: FnxImplementationClass::InCoreDecodedCache,
        graph_kind: FnxGraphKind::Any,
        numeric_profile: FNX_NUMERIC_PROFILE,
        rng_policy: "none",
        graph_laws: "unqualified whole-graph input defaults to undirected; explicit directed, reversed or undirected simple projection; unweighted; self-loops refused; exact Brandes shortest-path betweenness without endpoints; directed ordered-pair normalization by (|V|-1)(|V|-2), undirected unordered-pair normalization by (|V|-1)(|V|-2)/2; isolates retained",
        complexity: "O(|V| * |E|)",
        execution_kernel: "fgdb-prism/brandes-csr-source-order-v4",
    },
    foundation(
        "fnx.eigenvector_centrality",
        FnxOutput::Score,
        "explicit undirected simple projection; unweighted; self-loops refused; power iteration, at most 100 steps to tolerance 1e-6; non-convergence refuses",
        "O(100 * (|V| + |E|))",
        "fgdb-prism/eigenvector-csr-gather-v1",
    ),
    foundation(
        "fnx.core_number",
        FnxOutput::Core,
        "explicit undirected simple projection; unweighted; self-loops refused; k-core number of each vertex, an exact count",
        "O(|V| + |E|)",
        "fgdb-prism/core-number-bucket-csr-v1",
    ),
    FnxSignature {
        name: "fnx.cdlp",
        graph_input_arity: 1,
        parameters: CDLP_PARAMETERS,
        outputs: &[FnxOutput::Vertex, FnxOutput::Component],
        implementation: FnxImplementationClass::InCoreDecodedCache,
        graph_kind: FnxGraphKind::Any,
        numeric_profile: FNX_DISCRETE_PROFILE,
        rng_policy: "none",
        graph_laws: "explicit simple projection; self-loops refused; weights ignored; synchronous Graphalytics label propagation for max_iter updates; initial labels are VIds, ties choose the smallest VId, isolates retain their labels; directed in+out incidences count reciprocal neighbors twice",
        complexity: "O(|V| + |E| + k * (|V| + |E|)) time; O(|V|) kernel workspace",
        execution_kernel: "fgdb-prism/cdlp-synchronous-csr-v1",
    },
    FnxSignature {
        name: "fnx.directed_clustering_coefficient",
        graph_input_arity: 1,
        parameters: &[],
        outputs: &[FnxOutput::Vertex, FnxOutput::Score],
        implementation: FnxImplementationClass::InCoreDecodedCache,
        graph_kind: FnxGraphKind::Directed,
        numeric_profile: "fgdb-directed-clustering-exact-count-f64-ratio-v1",
        rng_policy: "none",
        graph_laws: "explicit directed simple projection; unweighted; self-loops ignored; neighbors are the union of predecessors and successors; directed arcs among distinct neighbors / (d*(d-1)); degree below two yields zero",
        complexity: crate::directed_clustering::COMPLEXITY,
        execution_kernel: "fgdb-prism/directed-clustering-oriented-multiplicity-v2",
    },
    directed_degree(
        "fnx.in_degree_centrality",
        "fgdb-prism/in-degree-directory-v2",
    ),
    directed_degree("fnx.out_degree_centrality", "fgdb-prism/out-degree-csr-v1"),
    foundation(
        "fnx.load_centrality",
        FnxOutput::Score,
        "explicit undirected simple projection; unweighted; self-loops refused; Newman load splits accumulated flow equally among shortest-path predecessors; normalized by (|V|-1)(|V|-2); isolates retained",
        "O(|V| + sum_components |C| * (|C| + |E_C|)); O(|V|) kernel workspace",
        "fgdb-prism/newman-load-csr-v2",
    ),
    FnxSignature {
        name: "fnx.hits_centrality",
        graph_input_arity: 1,
        parameters: &[],
        outputs: &[FnxOutput::Vertex, FnxOutput::Hub, FnxOutput::Authority],
        implementation: FnxImplementationClass::InCoreDecodedCache,
        graph_kind: FnxGraphKind::Any,
        numeric_profile: "fnx-hits-f64-canonical-node-order-v1",
        rng_policy: "none",
        graph_laws: "explicit simple projection; weights ignored; retained self-loops count once per row; uniform initial hubs; at most 100 updates, stopping at absolute hub L1 < 1e-8; last iterate returned at the cap; hubs and authorities each sum-normalized with uniform zero-sum fallback; isolates retained",
        complexity: "O(k * (|V| + |E|)); O(|V|) kernel workspace",
        execution_kernel: "fgdb-prism/hits-csr-source-order-v1",
    },
    FnxSignature {
        name: "fnx.articulation_points",
        graph_input_arity: 1,
        parameters: &[],
        outputs: &[FnxOutput::Vertex],
        implementation: FnxImplementationClass::InCoreDecodedCache,
        graph_kind: FnxGraphKind::Undirected,
        numeric_profile: FNX_DISCRETE_PROFILE,
        rng_policy: "none",
        graph_laws: "explicit undirected simple projection; caller-selected parallel-edge reduction; weights ignored; self-loops ignored; vertices whose removal increases connected-component count; VId order",
        complexity: "O(|V| + |E|) logical traversal plus separately bounded compressed history/lookup work; O(|V|) integer workspace and depth-bounded compressed cursors",
        execution_kernel: "fgdb-prism/articulation-points-lowlink-v1",
    },
    FnxSignature {
        name: "fnx.bridges",
        graph_input_arity: 1,
        parameters: &[],
        outputs: &[FnxOutput::Vertex, FnxOutput::Neighbor],
        implementation: FnxImplementationClass::InCoreDecodedCache,
        graph_kind: FnxGraphKind::Undirected,
        numeric_profile: FNX_DISCRETE_PROFILE,
        rng_policy: "none",
        graph_laws: "explicit undirected simple projection; caller-selected parallel-edge reduction; weights ignored; self-loops ignored; projected edges whose removal increases connected-component count; vertex < neighbor, lexicographic full-width VId pair order; endpoints identify a projected edge, not a raw EId",
        complexity: "O(|V| + |E|) logical traversal and canonical ordering plus separately bounded compressed history/lookup work; O(|V|) integer workspace and depth-bounded compressed cursors",
        execution_kernel: "fgdb-prism/bridges-lowlink-v1",
    },
];

const fn directed_degree(name: &'static str, execution_kernel: &'static str) -> FnxSignature {
    FnxSignature {
        name,
        graph_input_arity: 1,
        parameters: &[],
        outputs: &[FnxOutput::Vertex, FnxOutput::Score],
        implementation: FnxImplementationClass::InCoreDecodedCache,
        graph_kind: FnxGraphKind::Directed,
        numeric_profile: "fnx-directed-degree-f64-division-v1",
        rng_policy: "none",
        graph_laws: "explicit directed simple projection; unweighted; self-loops count once; selected in/out degree divided by (|V|-1); a single vertex scores 1; isolates retained",
        complexity: "O(|V|) admitted degree-directory reads; O(|V|) kernel workspace",
        execution_kernel,
    }
}

/// A catalog row: one undirected, unweighted, parameter-free graph input
/// over the admitted decoded projection, with results in VId order. Its
/// execution identity names the native CSR kernel and its numeric profile.
const fn foundation(
    name: &'static str,
    value: FnxOutput,
    graph_laws: &'static str,
    complexity: &'static str,
    execution_kernel: &'static str,
) -> FnxSignature {
    FnxSignature {
        name,
        graph_input_arity: 1,
        parameters: &[],
        outputs: match value {
            FnxOutput::Core => &[FnxOutput::Vertex, FnxOutput::Core],
            _ => &[FnxOutput::Vertex, FnxOutput::Score],
        },
        implementation: FnxImplementationClass::InCoreDecodedCache,
        graph_kind: FnxGraphKind::Undirected,
        numeric_profile: match value {
            FnxOutput::Core => FNX_DISCRETE_PROFILE,
            _ => FNX_NUMERIC_PROFILE,
        },
        rng_policy: "none",
        graph_laws,
        complexity,
        execution_kernel,
    }
}
pub struct FnxSignatureRegistry;
impl FnxSignatureRegistry {
    pub const fn version() -> u16 {
        FNX_SIGNATURE_REGISTRY_VERSION
    }
    pub const fn signatures() -> &'static [FnxSignature] {
        SIGNATURES
    }
    pub fn lookup(name: &str) -> Option<&'static FnxSignature> {
        // ubs:ignore -- public procedure-signature names, not secret material.
        SIGNATURES.iter().find(|signature| signature.name == name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FnxBindError {
    pub at: usize,
    pub kind: FnxBindErrorKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FnxBindErrorKind {
    TextTooLong,
    Expected(&'static str),
    UnknownProcedure,
    MissingParameter,
    MissingArgument(&'static str),
    InvalidArgument(&'static str),
    TooManyArguments,
    UnknownYield,
    DuplicateYield,
    TrailingInput,
}
/// Static procedure-signature text only, never a supplied argument value.
impl core::fmt::Display for FnxBindErrorKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TextTooLong => f.write_str("CALL text is too long"),
            Self::Expected(item) if item.bytes().any(|byte| byte.is_ascii_alphanumeric()) => {
                write!(f, "expected {item}")
            }
            Self::Expected(item) => write!(f, "expected \"{item}\""),
            Self::UnknownProcedure => f.write_str("unknown procedure"),
            Self::MissingParameter => f.write_str("missing parameter value"),
            Self::MissingArgument(name) => write!(f, "missing argument {name}"),
            Self::InvalidArgument(reason) => write!(f, "invalid argument: {reason}"),
            Self::TooManyArguments => f.write_str("too many arguments"),
            Self::UnknownYield => f.write_str("YIELD names an output the procedure does not have"),
            Self::DuplicateYield => f.write_str("YIELD repeats an output"),
            Self::TrailingInput => f.write_str("unexpected text after the CALL"),
        }
    }
}
impl core::fmt::Display for FnxBindError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "CALL at byte {}: {}", self.at, self.kind)
    }
}
impl core::error::Error for FnxBindError {}

/// Where a typed binding failed. The text binder maps a site to its byte
/// offset; a GQL `CALL` maps it to its argument or YIELD item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnxCallSite {
    /// The call as a whole (a jointly invalid option set).
    Call,
    Procedure,
    /// After the last supplied argument: a required parameter was omitted.
    Arguments,
    Argument(usize),
    Yield(usize),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FnxCallError {
    pub site: FnxCallSite,
    pub kind: FnxBindErrorKind,
}
impl core::fmt::Display for FnxCallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.site {
            FnxCallSite::Call => write!(f, "CALL: {}", self.kind),
            FnxCallSite::Procedure => write!(f, "CALL procedure: {}", self.kind),
            FnxCallSite::Arguments => write!(f, "CALL arguments: {}", self.kind),
            FnxCallSite::Argument(index) => write!(f, "CALL argument {index}: {}", self.kind),
            FnxCallSite::Yield(index) => write!(f, "CALL YIELD item {index}: {}", self.kind),
        }
    }
}
impl core::error::Error for FnxCallError {}

fn registered(namespace: &str, name: &str) -> Option<&'static FnxSignature> {
    SIGNATURES.iter().find(|signature| {
        // ubs:ignore -- public procedure-signature names, not secret material.
        namespace == "fnx" && signature.name.strip_prefix("fnx.") == Some(name)
    })
}

/// The requested answer: convergence under a named residual rule, or an
/// exact number of updates. Fixed iterations do not claim convergence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum PageRankStoppingRule {
    /// Compatibility rule: successive-iterate L1 < vertex count * tolerance.
    Convergence = 0,
    ExactIterations = 1,
    /// Absolute global rule: successive-iterate L1 < tolerance.
    GlobalL1 = 2,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageRankOptions {
    alpha: f64,
    max_iter: usize,
    tol: f64,
    weighted: bool,
    stopping_rule: PageRankStoppingRule,
    source: Option<VId>,
}
impl PageRankOptions {
    pub fn new(
        alpha: f64,
        max_iter: usize,
        tol: f64,
        weighted: bool,
    ) -> Result<Self, FnxBindError> {
        let invalid = |message| FnxBindError {
            at: 0,
            kind: FnxBindErrorKind::InvalidArgument(message),
        };
        if !alpha.is_finite() || !(0.0..1.0).contains(&alpha) {
            return Err(invalid("alpha must be finite and in [0,1)"));
        }
        if max_iter == 0 {
            return Err(invalid("max_iter must be positive"));
        }
        if !tol.is_finite() || tol <= 0.0 {
            return Err(invalid("tol must be finite and positive"));
        }
        Ok(Self {
            alpha: if alpha == 0.0 { 0.0 } else { alpha },
            max_iter,
            tol,
            weighted,
            stopping_rule: PageRankStoppingRule::Convergence,
            source: None,
        })
    }

    /// Concentrate teleportation and dangling redistribution at one stable
    /// vertex in the admitted projection. The initial vector stays uniform,
    /// and the selected stopping rule and canonical arithmetic order remain
    /// unchanged. An absent or excluded source refuses during execution.
    /// This is single-source personalization, not an arbitrary seed vector.
    pub const fn with_source(mut self, source: VId) -> Self {
        self.source = Some(source);
        self
    }

    /// None retains uniform teleportation and dangling redistribution.
    pub const fn source(self) -> Option<VId> {
        self.source
    }
    /// Converge when the global L1 change between successive rank vectors is
    /// below `l1_tol`, independently of the number of vertices. The graph,
    /// dangling redistribution, arithmetic order and maximum-iteration refusal
    /// are the same as `new`; only the stopping rule differs.
    ///
    /// This bounds the iteration change, not an exact-solution error. Under
    /// the usual nonnegative stochastic PageRank assumptions, the exact
    /// arithmetic error is at most alpha / (1 - alpha) times that change;
    /// floating-point rounding is not included in that bound.
    pub fn global_l1(
        alpha: f64,
        max_iter: usize,
        l1_tol: f64,
        weighted: bool,
    ) -> Result<Self, FnxBindError> {
        let mut options = Self::new(alpha, max_iter, l1_tol, weighted)?;
        options.stopping_rule = PageRankStoppingRule::GlobalL1;
        Ok(options)
    }

    /// Request exactly `iterations` PageRank updates, including dangling-mass
    /// redistribution. Positive counts are required; execution still admits
    /// the count against the caller's iteration/work limits. No convergence
    /// refusal is produced after the requested final update.
    pub fn fixed_iterations(
        alpha: f64,
        iterations: usize,
        weighted: bool,
    ) -> Result<Self, FnxBindError> {
        if iterations == 0 {
            return Err(FnxBindError {
                at: 0,
                kind: FnxBindErrorKind::InvalidArgument("iterations must be positive"),
            });
        }
        let mut options = Self::new(alpha, iterations, 1e-6, weighted)?;
        options.stopping_rule = PageRankStoppingRule::ExactIterations;
        Ok(options)
    }
    pub const fn alpha(self) -> f64 {
        self.alpha
    }
    pub const fn max_iter(self) -> usize {
        self.max_iter
    }
    /// The tolerance for the selected stopping rule: per-vertex under
    /// `Convergence`, absolute global L1 under `GlobalL1`, and unused under
    /// `ExactIterations`.
    pub const fn tolerance(self) -> f64 {
        self.tol
    }
    pub const fn weighted(self) -> bool {
        self.weighted
    }
    pub const fn stopping_rule(self) -> PageRankStoppingRule {
        self.stopping_rule
    }
    pub const fn iterations(self) -> Option<usize> {
        match self.stopping_rule {
            PageRankStoppingRule::Convergence | PageRankStoppingRule::GlobalL1 => None,
            PageRankStoppingRule::ExactIterations => Some(self.max_iter),
        }
    }
    pub(crate) fn iteration_complete(self, completed: usize, delta: f64, population: f64) -> bool {
        match self.stopping_rule {
            PageRankStoppingRule::Convergence => delta < population * self.tol,
            PageRankStoppingRule::GlobalL1 => delta < self.tol,
            PageRankStoppingRule::ExactIterations => completed == self.max_iter,
        }
    }
    pub(crate) const fn witness_algorithm(self) -> &'static str {
        match (self.source.is_some(), self.stopping_rule) {
            (false, PageRankStoppingRule::Convergence) => "pagerank_power_iteration",
            (false, PageRankStoppingRule::GlobalL1) => "pagerank_global_l1",
            (false, PageRankStoppingRule::ExactIterations) => "pagerank_fixed_iterations",
            (true, PageRankStoppingRule::Convergence) => "personalized_pagerank_power_iteration",
            (true, PageRankStoppingRule::GlobalL1) => "personalized_pagerank_global_l1",
            (true, PageRankStoppingRule::ExactIterations) => {
                "personalized_pagerank_fixed_iterations"
            }
        }
    }
}
impl Default for PageRankOptions {
    fn default() -> Self {
        Self {
            alpha: 0.85,
            max_iter: 100,
            tol: 1e-6,
            weighted: true,
            stopping_rule: PageRankStoppingRule::Convergence,
            source: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FnxAlgorithm {
    PageRank(PageRankOptions),
    SingleSourceShortestPathLength {
        source: VId,
        cutoff: Option<usize>,
    },
    ConnectedComponents,
    WeaklyConnectedComponents,
    StronglyConnectedComponents,
    SingleSourceDijkstraPathLength(DijkstraOptions),
    Triangles,
    ClusteringCoefficient,
    /// A pinned catalog procedure whose signature identifies its executor.
    Foundation(FoundationAlgorithm),
    /// Graphalytics synchronous label propagation, including zero updates.
    Cdlp {
        max_iter: usize,
    },
    /// Directed arcs among the union of each vertex's in/out neighbors.
    DirectedClusteringCoefficient,
    InDegreeCentrality,
    OutDegreeCentrality,
    HitsCentrality,
    ArticulationPoints,
    Bridges,
}
impl FnxAlgorithm {
    pub fn signature(self) -> &'static FnxSignature {
        &SIGNATURES[match self {
            Self::PageRank(_) => 0,
            Self::SingleSourceShortestPathLength { .. } => 1,
            Self::ConnectedComponents => 2,
            Self::WeaklyConnectedComponents => 3,
            Self::StronglyConnectedComponents => 4,
            Self::SingleSourceDijkstraPathLength(_) => 5,
            Self::Triangles => 6,
            Self::ClusteringCoefficient => 7,
            Self::Foundation(FoundationAlgorithm::LoadCentrality) => 18,
            Self::Foundation(algorithm) => 8 + algorithm as usize,
            Self::Cdlp { .. } => 14,
            Self::DirectedClusteringCoefficient => 15,
            Self::InDegreeCentrality => 16,
            Self::OutDegreeCentrality => 17,
            Self::HitsCentrality => 19,
            Self::ArticulationPoints => 20,
            Self::Bridges => 21,
        }]
    }
}

/// Catalog algorithms over the admitted in-core DECODED_CACHE. Each signature
/// identifies the native CSR kernel and its pinned numeric profile. The ordinal
/// selects the original six rows after the first eight kernels; later entries
/// append registry rows so existing procedure ordinals never move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum FoundationAlgorithm {
    DegreeCentrality = 0,
    ClosenessCentrality = 1,
    HarmonicCentrality = 2,
    BetweennessCentrality = 3,
    EigenvectorCentrality = 4,
    CoreNumber = 5,
    LoadCentrality = 6,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FnxOutputColumn {
    pub field: FnxOutput,
    pub name: String,
}
#[derive(Clone, Debug, PartialEq)]
pub struct FnxCallSpec {
    algorithm: FnxAlgorithm,
    outputs: Vec<FnxOutputColumn>,
    digest: Digest,
}

/// Syntax and source positions shared by routing and typed binding. A routing
/// decision must not change when a supplied kernel value fails validation.
struct ParsedFnxCall<'a> {
    namespace: &'a str,
    procedure: &'a str,
    signature: &'static FnxSignature,
    procedure_at: usize,
    arguments: Vec<FnxArgument>,
    names: Vec<&'a str>,
    positions: Vec<usize>,
    arguments_end: usize,
    yields: Vec<(&'a str, &'a str)>,
    yield_at: Vec<(usize, usize)>,
}

impl FnxCallSpec {
    pub fn pagerank(options: PageRankOptions) -> Self {
        Self::new(FnxAlgorithm::PageRank(options))
    }
    pub fn single_source_shortest_path_length(source: VId, cutoff: Option<usize>) -> Self {
        Self::new(FnxAlgorithm::SingleSourceShortestPathLength { source, cutoff })
    }
    pub fn connected_components() -> Self {
        Self::new(FnxAlgorithm::ConnectedComponents)
    }
    pub fn weakly_connected_components() -> Self {
        Self::new(FnxAlgorithm::WeaklyConnectedComponents)
    }
    pub fn strongly_connected_components() -> Self {
        Self::new(FnxAlgorithm::StronglyConnectedComponents)
    }
    pub fn single_source_dijkstra_path_length(options: DijkstraOptions) -> Self {
        Self::new(FnxAlgorithm::SingleSourceDijkstraPathLength(options))
    }
    pub fn triangles() -> Self {
        Self::new(FnxAlgorithm::Triangles)
    }
    pub fn clustering_coefficient() -> Self {
        Self::new(FnxAlgorithm::ClusteringCoefficient)
    }
    /// Synchronous Graphalytics community detection over the admitted simple
    /// projection. Each of exactly `max_iter` rounds reads the previous round;
    /// reciprocal directed neighbors vote twice and ties choose the smallest
    /// stable VId. Zero returns initial VId labels, including every isolate.
    /// The `component` output is the community label, not a WCC identifier.
    ///
    /// Text calls use `CALL fnx.cdlp(max_iter => 5) YIELD vertex,component`.
    /// The count is required; this kernel does not use a convergence stop.
    pub fn cdlp(max_iter: usize) -> Self {
        Self::new(FnxAlgorithm::Cdlp { max_iter })
    }
    /// Directed neighbor-union coefficient; requires a directed projection.
    pub fn directed_clustering_coefficient() -> Self {
        Self::new(FnxAlgorithm::DirectedClusteringCoefficient)
    }
    /// Pinned HITS hub/authority iteration over the explicit projection.
    /// The two score columns are separately sum-normalized. At the fixed
    /// 100-update cap the last iterate is returned, as in the fnx oracle.
    pub fn hits_centrality() -> Self {
        Self::new(FnxAlgorithm::HitsCentrality)
    }
    /// Cut vertices of the explicit undirected simple projection, in VId order.
    pub fn articulation_points() -> Self {
        Self::new(FnxAlgorithm::ArticulationPoints)
    }
    /// Cut edges as canonical `(vertex, neighbor)` full-width VId pairs.
    pub fn bridges() -> Self {
        Self::new(FnxAlgorithm::Bridges)
    }
    fn new(algorithm: FnxAlgorithm) -> Self {
        Self::compiled(algorithm, default_outputs(algorithm.signature()))
    }
    pub fn algorithm(&self) -> FnxAlgorithm {
        self.algorithm
    }
    pub fn signature(&self) -> &'static FnxSignature {
        self.algorithm.signature()
    }
    /// Numeric policy of this frozen call, not merely the parameterized signature.
    pub fn numeric_profile(&self) -> &'static str {
        match self.algorithm {
            FnxAlgorithm::SingleSourceDijkstraPathLength(options) => match options.comparison() {
                DijkstraComparison::Strict => "fgdb-f64-dijkstra-strict-fifo-v1",
                DijkstraComparison::FnxEpsilon => "fnx-f64-dijkstra-epsilon-1e-12-fifo-v1",
            },
            _ => self.signature().numeric_profile,
        }
    }
    pub fn options(&self) -> Option<PageRankOptions> {
        match self.algorithm {
            FnxAlgorithm::PageRank(options) => Some(options),
            _ => None,
        }
    }
    pub fn outputs(&self) -> &[FnxOutputColumn] {
        &self.outputs
    }
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// Parse `CALL fnx.<name>(args) [YIELD field [AS alias], ... | YIELD *]`.
    /// Arguments are either all positional or all `name => value`, resolved
    /// through [`Self::from_arguments`] or [`Self::from_named_arguments`].
    /// For example, `CALL fnx.pagerank(iterations => 2, weighted => false)`
    /// requests two updates without changing the convergence defaults of
    /// `CALL fnx.pagerank()`.
    pub fn bind(text: &str, parameters: &FnxParameters) -> Result<Self, FnxBindError> {
        Self::bind_text(text, Some(parameters))
    }

    /// Whether a complete call uses only the registered kernel's argument
    /// envelope. This classifies syntax, not executable validity: missing or
    /// invalid parameter values still require [`Self::bind`] before execution.
    /// Graph projection fields/maps, expressions and continuing GQL pipelines
    /// belong to the native query compiler instead.
    #[must_use]
    pub fn is_standalone_kernel_call(text: &str) -> bool {
        Self::parse_text(text, None).is_ok_and(|parsed| {
            parsed.names.iter().all(|name| {
                parsed
                    .signature
                    .parameters
                    .iter()
                    .any(|parameter| parameter.name == *name)
            })
        })
    }

    /// Whether `text` is a CALL that continues past its YIELD list (WHERE,
    /// WITH, MATCH, RETURN, ...): a read pipeline for the GQL engine, not a
    /// standalone call for [`Self::bind`]. Only structure is read; a `$name`
    /// argument needs no value, and the tail is found before any resolution.
    #[must_use]
    pub fn continues_past_call(text: &str) -> bool {
        matches!(
            Self::parse_text(text, None),
            Err(FnxBindError {
                kind: FnxBindErrorKind::TrailingInput,
                ..
            })
        )
    }

    /// `parameters: None` reads every `$name` as NULL: structure only.
    fn parse_text<'a>(
        text: &'a str,
        parameters: Option<&FnxParameters>,
    ) -> Result<ParsedFnxCall<'a>, FnxBindError> {
        if text.len() > MAX_FNX_CALL_BYTES {
            return Err(FnxBindError {
                at: MAX_FNX_CALL_BYTES,
                kind: FnxBindErrorKind::TextTooLong,
            });
        }
        let mut parser = Parser {
            text,
            pos: 0,
            unterminated_comment: None,
        };
        parser.expect_keyword("CALL")?;
        let namespace = parser.word()?;
        parser.expect(b'.', ".")?;
        let procedure = parser.word()?;
        let procedure_at = parser.pos;
        let signature = registered(namespace, procedure)
            .ok_or_else(|| parser.error(FnxBindErrorKind::UnknownProcedure))?;
        parser.expect(b'(', "(")?;
        let mut arguments = Vec::with_capacity(signature.parameters.len());
        let mut names = Vec::new();
        let mut positions = Vec::with_capacity(signature.parameters.len());
        let positional_count = signature.parameters.iter().filter(|p| p.positional).count();
        if !parser.take(b')') {
            loop {
                parser.space();
                positions.push(parser.pos);
                let name = parser.argument_name()?;
                if !arguments.is_empty() && name.is_some() != !names.is_empty() {
                    return Err(parser.error(FnxBindErrorKind::InvalidArgument(
                        "positional and named arguments cannot be mixed",
                    )));
                }
                let limit = if name.is_some() {
                    signature.parameters.len()
                } else {
                    positional_count
                };
                if arguments.len() == limit {
                    return Err(parser.error(FnxBindErrorKind::TooManyArguments));
                }
                if let Some(name) = name {
                    names.push(name);
                }
                arguments.push(parser.argument(parameters)?);
                if parser.take(b')') {
                    break;
                }
                parser.expect(b',', ", or )")?;
            }
        }
        let arguments_end = parser.pos;
        let mut yields = Vec::new();
        let mut yield_at = Vec::new();
        if parser.keyword("YIELD") && !parser.take(b'*') {
            loop {
                let name = parser.word()?;
                let name_end = parser.pos;
                let alias = if parser.keyword("AS") {
                    parser.word()?
                } else {
                    name
                };
                yields.push((name, alias));
                yield_at.push((name_end, parser.pos));
                if !parser.take(b',') {
                    break;
                }
            }
        }
        parser.take(b';');
        parser.space();
        if let Some(error) = parser.trivia_error() {
            return Err(error);
        }
        if parser.pos != text.len() {
            return Err(parser.error(FnxBindErrorKind::TrailingInput));
        }
        Ok(ParsedFnxCall {
            namespace,
            procedure,
            signature,
            procedure_at,
            arguments,
            names,
            positions,
            arguments_end,
            yields,
            yield_at,
        })
    }

    fn bind_text(text: &str, parameters: Option<&FnxParameters>) -> Result<Self, FnxBindError> {
        let ParsedFnxCall {
            namespace,
            procedure,
            signature: _,
            procedure_at,
            arguments,
            names,
            positions,
            arguments_end,
            yields,
            yield_at,
        } = Self::parse_text(text, parameters)?;
        let binding = if names.is_empty() {
            Self::from_arguments(namespace, procedure, &arguments, &yields)
        } else {
            let named: Vec<_> = names.into_iter().zip(arguments).collect();
            Self::from_named_arguments(namespace, procedure, &named, &yields)
        };
        binding.map_err(|error| {
            let at = match error.site {
                FnxCallSite::Call => 0,
                FnxCallSite::Procedure => procedure_at,
                FnxCallSite::Arguments => arguments_end,
                FnxCallSite::Argument(index) => {
                    positions.get(index).copied().unwrap_or(arguments_end)
                }
                FnxCallSite::Yield(index) => match yield_at.get(index) {
                    Some(&(name_end, _)) if error.kind == FnxBindErrorKind::UnknownYield => {
                        name_end
                    }
                    Some(&(_, item_end)) => item_end,
                    None => arguments_end,
                },
            };
            FnxBindError {
                at,
                kind: error.kind,
            }
        })
    }

    /// Bind a registered procedure from typed positional arguments. Omitted
    /// trailing parameters take their declared defaults and a required one
    /// refuses. `yields` names distinct outputs as `(field, alias)` in result
    /// order; empty means every output in declared order.
    pub fn from_arguments(
        namespace: &str,
        name: &str,
        supplied: &[FnxArgument],
        yields: &[(&str, &str)],
    ) -> Result<Self, FnxCallError> {
        let error = |site, kind| FnxCallError { site, kind };
        let signature = registered(namespace, name)
            .ok_or_else(|| error(FnxCallSite::Procedure, FnxBindErrorKind::UnknownProcedure))?;
        let positional_count = signature.parameters.iter().filter(|p| p.positional).count();
        if supplied.len() > positional_count {
            return Err(error(
                FnxCallSite::Argument(positional_count),
                FnxBindErrorKind::TooManyArguments,
            ));
        }
        let mut arguments = [FnxArgument::Null; 4];
        for (index, parameter) in signature
            .parameters
            .iter()
            .filter(|p| p.positional)
            .enumerate()
        {
            arguments[index] = match supplied.get(index) {
                Some(&argument) => argument,
                None => parameter.default.ok_or_else(|| {
                    error(
                        FnxCallSite::Arguments,
                        FnxBindErrorKind::MissingArgument(parameter.name),
                    )
                })?,
            };
        }
        let invalid = |index, message| {
            error(
                FnxCallSite::Argument(index),
                FnxBindErrorKind::InvalidArgument(message),
            )
        };
        // Option validators judge the call as a whole, not one argument.
        let whole = |failure: FnxBindError| error(FnxCallSite::Call, failure.kind);
        let algorithm = match signature.name {
            "fnx.pagerank" => {
                let alpha = float_argument(arguments[0])
                    .ok_or_else(|| invalid(0, "finite exact alpha required"))?;
                let max_iter = match arguments[1] {
                    FnxArgument::Integer(value) if value > 0 => usize::try_from(value).ok(),
                    _ => None,
                }
                .ok_or_else(|| invalid(1, "positive integer max_iter required"))?;
                let tol = float_argument(arguments[2])
                    .ok_or_else(|| invalid(2, "finite exact tol required"))?;
                let FnxArgument::Boolean(weighted) = arguments[3] else {
                    return Err(invalid(3, "boolean weighted required"));
                };
                FnxAlgorithm::PageRank(
                    PageRankOptions::new(alpha, max_iter, tol, weighted).map_err(whole)?,
                )
            }
            "fnx.single_source_shortest_path_length" => {
                let source = match arguments[0] {
                    FnxArgument::Vertex(vertex) => vertex,
                    FnxArgument::Integer(value) if value >= 0 => VId(value as u128),
                    _ => return Err(invalid(0, "VId or nonnegative integer source required")),
                };
                let cutoff = match arguments[1] {
                    FnxArgument::Null => None,
                    FnxArgument::Integer(value) if value >= 0 => Some(
                        usize::try_from(value)
                            .map_err(|_| invalid(1, "cutoff exceeds ordinal width"))?,
                    ),
                    _ => return Err(invalid(1, "nonnegative integer or NULL cutoff required")),
                };
                FnxAlgorithm::SingleSourceShortestPathLength { source, cutoff }
            }
            "fnx.single_source_dijkstra_path_length" => {
                let source = match arguments[0] {
                    FnxArgument::Vertex(vertex) => vertex,
                    FnxArgument::Integer(value) if value >= 0 => VId(value as u128),
                    _ => return Err(invalid(0, "VId or nonnegative integer source required")),
                };
                let cutoff = if arguments[1] == FnxArgument::Null {
                    None
                } else {
                    Some(
                        float_argument(arguments[1])
                            .filter(|&value| value >= 0.0)
                            .ok_or_else(|| {
                                invalid(1, "finite nonnegative exact cost or NULL required")
                            })?,
                    )
                };
                let FnxArgument::Boolean(strict) = arguments[2] else {
                    return Err(invalid(2, "boolean strict required"));
                };
                let comparison = if strict {
                    DijkstraComparison::Strict
                } else {
                    DijkstraComparison::FnxEpsilon
                };
                FnxAlgorithm::SingleSourceDijkstraPathLength(
                    DijkstraOptions::new(source, cutoff)
                        .map_err(whole)?
                        .with_comparison(comparison),
                )
            }
            "fnx.connected_components" => FnxAlgorithm::ConnectedComponents,
            "fnx.weakly_connected_components" => FnxAlgorithm::WeaklyConnectedComponents,
            "fnx.strongly_connected_components" => FnxAlgorithm::StronglyConnectedComponents,
            "fnx.triangles" => FnxAlgorithm::Triangles,
            "fnx.clustering_coefficient" => FnxAlgorithm::ClusteringCoefficient,
            "fnx.directed_clustering_coefficient" => FnxAlgorithm::DirectedClusteringCoefficient,
            "fnx.degree_centrality" => {
                FnxAlgorithm::Foundation(FoundationAlgorithm::DegreeCentrality)
            }
            "fnx.closeness_centrality" => {
                FnxAlgorithm::Foundation(FoundationAlgorithm::ClosenessCentrality)
            }
            "fnx.harmonic_centrality" => {
                FnxAlgorithm::Foundation(FoundationAlgorithm::HarmonicCentrality)
            }
            "fnx.betweenness_centrality" => {
                FnxAlgorithm::Foundation(FoundationAlgorithm::BetweennessCentrality)
            }
            "fnx.eigenvector_centrality" => {
                FnxAlgorithm::Foundation(FoundationAlgorithm::EigenvectorCentrality)
            }
            "fnx.core_number" => FnxAlgorithm::Foundation(FoundationAlgorithm::CoreNumber),
            "fnx.load_centrality" => FnxAlgorithm::Foundation(FoundationAlgorithm::LoadCentrality),
            "fnx.in_degree_centrality" => FnxAlgorithm::InDegreeCentrality,
            "fnx.out_degree_centrality" => FnxAlgorithm::OutDegreeCentrality,
            "fnx.hits_centrality" => FnxAlgorithm::HitsCentrality,
            "fnx.articulation_points" => FnxAlgorithm::ArticulationPoints,
            "fnx.bridges" => FnxAlgorithm::Bridges,
            "fnx.cdlp" => {
                let max_iter = match arguments[0] {
                    FnxArgument::Integer(value) if value >= 0 => usize::try_from(value).ok(),
                    _ => None,
                }
                .ok_or_else(|| invalid(0, "nonnegative integer max_iter required"))?;
                FnxAlgorithm::Cdlp { max_iter }
            }
            _ => {
                return Err(error(
                    FnxCallSite::Procedure,
                    FnxBindErrorKind::UnknownProcedure,
                ));
            }
        };
        let mut outputs = default_outputs(signature);
        if !yields.is_empty() {
            outputs.clear();
            for (index, &(field_name, alias)) in yields.iter().enumerate() {
                let field = signature
                    .outputs
                    .iter()
                    .copied()
                    .find(|field| field.is_named(field_name))
                    .ok_or_else(|| {
                        error(FnxCallSite::Yield(index), FnxBindErrorKind::UnknownYield)
                    })?;
                if outputs
                    .iter()
                    // ubs:ignore -- public YIELD field names and aliases, not secret material.
                    .any(|column| column.field == field || column.name == alias)
                {
                    return Err(error(
                        FnxCallSite::Yield(index),
                        FnxBindErrorKind::DuplicateYield,
                    ));
                }
                outputs.push(FnxOutputColumn {
                    field,
                    name: alias.to_owned(),
                });
            }
        }
        Ok(Self::compiled(algorithm, outputs))
    }

    /// Bind registered `name => value` arguments in any order. Missing
    /// positional parameters use their declared defaults; a required source
    /// still refuses. Error argument indices refer to `supplied`, not the
    /// registry's parameter order.
    ///
    /// PageRank's named-only `iterations` requests an exact positive number
    /// of updates. It conflicts with explicitly supplied `max_iter`, `tol`
    /// or `l1_tol`, including their normal default values. Named-only `l1_tol`
    /// selects an absolute global L1 threshold and conflicts with `tol` and
    /// `iterations`. Omitting both named-only controls keeps the existing
    /// N * tol convergence rule. Names may not repeat, and this API never
    /// interprets a name as a positional slot.
    /// Named-only `source` puts teleportation and dangling redistribution at
    /// one full-width vertex without changing the four positional arguments.
    pub fn from_named_arguments(
        namespace: &str,
        name: &str,
        supplied: &[(&str, FnxArgument)],
        yields: &[(&str, &str)],
    ) -> Result<Self, FnxCallError> {
        let error = |site, kind| FnxCallError { site, kind };
        let invalid = |index, message| {
            error(
                FnxCallSite::Argument(index),
                FnxBindErrorKind::InvalidArgument(message),
            )
        };
        let registration = registered(namespace, name)
            .ok_or_else(|| error(FnxCallSite::Procedure, FnxBindErrorKind::UnknownProcedure))?;
        let mut named = vec![None; registration.parameters.len()];
        for (index, &(name, value)) in supplied.iter().enumerate() {
            let slot = registration
                .parameters
                .iter()
                .position(|p| p.name == name)
                .ok_or_else(|| invalid(index, "unknown named argument"))?;
            if named[slot].replace((index, value)).is_some() {
                return Err(invalid(index, "duplicate named argument"));
            }
        }
        let source = registration
            .parameters
            .iter()
            .zip(&named)
            .find_map(|(parameter, value)| {
                (parameter.name == "source" && !parameter.positional)
                    .then_some(*value)
                    .flatten()
            })
            .map(|(index, value)| match value {
                FnxArgument::Vertex(vertex) => Ok(vertex),
                FnxArgument::Integer(value) if value >= 0 => Ok(VId(value as u128)),
                _ => Err(invalid(index, "VId or nonnegative integer source required")),
            })
            .transpose()?;
        let iterations =
            registration
                .parameters
                .iter()
                .zip(&named)
                .find_map(|(parameter, value)| {
                    (parameter.name == "iterations").then_some(*value).flatten()
                });
        let global = registration
            .parameters
            .iter()
            .zip(&named)
            .find_map(|(parameter, value)| {
                (parameter.name == "l1_tol").then_some(*value).flatten()
            });
        let global = if let Some((index, value)) = global {
            for (supplied_index, &(parameter, _)) in supplied.iter().enumerate() {
                if matches!(parameter, "tol" | "iterations") {
                    return Err(invalid(
                        supplied_index,
                        "l1_tol conflicts with tol and iterations",
                    ));
                }
            }
            Some(
                float_argument(value)
                    .filter(|&value| value > 0.0)
                    .ok_or_else(|| invalid(index, "finite positive exact l1_tol required"))?,
            )
        } else {
            None
        };
        let exact = if let Some((index, value)) = iterations {
            for (supplied_index, &(parameter, _)) in supplied.iter().enumerate() {
                if matches!(parameter, "max_iter" | "tol" | "l1_tol") {
                    return Err(invalid(
                        supplied_index,
                        "iterations conflicts with max_iter, tol and l1_tol",
                    ));
                }
            }
            Some(
                match value {
                    FnxArgument::Integer(count) if count > 0 => usize::try_from(count).ok(),
                    _ => None,
                }
                .ok_or_else(|| invalid(index, "positive integer iterations required"))?,
            )
        } else {
            None
        };
        let mut positional = Vec::new();
        let mut supplied_indices = Vec::new();
        for (parameter, value) in registration.parameters.iter().zip(named) {
            if !parameter.positional {
                continue;
            }
            let (index, value) = match value {
                Some((index, value)) => (Some(index), value),
                None => (
                    None,
                    parameter.default.ok_or_else(|| {
                        error(
                            FnxCallSite::Arguments,
                            FnxBindErrorKind::MissingArgument(parameter.name),
                        )
                    })?,
                ),
            };
            positional.push(value);
            supplied_indices.push(index);
        }
        // The positional API retains its existing whole-call domain errors.
        // Named calls can identify these independent scalar errors precisely.
        if registration.name == "fnx.pagerank" {
            for (index, &(parameter, value)) in supplied.iter().enumerate() {
                match (parameter, float_argument(value)) {
                    ("alpha", Some(alpha)) if !(0.0..1.0).contains(&alpha) => {
                        return Err(invalid(index, "alpha must be finite and in [0,1)"));
                    }
                    ("tol", Some(tol)) if tol <= 0.0 => {
                        return Err(invalid(index, "tol must be finite and positive"));
                    }
                    _ => {}
                }
            }
        }
        let call =
            Self::from_arguments(namespace, name, &positional, yields).map_err(|mut failure| {
                if let FnxCallSite::Argument(index) = failure.site {
                    failure.site = supplied_indices
                        .get(index)
                        .copied()
                        .flatten()
                        .map_or(FnxCallSite::Arguments, FnxCallSite::Argument);
                }
                failure
            })?;
        let call = match (exact, global) {
            (None, None) => Ok(call),
            (None, Some(tolerance)) => {
                let Some(options) = call.options() else {
                    return Err(error(
                        FnxCallSite::Procedure,
                        FnxBindErrorKind::UnknownProcedure,
                    ));
                };
                let options = PageRankOptions::global_l1(
                    options.alpha(),
                    options.max_iter(),
                    tolerance,
                    options.weighted(),
                )
                .map_err(|failure| error(FnxCallSite::Call, failure.kind))?;
                Ok(Self::compiled(
                    FnxAlgorithm::PageRank(options),
                    call.outputs,
                ))
            }
            (Some(iterations), _) => {
                let Some(options) = call.options() else {
                    return Err(error(
                        FnxCallSite::Procedure,
                        FnxBindErrorKind::UnknownProcedure,
                    ));
                };
                let options = PageRankOptions::fixed_iterations(
                    options.alpha(),
                    iterations,
                    options.weighted(),
                )
                .map_err(|failure| error(FnxCallSite::Call, failure.kind))?;
                Ok(Self::compiled(
                    FnxAlgorithm::PageRank(options),
                    call.outputs,
                ))
            }
        }?;
        if let Some(source) = source {
            let options = call
                .options()
                .ok_or_else(|| error(FnxCallSite::Procedure, FnxBindErrorKind::UnknownProcedure))?;
            Ok(Self::compiled(
                FnxAlgorithm::PageRank(options.with_source(source)),
                call.outputs,
            ))
        } else {
            Ok(call)
        }
    }

    fn compiled(algorithm: FnxAlgorithm, outputs: Vec<FnxOutputColumn>) -> Self {
        let mut hash = Hasher::new();
        hash.update(b"fgdb:prism:bound-call:v3");
        hash.update(&FNX_SIGNATURE_REGISTRY_VERSION.to_le_bytes());
        let name = algorithm.signature().name;
        hash.update(&(name.len() as u128).to_le_bytes());
        hash.update(name.as_bytes());
        match algorithm {
            FnxAlgorithm::PageRank(options) => {
                hash.update(&[options.stopping_rule as u8]);
                hash.update(&options.alpha.to_bits().to_le_bytes());
                hash.update(&(options.max_iter as u128).to_le_bytes());
                if options.stopping_rule != PageRankStoppingRule::ExactIterations {
                    hash.update(&options.tol.to_bits().to_le_bytes());
                }
                hash.update(&[u8::from(options.weighted)]);
                match options.source {
                    None => {
                        hash.update(&[0]);
                    }
                    Some(source) => {
                        hash.update(&[1]);
                        hash.update(&source.0.to_le_bytes());
                    }
                }
            }
            FnxAlgorithm::SingleSourceShortestPathLength { source, cutoff } => {
                hash.update(&source.0.to_le_bytes());
                match cutoff {
                    None => {
                        hash.update(&[0]);
                    }
                    Some(cutoff) => {
                        hash.update(&[1]);
                        hash.update(&(cutoff as u128).to_le_bytes());
                    }
                }
            }
            FnxAlgorithm::SingleSourceDijkstraPathLength(options) => {
                hash.update(&options.source().0.to_le_bytes());
                match options.cutoff() {
                    None => {
                        hash.update(&[0]);
                    }
                    Some(cutoff) => {
                        hash.update(&[1]);
                        hash.update(&cutoff.to_bits().to_le_bytes());
                    }
                }
                hash.update(&[options.comparison() as u8]);
            }
            FnxAlgorithm::Cdlp { max_iter } => {
                hash.update(&(max_iter as u128).to_le_bytes());
            }
            _ => {}
        }
        hash.update(&(outputs.len() as u128).to_le_bytes());
        for output in &outputs {
            hash.update(&[output.field as u8]);
            hash.update(&(output.name.len() as u128).to_le_bytes());
            hash.update(output.name.as_bytes());
        }
        Self {
            algorithm,
            outputs,
            digest: hash.finalize(),
        }
    }
}
fn default_outputs(signature: &FnxSignature) -> Vec<FnxOutputColumn> {
    signature
        .outputs
        .iter()
        .copied()
        .map(|field| FnxOutputColumn {
            field,
            name: field.name().to_owned(),
        })
        .collect()
}
fn float_argument(argument: FnxArgument) -> Option<f64> {
    match argument {
        FnxArgument::Float(value) if value.is_finite() => Some(value),
        FnxArgument::Integer(value)
            if (-9_007_199_254_740_992..=9_007_199_254_740_992).contains(&value) =>
        {
            Some(value as f64)
        }
        _ => None,
    }
}

struct Parser<'a> {
    text: &'a str,
    pos: usize,
    unterminated_comment: Option<usize>,
}
impl<'a> Parser<'a> {
    fn error(&self, kind: FnxBindErrorKind) -> FnxBindError {
        FnxBindError { at: self.pos, kind }
    }
    fn trivia_error(&self) -> Option<FnxBindError> {
        self.unterminated_comment.map(|at| FnxBindError {
            at,
            kind: FnxBindErrorKind::Expected("closing */"),
        })
    }
    /// Standalone CALL text follows native GQL trivia: ASCII whitespace,
    /// // line comments and /* block comments */. Markers inside tokens are
    /// never consumed because token parsers invoke this only at boundaries.
    fn space(&mut self) {
        loop {
            while self
                .text
                .as_bytes()
                .get(self.pos)
                .is_some_and(u8::is_ascii_whitespace)
            {
                self.pos += 1;
            }
            let bytes = self.text.as_bytes();
            if bytes.get(self.pos) == Some(&b'/') && bytes.get(self.pos + 1) == Some(&b'/') {
                self.pos += 2;
                while bytes.get(self.pos).is_some_and(|byte| *byte != b'\n') {
                    self.pos += 1;
                }
                continue;
            }
            if bytes.get(self.pos) == Some(&b'/') && bytes.get(self.pos + 1) == Some(&b'*') {
                let start = self.pos;
                self.pos += 2;
                let mut closed = false;
                while self.pos + 1 < bytes.len() {
                    if bytes[self.pos] == b'*' && bytes[self.pos + 1] == b'/' {
                        self.pos += 2;
                        closed = true;
                        break;
                    }
                    self.pos += 1;
                }
                if !closed {
                    self.unterminated_comment = Some(start);
                    self.pos = bytes.len();
                    return;
                }
                continue;
            }
            return;
        }
    }
    fn take(&mut self, byte: u8) -> bool {
        self.space();
        if self.unterminated_comment.is_some() {
            return false;
        }
        if self.text.as_bytes().get(self.pos) == Some(&byte) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, byte: u8, name: &'static str) -> Result<(), FnxBindError> {
        if self.take(byte) {
            Ok(())
        } else if let Some(error) = self.trivia_error() {
            Err(error)
        } else {
            Err(self.error(FnxBindErrorKind::Expected(name)))
        }
    }
    fn word(&mut self) -> Result<&'a str, FnxBindError> {
        self.space();
        if let Some(error) = self.trivia_error() {
            return Err(error);
        }
        let start = self.pos;
        let Some(&first) = self.text.as_bytes().get(self.pos) else {
            return Err(self.error(FnxBindErrorKind::Expected("identifier")));
        };
        if !first.is_ascii_alphabetic() && first != b'_' {
            return Err(self.error(FnxBindErrorKind::Expected("identifier")));
        }
        self.pos += 1;
        while self
            .text
            .as_bytes()
            .get(self.pos)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            self.pos += 1;
        }
        Ok(&self.text[start..self.pos])
    }
    fn keyword(&mut self, expected: &str) -> bool {
        let before = self.pos;
        if self
            .word()
            .is_ok_and(|word| word.eq_ignore_ascii_case(expected))
        {
            true
        } else {
            self.pos = before;
            false
        }
    }
    fn expect_keyword(&mut self, expected: &'static str) -> Result<(), FnxBindError> {
        if self.keyword(expected) {
            Ok(())
        } else {
            Err(self.error(FnxBindErrorKind::Expected(expected)))
        }
    }
    fn argument_name(&mut self) -> Result<Option<&'a str>, FnxBindError> {
        self.space();
        let before = self.pos;
        if !self
            .text
            .as_bytes()
            .get(before)
            .is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        {
            return Ok(None);
        }
        let name = self.word()?;
        self.space();
        if let Some(error) = self.trivia_error() {
            return Err(error);
        }
        if self.text.as_bytes().get(self.pos..self.pos + 2) == Some(b"=>") {
            self.pos += 2;
            Ok(Some(name))
        } else {
            self.pos = before;
            Ok(None)
        }
    }
    fn argument(
        &mut self,
        parameters: Option<&FnxParameters>,
    ) -> Result<FnxArgument, FnxBindError> {
        if self.take(b'$') {
            let name = self.word()?;
            let Some(parameters) = parameters else {
                return Ok(FnxArgument::Null);
            };
            return parameters
                .get(name)
                .copied()
                .ok_or_else(|| self.error(FnxBindErrorKind::MissingParameter));
        }
        if self.keyword("true") {
            return Ok(FnxArgument::Boolean(true));
        }
        if self.keyword("false") {
            return Ok(FnxArgument::Boolean(false));
        }
        if self.keyword("null") {
            return Ok(FnxArgument::Null);
        }
        self.space();
        if let Some(error) = self.trivia_error() {
            return Err(error);
        }
        // Native numeric spelling: optional sign, at least one leading digit,
        // an optional fractional part with at least one digit, and an optional
        // exponent with at least one digit. Thus .5 and 1. refuse while 1e5
        // and 1.5e-3 bind exactly as native GQL does.
        let start = self.pos;
        let bytes = self.text.as_bytes();
        if bytes
            .get(self.pos)
            .is_some_and(|byte| matches!(*byte, b'+' | b'-'))
        {
            self.pos += 1;
        }
        let integer_start = self.pos;
        while bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        if self.pos == integer_start {
            self.pos = start;
            return Err(self.error(FnxBindErrorKind::Expected("typed argument")));
        }
        let mut float = false;
        if bytes.get(self.pos) == Some(&b'.')
            && bytes.get(self.pos + 1).is_some_and(u8::is_ascii_digit)
        {
            float = true;
            self.pos += 1;
            while bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
                self.pos += 1;
            }
        }
        if bytes
            .get(self.pos)
            .is_some_and(|byte| matches!(*byte, b'e' | b'E'))
        {
            let exponent = self.pos;
            let mut at = exponent + 1;
            if bytes
                .get(at)
                .is_some_and(|byte| matches!(*byte, b'+' | b'-'))
            {
                at += 1;
            }
            let digits = at;
            while bytes.get(at).is_some_and(u8::is_ascii_digit) {
                at += 1;
            }
            if at > digits {
                float = true;
                self.pos = at;
            }
        }
        let token = &self.text[start..self.pos];
        if float {
            token
                .parse::<f64>()
                .map(FnxArgument::Float)
                .map_err(|_| self.error(FnxBindErrorKind::InvalidArgument("invalid number")))
        } else if let Ok(value) = token.parse::<i64>() {
            Ok(FnxArgument::Integer(value))
        } else {
            // Large exact positive literals belong only to the VId domain;
            // they cannot silently become approximate f64 numeric arguments.
            token
                .strip_prefix('+')
                .unwrap_or(token)
                .parse::<u128>()
                .map(|value| FnxArgument::Vertex(VId(value)))
                .map_err(|_| self.error(FnxBindErrorKind::InvalidArgument("integer overflow")))
        }
    }
}

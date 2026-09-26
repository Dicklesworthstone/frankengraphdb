//! Frozen, typed CALL programs. Only implemented in-core signatures enter this
//! registry; none of these entries claims external-memory execution support.

use crate::{DijkstraComparison, DijkstraOptions};
use fgdb_crypto::{Digest, Hasher};
use fgdb_types::VId;
use std::collections::BTreeMap;

pub const FNX_SIGNATURE_REGISTRY_VERSION: u16 = 3;
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
    Boolean,
    Vertex,
    OptionalNonNegativeInteger,
    OptionalNonNegativeFloat,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FnxParameterSpec {
    pub name: &'static str,
    pub value_type: FnxParameterType,
    /// None means REQUIRED, not a fabricated source vertex or an implicit NULL.
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
}
impl FnxOutput {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Vertex => "vertex",
            Self::Score => "score",
            Self::Distance => "distance",
            Self::Component => "component",
            Self::Triangles => "triangles",
        }
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
        default: Some(FnxArgument::Float(0.85)),
    },
    FnxParameterSpec {
        name: "max_iter",
        value_type: FnxParameterType::PositiveInteger,
        default: Some(FnxArgument::Integer(100)),
    },
    FnxParameterSpec {
        name: "tol",
        value_type: FnxParameterType::FiniteFloat,
        default: Some(FnxArgument::Float(1e-6)),
    },
    FnxParameterSpec {
        name: "weighted",
        value_type: FnxParameterType::Boolean,
        default: Some(FnxArgument::Boolean(true)),
    },
];
const BFS_PARAMETERS: &[FnxParameterSpec] = &[
    FnxParameterSpec {
        name: "source",
        value_type: FnxParameterType::Vertex,
        default: None,
    },
    FnxParameterSpec {
        name: "cutoff",
        value_type: FnxParameterType::OptionalNonNegativeInteger,
        default: Some(FnxArgument::Null),
    },
];
const DIJKSTRA_PARAMETERS: &[FnxParameterSpec] = &[
    FnxParameterSpec {
        name: "source",
        value_type: FnxParameterType::Vertex,
        default: None,
    },
    FnxParameterSpec {
        name: "cutoff",
        value_type: FnxParameterType::OptionalNonNegativeFloat,
        default: Some(FnxArgument::Null),
    },
    FnxParameterSpec {
        name: "strict",
        value_type: FnxParameterType::Boolean,
        default: Some(FnxArgument::Boolean(false)),
    },
];
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
        graph_laws: "explicit projection; finite nonnegative weights when weighted; isolates retained",
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
        execution_kernel: "fgdb-prism/bfs-row-cursor-v1",
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
        execution_kernel: "fgdb-prism/components-row-cursor-v1",
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
        execution_kernel: "fgdb-prism/weak-components-row-cursor-v1",
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
        complexity: "O((|V| + |E|) * log(1 + |V|))",
        execution_kernel: "fgdb-prism/dijkstra-indexed-heap-v1",
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
        execution_kernel: "fgdb-prism/triangles-degree-mark-v1",
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
        execution_kernel: "fgdb-prism/clustering-degree-mark-v1",
    },
];
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
impl core::fmt::Display for FnxBindError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "Prism CALL bind error at byte {}: {:?}",
            self.at, self.kind
        )
    }
}
impl core::error::Error for FnxBindError {}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PageRankOptions {
    alpha: f64,
    max_iter: usize,
    tol: f64,
    weighted: bool,
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
        })
    }
    pub const fn alpha(self) -> f64 {
        self.alpha
    }
    pub const fn max_iter(self) -> usize {
        self.max_iter
    }
    pub const fn tolerance(self) -> f64 {
        self.tol
    }
    pub const fn weighted(self) -> bool {
        self.weighted
    }
}
impl Default for PageRankOptions {
    fn default() -> Self {
        Self {
            alpha: 0.85,
            max_iter: 100,
            tol: 1e-6,
            weighted: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FnxAlgorithm {
    PageRank(PageRankOptions),
    SingleSourceShortestPathLength { source: VId, cutoff: Option<usize> },
    ConnectedComponents,
    WeaklyConnectedComponents,
    StronglyConnectedComponents,
    SingleSourceDijkstraPathLength(DijkstraOptions),
    Triangles,
    ClusteringCoefficient,
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
        }]
    }
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

    pub fn bind(text: &str, parameters: &FnxParameters) -> Result<Self, FnxBindError> {
        if text.len() > MAX_FNX_CALL_BYTES {
            return Err(FnxBindError {
                at: MAX_FNX_CALL_BYTES,
                kind: FnxBindErrorKind::TextTooLong,
            });
        }
        let mut parser = Parser { text, pos: 0 };
        parser.expect_keyword("CALL")?;
        let namespace = parser.word()?;
        parser.expect(b'.', ".")?;
        let procedure = parser.word()?;
        let signature = SIGNATURES
            .iter()
            .find(|signature| {
                // ubs:ignore -- public procedure-signature names, not secret material.
                namespace == "fnx" && signature.name.strip_prefix("fnx.") == Some(procedure)
            })
            .ok_or_else(|| parser.error(FnxBindErrorKind::UnknownProcedure))?;
        parser.expect(b'(', "(")?;
        let mut arguments = [FnxArgument::Null; 4];
        let mut positions = [parser.pos; 4];
        let mut count = 0;
        if !parser.take(b')') {
            loop {
                if count == signature.parameters.len() {
                    return Err(parser.error(FnxBindErrorKind::TooManyArguments));
                }
                parser.space();
                positions[count] = parser.pos;
                arguments[count] = parser.argument(parameters)?;
                count += 1;
                if parser.take(b')') {
                    break;
                }
                parser.expect(b',', ", or )")?;
            }
        }
        for index in count..signature.parameters.len() {
            arguments[index] = signature.parameters[index].default.ok_or_else(|| {
                parser.error(FnxBindErrorKind::MissingArgument(
                    signature.parameters[index].name,
                ))
            })?;
        }
        let invalid = |index, message| FnxBindError {
            at: positions[index],
            kind: FnxBindErrorKind::InvalidArgument(message),
        };
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
                FnxAlgorithm::PageRank(PageRankOptions::new(alpha, max_iter, tol, weighted)?)
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
                    DijkstraOptions::new(source, cutoff)?.with_comparison(comparison),
                )
            }
            "fnx.connected_components" => FnxAlgorithm::ConnectedComponents,
            "fnx.weakly_connected_components" => FnxAlgorithm::WeaklyConnectedComponents,
            "fnx.strongly_connected_components" => FnxAlgorithm::StronglyConnectedComponents,
            "fnx.triangles" => FnxAlgorithm::Triangles,
            "fnx.clustering_coefficient" => FnxAlgorithm::ClusteringCoefficient,
            _ => return Err(parser.error(FnxBindErrorKind::UnknownProcedure)),
        };
        let mut outputs = default_outputs(signature);
        if parser.keyword("YIELD") && !parser.take(b'*') {
            outputs.clear();
            loop {
                let name = parser.word()?;
                let field = signature
                    .outputs
                    .iter()
                    .copied()
                    // ubs:ignore -- public YIELD field names, not secret material.
                    .find(|field| field.name() == name)
                    .ok_or_else(|| parser.error(FnxBindErrorKind::UnknownYield))?;
                let alias = if parser.keyword("AS") {
                    parser.word()?
                } else {
                    name
                };
                if outputs
                    .iter()
                    // ubs:ignore -- public YIELD field names and aliases, not secret material.
                    .any(|column| column.field == field || column.name == alias)
                {
                    return Err(parser.error(FnxBindErrorKind::DuplicateYield));
                }
                outputs.push(FnxOutputColumn {
                    field,
                    name: alias.to_owned(),
                });
                if !parser.take(b',') {
                    break;
                }
            }
        }
        parser.take(b';');
        parser.space();
        if parser.pos != text.len() {
            return Err(parser.error(FnxBindErrorKind::TrailingInput));
        }
        Ok(Self::compiled(algorithm, outputs))
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
                hash.update(&options.alpha.to_bits().to_le_bytes());
                hash.update(&(options.max_iter as u128).to_le_bytes());
                hash.update(&options.tol.to_bits().to_le_bytes());
                hash.update(&[u8::from(options.weighted)]);
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
}
impl<'a> Parser<'a> {
    fn error(&self, kind: FnxBindErrorKind) -> FnxBindError {
        FnxBindError { at: self.pos, kind }
    }
    fn space(&mut self) {
        while self
            .text
            .as_bytes()
            .get(self.pos)
            .is_some_and(u8::is_ascii_whitespace)
        {
            self.pos += 1;
        }
    }
    fn take(&mut self, byte: u8) -> bool {
        self.space();
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
        } else {
            Err(self.error(FnxBindErrorKind::Expected(name)))
        }
    }
    fn word(&mut self) -> Result<&'a str, FnxBindError> {
        self.space();
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
    fn argument(&mut self, parameters: &FnxParameters) -> Result<FnxArgument, FnxBindError> {
        if self.take(b'$') {
            let name = self.word()?;
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
        let start = self.pos;
        while self.text.as_bytes().get(self.pos).is_some_and(|byte| {
            byte.is_ascii_digit() || matches!(*byte, b'+' | b'-' | b'.' | b'e' | b'E')
        }) {
            self.pos += 1;
        }
        if self.pos == start {
            return Err(self.error(FnxBindErrorKind::Expected("typed argument")));
        }
        let token = &self.text[start..self.pos];
        if token.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
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
                .parse::<u128>()
                .map(|value| FnxArgument::Vertex(VId(value)))
                .map_err(|_| self.error(FnxBindErrorKind::InvalidArgument("integer overflow")))
        }
    }
}

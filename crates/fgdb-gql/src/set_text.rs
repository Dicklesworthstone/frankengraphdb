//! Compound query preparation over the shared graph-text lexer and compiler.
//! Syntax is parsed before catalog access; execution sees only PreparedGraphSet.

pub(crate) mod aggregate;
pub(crate) mod multipart;

use crate::algebra::{GraphOrderError, GraphValueOrder, IntegerComparison};
use crate::set_ops::MAX_GRAPH_SET_DEPTH;
use crate::{
    GqlParameterSpec, GqlParameterType, GqlParameterValue, GqlParameters, GraphPatternTextError,
    GraphPatternTextErrorKind, GraphSetBuildError, GraphSetColumnType, GraphSetOperation,
    GraphSetQuantifier, GraphSymbol, GraphSymbolKind, MAX_GRAPH_SET_OPERANDS, PreparedGraphSet,
    PreparedGraphText,
};
use fgdb_delta_types::PropertyKeyId;
use std::collections::{BTreeMap, BTreeSet};

/// Original byte offsets, never query fragments, names, or argument payloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphSetTextError {
    pub offset: usize,
    pub kind: GraphSetTextErrorKind,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphSetTextErrorKind {
    CallBodyWrite,
    Expected(&'static str),
    Pattern(GraphPatternTextErrorKind),
    SetBuild(GraphSetBuildError),
    OrderBuild(GraphOrderError),
    ProjectionBuild(crate::GraphSetProjectionError),
    FilterBuild(crate::GraphSetFilterError),
    AggregateBuild(crate::GraphAggregateBuildError),
    /// UNION operands must expose the same names, independent of column order.
    DifferentColumnsInUnion,
    /// One unparenthesized UNION chain cannot mix ALL and DISTINCT.
    InvalidClauseComposition,
    IntegerExpression(crate::GraphIntegerBuildError),
    IntegerOperand,
    IntegerNesting {
        limit: usize,
    },
}
impl core::fmt::Display for GraphSetTextErrorKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::CallBodyWrite => f.write_str("writes inside inline CALL are not supported"),
            Self::Expected(item) => crate::graph_text::write_expected(f, item),
            Self::Pattern(kind) => kind.fmt(f),
            Self::SetBuild(error) => error.fmt(f),
            Self::OrderBuild(error) => error.fmt(f),
            Self::ProjectionBuild(error) => error.fmt(f),
            Self::FilterBuild(error) => error.fmt(f),
            Self::AggregateBuild(error) => error.fmt(f),
            Self::DifferentColumnsInUnion => {
                f.write_str("UNION operands must return the same column names")
            }
            Self::InvalidClauseComposition => {
                f.write_str("an unparenthesized UNION chain cannot mix ALL and DISTINCT")
            }
            Self::IntegerExpression(error) => error.fmt(f),
            Self::IntegerOperand => f.write_str("expected an integer operand"),
            Self::IntegerNesting { limit } => {
                write!(f, "integer expression nests deeper than {limit} levels")
            }
        }
    }
}
impl GraphSetTextErrorKind {
    #[must_use]
    pub fn class(&self) -> crate::GqlRefusalClass {
        use crate::GqlRefusalClass as C;
        match self {
            Self::CallBodyWrite => C::Unsupported,
            Self::Expected(_) | Self::IntegerOperand => C::Syntax,
            Self::Pattern(kind) => kind.class(),
            Self::SetBuild(_)
            | Self::OrderBuild(_)
            | Self::ProjectionBuild(_)
            | Self::FilterBuild(_)
            | Self::AggregateBuild(_)
            | Self::DifferentColumnsInUnion
            | Self::InvalidClauseComposition
            | Self::IntegerExpression(_)
            | Self::IntegerNesting { .. } => C::Statement,
        }
    }
}
impl GraphSetTextError {
    #[must_use]
    pub fn class(&self) -> crate::GqlRefusalClass {
        self.kind.class()
    }
}
impl core::fmt::Display for GraphSetTextError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "byte {}: {}", self.offset, self.kind)
    }
}
impl core::error::Error for GraphSetTextError {}
impl From<GraphPatternTextError> for GraphSetTextError {
    fn from(error: GraphPatternTextError) -> Self {
        pattern_error(0, error)
    }
}
fn fail(offset: usize, kind: GraphSetTextErrorKind) -> GraphSetTextError {
    GraphSetTextError { offset, kind }
}
fn expected(offset: usize, item: &'static str) -> GraphSetTextError {
    fail(offset, GraphSetTextErrorKind::Expected(item))
}
fn pattern_error(base: usize, error: GraphPatternTextError) -> GraphSetTextError {
    fail(
        base + error.offset,
        GraphSetTextErrorKind::Pattern(error.kind),
    )
}
fn rebase_error(base: usize, mut error: GraphSetTextError) -> GraphSetTextError {
    error.offset += base;
    error
}
fn pattern_kind(offset: usize, kind: GraphPatternTextErrorKind) -> GraphSetTextError {
    fail(offset, GraphSetTextErrorKind::Pattern(kind))
}

// Native leaf preparation owns these immutable templates. Only shared scalar
// bytecode and catalog-bound GLA projections reach the execution boundary.
#[derive(Clone)]
pub(crate) enum ReadValueTemplate {
    Column(usize),
    List(Vec<ReadValueTemplate>),
    Index {
        list: Box<ReadValueTemplate>,
        index: Box<ReadValueTemplate>,
    },
    Size(Box<ReadValueTemplate>),
    In {
        value: Box<ReadValueTemplate>,
        list: Box<ReadValueTemplate>,
    },
    /// Three-valued `left = right` over composite values (fgdb-9cawv).
    Equal {
        left: Box<ReadValueTemplate>,
        right: Box<ReadValueTemplate>,
    },
    /// Three-valued `<`, `<=`, `>` or `>=` over lists (openCypher
    /// comparability): `[1, 0] >= [1]`.
    Ordered {
        comparison: crate::algebra::IntegerComparison,
        left: Box<ReadValueTemplate>,
        right: Box<ReadValueTemplate>,
    },
    Literal(crate::GqlScalarParameter),
    Parameter {
        index: usize,
        at: usize,
        /// Captured before binding replaces an argument with its value. All
        /// later scalar-kind refinements preserve this static value domain.
        null_type: GraphSetColumnType,
    },
    Integer {
        program: Vec<crate::mutation_text::MutationIntegerTemplateOp>,
        at: usize,
    },
    /// A list-comprehension element, `k` bindings out (fgdb-20foe).
    Local(usize),
    Comprehension {
        list: Box<ReadValueTemplate>,
        filter: Option<Box<ReadValueTemplate>>,
        map: Option<Box<ReadValueTemplate>>,
    },
    Quantifier {
        kind: crate::GraphListQuantifier,
        list: Box<ReadValueTemplate>,
        predicate: Box<ReadValueTemplate>,
    },
    Slice {
        list: Box<ReadValueTemplate>,
        from: Option<Box<ReadValueTemplate>>,
        to: Option<Box<ReadValueTemplate>>,
    },
    Range {
        start: Box<ReadValueTemplate>,
        end: Box<ReadValueTemplate>,
        step: Option<Box<ReadValueTemplate>>,
    },
    Reduce {
        init: Box<ReadValueTemplate>,
        list: Box<ReadValueTemplate>,
        expr: Box<ReadValueTemplate>,
    },
    /// `{k: e, ...}`, keys sorted and unique (fgdb-2jw3z). A map projection
    /// `n{.a, ...}` carries its source as `guard` (NULL source, NULL map).
    MapLiteral {
        keys: Box<[Box<str>]>,
        values: Vec<ReadValueTemplate>,
        guard: Option<Box<ReadValueTemplate>>,
    },
    /// A map source plus explicit overrides, with NULL short-circuiting values.
    MapOverlay {
        base: Box<ReadValueTemplate>,
        keys: Box<[Box<str>]>,
        values: Vec<ReadValueTemplate>,
    },
    MapGet {
        map: Box<ReadValueTemplate>,
        key: Box<str>,
        property: ElementKeySlot,
    },
    Keys(Box<ReadValueTemplate>),
    /// A scalar expression over composite operands (fgdb-o5o9t): `values`
    /// evaluate over the enclosing row, then `body` over that row extended
    /// by one slot per value, value `i` at `Local(values.len() - 1 - i)`.
    Let {
        values: Vec<ReadValueTemplate>,
        body: Box<ReadValueTemplate>,
    },
    /// A temporal constructor (fgdb-txbg9), with the statement's pinned
    /// tzdb for named zones.
    Temporal {
        function: crate::GraphTemporalFunction,
        argument: Box<ReadValueTemplate>,
        zones: Option<crate::GqlTimeZones>,
    },
    /// reverse() or split() (fgdb-13l85); a longer call's argument is the
    /// list of its arguments.
    Function {
        function: crate::GraphValueFunction,
        argument: Box<ReadValueTemplate>,
    },
    /// A searched CASE whose results may be composite (fgdb-13l85): the
    /// first TRUE condition selects its value, else `otherwise` or NULL.
    Case {
        arms: Vec<(ReadValueTemplate, ReadValueTemplate)>,
        otherwise: Option<Box<ReadValueTemplate>>,
    },
    /// rand() at call `site`, under `scope` element locals (fgdb-13l85).
    Random {
        site: u32,
        scope: u32,
    },
}

/// The catalog binding of one `x.key` read (fgdb-xnd7x). The parser records
/// each read's key with an empty slot. Preparation resolves every recorded key
/// once, through the statement's resolver, before any binding, and binding
/// stamps the answer on the bound MapGet. A slot that no resolution filled
/// binds Unbound, so an element operand stays a typed error rather than NULL.
#[derive(Clone, Default)]
pub(crate) struct ElementKeySlot(std::sync::Arc<std::sync::OnceLock<Option<PropertyKeyId>>>);

impl ElementKeySlot {
    pub(crate) fn resolve(&self, key: Option<PropertyKeyId>) {
        // One statement resolves through one resolver, so a repeated
        // resolution would give the same answer.
        let _ = self.0.set(key);
    }

    pub(crate) fn bind(&self) -> crate::GraphElementKey {
        match self.0.get() {
            None => crate::GraphElementKey::Unbound,
            Some(None) => crate::GraphElementKey::Missing,
            Some(Some(key)) => crate::GraphElementKey::Property(*key),
        }
    }
}

impl ReadValueTemplate {
    /// Whether this expression can run after grouping without an input row.
    /// Element locals belong to their enclosing comprehension/LET; random
    /// draws remain row-dependent even when all explicit arguments are fixed.
    pub(crate) fn row_independent(&self) -> bool {
        use crate::GraphIntegerOp;
        use crate::mutation_text::MutationIntegerTemplateOp;
        match self {
            Self::Column(_) | Self::Random { .. } => false,
            Self::Literal(_) | Self::Parameter { .. } | Self::Local(_) => true,
            Self::Integer { program, .. } => !program.iter().any(|op| {
                matches!(
                    op,
                    MutationIntegerTemplateOp::Bound(
                        GraphIntegerOp::Column(_)
                            | GraphIntegerOp::ScalarColumn(_)
                            | GraphIntegerOp::IdentityColumn { .. }
                    )
                )
            }),
            Self::List(values) => values.iter().all(Self::row_independent),
            Self::Index { list, index } => list.row_independent() && index.row_independent(),
            Self::Size(value) | Self::Keys(value) => value.row_independent(),
            Self::In { value, list } => value.row_independent() && list.row_independent(),
            Self::Equal { left, right } | Self::Ordered { left, right, .. } => {
                left.row_independent() && right.row_independent()
            }
            Self::Temporal { argument, .. } | Self::Function { argument, .. } => {
                argument.row_independent()
            }
            Self::Comprehension { list, filter, map } => {
                list.row_independent()
                    && filter.as_deref().is_none_or(Self::row_independent)
                    && map.as_deref().is_none_or(Self::row_independent)
            }
            Self::Quantifier {
                list, predicate, ..
            } => list.row_independent() && predicate.row_independent(),
            Self::Slice { list, from, to } => {
                list.row_independent()
                    && from.as_deref().is_none_or(Self::row_independent)
                    && to.as_deref().is_none_or(Self::row_independent)
            }
            Self::Range { start, end, step } => {
                start.row_independent()
                    && end.row_independent()
                    && step.as_deref().is_none_or(Self::row_independent)
            }
            Self::Reduce { init, list, expr } => {
                init.row_independent() && list.row_independent() && expr.row_independent()
            }
            Self::MapLiteral { values, guard, .. } => {
                values.iter().all(Self::row_independent)
                    && guard.as_deref().is_none_or(Self::row_independent)
            }
            Self::MapGet { map, .. } => map.row_independent(),
            Self::MapOverlay { base, values, .. } => {
                base.row_independent() && values.iter().all(Self::row_independent)
            }
            Self::Let { values, body } => {
                values.iter().all(Self::row_independent) && body.row_independent()
            }
            Self::Case { arms, otherwise } => {
                arms.iter().all(|(condition, value)| {
                    condition.row_independent() && value.row_independent()
                }) && otherwise.as_deref().is_none_or(Self::row_independent)
            }
        }
    }

    pub(crate) fn parameter(index: usize, at: usize, kind: GqlParameterType) -> Self {
        Self::Parameter {
            index,
            at,
            null_type: GraphSetColumnType::parameter_domain(kind),
        }
    }

    pub(crate) fn column_type(
        &self,
        input: &[GraphSetColumnType],
        parameters: &[GqlParameterSpec],
    ) -> GraphSetColumnType {
        match self {
            Self::Column(index) => input[*index],
            // NULL is dynamically typed (bound as TypedNull(Any)).
            Self::Literal(value) if matches!(value.value(), fgdb_types::CanonicalScalar::Null) => {
                GraphSetColumnType::Any
            }
            Self::List(_)
            | Self::Comprehension { .. }
            | Self::Slice { .. }
            | Self::Range { .. } => GraphSetColumnType::List,
            // A temporal value reads its fields like a map entry (`d.year`).
            Self::Reduce { .. }
            | Self::MapLiteral { .. }
            | Self::MapOverlay { .. }
            | Self::MapGet { .. }
            | Self::Temporal { .. } => GraphSetColumnType::Any,
            Self::Keys(_) => GraphSetColumnType::List,
            // reverse() is a list or a text; split() is a list; a sum with a
            // list operand is a list or, over scalars, a scalar.
            Self::Function { function, .. } => match function {
                crate::GraphValueFunction::Reverse
                | crate::GraphValueFunction::Add
                | crate::GraphValueFunction::Subtract
                | crate::GraphValueFunction::Coalesce
                | crate::GraphValueFunction::PathNodes
                | crate::GraphValueFunction::PathRelationships => GraphSetColumnType::Any,
                crate::GraphValueFunction::Split => GraphSetColumnType::List,
                crate::GraphValueFunction::PercentileDisc
                | crate::GraphValueFunction::PercentileCont
                | crate::GraphValueFunction::StDev
                | crate::GraphValueFunction::StDevP => GraphSetColumnType::Scalar,
            },
            Self::Let { body, .. } => body.column_type(input, parameters),
            Self::Local(_) | Self::Case { .. } => GraphSetColumnType::Any,
            Self::Index { .. } => GraphSetColumnType::Any,
            Self::Parameter { index, .. } => {
                GraphSetColumnType::parameter_domain(parameters[*index].parameter_type)
            }
            _ => GraphSetColumnType::Scalar,
        }
    }
}
#[derive(Clone)]
pub(crate) struct ReadProjectionTemplate {
    pub(crate) name: String,
    pub(crate) value: ReadValueTemplate,
}
#[derive(Clone)]
pub(crate) enum ReadPageNumber {
    Literal(u64),
    Parameter(usize),
}
impl ReadValueTemplate {
    /// Shared resolved-unbound transcript: structural identity only. Literal
    /// scalars keep their canonical bytes; parameters appear as declaration
    /// indices; statement offsets never enter.
    pub(crate) fn append_template_transcript(&self, bytes: &mut Vec<u8>) {
        fn ordinal(bytes: &mut Vec<u8>, value: usize) {
            bytes.extend_from_slice(&(value as u64).to_be_bytes());
        }
        fn optional(bytes: &mut Vec<u8>, value: Option<&ReadValueTemplate>) {
            match value {
                Some(value) => {
                    bytes.push(1);
                    value.append_template_transcript(bytes);
                }
                None => bytes.push(0),
            }
        }
        match self {
            Self::Column(index) => {
                bytes.push(0);
                ordinal(bytes, *index);
            }
            Self::List(items) => {
                bytes.push(1);
                ordinal(bytes, items.len());
                for item in items {
                    item.append_template_transcript(bytes);
                }
            }
            Self::Index { list, index } => {
                bytes.push(2);
                list.append_template_transcript(bytes);
                index.append_template_transcript(bytes);
            }
            Self::Size(inner) => {
                bytes.push(3);
                inner.append_template_transcript(bytes);
            }
            Self::Literal(scalar) => {
                bytes.push(4);
                let encoded = scalar.canonical_bytes();
                ordinal(bytes, encoded.len());
                bytes.extend_from_slice(encoded);
            }
            Self::Parameter { index, .. } => {
                bytes.push(5);
                ordinal(bytes, *index);
            }
            Self::Integer { program, .. } => {
                bytes.push(6);
                ordinal(bytes, program.len());
                for op in program {
                    op.append_template_transcript(bytes);
                }
            }
            Self::In { value, list } => {
                bytes.push(7);
                value.append_template_transcript(bytes);
                list.append_template_transcript(bytes);
            }
            Self::Local(offset) => {
                bytes.push(8);
                ordinal(bytes, *offset);
            }
            Self::Comprehension { list, filter, map } => {
                bytes.push(9);
                list.append_template_transcript(bytes);
                for part in [filter, map] {
                    match part {
                        Some(part) => {
                            bytes.push(1);
                            part.append_template_transcript(bytes);
                        }
                        None => bytes.push(0),
                    }
                }
            }
            Self::Quantifier {
                kind,
                list,
                predicate,
            } => {
                bytes.extend_from_slice(&[10, kind.tag()]);
                list.append_template_transcript(bytes);
                predicate.append_template_transcript(bytes);
            }
            Self::Slice { list, from, to } => {
                bytes.push(11);
                list.append_template_transcript(bytes);
                for bound in [from, to] {
                    optional(bytes, bound.as_deref());
                }
            }
            Self::Range { start, end, step } => {
                bytes.push(12);
                start.append_template_transcript(bytes);
                end.append_template_transcript(bytes);
                optional(bytes, step.as_deref());
            }
            Self::Reduce { init, list, expr } => {
                bytes.push(13);
                init.append_template_transcript(bytes);
                list.append_template_transcript(bytes);
                expr.append_template_transcript(bytes);
            }
            Self::MapLiteral {
                keys,
                values,
                guard,
            } => {
                // An unguarded literal keeps its original bytes.
                if let Some(guard) = guard {
                    bytes.push(17);
                    guard.append_template_transcript(bytes);
                } else {
                    bytes.push(14);
                }
                ordinal(bytes, keys.len());
                for (key, value) in keys.iter().zip(values) {
                    ordinal(bytes, key.len());
                    bytes.extend_from_slice(key.as_bytes());
                    value.append_template_transcript(bytes);
                }
            }
            Self::MapGet { map, key, .. } => {
                bytes.push(15);
                map.append_template_transcript(bytes);
                ordinal(bytes, key.len());
                bytes.extend_from_slice(key.as_bytes());
            }
            Self::Keys(map) => {
                bytes.push(16);
                map.append_template_transcript(bytes);
            }
            Self::MapOverlay { base, keys, values } => {
                bytes.push(18);
                base.append_template_transcript(bytes);
                ordinal(bytes, keys.len());
                for (key, value) in keys.iter().zip(values) {
                    ordinal(bytes, key.len());
                    bytes.extend_from_slice(key.as_bytes());
                    value.append_template_transcript(bytes);
                }
            }
            Self::Let { values, body } => {
                bytes.push(19);
                ordinal(bytes, values.len());
                for value in values {
                    value.append_template_transcript(bytes);
                }
                body.append_template_transcript(bytes);
            }
            Self::Equal { left, right } => {
                bytes.push(20);
                left.append_template_transcript(bytes);
                right.append_template_transcript(bytes);
            }
            Self::Temporal {
                function,
                argument,
                zones,
            } => {
                // A pinned tzdb is part of the plan: tag 26 and its identity.
                match zones {
                    None => bytes.extend_from_slice(&[21, function.tag()]),
                    Some(zones) => {
                        bytes.extend_from_slice(&[26, function.tag()]);
                        bytes.extend_from_slice(zones.tzdb().as_bytes());
                    }
                }
                argument.append_template_transcript(bytes);
            }
            Self::Function { function, argument } => {
                bytes.extend_from_slice(&[23, function.tag()]);
                argument.append_template_transcript(bytes);
            }
            Self::Ordered {
                comparison,
                left,
                right,
            } => {
                bytes.extend_from_slice(&[22, comparison.tag()]);
                left.append_template_transcript(bytes);
                right.append_template_transcript(bytes);
            }
            Self::Case { arms, otherwise } => {
                bytes.push(24);
                ordinal(bytes, arms.len());
                for (condition, value) in arms {
                    condition.append_template_transcript(bytes);
                    value.append_template_transcript(bytes);
                }
                optional(bytes, otherwise.as_deref());
            }
            Self::Random { site, scope } => {
                bytes.push(25);
                bytes.extend_from_slice(&site.to_be_bytes());
                bytes.extend_from_slice(&scope.to_be_bytes());
            }
        }
    }
}
impl ReadProjectionTemplate {
    pub(crate) fn append_template_transcript(&self, bytes: &mut Vec<u8>) {
        bytes.extend_from_slice(&(self.name.len() as u64).to_be_bytes());
        bytes.extend_from_slice(self.name.as_bytes());
        self.value.append_template_transcript(bytes);
    }
}
impl ReadPageNumber {
    pub(crate) fn append_template_transcript(&self, bytes: &mut Vec<u8>) {
        match self {
            Self::Literal(value) => {
                bytes.push(0);
                bytes.extend_from_slice(&value.to_be_bytes());
            }
            Self::Parameter(index) => {
                bytes.push(1);
                bytes.extend_from_slice(&(*index as u64).to_be_bytes());
            }
        }
    }
}
impl ReadFilterOperand {
    pub(crate) fn append_template_transcript(&self, bytes: &mut Vec<u8>) {
        match self {
            Self::Column(index) => {
                bytes.push(0);
                bytes.extend_from_slice(&(*index as u64).to_be_bytes());
            }
            Self::Literal(scalar) => {
                bytes.push(1);
                let encoded = scalar.canonical_bytes();
                bytes.extend_from_slice(&(encoded.len() as u64).to_be_bytes());
                bytes.extend_from_slice(encoded);
            }
            Self::Parameter { index, .. } => {
                bytes.push(2);
                bytes.extend_from_slice(&(*index as u64).to_be_bytes());
            }
        }
    }
}
impl ReadFilterOp {
    pub(crate) fn append_template_transcript(&self, bytes: &mut Vec<u8>) {
        match self {
            Self::Compare {
                left,
                comparison,
                right,
            } => {
                bytes.push(0);
                left.append_template_transcript(bytes);
                bytes.push(comparison_tag(*comparison));
                right.append_template_transcript(bytes);
            }
            Self::IsNull { operand, is_null } => {
                bytes.push(1);
                operand.append_template_transcript(bytes);
                bytes.push(u8::from(*is_null));
            }
            Self::Truth(value) => {
                bytes.push(2);
                bytes.push(match value {
                    None => 0,
                    Some(false) => 1,
                    Some(true) => 2,
                });
            }
            Self::Not => bytes.push(3),
            Self::And => bytes.push(4),
            Self::Or => bytes.push(5),
        }
    }
}
impl ReadStageTemplate {
    /// Count the physical relational nodes produced by native binding.
    pub(crate) fn depth(&self) -> usize {
        match self {
            Self::Arguments { .. } => 0,
            Self::Page { computed, .. } => usize::from(computed.is_some()) * 2,
            Self::Aggregate { .. } => 3,
            _ => 1,
        }
    }

    pub(crate) fn append_template_transcript(&self, bytes: &mut Vec<u8>) {
        match self {
            Self::Arguments { columns, types } => {
                bytes.push(9);
                bytes.extend_from_slice(b"fgdb:gql:call-arguments:v1\0");
                bytes.extend_from_slice(&(columns.len() as u64).to_be_bytes());
                for (name, kind) in columns.iter().zip(types) {
                    bytes.extend_from_slice(&(name.len() as u64).to_be_bytes());
                    bytes.extend_from_slice(name.as_bytes());
                    bytes.push(set_column_type_tag(*kind));
                }
            }
            Self::Aggregate { stage, .. } => {
                bytes.push(4);
                stage.append_template_transcript(bytes);
            }
            Self::Unwind { name, value, .. } => {
                bytes.push(0);
                bytes.extend_from_slice(&(name.len() as u64).to_be_bytes());
                bytes.extend_from_slice(name.as_bytes());
                value.append_template_transcript(bytes);
            }
            Self::InputRows { name } => {
                bytes.push(0xff);
                bytes.extend_from_slice(b"fgdb:gql:initial-rows:v1\0");
                bytes.extend_from_slice(&(name.len() as u64).to_be_bytes());
                bytes.extend_from_slice(name.as_bytes());
            }
            Self::Call {
                namespace,
                name,
                arguments,
                names,
                outputs,
                vertices,
                graph_projection,
                ..
            } => {
                // A named call has its own tag, so every positional call keeps
                // its exact template bytes.
                bytes.push(if graph_projection.is_some() {
                    8
                } else if names.is_empty() {
                    5
                } else {
                    6
                });
                for text in [namespace, name] {
                    bytes.extend_from_slice(&(text.len() as u64).to_be_bytes());
                    bytes.extend_from_slice(text.as_bytes());
                }
                if let Some(projection) = graph_projection {
                    projection.append_transcript(bytes);
                }
                bytes.extend_from_slice(&(arguments.len() as u64).to_be_bytes());
                for (index, argument) in arguments.iter().enumerate() {
                    if let Some(name) = names.get(index) {
                        bytes.extend_from_slice(&(name.len() as u64).to_be_bytes());
                        bytes.extend_from_slice(name.as_bytes());
                    }
                    argument.append_template_transcript(bytes);
                }
                bytes.extend_from_slice(&(outputs.len() as u64).to_be_bytes());
                for (output, alias) in outputs {
                    for text in [output, alias] {
                        bytes.extend_from_slice(&(text.len() as u64).to_be_bytes());
                        bytes.extend_from_slice(text.as_bytes());
                    }
                }
                bytes.extend_from_slice(&(vertices.len() as u64).to_be_bytes());
                for &output in vertices {
                    bytes.extend_from_slice(&(output as u64).to_be_bytes());
                }
            }
            Self::Project {
                projection,
                quantifier,
                ..
            } => {
                bytes.push(1);
                bytes.push(match quantifier {
                    GraphSetQuantifier::All => 0,
                    GraphSetQuantifier::Distinct => 1,
                });
                append_projection_transcript(bytes, projection);
            }
            Self::Filter { code, .. } => {
                bytes.push(2);
                bytes.extend_from_slice(&(code.len() as u64).to_be_bytes());
                for op in code {
                    op.append_template_transcript(bytes);
                }
            }
            Self::Page {
                order,
                offset,
                count,
                computed,
                ..
            } => {
                // Existing column-only pages keep their exact identity.
                bytes.push(if computed.is_some() { 7 } else { 3 });
                if let Some(computed) = computed {
                    bytes.extend_from_slice(&(computed.visible as u64).to_be_bytes());
                    bytes.extend_from_slice(&(computed.values.len() as u64).to_be_bytes());
                    for value in &computed.values {
                        value.append_template_transcript(bytes);
                    }
                }
                bytes.extend_from_slice(&(order.len() as u64).to_be_bytes());
                for column in order {
                    bytes.push(u8::from(column.descending));
                    bytes.extend_from_slice(&(column.column as u64).to_be_bytes());
                    bytes.push(u8::from(column.nulls_first));
                }
                offset.append_template_transcript(bytes);
                match count {
                    None => bytes.push(0),
                    Some(count) => {
                        bytes.push(1);
                        count.append_template_transcript(bytes);
                    }
                }
            }
        }
    }
}

/// Comparison tag shared with the boolean/aggregate transcript convention.
pub(crate) fn comparison_tag(comparison: IntegerComparison) -> u8 {
    match comparison {
        IntegerComparison::Equal => 0,
        IntegerComparison::NotEqual => 1,
        IntegerComparison::Less => 2,
        IntegerComparison::LessOrEqual => 3,
        IntegerComparison::Greater => 4,
        IntegerComparison::GreaterOrEqual => 5,
    }
}

pub(crate) fn append_projection_transcript(bytes: &mut Vec<u8>, items: &[ReadProjectionTemplate]) {
    bytes.extend_from_slice(&(items.len() as u64).to_be_bytes());
    for item in items {
        item.append_template_transcript(bytes);
    }
}

impl BoundSetTextInput {
    /// Shared resolved-unbound input transcript: nested selection template,
    /// parameter schema names+types, projection, stage pipelines and
    /// correlations. Statement text and argument values never enter.
    pub(crate) fn append_template_transcript(&self, bytes: &mut Vec<u8>) {
        fn ordinal(bytes: &mut Vec<u8>, value: usize) {
            bytes.extend_from_slice(&(value as u64).to_be_bytes());
        }
        if !self.row_fields.is_empty() {
            // Input tags 0/1 are the original selection, 2/3 are multipart
            // owners. Keep every existing definition byte-identical while
            // giving computed join keys a distinct, self-delimiting domain.
            bytes.push(4);
            bytes.extend_from_slice(b"fgdb:gql:row-field-correlations:v1\0");
            ordinal(bytes, self.row_fields.len());
            for value in &self.row_fields {
                value.append_template_transcript(bytes);
            }
        }
        match &self.selection {
            None => bytes.push(0),
            Some(selection) => {
                bytes.push(1);
                let encoded = selection.template_bytes();
                ordinal(bytes, encoded.len());
                bytes.extend_from_slice(&encoded);
            }
        }
        ordinal(bytes, self.parameters.len());
        for spec in &self.parameters {
            ordinal(bytes, spec.name.len());
            bytes.extend_from_slice(spec.name.as_bytes());
            bytes.push(parameter_type_tag(spec.parameter_type));
        }
        match &self.projection {
            None => bytes.push(0),
            Some(items) => {
                bytes.push(1);
                append_projection_transcript(bytes, items);
            }
        }
        bytes.push(match self.quantifier {
            GraphSetQuantifier::All => 0,
            GraphSetQuantifier::Distinct => 1,
        });
        ordinal(bytes, self.pipeline.len());
        for stage in &self.pipeline {
            stage.append_template_transcript(bytes);
        }
        bytes.push(u8::from(self.singleton));
        ordinal(bytes, self.leading.len());
        for stage in &self.leading {
            stage.append_template_transcript(bytes);
        }
        ordinal(bytes, self.correlations.len());
        for (from, to) in &self.correlations {
            ordinal(bytes, *from);
            ordinal(bytes, *to);
        }
        // Only an owned input appends: every other transcript keeps its bytes.
        if let Some((columns, types)) = self.owned.as_deref() {
            bytes.push(0xff);
            ordinal(bytes, columns.len());
            for (name, kind) in columns.iter().zip(types) {
                ordinal(bytes, name.len());
                bytes.extend_from_slice(name.as_bytes());
                bytes.push(set_column_type_tag(*kind));
            }
        }
    }
}

fn parameter_type_tag(kind: GqlParameterType) -> u8 {
    match kind {
        GqlParameterType::Int64 => 0,
        GqlParameterType::UInt64 => 1,
        GqlParameterType::List => 2,
        GqlParameterType::Scalar(kind) => 3 + kind as u8,
        // Keep every existing parameter tag unchanged; canonical scalar kinds
        // occupy tags 3..=10.
        GqlParameterType::Map => 11,
    }
}

fn set_column_type_tag(kind: GraphSetColumnType) -> u8 {
    kind.tag()
}
#[derive(Clone)]
pub(crate) enum ReadFilterOperand {
    Column(usize),
    Literal(crate::GqlScalarParameter),
    Parameter { index: usize, at: usize },
}
#[derive(Clone)]
pub(crate) enum ReadFilterOp {
    Compare {
        left: ReadFilterOperand,
        comparison: crate::algebra::IntegerComparison,
        right: ReadFilterOperand,
    },
    IsNull {
        operand: ReadFilterOperand,
        is_null: bool,
    },

    Truth(Option<bool>),
    Not,
    And,
    Or,
}
#[derive(Clone)]
pub(crate) enum ReadStageTemplate {
    /// A CALL body's typed lexical imports. Binding replaces its unit root
    /// with the nearest Apply frame; it never reads a write's owned rows.
    Arguments {
        columns: Vec<String>,
        types: Vec<GraphSetColumnType>,
    },
    Aggregate {
        at: usize,
        stage: aggregate::ReadAggregateStage,
    },
    /// `CALL namespace.name(arguments) YIELD output [AS alias], ...`, the
    /// first stage of a read pipeline (Appendix C `ProcedureCall`).
    Call {
        at: usize,
        namespace: String,
        name: String,
        arguments: Vec<ReadValueTemplate>,
        /// One name per argument when the call names them (`k => 10`);
        /// empty for a positional call. A call never mixes the two.
        names: Vec<String>,
        /// Byte offset of each argument's value, for binding refusals.
        offsets: Vec<usize>,
        /// (procedure output, column alias), in YIELD order.
        outputs: Vec<(String, String)>,
        /// YIELD positions the statement matches as vertices (`MATCH (n)` on
        /// the column, written or implied by `n.p`), ascending. The host
        /// refuses unless each is a vertex-valued output (fgdb-luq0b).
        vertices: Vec<usize>,
        /// The first positional fnx map after its literal schema names have
        /// been resolved. It is not an algorithm argument at execution.
        graph_projection: Option<crate::ProcedureGraphProjection>,
    },
    Unwind {
        at: usize,
        name: String,
        value: ReadValueTemplate,
    },
    /// A typed external input relation, installed only after validating the
    /// leading UNWIND source. Each input row has one canonical map cell.
    InputRows {
        name: String,
    },
    Project {
        at: usize,
        projection: Vec<ReadProjectionTemplate>,
        quantifier: GraphSetQuantifier,
    },
    Filter {
        at: usize,
        code: Vec<ReadFilterOp>,
    },
    Page {
        at: usize,
        order: Vec<GraphValueOrder>,
        offset: ReadPageNumber,
        count: Option<ReadPageNumber>,
        computed: Option<ReadOrderProjection>,
    },
}

/// Scalar keys are evaluated over the frozen input row, then ranked and
/// removed. `visible` also removes graph/input cells retained solely for this
/// page. The complete projection remains private through successful paging.
#[derive(Clone)]
pub(crate) struct ReadOrderProjection {
    pub(crate) values: Vec<ReadValueTemplate>,
    pub(crate) visible: usize,
}

/// A catalog-free output-row page compiled by the shared expression parser.
/// Parameter ordinals are local to this page; the compound statement admits
/// their names and types together with every leaf before resolving any symbol.
#[derive(Clone)]
pub(crate) struct CompoundOrderTemplate {
    pub(crate) stage: ReadStageTemplate,
    pub(crate) parameters: Vec<GqlParameterSpec>,
    pub(crate) parameter_offsets: Vec<usize>,
}

#[derive(Clone)]
pub(crate) struct BoundSetTextInput {
    pub(crate) selection: Option<PreparedGraphText>,
    pub(crate) parameters: Vec<GqlParameterSpec>,
    pub(crate) parameter_offsets: Vec<usize>,
    pub(crate) return_at: usize,
    pub(crate) projection: Option<Vec<ReadProjectionTemplate>>,
    pub(crate) quantifier: GraphSetQuantifier,
    pub(crate) pipeline: Vec<ReadStageTemplate>,
    pub(crate) singleton: bool,
    pub(crate) leading: Vec<ReadStageTemplate>,
    pub(crate) correlations: Vec<(usize, usize)>,
    pub(crate) row_fields: Vec<ReadValueTemplate>,
    /// A chained write's input (`CREATE (a) WITH a CREATE (b)`): the previous
    /// write's output columns, read through one owned leaf (`SetNode::Owned`)
    /// that the executing host fills with that write's rows. No graph source.
    /// Boxed: only chain steps have one, and every read template embeds this.
    #[allow(clippy::type_complexity)]
    pub(crate) owned: Option<Box<(Vec<String>, Vec<GraphSetColumnType>)>>,
}

// A token view, not a lexer. Only graph_text's existing Lexer constructs these.
// Quoted strings remain opaque tokens and cannot introduce set delimiters.
#[derive(Clone, Copy)]
pub(crate) enum TextKind<'a> {
    Word(&'a str),
    Digits(&'a str),
    Parameter(&'a str),
    Quoted,
    Punct(u8),
    End,
}
#[derive(Clone, Copy)]
pub(crate) struct TextToken<'a> {
    pub(crate) kind: TextKind<'a>,
    pub(crate) at: usize,
}
impl TextToken<'_> {
    fn word(self, expected: &str) -> bool {
        matches!(self.kind, TextKind::Word(word) if word.eq_ignore_ascii_case(expected))
    }
    fn punct(self, expected: u8) -> bool {
        matches!(self.kind, TextKind::Punct(ch) if ch == expected)
    }
}

#[derive(Clone)]
enum PageNumber {
    Literal(u64),
    Parameter(String),
}
impl PageNumber {
    fn value(&self, arguments: &GqlParameters) -> u64 {
        match self {
            Self::Literal(value) => *value,
            Self::Parameter(name) => match arguments.get(name) {
                Some(GqlParameterValue::UInt64(value)) => value,
                _ => unreachable!("the complete argument schema was validated before binding"),
            },
        }
    }
}
#[derive(Clone)]
struct OrderKey {
    name: String,
    /// `Some(var)` for an `ORDER BY var.name` key (fgdb-2t30i).
    variable: Option<String>,
    at: usize,
    order: GraphValueOrder,
}
/// Source spans exist only during compound preparation. Once validated, the
/// page holds a native expression template; binding never scans the span again.
#[derive(Clone)]
struct OrderExpression {
    span: std::ops::Range<usize>,
    template: Option<CompoundOrderTemplate>,
}
impl OrderExpression {
    fn prepared(&self) -> &CompoundOrderTemplate {
        self.template
            .as_ref()
            .expect("compound ordering is checked before catalog access")
    }
}

#[derive(Clone)]
enum NodeKind {
    Leaf(usize),
    Scope(Box<Node>),
    Binary {
        operation: GraphSetOperation,
        quantifier: GraphSetQuantifier,
        left: Box<Node>,
        right: Box<Node>,
    },
}
#[derive(Clone)]
struct Node {
    kind: NodeKind,
    at: usize,
    depth: usize,
    order: Vec<OrderKey>,
    offset: PageNumber,
    count: Option<PageNumber>,
    expression_order: Option<OrderExpression>,
}
impl Node {
    fn new(kind: NodeKind, at: usize, depth: usize) -> Result<Self, GraphSetTextError> {
        if depth > MAX_GRAPH_SET_DEPTH {
            return Err(fail(
                at,
                GraphSetTextErrorKind::SetBuild(GraphSetBuildError::TooDeep {
                    limit: MAX_GRAPH_SET_DEPTH,
                    observed: depth,
                }),
            ));
        }
        Ok(Self {
            kind,
            at,
            depth,
            order: Vec::new(),
            offset: PageNumber::Literal(0),
            count: None,
            expression_order: None,
        })
    }
    /// Whether every ORDER BY key names an output column of `schema`, exactly
    /// as validate resolves a single read's keys.
    fn order_resolves(&self, schema: &Schema) -> bool {
        self.order.iter().all(|key| match &key.variable {
            None => schema.columns.contains(&key.name),
            Some(variable) => schema.returned.iter().any(|source| {
                source
                    .as_ref()
                    .is_some_and(|(v, p)| v == variable && *p == key.name)
            }),
        })
    }
    fn validate(
        &mut self,
        schemas: &[Schema],
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        uses: &mut Vec<(usize, GqlParameterSpec)>,
    ) -> Result<Schema, GraphSetTextError> {
        // Every subtree returns its derived schema. A leftmost leaf alone is
        // insufficient once a descendant set widens a concrete column to Any.
        let mut schema = match &mut self.kind {
            NodeKind::Leaf(at) => {
                self.depth = schemas[*at].depth;
                schemas[*at].clone()
            }
            NodeKind::Scope(input) => {
                let schema = input.validate(schemas, statement, declarations, uses)?;
                self.depth = input.depth + 1;
                schema
            }
            NodeKind::Binary {
                operation,
                left,
                right,
                ..
            } => {
                let mut left_schema = left.validate(schemas, statement, declarations, uses)?;
                let right_schema = right.validate(schemas, statement, declarations, uses)?;
                let alignment = (*operation == GraphSetOperation::Union)
                    .then(|| union_column_order(&left_schema.columns, &right_schema.columns))
                    .transpose()
                    .map_err(|()| fail(self.at, GraphSetTextErrorKind::DifferentColumnsInUnion))?;
                let reorders = alignment
                    .as_ref()
                    .is_some_and(|columns| columns.iter().enumerate().any(|(i, &j)| i != j));
                self.depth = 1 + left.depth.max(right.depth + usize::from(reorders));
                if left_schema.types.len() != right_schema.types.len() {
                    return Err(fail(
                        self.at,
                        GraphSetTextErrorKind::SetBuild(GraphSetBuildError::ColumnCount {
                            left: left_schema.types.len(),
                            right: right_schema.types.len(),
                        }),
                    ));
                }
                for (column, left) in left_schema.types.iter_mut().enumerate() {
                    let right = right_schema.types
                        [alignment.as_ref().map_or(column, |columns| columns[column])];
                    *left = left.common_set_domain(right).ok_or_else(|| {
                        fail(
                            self.at,
                            GraphSetTextErrorKind::SetBuild(GraphSetBuildError::ColumnType {
                                column,
                                left: *left,
                                right,
                            }),
                        )
                    })?;
                }
                left_schema
            }
        };
        if let Some(order) = &mut self.expression_order {
            let template = CompoundOrderTemplate::prepare(
                &statement[order.span.clone()],
                declarations,
                &schema.columns,
                &schema.types,
            )
            .map_err(|error| rebase_error(order.span.start, error))?;
            // The ordinary native page adds both its key projection and its
            // order-preserving private-column removal to the physical depth.
            self.depth += template.stage.depth();
            for (spec, offset) in template.parameters.iter().zip(&template.parameter_offsets) {
                uses.push((order.span.start + *offset, spec.clone()));
            }
            order.template = Some(template);
        }
        if self.depth > MAX_GRAPH_SET_DEPTH {
            return Err(fail(
                self.at,
                GraphSetTextErrorKind::SetBuild(GraphSetBuildError::TooDeep {
                    limit: MAX_GRAPH_SET_DEPTH,
                    observed: self.depth,
                }),
            ));
        }
        schema.depth = self.depth;
        let mut used = BTreeSet::new();
        for key in &mut self.order {
            let column = match &key.variable {
                None => schema
                    .columns
                    .iter()
                    .position(|name| *name == key.name)
                    .ok_or_else(|| expected(key.at, "a leftmost output column name"))?,
                // `ORDER BY var.name` names the RETURN item that is exactly
                // that property (fgdb-2t30i). Only a query's own RETURN: after
                // a set operator, ORDER BY names output columns only.
                Some(variable) => matches!(self.kind, NodeKind::Leaf(_))
                    .then(|| {
                        schema.returned.iter().position(|source| {
                            source
                                .as_ref()
                                .is_some_and(|(v, p)| v == variable && *p == key.name)
                        })
                    })
                    .flatten()
                    .ok_or_else(|| expected(key.at, "a returned property or output column name"))?,
            };
            if !used.insert(column) {
                return Err(fail(
                    key.at,
                    GraphSetTextErrorKind::OrderBuild(GraphOrderError::DuplicateColumn { column }),
                ));
            }
            // A resolved key names its output column, so the template records
            // the statement, not its spelling: `ORDER BY m.p` over
            // `RETURN m.p AS v` equals `ORDER BY v`.
            if key.variable.take().is_some() {
                key.name.clone_from(&schema.columns[column]);
            }
            key.order.column = column;
        }
        Ok(schema)
    }
    fn bind(
        &self,
        inputs: &mut [Option<PreparedGraphSet>],
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        let mut bound = match &self.kind {
            NodeKind::Leaf(at) => inputs[*at].take().expect("each syntax leaf is unique"),
            NodeKind::Scope(input) => input
                .bind(inputs, arguments)?
                .nested()
                .map_err(|kind| fail(self.at, GraphSetTextErrorKind::SetBuild(kind)))?,
            NodeKind::Binary {
                operation,
                quantifier,
                left,
                right,
            } => {
                let left = left.bind(inputs, arguments)?;
                let mut right = right.bind(inputs, arguments)?;
                if *operation == GraphSetOperation::Union && left.columns() != right.columns() {
                    let columns =
                        union_column_order(left.columns(), right.columns()).map_err(|()| {
                            fail(self.at, GraphSetTextErrorKind::DifferentColumnsInUnion)
                        })?;
                    let projection = left
                        .columns()
                        .iter()
                        .zip(columns)
                        .map(|(name, column)| {
                            crate::GraphSetProjection::new(
                                name.clone(),
                                crate::GraphSetValue::Column(column),
                            )
                        })
                        .collect();
                    // Preserve each arm's own order/page and multiplicity;
                    // the enclosing UNION owns its ALL/DISTINCT semantics.
                    right = right
                        .project(projection, GraphSetQuantifier::All)
                        .map_err(|kind| {
                            fail(self.at, GraphSetTextErrorKind::ProjectionBuild(kind))
                        })?;
                }
                left.combine(*operation, *quantifier, right)
                    .map_err(|kind| fail(self.at, GraphSetTextErrorKind::SetBuild(kind)))?
            }
        };
        if !self.order.is_empty() {
            let order: Vec<_> = self.order.iter().map(|key| key.order).collect();
            bound = bound
                .with_order_by(&order)
                .map_err(|kind| fail(self.at, GraphSetTextErrorKind::OrderBuild(kind)))?;
        }
        bound = bound.with_page(
            self.offset.value(arguments),
            self.count.as_ref().map(|n| n.value(arguments)),
        );
        if let Some(order) = &self.expression_order {
            bound = order
                .prepared()
                .bind(bound, arguments)
                .map_err(|error| rebase_error(order.span.start, error))?;
        }
        Ok(bound)
    }
}
#[derive(Clone)]
struct Schema {
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    depth: usize,
    /// Per column, `(var, name)` when its RETURN item is exactly `var.name`.
    returned: Vec<Option<(String, String)>>,
}

fn union_column_order(left: &[String], right: &[String]) -> Result<Vec<usize>, ()> {
    if left.len() != right.len() {
        return Err(());
    }
    left.iter()
        .map(|name| {
            right
                .iter()
                .position(|candidate| candidate == name)
                .ok_or(())
        })
        .collect()
}

/// Whether the token at `at` opens a read pipeline: MATCH, OPTIONAL MATCH,
/// UNWIND, CALL, RETURN or WITH.
fn opens_pipeline(tokens: &[TextToken<'_>], at: usize) -> bool {
    tokens.get(at).is_some_and(|token| {
        token.word("MATCH")
            || token.word("UNWIND")
            || token.word("CALL")
            || token.word("RETURN")
            || token.word("WITH")
            || (token.word("OPTIONAL") && tokens.get(at + 1).is_some_and(|next| next.word("MATCH")))
    })
}

/// The terminal RETURN's plain property items, one entry per output column:
/// `(var, name)` for an item that is exactly `var.name`, with or without an
/// alias. An entry is kept only when the item's position names the column it
/// should (its alias, else its source text `var.name`), so a RETURN this
/// cannot read item by item yields no entries rather than a misaligned one.
fn returned_properties(
    tokens: &[TextToken<'_>],
    columns: &[String],
) -> Vec<Option<(String, String)>> {
    let mut depth = 0_usize;
    let mut start = None;
    for (at, token) in tokens.iter().enumerate() {
        match token.kind {
            TextKind::Punct(b'(' | b'[' | b'{') => depth += 1,
            TextKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
            _ if depth == 0 && token.word("RETURN") => start = Some(at + 1),
            _ => {}
        }
    }
    let mut items = Vec::new();
    if let Some(mut at) = start {
        if tokens.get(at).is_some_and(|token| token.word("DISTINCT")) {
            at += 1;
        }
        let mut item = Vec::new();
        depth = 0;
        for token in &tokens[at..] {
            match token.kind {
                TextKind::Punct(b'(' | b'[' | b'{') => depth += 1,
                TextKind::Punct(b')' | b']' | b'}') => depth = depth.saturating_sub(1),
                TextKind::Punct(b',') if depth == 0 => {
                    items.push(std::mem::take(&mut item));
                    continue;
                }
                TextKind::End => break,
                _ => {}
            }
            item.push(token.kind);
        }
        items.push(item);
    }
    if items.len() != columns.len() {
        return vec![None; columns.len()];
    }
    items
        .iter()
        .zip(columns)
        .map(|(item, column)| match item.as_slice() {
            [
                TextKind::Word(v),
                TextKind::Punct(b'.'),
                TextKind::Word(p),
                rest @ ..,
            ] => {
                let named = match rest {
                    [] => format!("{v}.{p}"),
                    [TextKind::Word(as_), TextKind::Word(alias)]
                        if as_.eq_ignore_ascii_case("AS") =>
                    {
                        (*alias).to_owned()
                    }
                    _ => return None,
                };
                (named == *column).then(|| ((*v).to_owned(), (*p).to_owned()))
            }
            _ => None,
        })
        .collect()
}
struct Span {
    start: usize,
    end: usize,
    first_token: usize,
    last_token: usize,
}
struct Composition<'a> {
    tokens: Vec<TextToken<'a>>,
    at: usize,
    spans: Vec<Span>,
    page_parameters: Vec<(usize, GqlParameterSpec)>,
}
impl<'a> Composition<'a> {
    fn current(&self) -> TextToken<'a> {
        self.tokens[self.at]
    }
    fn advance(&mut self) {
        if !matches!(self.current().kind, TextKind::End) {
            self.at += 1;
        }
    }
    fn take_word(&mut self, word: &str) -> bool {
        if !self.current().word(word) {
            return false;
        }
        self.advance();
        true
    }
    fn take(&mut self, ch: u8) -> bool {
        if !self.current().punct(ch) {
            return false;
        }
        self.advance();
        true
    }
    fn require(&mut self, ch: u8, item: &'static str) -> Result<(), GraphSetTextError> {
        if self.take(ch) {
            Ok(())
        } else {
            Err(expected(self.current().at, item))
        }
    }
    fn expression(&mut self, depth: usize) -> Result<Node, GraphSetTextError> {
        if depth >= MAX_GRAPH_SET_DEPTH {
            return Err(fail(
                self.current().at,
                GraphSetTextErrorKind::SetBuild(GraphSetBuildError::TooDeep {
                    limit: MAX_GRAPH_SET_DEPTH,
                    observed: depth + 1,
                }),
            ));
        }
        let mut node = self.intersection(depth)?;
        let mut union_quantifier = None;
        while self.current().word("UNION") || self.current().word("EXCEPT") {
            let token = self.current();
            let operation = if token.word("UNION") {
                GraphSetOperation::Union
            } else {
                GraphSetOperation::Except
            };
            self.advance();
            let quantifier = self.quantifier();
            if operation == GraphSetOperation::Union {
                if union_quantifier.is_some_and(|previous| previous != quantifier) {
                    return Err(fail(
                        token.at,
                        GraphSetTextErrorKind::InvalidClauseComposition,
                    ));
                }
                union_quantifier = Some(quantifier);
            }
            let right = self.intersection(depth)?;
            let height = 1 + node.depth.max(right.depth);
            node = Node::new(
                NodeKind::Binary {
                    operation,
                    quantifier,
                    left: Box::new(node),
                    right: Box::new(right),
                },
                token.at,
                height,
            )?;
        }
        if self.has_expression_order(matches!(node.kind, NodeKind::Leaf(_))) {
            let start = self.current().at;
            let end = self.expression_order_end()?;
            if let NodeKind::Leaf(leaf) = node.kind {
                // A single read keeps its existing graph/row expression scope.
                self.spans[leaf].end = end;
                self.spans[leaf].last_token = self.at;
            } else {
                // A compound page sees only the completed output schema. Keep
                // the original bytes, including original diagnostic positions;
                // no synthetic RETURN/MATCH text or execution-time parser.
                node.expression_order = Some(OrderExpression {
                    span: start..end,
                    template: None,
                });
            }
        } else {
            self.tail(&mut node)?;
        }
        Ok(node)
    }

    /// Find one untouched page span using the already admitted token stream.
    /// An enclosing set parenthesis is not part of the scalar expression.
    /// Check delimiter kinds as well as depth before any catalog callback.
    fn expression_order_end(&mut self) -> Result<usize, GraphSetTextError> {
        let mut delimiters = Vec::new();
        loop {
            let token = self.current();
            if matches!(token.kind, TextKind::End)
                || (delimiters.is_empty() && (token.punct(b')') || token.punct(b';')))
            {
                break;
            }
            match token.kind {
                TextKind::Punct(ch @ (b'(' | b'[' | b'{')) => {
                    if delimiters.len() == MAX_GRAPH_SET_DEPTH {
                        return Err(expected(token.at, "bounded graph-expression nesting"));
                    }
                    delimiters.push(ch);
                }
                TextKind::Punct(ch @ (b')' | b']' | b'}')) => {
                    let opening = match ch {
                        b')' => b'(',
                        b']' => b'[',
                        _ => b'{',
                    };
                    if delimiters.pop() != Some(opening) {
                        return Err(expected(token.at, "matching graph-expression delimiter"));
                    }
                }
                _ => {}
            }
            self.advance();
        }
        if !delimiters.is_empty() {
            return Err(expected(
                self.current().at,
                "closing graph-expression delimiter",
            ));
        }
        Ok(self.current().at)
    }

    /// Routing over admitted tokens only. The ordinary scalar compiler owns
    /// every expression; this recognizes the old column-only tail so its
    /// template and bound-plan bytes remain unchanged.
    fn has_expression_order(&self, allow_returned_property: bool) -> bool {
        let token = |at| self.tokens.get(at).copied();
        if !self.current().word("ORDER")
            || !token(self.at + 1).is_some_and(|token| token.word("BY"))
        {
            return false;
        }
        let mut at = self.at + 2;
        loop {
            if !token(at).is_some_and(|token| matches!(token.kind, TextKind::Word(_))) {
                return true;
            }
            at += 1;
            if token(at).is_some_and(|token| token.punct(b'.')) {
                // After a set or parenthesized output boundary, `doc.rank`
                // reads a returned row value, not an arm's graph binding.
                // Let the shared expression compiler resolve MAP/temporal
                // fields. Single reads retain their projected-property alias
                // path; a quoted `doc.rank` is still one plain column token.
                if !allow_returned_property {
                    return true;
                }
                at += 1;
                if !token(at).is_some_and(|token| matches!(token.kind, TextKind::Word(_))) {
                    return true;
                }
                at += 1;
            }
            if token(at).is_some_and(|token| {
                ["ASC", "ASCENDING", "DESC", "DESCENDING"]
                    .iter()
                    .any(|direction| token.word(direction))
            }) {
                at += 1;
            }
            if token(at).is_some_and(|token| token.word("NULLS")) {
                at += 1;
                if !token(at).is_some_and(|token| token.word("FIRST") || token.word("LAST")) {
                    return true;
                }
                at += 1;
            }
            if token(at).is_some_and(|token| token.punct(b',')) {
                at += 1;
                continue;
            }
            return !token(at).is_some_and(|token| {
                matches!(token.kind, TextKind::End)
                    || token.punct(b')')
                    || token.punct(b';')
                    || token.word("SKIP")
                    || token.word("LIMIT")
            });
        }
    }
    fn intersection(&mut self, depth: usize) -> Result<Node, GraphSetTextError> {
        let mut node = self.term(depth)?;
        while self.current().word("INTERSECT") {
            let at = self.current().at;
            self.advance();
            let quantifier = self.quantifier();
            let right = self.term(depth)?;
            let height = 1 + node.depth.max(right.depth);
            node = Node::new(
                NodeKind::Binary {
                    operation: GraphSetOperation::Intersect,
                    quantifier,
                    left: Box::new(node),
                    right: Box::new(right),
                },
                at,
                height,
            )?;
        }
        Ok(node)
    }
    fn quantifier(&mut self) -> GraphSetQuantifier {
        if self.take_word("ALL") {
            GraphSetQuantifier::All
        } else {
            self.take_word("DISTINCT");
            GraphSetQuantifier::Distinct
        }
    }
    fn term(&mut self, depth: usize) -> Result<Node, GraphSetTextError> {
        let at = self.current().at;
        if self.take(b'(') {
            let input = self.expression(depth + 1)?;
            self.require(b')', "closing set-expression parenthesis")?;
            let height = input.depth + 1;
            return Node::new(NodeKind::Scope(Box::new(input)), at, height);
        }
        if !opens_pipeline(&self.tokens, self.at) {
            return Err(expected(
                at,
                "read pipeline or parenthesized set expression",
            ));
        }
        if self.spans.len() == MAX_GRAPH_SET_OPERANDS {
            return Err(fail(
                at,
                GraphSetTextErrorKind::SetBuild(GraphSetBuildError::TooManyOperands {
                    limit: MAX_GRAPH_SET_OPERANDS,
                    observed: self.spans.len() + 1,
                }),
            ));
        }
        let first_token = self.at;
        let mut delimiters = Vec::new();
        let mut returned = false;
        loop {
            let token = self.current();
            if matches!(token.kind, TextKind::End) {
                break;
            }
            if delimiters.is_empty() {
                if token.punct(b')') {
                    break;
                }
                if returned
                    && (token.word("ORDER")
                        || token.word("SKIP")
                        || token.word("LIMIT")
                        || self.set_separator())
                {
                    break;
                }
                if token.word("RETURN") {
                    returned = true;
                }
            }
            match token.kind {
                TextKind::Punct(ch @ (b'(' | b'[' | b'{')) => {
                    if delimiters.len() == MAX_GRAPH_SET_DEPTH {
                        return Err(expected(token.at, "bounded graph-expression nesting"));
                    }
                    delimiters.push(ch);
                }
                TextKind::Punct(ch @ (b')' | b']' | b'}')) => {
                    let opening = match ch {
                        b')' => b'(',
                        b']' => b'[',
                        _ => b'{',
                    };
                    if delimiters.pop() != Some(opening) {
                        return Err(expected(token.at, "matching graph-expression delimiter"));
                    }
                }
                _ => {}
            }
            self.advance();
        }
        if !delimiters.is_empty() {
            return Err(expected(
                self.current().at,
                "closing graph-expression delimiter",
            ));
        }
        let leaf = self.spans.len();
        self.spans.push(Span {
            start: at,
            end: self.current().at,
            first_token,
            last_token: self.at,
        });
        Node::new(NodeKind::Leaf(leaf), at, 1)
    }
    fn set_separator(&self) -> bool {
        let token = self.current();
        if !(token.word("UNION") || token.word("INTERSECT") || token.word("EXCEPT")) {
            return false;
        }
        if self.at > 0
            && (self.tokens[self.at - 1].punct(b'.') || self.tokens[self.at - 1].word("AS"))
        {
            return false;
        }
        let mut next = self.at + 1;
        if self
            .tokens
            .get(next)
            .is_some_and(|token| token.word("ALL") || token.word("DISTINCT"))
        {
            next += 1;
        }
        opens_pipeline(&self.tokens, next)
            || self.tokens.get(next).is_some_and(|token| token.punct(b'('))
    }
    fn tail(&mut self, node: &mut Node) -> Result<(), GraphSetTextError> {
        if self.take_word("ORDER") {
            if !self.take_word("BY") {
                return Err(expected(self.current().at, "BY"));
            }
            loop {
                let at = self.current().at;
                let TextKind::Word(name) = self.current().kind else {
                    return Err(expected(at, "output column name"));
                };
                let mut name = name.to_owned();
                self.advance();
                let mut variable = None;
                if self.take(b'.') {
                    let TextKind::Word(property) = self.current().kind else {
                        return Err(expected(self.current().at, "property name"));
                    };
                    variable = Some(std::mem::replace(&mut name, property.to_owned()));
                    self.advance();
                }
                // openCypher and GQL spell the directions ASC/ASCENDING and
                // DESC/DESCENDING.
                let descending = self.take_word("DESC") || self.take_word("DESCENDING");
                if !descending && !self.take_word("ASC") {
                    self.take_word("ASCENDING");
                }
                let nulls_first = if self.take_word("NULLS") {
                    if self.take_word("FIRST") {
                        true
                    } else if self.take_word("LAST") {
                        false
                    } else {
                        return Err(expected(self.current().at, "FIRST or LAST"));
                    }
                } else {
                    false
                };
                if node.order.len() == crate::algebra::MAX_PATTERN_VERTICES {
                    return Err(fail(
                        at,
                        GraphSetTextErrorKind::OrderBuild(GraphOrderError::TooManyColumns {
                            limit: crate::algebra::MAX_PATTERN_VERTICES,
                            observed: node.order.len() + 1,
                        }),
                    ));
                }
                node.order.push(OrderKey {
                    name,
                    variable,
                    at,
                    order: GraphValueOrder {
                        column: 0,
                        descending,
                        nulls_first,
                    },
                });
                if !self.take(b',') {
                    break;
                }
            }
        }
        if self.take_word("SKIP") {
            node.offset = self.page_number()?;
        }
        if self.take_word("LIMIT") {
            node.count = Some(self.page_number()?);
        }
        Ok(())
    }
    fn page_number(&mut self) -> Result<PageNumber, GraphSetTextError> {
        let token = self.current();
        let result = match token.kind {
            TextKind::Digits(digits) => {
                PageNumber::Literal(crate::graph_text::integer_magnitude(digits).ok_or_else(
                    || pattern_kind(token.at, GraphPatternTextErrorKind::IntegerOutOfRange),
                )?)
            }
            TextKind::Parameter(name) => {
                let name = name.to_owned();
                self.page_parameters.push((
                    token.at,
                    GqlParameterSpec {
                        name: name.clone(),
                        parameter_type: GqlParameterType::UInt64,
                        requires_positive: false,
                        occurrences: 1,
                    },
                ));
                PageNumber::Parameter(name)
            }
            _ => return Err(expected(token.at, "unsigned integer or UInt64 parameter")),
        };
        self.advance();
        Ok(result)
    }
}

/// Each unique (domain, name) resolves once across every arm. The reverse
/// catalog is the caller's, so `labels(n)` and `type(r)` name every id the
/// caller's catalog names, not only the ones the statement text mentions.
struct CachedSymbols<'r, R: ?Sized> {
    resolve: &'r mut R,
    cache: BTreeMap<(GraphSymbolKind, String), GraphSymbol>,
}

impl<R> crate::graph_text::GraphSymbolResolver for CachedSymbols<'_, R>
where
    R: crate::graph_text::GraphSymbolResolver + ?Sized,
{
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        let key = (kind, name.to_owned());
        if let Some(value) = self.cache.get(&key) {
            return Some(*value);
        }
        let value = self.resolve.resolve_symbol(kind, name)?;
        self.cache.insert(key, value);
        Some(value)
    }

    fn time_zones(&self) -> Option<crate::GqlTimeZones> {
        self.resolve.time_zones()
    }

    fn reverse_catalog(&self) -> Option<crate::graph_text::ReverseSymbolCatalog> {
        self.resolve.reverse_catalog()
    }

    fn reverse_label(&self, id: fgdb_delta_types::LabelId) -> Option<String> {
        self.resolve.reverse_label(id)
    }

    fn reverse_relation(&self, id: fgdb_delta_types::RelationId) -> Option<String> {
        self.resolve.reverse_relation(id)
    }
}

/// One immutable compound text definition. MATCH operands retain their own
/// variables and scopes. UNION aligns matching output names to the left side;
/// INTERSECT and EXCEPT retain positional output names from the left side.
/// Binding never reparses, calls the catalog, reads a database or substitutes
/// argument values into text. The resulting PreparedGraphSet uses all existing
/// live/historical/pinned/transaction set-execution entrypoints.
#[derive(Clone)]
pub struct PreparedGraphSetText {
    statement: String,
    root: Node,
    inputs: Vec<(usize, multipart::BoundReadInput)>,
    columns: Vec<String>,
    types: Vec<GraphSetColumnType>,
    parameters: Vec<GqlParameterSpec>,
    parameter_offsets: Vec<usize>,
}
impl core::fmt::Debug for PreparedGraphSetText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphSetText")
            .field("operands", &self.inputs.len())
            .field("columns", &self.columns.len())
            .field("parameters", &self.parameters.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphSetText {
    /// UNION/EXCEPT associate left; INTERSECT binds more tightly. Every set
    /// operator defaults to DISTINCT and may explicitly select ALL/DISTINCT.
    /// UNION operands must return the same column names, in any order, and one
    /// unparenthesized UNION chain must use ALL throughout or DISTINCT throughout.
    /// Parentheses preserve operand-local order/page. An unparenthesized final
    /// ORDER BY/SKIP/LIMIT applies to the complete set, never only its last arm.
    /// Compound ORDER BY accepts expressions over leftmost output columns,
    /// including parameters, ASC/DESC and NULLS FIRST/LAST. Private arm bindings
    /// are out of scope. Keys are evaluated after set arithmetic, before
    /// ranking/windowing, then removed from the ordered public output.
    /// Existing output-column-only pages keep their exact plan encoding.
    /// Without a set operator it also accepts `var.prop` when the RETURN
    /// projected exactly that property, aliased or not, and orders by that
    /// output column.
    /// Use parentheses around an operand with a local order or page.
    ///
    /// Leaf syntax is the shared graph-pattern profile, including WALK,
    /// OPTIONAL and EXISTS. RETURN also accepts scalar literals, parameters and
    /// checked nullable i64 arithmetic (+ - * / %, signs, ABS, NULLIF, COALESCE).
    /// An unaliased RETURN item is named by its source text, as in openCypher
    /// (`n.p`, `count(*)`, `n.p + 1`). Computed values evaluate before DISTINCT,
    /// ordering and pagination; all selected property inputs retain eager source
    /// error behavior. Constants preserve match multiplicity. Use this same
    /// entrypoint for a single MATCH with computed outputs or a compound set.
    ///
    /// WITH adds bounded row stages after MATCH: projection/renaming (including
    /// the same checked arithmetic/CASE), optional DISTINCT, ORDER BY/SKIP/LIMIT,
    /// then optional WHERE over the projected aliases. Repeated WITH stages end
    /// in RETURN. A stage's page precedes its following WHERE and next stage;
    /// only explicitly projected names survive. WHERE supports comparisons,
    /// IS [NOT] NULL and NOT/AND/OR with three-valued semantics. Project computed
    /// predicate operands first. A further MATCH joins against the completed
    /// WITH rows: shared vertex names are identity correlations, while new
    /// bindings expand each input occurrence. Earlier DISTINCT/filter/page
    /// boundaries remain before that join. The next WITH or RETURN may combine
    /// imported columns with the new pattern's properties. Every graph source
    /// executes once under the existing set engine and cumulative budget.
    /// Row-only WITH stages can group keys and native aggregate arguments,
    /// then feed their checked row values to WHERE, UNWIND or later MATCH.
    /// Wide numeric results and nonnull exact averages refuse row conversion.
    /// Row-only stages do not dereference graph values. Writes and imported
    /// bindings used only inside a later scoped clause remain unsupported.
    /// Aggregate RETURN operands remain unsupported. Byte/token admission is
    /// definition-wide; no branch resets those caps.
    pub fn prepare(
        statement: &str,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphSetTextError> {
        Self::prepare_with_parameter_types(statement, &[], resolve)
    }

    /// Declarations are global. A declaration used only in one arm is valid;
    /// unused declarations and conflicting uses across arms or pages refuse
    /// before catalog access. Each unique (domain, name) resolves once across
    /// all arms and their internal MATCH scopes.
    pub fn prepare_with_parameter_types(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        mut resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, GraphSetTextError> {
        Self::prepare_with_parameter_types_and_resolver(statement, declarations, &mut resolve)
    }

    /// As [`Self::prepare_with_parameter_types`], with a resolver whose reverse
    /// catalog names every label and relation id. `labels(n)` and `type(r)`
    /// columns read names from that catalog. A bare closure has none, so its
    /// arms fall back to the names their own text mentions.
    pub fn prepare_with_parameter_types_and_resolver<R>(
        statement: &str,
        declarations: &[(&str, GqlParameterType)],
        resolve: &mut R,
    ) -> Result<Self, GraphSetTextError>
    where
        R: crate::graph_text::GraphSymbolResolver + ?Sized,
    {
        let zones = resolve.time_zones();
        let tokens = PreparedGraphText::composition_tokens(statement, declarations)
            .map_err(|error| pattern_error(0, error))?;
        let mut parser = Composition {
            tokens,
            at: 0,
            spans: Vec::new(),
            page_parameters: Vec::new(),
        };
        let mut root = parser.expression(0)?;
        if !matches!(parser.current().kind, TextKind::End) {
            return Err(expected(parser.current().at, "end of compound statement"));
        }
        let page_parameters = std::mem::take(&mut parser.page_parameters);
        let tokens = &parser.tokens;
        let compile = |spans: &[Span], mut uses: Vec<(usize, GqlParameterSpec)>| {
            let mut pending = Vec::new();
            let mut schemas = Vec::new();
            let mut operands = 0;
            for span in spans {
                let names: BTreeSet<_> = tokens[span.first_token..span.last_token]
                    .iter()
                    .filter_map(|token| match token.kind {
                        TextKind::Parameter(name) => Some(name),
                        _ => None,
                    })
                    .collect();
                let local: Vec<_> = declarations
                    .iter()
                    .copied()
                    .filter(|(name, _)| names.contains(name))
                    .collect();
                let input = PreparedGraphText::unresolved_read_input(
                    &statement[span.start..span.end],
                    &local,
                    &tokens[span.first_token..span.last_token],
                    zones.as_ref(),
                )
                .map_err(|error| rebase_error(span.start, error))?;
                operands += input.operand_count();
                if operands > MAX_GRAPH_SET_OPERANDS {
                    return Err(fail(
                        span.start,
                        GraphSetTextErrorKind::SetBuild(GraphSetBuildError::TooManyOperands {
                            limit: MAX_GRAPH_SET_OPERANDS,
                            observed: operands,
                        }),
                    ));
                }
                let (columns, types) = input.column_schema();
                let returned =
                    returned_properties(&tokens[span.first_token..span.last_token], &columns);
                schemas.push(Schema {
                    columns,
                    types,
                    depth: input.depth(),
                    returned,
                });
                for (spec, offset) in input
                    .parameter_schema()
                    .iter()
                    .zip(input.parameter_offsets())
                {
                    uses.push((span.start + *offset, spec.clone()));
                }
                pending.push(input);
            }
            Ok::<_, GraphSetTextError>((pending, schemas, uses))
        };
        let (mut pending, mut schemas, mut uses) = compile(&parser.spans, page_parameters)?;
        // A single read's ORDER BY that names no output column ranks by the
        // read's own scope, as `RETURN n ORDER BY k` after `WITH ..., x AS k`
        // or `UNWIND ... AS k` (fgdb-ujuga). Only a compound set's tail is
        // limited to output columns, so recompile that read with the complete
        // tail it owns. A tail naming only outputs keeps its column template.
        if let NodeKind::Leaf(leaf) = root.kind
            && !root.order_resolves(&schemas[leaf])
        {
            let (end, last_token) = (parser.current().at, parser.at);
            parser.spans[leaf].end = end;
            parser.spans[leaf].last_token = last_token;
            root.order.clear();
            root.offset = PageNumber::Literal(0);
            root.count = None;
            // Only this tail recorded page parameters: the read now owns them.
            (pending, schemas, uses) = compile(&parser.spans, Vec::new())?;
        }
        let schema = root.validate(&schemas, statement, declarations, &mut uses)?;
        uses.sort_by_key(|(offset, _)| *offset);
        let mut parameters: Vec<GqlParameterSpec> = Vec::new();
        let mut parameter_offsets = Vec::new();
        let mut parameter_index = BTreeMap::new();
        for (offset, spec) in uses {
            if let Some(&at) = parameter_index.get(&spec.name) {
                let previous: &mut GqlParameterSpec = &mut parameters[at];
                if previous.parameter_type != spec.parameter_type {
                    return Err(pattern_kind(
                        offset,
                        GraphPatternTextErrorKind::ConflictingParameterTypes,
                    ));
                }
                previous.occurrences += spec.occurrences;
                previous.requires_positive |= spec.requires_positive;
            } else {
                parameter_index.insert(spec.name.clone(), parameters.len());
                parameter_offsets.push(offset);
                parameters.push(spec);
            }
        }
        for &(name, kind) in declarations {
            let Some(&at) = parameter_index.get(name) else {
                return Err(pattern_kind(
                    statement.len(),
                    GraphPatternTextErrorKind::UnusedParameterDeclaration,
                ));
            };
            if parameters[at].parameter_type != kind {
                return Err(pattern_kind(
                    parameter_offsets[at],
                    GraphPatternTextErrorKind::ConflictingParameterTypes,
                ));
            }
        }
        // No catalog calls occurred above. Keep one domain-aware cache around
        // the existing per-pattern resolver; errors retain original offsets.
        let mut symbols = CachedSymbols {
            resolve,
            cache: BTreeMap::new(),
        };
        let mut inputs = Vec::new();
        for (input, span) in pending.into_iter().zip(&parser.spans) {
            let input = input
                .resolve(&mut symbols)
                .map_err(|error| pattern_error(span.start, error))?;
            inputs.push((span.start, input));
        }
        Ok(Self {
            statement: statement.to_owned(),
            root,
            inputs,
            columns: schema.columns,
            types: schema.types,
            parameters,
            parameter_offsets,
        })
    }
    #[must_use]
    pub fn statement(&self) -> &str {
        &self.statement
    }
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
    #[must_use]
    pub fn column_types(&self) -> &[GraphSetColumnType] {
        &self.types
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        &self.parameters
    }
    /// Versioned, value-independent template transcript: the set-operator
    /// tree, per-leaf nested input templates, ordering and paging shape, and
    /// the declaration-wide parameter schema. Statement text, byte offsets and
    /// parameter values never enter.
    #[must_use]
    pub fn canonical_template_bytes(&self) -> Vec<u8> {
        fn ordinal(bytes: &mut Vec<u8>, value: usize) {
            bytes.extend_from_slice(&(value as u64).to_be_bytes());
        }
        fn name(bytes: &mut Vec<u8>, value: &str) {
            ordinal(bytes, value.len());
            bytes.extend_from_slice(value.as_bytes());
        }
        fn page(bytes: &mut Vec<u8>, number: &PageNumber) {
            match number {
                PageNumber::Literal(value) => {
                    bytes.push(0);
                    bytes.extend_from_slice(&value.to_be_bytes());
                }
                PageNumber::Parameter(parameter) => {
                    bytes.push(1);
                    name(bytes, parameter);
                }
            }
        }
        fn node(bytes: &mut Vec<u8>, tree: &Node) {
            // Tag 3 is an expression-page wrapper around the existing node
            // encoding. Unaffected trees retain their exact v1 transcript.
            if tree.expression_order.is_some() {
                bytes.push(3);
            }
            match &tree.kind {
                NodeKind::Leaf(input) => {
                    bytes.push(0);
                    ordinal(bytes, *input);
                }
                NodeKind::Scope(inner) => {
                    bytes.push(1);
                    node(bytes, inner);
                }
                NodeKind::Binary {
                    operation,
                    quantifier,
                    left,
                    right,
                } => {
                    bytes.push(2);
                    bytes.push(match operation {
                        GraphSetOperation::Union => 0,
                        GraphSetOperation::Intersect => 1,
                        GraphSetOperation::Except => 2,
                    });
                    bytes.push(match quantifier {
                        GraphSetQuantifier::All => 0,
                        GraphSetQuantifier::Distinct => 1,
                    });
                    node(bytes, left);
                    node(bytes, right);
                }
            }
            ordinal(bytes, tree.order.len());
            for key in &tree.order {
                name(bytes, &key.name);
                bytes.push(u8::from(key.order.descending));
                ordinal(bytes, key.order.column);
                bytes.push(u8::from(key.order.nulls_first));
            }
            page(bytes, &tree.offset);
            match &tree.count {
                None => bytes.push(0),
                Some(count) => {
                    bytes.push(1);
                    page(bytes, count);
                }
            }
            if let Some(order) = &tree.expression_order {
                let template = order.prepared();
                template.stage.append_template_transcript(bytes);
                // Expressions use page-local parameter ordinals. Retaining
                // their names prevents different global argument bindings
                // from aliasing the same resolved-unbound plan identity.
                ordinal(bytes, template.parameters.len());
                for spec in &template.parameters {
                    name(bytes, &spec.name);
                    bytes.push(parameter_type_tag(spec.parameter_type));
                }
            }
        }
        let mut bytes = b"fgdb:gql:set-text-template:v1\0".to_vec();
        node(&mut bytes, &self.root);
        bytes.extend_from_slice(&(self.inputs.len() as u64).to_be_bytes());
        for (_, input) in &self.inputs {
            input.append_template_transcript(&mut bytes);
        }
        ordinal(&mut bytes, self.columns.len());
        for column in &self.columns {
            name(&mut bytes, column);
        }
        ordinal(&mut bytes, self.types.len());
        for kind in &self.types {
            bytes.push(set_column_type_tag(*kind));
        }
        ordinal(&mut bytes, self.parameters.len());
        for spec in &self.parameters {
            name(&mut bytes, &spec.name);
            bytes.push(parameter_type_tag(spec.parameter_type));
        }
        bytes
    }

    /// Logical template operators of the compound statement in evaluation
    /// order, derived from the resolved set-operator tree: leaf scan, scope
    /// nesting, binary set operators, per-node ordering/pagination.
    #[must_use]
    pub fn template_operators(&self) -> Vec<&'static str> {
        fn node(operators: &mut Vec<&'static str>, tree: &Node) {
            match &tree.kind {
                NodeKind::Leaf(_) => operators.push("ScanLeaf"),
                NodeKind::Scope(inner) => {
                    operators.push("ScopeBegin");
                    node(operators, inner);
                    operators.push("ScopeEnd");
                }
                NodeKind::Binary {
                    operation,
                    left,
                    right,
                    ..
                } => {
                    node(operators, left);
                    node(operators, right);
                    operators.push(match operation {
                        GraphSetOperation::Union => "Union",
                        GraphSetOperation::Intersect => "Intersect",
                        GraphSetOperation::Except => "Except",
                    });
                }
            }
            if !tree.order.is_empty() {
                operators.push("OrderByValues");
            }
            if !matches!(tree.offset, PageNumber::Literal(0)) || tree.count.is_some() {
                operators.push("Limit");
            }
            if let Some(expression) = &tree.expression_order {
                let ReadStageTemplate::Page {
                    order,
                    offset,
                    count,
                    computed,
                    ..
                } = &expression.prepared().stage
                else {
                    unreachable!("compound ordering compiles only a native page");
                };
                if computed.is_some() {
                    operators.push("ProjectOrderKeys");
                }
                if !order.is_empty() {
                    operators.push("OrderByValues");
                }
                if !matches!(offset, ReadPageNumber::Literal(0)) || count.is_some() {
                    operators.push("Limit");
                }
                if computed.is_some() {
                    operators.push("ProjectOrderedOutput");
                }
            }
        }
        let mut operators = Vec::new();
        node(&mut operators, &self.root);
        operators
    }

    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphSet, GraphSetTextError> {
        // Signed client integers for SKIP/LIMIT parameters (fgdb-3dnpu).
        let admitted = arguments.admit_unsigned(&self.parameters);
        let arguments: &GqlParameters = &admitted;
        // Reject the complete argument map before constructing any leaf plan.
        for (spec, &offset) in self.parameters.iter().zip(&self.parameter_offsets) {
            let value = arguments
                .get(&spec.name)
                .ok_or_else(|| pattern_kind(offset, GraphPatternTextErrorKind::MissingParameter))?;
            if !spec.parameter_type.accepts(value.parameter_type()) {
                return Err(pattern_kind(
                    offset,
                    GraphPatternTextErrorKind::ParameterTypeMismatch {
                        expected: spec.parameter_type,
                        found: value.parameter_type(),
                    },
                ));
            }
        }
        if arguments.len() != self.parameters.len() {
            return Err(pattern_kind(
                self.statement.len(),
                GraphPatternTextErrorKind::UnexpectedArguments,
            ));
        }
        let mut inputs = Vec::new();
        for (offset, input) in &self.inputs {
            let mut local = GqlParameters::new();
            for spec in input.parameter_schema() {
                local
                    .insert(
                        spec.name.clone(),
                        arguments.get(&spec.name).expect("validated argument"),
                    )
                    .expect("prepared local names are valid and unique");
            }
            let bound = input
                .bind_parameters(&local)
                .map_err(|error| rebase_error(*offset, error))?;
            inputs.push(Some(bound));
        }
        self.root.bind(&mut inputs, arguments)
    }
}

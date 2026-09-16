//! Native, reusable write scripts lower to the existing atomic write program.
//!
//! The script is preparation metadata only. Binding returns the same typed
//! program as explicit statement composition; no source text enters execution.

use crate::{
    GqlParameterSpec, GqlParameters, GraphMutationProgramBuildError,
    GraphMutationProgramTemplateError, GraphPatternTextError, GraphPatternTextErrorKind,
    GraphWriteProgramTemplateError, GraphWriteTemplateStatement, PreparedGraphWriteProgram,
    PreparedGraphWriteProgramTemplate,
};
use core::ops::Range;

/// Whole-script admission. Individual statements retain their native byte and
/// token limits, and a script retains the existing 64-statement program bound.
pub const MAX_GRAPH_WRITE_SCRIPT_BYTES: usize =
    crate::MAX_GRAPH_TEXT_BYTES * crate::MAX_GRAPH_MUTATION_STATEMENTS;

#[derive(Debug)]
pub enum GraphWriteScriptErrorKind {
    DefinitionTooLarge { limit: usize, observed: usize },
    EmptyStatement,
    TooManyStatements { limit: usize, observed: usize },
    Syntax(GraphPatternTextErrorKind),
    /// Native statement preparation, parameter binding or program-schema error.
    /// Nested statement offsets remain local; the enclosing offset is global.
    Program(GraphWriteProgramTemplateError),
}

/// A zero-based statement index and UTF-8 byte offset into the ORIGINAL script.
/// Script-wide declaration/schema errors without a statement use None and zero.
/// Names, source bytes and supplied parameter values are not included.
#[derive(Debug)]
pub struct GraphWriteScriptError {
    pub statement: Option<usize>,
    pub offset: usize,
    pub kind: GraphWriteScriptErrorKind,
}
impl core::fmt::Display for GraphWriteScriptError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "graph write script")?;
        if let Some(statement) = self.statement {
            write!(f, " statement {statement}")?;
        }
        write!(f, " at byte {}: {:?}", self.offset, self.kind)
    }
}
impl core::error::Error for GraphWriteScriptError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match &self.kind {
            GraphWriteScriptErrorKind::Program(source) => Some(source),
            _ => None,
        }
    }
}
impl GraphWriteScriptError {
    pub(crate) fn syntax(statement: Option<usize>, base: usize, source: GraphPatternTextError) -> Self {
        Self { statement, offset: base + source.offset, kind: GraphWriteScriptErrorKind::Syntax(source.kind) }
    }

    pub(crate) fn program(spans: &[Range<usize>], source: GraphWriteProgramTemplateError) -> Self {
        use GraphMutationProgramTemplateError as M;
        use GraphWriteProgramTemplateError as W;
        let location = match &source {
            W::InsertBind { statement, source } => Some((*statement, source.offset)),
            W::VertexMergeBind { statement, source } => Some((*statement, source.offset)),
            W::VertexUpsertBind { statement, source } => Some((*statement, source.offset)),
            W::EdgeMergeBind { statement, source } => Some((*statement, source.offset)),
            W::EdgeUpsertBind { statement, source } => Some((*statement, source.offset)),
            W::DeleteBind { statement, source } => Some((*statement, source.offset)),
            W::Program(M::Bind { statement, source }) => Some((*statement, source.offset)),
            W::Program(M::ConflictingParameterTypes { statement, .. })
            | W::Program(M::Definition(GraphMutationProgramBuildError::MixedRelation { statement })) => {
                Some((*statement, 0))
            }
            W::Program(M::Definition(_) | M::UnexpectedArguments) => None,
        };
        let (statement, offset) = match location {
            Some((statement, offset)) => (Some(statement), spans[statement].start + offset),
            None => (None, 0),
        };
        Self { statement, offset, kind: GraphWriteScriptErrorKind::Program(source) }
    }
}

/// Binding is completed before entering the database's program executor.
/// Program errors retain the ordinary staging/commit outcome vocabulary: an
/// unknown or committed-needs-recovery outcome is never relabeled a bind failure
/// or a successful rollback. No execution receipt accompanies either error arm.
#[derive(Debug)]
pub enum GraphWriteScriptExecutionError<E, A, C> {
    Binding(GraphWriteScriptError),
    Program(crate::GraphWriteProgramError<E, A, C>),
}

impl<E: core::fmt::Display, A: core::fmt::Display, C: core::fmt::Display>
    core::fmt::Display for GraphWriteScriptExecutionError<E, A, C>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Binding(source) => source.fmt(f),
            Self::Program(source) => source.fmt(f),
        }
    }
}
impl<E: core::error::Error + 'static, A: core::error::Error + 'static,
    C: core::error::Error + 'static> core::error::Error for GraphWriteScriptExecutionError<E, A, C>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Binding(source) => Some(source),
            Self::Program(source) => Some(source),
        }
    }
}

/// Semicolon-separated native CREATE, MATCH mutation, plain DELETE and
/// vertex/relationship MERGE, including ON MATCH/ON CREATE. DELETE remains
/// non-detaching; only explicit DETACH DELETE permits cascades. One final
/// semicolon is legal; empty statements, reads, transaction-control commands
/// and unsupported syntax refuse. Variables are statement-local; later MATCH
/// reads earlier staged work.
///
/// Every statement shares one relation coordinate and one frozen name-to-symbol
/// resolution per (kind, name). This does not negotiate catalog epochs or grant
/// authorization. Bind the COMPLETE script before giving its typed program to
/// WriteTxn or Database's ordinary program-autocommit API. The existing program
/// engine owns cumulative execution quotas, rollback, identities and durability.
#[derive(Clone)]
pub struct PreparedGraphWriteScript {
    pub(crate) script: String,
    pub(crate) program: PreparedGraphWriteProgramTemplate,
    pub(crate) spans: Box<[Range<usize>]>,
}
impl core::fmt::Debug for PreparedGraphWriteScript {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphWriteScript")
            .field("statements", &self.spans.len())
            .field("parameters", &self.program.parameter_schema().len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphWriteScript {
    #[must_use]
    pub fn script(&self) -> &str { &self.script }
    #[must_use]
    pub fn statements(&self) -> &[GraphWriteTemplateStatement] { self.program.statements() }
    #[must_use]
    pub fn statement_span(&self, statement: usize) -> Option<Range<usize>> {
        self.spans.get(statement).cloned()
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] { self.program.parameter_schema() }

    /// No reparsing, catalog access, identity allocation, database observation
    /// or staging. A failure discards every already-bound private statement.
    pub fn bind_parameters(&self, arguments: &GqlParameters)
        -> Result<PreparedGraphWriteProgram, GraphWriteScriptError>
    {
        self.program.bind_parameters(arguments)
            .map_err(|source| GraphWriteScriptError::program(&self.spans, source))
    }
}

/// A batch is still ONE ordinary atomic program. Its limit counts the expanded
/// statements across all parameter sets; batching never multiplies work quotas.
#[derive(Debug)]
pub enum GraphWriteScriptBatchError {
    Empty,
    TooManyStatements { limit: usize, observed: u128 },
    Arguments { argument_set: usize, source: GraphWriteScriptError },
    Definition(GraphMutationProgramBuildError),
}
impl core::fmt::Display for GraphWriteScriptBatchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Empty => f.write_str("graph write script batch requires at least one argument set"),
            Self::TooManyStatements { limit, observed } =>
                write!(f, "graph write script batch expands to {observed} statements; limit {limit}"),
            Self::Arguments { argument_set, source } =>
                write!(f, "graph write script batch argument set {argument_set}: {source}"),
            Self::Definition(source) => source.fmt(f),
        }
    }
}
impl core::error::Error for GraphWriteScriptBatchError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Arguments { source, .. } => Some(source),
            Self::Definition(source) => Some(source),
            Self::Empty | Self::TooManyStatements { .. } => None,
        }
    }
}

/// Translate a flat program statement/identity request/receipt index back to
/// its zero-based input-record index and original script statement and bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphWriteScriptBatchLocation {
    pub argument_set: usize,
    pub statement: usize,
    pub span: Range<usize>,
}

/// All parameter sets have been checked and lowered before this value exists.
/// Later sets execute after earlier sets in the canonical transaction overlay.
/// Give program() to the ordinary WriteTxn or autocommit program API: one shared
/// allowance, one rollback boundary, one completion and no partial receipts.
///
/// This is bounded ingestion, not an unbounded bulk loader or a sequence of
/// independently committed records. The expanded batch retains the ordinary
/// 64-statement maximum. Identity allocation remains external and is never
/// rewound by rollback. Its flat request indices can be translated by location().
#[derive(Clone)]
pub struct BoundGraphWriteScriptBatch {
    program: PreparedGraphWriteProgram,
    argument_sets: usize,
    spans: Box<[Range<usize>]>,
}
impl core::fmt::Debug for BoundGraphWriteScriptBatch {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BoundGraphWriteScriptBatch")
            .field("argument_sets", &self.argument_sets)
            .field("statements", &self.program.statements().len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl BoundGraphWriteScriptBatch {
    #[must_use]
    pub fn program(&self) -> &PreparedGraphWriteProgram { &self.program }
    #[must_use]
    pub fn into_program(self) -> PreparedGraphWriteProgram { self.program }
    #[must_use]
    pub const fn argument_sets(&self) -> usize { self.argument_sets }

    /// The corresponding contiguous slice of a successful program receipt.
    #[must_use]
    pub fn statement_range(&self, argument_set: usize) -> Option<Range<usize>> {
        if argument_set >= self.argument_sets { return None; }
        let start = argument_set * self.spans.len();
        Some(start..start + self.spans.len())
    }

    #[must_use]
    pub fn location(&self, flat_statement: usize) -> Option<GraphWriteScriptBatchLocation> {
        if flat_statement >= self.program.statements().len() { return None; }
        let statement = flat_statement % self.spans.len();
        Some(GraphWriteScriptBatchLocation {
            argument_set: flat_statement / self.spans.len(),
            statement,
            span: self.spans[statement].clone(),
        })
    }
}

impl PreparedGraphWriteScript {
    /// Bind a finite list of input records into ONE atomic write program.
    /// Admission of the expanded statement count precedes all value binding.
    /// Any invalid record discards the entire private bound prefix. No parsing,
    /// catalog access, identity allocation, database read or mutation occurs.
    pub fn bind_parameter_sets(&self, arguments: &[GqlParameters])
        -> Result<BoundGraphWriteScriptBatch, GraphWriteScriptBatchError>
    {
        if arguments.is_empty() { return Err(GraphWriteScriptBatchError::Empty); }
        let observed = arguments.len() as u128 * self.spans.len() as u128;
        if observed > crate::MAX_GRAPH_MUTATION_STATEMENTS as u128 {
            return Err(GraphWriteScriptBatchError::TooManyStatements {
                limit: crate::MAX_GRAPH_MUTATION_STATEMENTS, observed,
            });
        }
        let mut statements = Vec::with_capacity(observed as usize);
        for (argument_set, values) in arguments.iter().enumerate() {
            let program = self.bind_parameters(values)
                .map_err(|source| GraphWriteScriptBatchError::Arguments { argument_set, source })?;
            statements.extend(program.statements().iter().cloned());
        }
        let program = PreparedGraphWriteProgram::prepare(statements)
            .map_err(GraphWriteScriptBatchError::Definition)?;
        Ok(BoundGraphWriteScriptBatch {
            program, argument_sets: arguments.len(), spans: self.spans.clone(),
        })
    }
}

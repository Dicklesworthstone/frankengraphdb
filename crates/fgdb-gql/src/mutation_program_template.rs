//! Rebind a complete ordered mutation program from one exact argument map.
//!
//! Statement preparation remains owned by PreparedGraphMutationText. This type
//! composes those already-checked definitions; it does not parse a second script
//! language, choose an ambient catalog or grant storage authority.

pub(crate) mod mixed;

use crate::{
    GqlParameterSpec, GqlParameters, GraphMutationProgramBuildError, GraphMutationTextError,
    MAX_GRAPH_MUTATION_STATEMENTS, PreparedGraphMutationProgram, PreparedGraphMutationText,
};
use fgdb_delta_types::RelationId;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphMutationProgramTemplateError {
    Definition(GraphMutationProgramBuildError),
    ConflictingParameterTypes {
        parameter: usize,
        first_statement: usize,
        statement: usize,
    },
    UnexpectedArguments,
    Bind {
        statement: usize,
        source: GraphMutationTextError,
    },
}
impl core::fmt::Display for GraphMutationProgramTemplateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Definition(source) => source.fmt(f),
            Self::ConflictingParameterTypes {
                parameter,
                first_statement,
                statement,
            } => {
                write!(
                    f,
                    "program parameter {parameter} has incompatible types in statements {first_statement} and {statement}"
                )
            }
            Self::UnexpectedArguments => {
                f.write_str("mutation program received undeclared arguments")
            }
            Self::Bind { statement, source } => {
                write!(f, "mutation program statement {statement}: {source}")
            }
        }
    }
}
impl core::error::Error for GraphMutationProgramTemplateError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Definition(source) => Some(source),
            Self::Bind { source, .. } => Some(source),
            Self::ConflictingParameterTypes { .. } | Self::UnexpectedArguments => None,
        }
    }
}

/// Shared schema law for mutation-only and mixed write programs. The iterator
/// borrows already-prepared schemas and never copies/reparses source text.
fn program_parameters<'a>(
    inputs: impl ExactSizeIterator<Item = (RelationId, &'a [GqlParameterSpec])>,
    require_same_relation: bool,
) -> Result<Vec<GqlParameterSpec>, GraphMutationProgramTemplateError> {
    use GraphMutationProgramTemplateError as Error;
    let count = inputs.len();
    if count == 0 {
        return Err(Error::Definition(GraphMutationProgramBuildError::Empty));
    }
    if count > MAX_GRAPH_MUTATION_STATEMENTS {
        return Err(Error::Definition(
            GraphMutationProgramBuildError::TooManyStatements {
                limit: MAX_GRAPH_MUTATION_STATEMENTS,
                observed: count,
            },
        ));
    }
    let mut relation = None;
    let mut parameters: Vec<GqlParameterSpec> = Vec::new();
    let mut origins = Vec::new();
    let mut index = BTreeMap::<String, usize>::new();
    for (statement, (coordinate, schema)) in inputs.enumerate() {
        if let Some(expected) = relation {
            if require_same_relation && coordinate != expected {
                return Err(Error::Definition(
                    GraphMutationProgramBuildError::MixedRelation { statement },
                ));
            }
        } else {
            relation = Some(coordinate);
        }
        for spec in schema {
            if let Some(&at) = index.get(&spec.name) {
                let previous = &mut parameters[at];
                if previous.parameter_type != spec.parameter_type {
                    return Err(Error::ConflictingParameterTypes {
                        parameter: at,
                        first_statement: origins[at],
                        statement,
                    });
                }
                // Each input's shared lexer caps occurrences; 64 admitted
                // statements cannot overflow usize even on wasm32.
                previous.occurrences += spec.occurrences;
                previous.requires_positive |= spec.requires_positive;
            } else {
                index.insert(spec.name.clone(), parameters.len());
                origins.push(statement);
                parameters.push(spec.clone());
            }
        }
    }
    Ok(parameters)
}

fn check_program_arguments(
    parameters: &[GqlParameterSpec],
    arguments: &GqlParameters,
) -> Result<(), GraphMutationProgramTemplateError> {
    let recognized = parameters
        .iter()
        .filter(|spec| arguments.get(&spec.name).is_some())
        .count();
    if recognized != arguments.len() {
        return Err(GraphMutationProgramTemplateError::UnexpectedArguments);
    }
    Ok(())
}
fn local_arguments(schema: &[GqlParameterSpec], arguments: &GqlParameters) -> GqlParameters {
    let mut local = GqlParameters::new();
    for spec in schema {
        if let Some(value) = arguments.get(&spec.name) {
            local
                .insert(spec.name.clone(), value)
                .expect("prepared argument names are valid and locally unique");
        }
    }
    local
}

/// One reusable program schema across separately prepared statements. The host
/// must prepare them under the same catalog/authority contract, just as it must
/// for a typed program. All statements are bound successfully BEFORE the result
/// can reach WriteTxn. Invalid arguments cannot execute an earlier valid step.
/// Parameter names are case-sensitive; duplicates share one exact type, with
/// occurrences summed and scalar payloads shared through GqlParameters.
#[derive(Clone)]
pub struct PreparedGraphMutationProgramTemplate {
    statements: Box<[PreparedGraphMutationText]>,
    parameters: Vec<GqlParameterSpec>,
}
impl core::fmt::Debug for PreparedGraphMutationProgramTemplate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphMutationProgramTemplate")
            .field("statements", &self.statements.len())
            .field("parameters", &self.parameters.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphMutationProgramTemplate {
    pub fn prepare(
        statements: Vec<PreparedGraphMutationText>,
    ) -> Result<Self, GraphMutationProgramTemplateError> {
        let parameters = program_parameters(
            statements
                .iter()
                .map(|input| (input.relation, input.parameter_schema())),
            true,
        )?;
        Ok(Self {
            statements: statements.into_boxed_slice(),
            parameters,
        })
    }

    /// Explicit metadata access; Debug never exports statements or names.
    #[must_use]
    pub fn statements(&self) -> &[PreparedGraphMutationText] {
        &self.statements
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        &self.parameters
    }

    /// Build one complete immutable program, with no parsing, catalog callbacks,
    /// storage reads or staging. An argument used in just one statement is valid;
    /// an argument used nowhere refuses. Each statement receives its exact local
    /// map and its existing binder owns type/null validation and original UTF-8
    /// error offsets. A failure discards all already-bound private plans.
    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphMutationProgram, GraphMutationProgramTemplateError> {
        check_program_arguments(&self.parameters, arguments)?;
        let mut statements = Vec::with_capacity(self.statements.len());
        for (statement, input) in self.statements.iter().enumerate() {
            let local = local_arguments(input.parameter_schema(), arguments);
            let bound = input
                .bind_parameters(&local)
                .map_err(|source| GraphMutationProgramTemplateError::Bind { statement, source })?;
            statements.push(bound);
        }
        PreparedGraphMutationProgram::prepare(statements)
            .map_err(GraphMutationProgramTemplateError::Definition)
    }
}

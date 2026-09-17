//! Reusable mixed programs share the mutation template's parameter/schema laws.
//! Statements remain separately prepared under one host catalog/authority
//! contract. This module neither parses scripts nor executes partially bound work.

use super::*;
use crate::{
    GraphDeleteTextError, GraphEdgeMergeTextError, GraphEdgeUpsertTextError, GraphInsertTextError,
    GraphVertexMergeTextError, GraphVertexUpsertTextError, GraphWriteStatement,
    PreparedGraphDeleteText, PreparedGraphEdgeMergeText, PreparedGraphEdgeUpsertText,
    PreparedGraphInsertText, PreparedGraphVertexMergeText, PreparedGraphVertexUpsertText,
    PreparedGraphWriteProgram,
};

#[derive(Clone, Debug)]
pub enum GraphWriteTemplateStatement {
    Mutation(PreparedGraphMutationText),
    Insert(PreparedGraphInsertText),
    VertexMerge(PreparedGraphVertexMergeText),
    VertexUpsert(PreparedGraphVertexUpsertText),
    EdgeMerge(PreparedGraphEdgeMergeText),
    EdgeUpsert(PreparedGraphEdgeUpsertText),
    Delete(PreparedGraphDeleteText),
}
impl From<PreparedGraphMutationText> for GraphWriteTemplateStatement {
    fn from(value: PreparedGraphMutationText) -> Self {
        Self::Mutation(value)
    }
}
impl From<PreparedGraphInsertText> for GraphWriteTemplateStatement {
    fn from(value: PreparedGraphInsertText) -> Self {
        Self::Insert(value)
    }
}
impl From<PreparedGraphVertexMergeText> for GraphWriteTemplateStatement {
    fn from(value: PreparedGraphVertexMergeText) -> Self {
        Self::VertexMerge(value)
    }
}
impl From<PreparedGraphVertexUpsertText> for GraphWriteTemplateStatement {
    fn from(value: PreparedGraphVertexUpsertText) -> Self {
        Self::VertexUpsert(value)
    }
}
impl From<PreparedGraphEdgeMergeText> for GraphWriteTemplateStatement {
    fn from(value: PreparedGraphEdgeMergeText) -> Self {
        Self::EdgeMerge(value)
    }
}
impl From<PreparedGraphEdgeUpsertText> for GraphWriteTemplateStatement {
    fn from(value: PreparedGraphEdgeUpsertText) -> Self {
        Self::EdgeUpsert(value)
    }
}
impl From<PreparedGraphDeleteText> for GraphWriteTemplateStatement {
    fn from(value: PreparedGraphDeleteText) -> Self {
        Self::Delete(value)
    }
}
impl GraphWriteTemplateStatement {
    #[must_use]
    pub fn relation(&self) -> RelationId {
        match self {
            Self::Mutation(input) => input.relation,
            Self::Insert(input) => input.relation,
            Self::VertexMerge(input) => input.relation,
            Self::VertexUpsert(input) => input.relation(),
            Self::EdgeMerge(input) => input.relation,
            Self::EdgeUpsert(input) => input.merge.relation,
            Self::Delete(input) => input.relation,
        }
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        match self {
            Self::Mutation(input) => input.parameter_schema(),
            Self::Insert(input) => input.parameter_schema(),
            Self::VertexMerge(input) => input.parameter_schema(),
            Self::VertexUpsert(input) => input.parameter_schema(),
            Self::EdgeMerge(input) => input.parameter_schema(),
            Self::EdgeUpsert(input) => input.parameter_schema(),
            Self::Delete(input) => input.parameter_schema(),
        }
    }
    /// Explicit source access. Debug remains redacted for every variant.
    #[must_use]
    pub fn statement(&self) -> &str {
        match self {
            Self::Mutation(input) => input.statement(),
            Self::Insert(input) => input.statement(),
            Self::VertexMerge(input) => input.statement(),
            Self::VertexUpsert(input) => input.statement(),
            Self::EdgeMerge(input) => input.statement(),
            Self::EdgeUpsert(input) => input.statement(),
            Self::Delete(input) => input.statement(),
        }
    }
}

#[derive(Debug)]
pub enum GraphWriteProgramTemplateError {
    Program(GraphMutationProgramTemplateError),
    InsertBind {
        statement: usize,
        source: GraphInsertTextError,
    },
    VertexMergeBind {
        statement: usize,
        source: GraphVertexMergeTextError,
    },
    VertexUpsertBind {
        statement: usize,
        source: GraphVertexUpsertTextError,
    },
    EdgeMergeBind {
        statement: usize,
        source: GraphEdgeMergeTextError,
    },
    EdgeUpsertBind {
        statement: usize,
        source: GraphEdgeUpsertTextError,
    },
    DeleteBind {
        statement: usize,
        source: GraphDeleteTextError,
    },
}
impl From<GraphMutationProgramTemplateError> for GraphWriteProgramTemplateError {
    fn from(error: GraphMutationProgramTemplateError) -> Self {
        Self::Program(error)
    }
}
impl core::fmt::Display for GraphWriteProgramTemplateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Program(error) => error.fmt(f),
            Self::InsertBind { statement, source } => {
                write!(f, "write program creation statement {statement}: {source}")
            }
            Self::VertexMergeBind { statement, source } => write!(
                f,
                "write program vertex MERGE statement {statement}: {source}"
            ),
            Self::VertexUpsertBind { statement, source } => write!(
                f,
                "write program vertex upsert statement {statement}: {source}"
            ),
            Self::EdgeMergeBind { statement, source } => write!(
                f,
                "write program relationship MERGE statement {statement}: {source}"
            ),
            Self::EdgeUpsertBind { statement, source } => write!(
                f,
                "write program relationship upsert statement {statement}: {source}"
            ),
            Self::DeleteBind { statement, source } => write!(
                f,
                "write program plain DELETE statement {statement}: {source}"
            ),
        }
    }
}
impl core::error::Error for GraphWriteProgramTemplateError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Program(error) => Some(error),
            Self::InsertBind { source, .. } => Some(source),
            Self::VertexMergeBind { source, .. } => Some(source),
            Self::VertexUpsertBind { source, .. } => Some(source),
            Self::EdgeMergeBind { source, .. } => Some(source),
            Self::EdgeUpsertBind { source, .. } => Some(source),
            Self::DeleteBind { source, .. } => Some(source),
        }
    }
}

/// One exact parameter contract for precompiled creation, mutation and MERGE
/// steps. All bindings complete before an executable program is returned.
/// Rebinding performs no catalog access, source parsing, graph-ID allocation,
/// database observation or staging. Scalar payloads retain shared storage.
#[derive(Clone)]
pub struct PreparedGraphWriteProgramTemplate {
    statements: Box<[GraphWriteTemplateStatement]>,
    parameters: Vec<GqlParameterSpec>,
}
impl core::fmt::Debug for PreparedGraphWriteProgramTemplate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PreparedGraphWriteProgramTemplate")
            .field("statements", &self.statements.len())
            .field("parameters", &self.parameters.len())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}
impl PreparedGraphWriteProgramTemplate {
    pub fn prepare(
        statements: Vec<GraphWriteTemplateStatement>,
    ) -> Result<Self, GraphWriteProgramTemplateError> {
        let parameters = program_parameters(
            statements
                .iter()
                .map(|input| (input.relation(), input.parameter_schema())),
        )?;
        Ok(Self {
            statements: statements.into_boxed_slice(),
            parameters,
        })
    }
    #[must_use]
    pub fn statements(&self) -> &[GraphWriteTemplateStatement] {
        &self.statements
    }
    #[must_use]
    pub fn parameter_schema(&self) -> &[GqlParameterSpec] {
        &self.parameters
    }

    pub fn bind_parameters(
        &self,
        arguments: &GqlParameters,
    ) -> Result<PreparedGraphWriteProgram, GraphWriteProgramTemplateError> {
        check_program_arguments(&self.parameters, arguments)?;
        let mut statements = Vec::with_capacity(self.statements.len());
        for (statement, input) in self.statements.iter().enumerate() {
            let local = local_arguments(input.parameter_schema(), arguments);
            let bound = match input {
                GraphWriteTemplateStatement::Mutation(input) => {
                    GraphWriteStatement::Mutation(input.bind_parameters(&local).map_err(
                        |source| GraphMutationProgramTemplateError::Bind { statement, source },
                    )?)
                }
                GraphWriteTemplateStatement::Insert(input) => {
                    GraphWriteStatement::Insert(input.bind_parameters(&local).map_err(
                        |source| GraphWriteProgramTemplateError::InsertBind { statement, source },
                    )?)
                }
                GraphWriteTemplateStatement::VertexMerge(input) => {
                    GraphWriteStatement::VertexMerge(input.bind_parameters(&local).map_err(
                        |source| GraphWriteProgramTemplateError::VertexMergeBind {
                            statement,
                            source,
                        },
                    )?)
                }
                GraphWriteTemplateStatement::VertexUpsert(input) => {
                    GraphWriteStatement::VertexUpsert(input.bind_parameters(&local).map_err(
                        |source| GraphWriteProgramTemplateError::VertexUpsertBind {
                            statement,
                            source,
                        },
                    )?)
                }
                GraphWriteTemplateStatement::EdgeMerge(input) => GraphWriteStatement::EdgeMerge(
                    input.bind_parameters(&local).map_err(|source| {
                        GraphWriteProgramTemplateError::EdgeMergeBind { statement, source }
                    })?,
                ),
                GraphWriteTemplateStatement::EdgeUpsert(input) => GraphWriteStatement::EdgeUpsert(
                    input.bind_parameters(&local).map_err(|source| {
                        GraphWriteProgramTemplateError::EdgeUpsertBind { statement, source }
                    })?,
                ),
                GraphWriteTemplateStatement::Delete(input) => {
                    GraphWriteStatement::Delete(input.bind_parameters(&local).map_err(
                        |source| GraphWriteProgramTemplateError::DeleteBind { statement, source },
                    )?)
                }
            };
            statements.push(bound);
        }
        PreparedGraphWriteProgram::prepare(statements)
            .map_err(|source| GraphMutationProgramTemplateError::Definition(source).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        GqlParameterType, GraphInsertTextErrorKind, GraphPatternTextErrorKind, GraphSymbol,
        GraphSymbolKind,
    };
    use fgdb_delta_types::{LabelId, PropertyKeyId};
    use fgdb_types::{CanonicalScalar, CanonicalScalarKind};
    use std::cell::Cell;

    const R: RelationId = RelationId(1);
    fn symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Property, "p") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "q") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            (GraphSymbolKind::Label, "Person") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Relation, "R") => Some(GraphSymbol::Relation(R)),
            _ => None,
        }
    }
    fn insert(text: &str) -> PreparedGraphInsertText {
        PreparedGraphInsertText::prepare(text, R, symbols).unwrap()
    }
    fn mutation(text: &str) -> PreparedGraphMutationText {
        PreparedGraphMutationText::prepare(text, R, symbols).unwrap()
    }

    #[test]
    fn one_argument_map_binds_every_step_without_catalog_reentry() {
        let calls = Cell::new(0);
        let mut resolve = |kind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        };
        let a = PreparedGraphInsertText::prepare("CREATE (x {p:$seed,q:$shared})", R, &mut resolve)
            .unwrap();
        let b =
            PreparedGraphMutationText::prepare("MATCH (n) SET n.p=n.p+$shared", R, &mut resolve)
                .unwrap();
        let c = PreparedGraphInsertText::prepare(
            "MATCH (n) WHERE n.p >= $shared CREATE (y {p:$tail})",
            R,
            &mut resolve,
        )
        .unwrap();
        let before = calls.get();
        let template = PreparedGraphWriteProgramTemplate::prepare(vec![
            a.clone().into(),
            b.clone().into(),
            c.clone().into(),
        ])
        .unwrap();
        let schema = template.parameter_schema();
        assert_eq!(
            schema
                .iter()
                .map(|spec| (spec.name.as_str(), spec.occurrences))
                .collect::<Vec<_>>(),
            vec![("seed", 1), ("shared", 3), ("tail", 1)]
        );
        let arguments = GqlParameters::new()
            .with_int64("seed", 1)
            .unwrap()
            .with_int64("shared", 2)
            .unwrap()
            .with_int64("tail", 3)
            .unwrap();
        let actual = template.bind_parameters(&arguments).unwrap();
        let expected = PreparedGraphWriteProgram::prepare(vec![
            a.bind_parameters(
                &GqlParameters::new()
                    .with_int64("seed", 1)
                    .unwrap()
                    .with_int64("shared", 2)
                    .unwrap(),
            )
            .unwrap()
            .into(),
            b.bind_parameters(&GqlParameters::new().with_int64("shared", 2).unwrap())
                .unwrap()
                .into(),
            c.bind_parameters(
                &GqlParameters::new()
                    .with_int64("shared", 2)
                    .unwrap()
                    .with_int64("tail", 3)
                    .unwrap(),
            )
            .unwrap()
            .into(),
        ])
        .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(actual, template.bind_parameters(&arguments).unwrap());
        assert_eq!(calls.get(), before);
        let changed = GqlParameters::new()
            .with_int64("seed", 4)
            .unwrap()
            .with_int64("shared", 2)
            .unwrap()
            .with_int64("tail", 3)
            .unwrap();
        assert_ne!(
            actual.canonical_bytes(),
            template
                .bind_parameters(&changed)
                .unwrap()
                .canonical_bytes()
        );
        assert!(!format!("{template:?}").contains("shared"));
    }

    #[test]
    fn parameter_types_relations_and_program_sizes_are_checked_once() {
        let boolean = CanonicalScalarKind::of(&CanonicalScalar::Bool(true));
        let a = PreparedGraphInsertText::prepare_with_parameter_types(
            "CREATE (x {p:$x})",
            R,
            &[("x", GqlParameterType::Scalar(boolean))],
            symbols,
        )
        .unwrap();
        let b = mutation("MATCH (n) SET n.p=$x");
        let error =
            PreparedGraphWriteProgramTemplate::prepare(vec![a.into(), b.into()]).unwrap_err();
        assert!(matches!(
            error,
            GraphWriteProgramTemplateError::Program(
                GraphMutationProgramTemplateError::ConflictingParameterTypes {
                    parameter: 0,
                    first_statement: 0,
                    statement: 1,
                }
            )
        ));
        let foreign =
            PreparedGraphInsertText::prepare("CREATE (x)", RelationId(2), symbols).unwrap();
        assert!(matches!(
            PreparedGraphWriteProgramTemplate::prepare(vec![
                insert("CREATE (x)").into(),
                foreign.into()
            ]),
            Err(GraphWriteProgramTemplateError::Program(
                GraphMutationProgramTemplateError::Definition(
                    GraphMutationProgramBuildError::MixedRelation { statement: 1 }
                )
            ))
        ));
        assert!(PreparedGraphWriteProgramTemplate::prepare(vec![]).is_err());
        assert!(
            PreparedGraphWriteProgramTemplate::prepare(vec![
                insert("CREATE (x)").into();
                MAX_GRAPH_MUTATION_STATEMENTS + 1
            ])
            .is_err()
        );
    }

    #[test]
    fn later_bind_failures_retain_statement_index_and_original_utf8_offsets() {
        let text = "\u{2003}CREATE (z {p:$missing})";
        let template = PreparedGraphWriteProgramTemplate::prepare(vec![
            insert("CREATE (x {p:1})").into(),
            mutation("MATCH (n) SET n.p=2").into(),
            insert(text).into(),
        ])
        .unwrap();
        let error = template.bind_parameters(&GqlParameters::new()).unwrap_err();
        assert!(
            matches!(error, GraphWriteProgramTemplateError::InsertBind { statement: 2, source }
            if source.offset == text.find('$').unwrap() && matches!(source.kind, GraphInsertTextErrorKind::Query(GraphPatternTextErrorKind::MissingParameter)))
        );
        let text = "\u{2003}MATCH (n) SET n.p=$missing";
        let template = PreparedGraphWriteProgramTemplate::prepare(vec![
            insert("CREATE (x)").into(),
            mutation(text).into(),
        ])
        .unwrap();
        let error = template.bind_parameters(&GqlParameters::new()).unwrap_err();
        assert!(
            matches!(error, GraphWriteProgramTemplateError::Program(GraphMutationProgramTemplateError::Bind { statement: 1, source })
            if source.offset == text.find('$').unwrap())
        );
        let extra = GqlParameters::new().with_int64("unused", 3).unwrap();
        assert!(matches!(
            template.bind_parameters(&extra),
            Err(GraphWriteProgramTemplateError::Program(
                GraphMutationProgramTemplateError::UnexpectedArguments
            ))
        ));
    }

    #[test]
    fn text_arguments_remain_values_in_both_creation_and_mutation() {
        let payload = "private '}) CREATE (escape) --";
        let kind = CanonicalScalarKind::of(&CanonicalScalar::ucs_basic_text(payload).unwrap());
        let a = PreparedGraphInsertText::prepare_with_parameter_types(
            "CREATE (x {p:$value})",
            R,
            &[("value", GqlParameterType::Scalar(kind))],
            symbols,
        )
        .unwrap();
        let b = PreparedGraphMutationText::prepare_with_parameter_types(
            "MATCH (n) SET n.q=$value",
            R,
            &[("value", GqlParameterType::Scalar(kind))],
            symbols,
        )
        .unwrap();
        let template =
            PreparedGraphWriteProgramTemplate::prepare(vec![a.into(), b.into()]).unwrap();
        let args = GqlParameters::new().with_text("value", payload).unwrap();
        let program = template.bind_parameters(&args).unwrap();
        assert_eq!(program.statements().len(), 2);
        assert_eq!(template.parameter_schema()[0].occurrences, 2);
        assert!(!format!("{template:?} {program:?}").contains(payload));
        assert!(
            template
                .bind_parameters(&GqlParameters::new().with_int64("value", 1).unwrap())
                .is_err()
        );
    }

    #[test]
    fn mutation_only_templates_preserve_the_existing_schema_and_bindings() {
        let a = mutation("MATCH (n) SET n.p=$x");
        let b = mutation("MATCH (n) SET n.q=$y+$x");
        let old =
            PreparedGraphMutationProgramTemplate::prepare(vec![a.clone(), b.clone()]).unwrap();
        let new = PreparedGraphWriteProgramTemplate::prepare(vec![a.into(), b.into()]).unwrap();
        assert_eq!(old.parameter_schema(), new.parameter_schema());
        let args = GqlParameters::new()
            .with_int64("x", 1)
            .unwrap()
            .with_int64("y", 2)
            .unwrap();
        let bound = new.bind_parameters(&args).unwrap();
        let inputs = bound
            .statements()
            .iter()
            .map(|step| match step {
                GraphWriteStatement::Mutation(input) => input.clone(),
                _ => panic!("unexpected non-mutation step"),
            })
            .collect();
        assert_eq!(
            PreparedGraphMutationProgram::prepare(inputs).unwrap(),
            old.bind_parameters(&args).unwrap()
        );
    }

    #[test]
    fn native_merges_share_one_parameter_contract_and_bind_without_catalog_access() {
        let calls = Cell::new(0);
        let mut resolve = |kind, name: &str| {
            calls.set(calls.get() + 1);
            symbols(kind, name)
        };
        let merge =
            PreparedGraphVertexMergeText::prepare("MERGE (n:Person {p:$key})", R, &mut resolve)
                .unwrap();
        let upsert = PreparedGraphVertexUpsertText::prepare(
            "MERGE (n:Person {p:$key}) ON MATCH SET n.q=$value ON CREATE SET n.q=0",
            R,
            &mut resolve,
        )
        .unwrap();
        let update = PreparedGraphMutationText::prepare(
            "MATCH (n:Person) WHERE n.p=$key SET n.q=$value",
            R,
            &mut resolve,
        )
        .unwrap();
        let resolved = calls.get();
        let template = PreparedGraphWriteProgramTemplate::prepare(vec![
            merge.clone().into(),
            upsert.clone().into(),
            update.clone().into(),
        ])
        .unwrap();
        let args = GqlParameters::new()
            .with_int64("key", 7)
            .unwrap()
            .with_int64("value", 9)
            .unwrap();
        let actual = template.bind_parameters(&args).unwrap();
        let expected = PreparedGraphWriteProgram::prepare(vec![
            merge
                .bind_parameters(&GqlParameters::new().with_int64("key", 7).unwrap())
                .unwrap()
                .into(),
            upsert.bind_parameters(&args).unwrap().into(),
            update.bind_parameters(&args).unwrap().into(),
        ])
        .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(actual, template.bind_parameters(&args).unwrap());
        assert_eq!(calls.get(), resolved);
        assert!(!format!("{template:?} {actual:?}").contains("Person"));
        assert!(matches!(
            template.bind_parameters(&GqlParameters::new().with_int64("key", 7).unwrap()),
            Err(GraphWriteProgramTemplateError::VertexUpsertBind { statement: 1, .. })
        ));
        assert!(matches!(
            template.bind_parameters(&GqlParameters::new()),
            Err(GraphWriteProgramTemplateError::VertexMergeBind { statement: 0, .. })
        ));
    }
}

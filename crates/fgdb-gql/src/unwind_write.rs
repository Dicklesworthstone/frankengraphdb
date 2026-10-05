//! Bounded, parameterized `UNWIND $rows AS row` graph-write batches.
//!
//! This adapter lowers a single MERGE or MATCH mutation to the existing native
//! write compiler, then binds every input row before returning ONE ordinary
//! atomic program. It does not execute source text or interpolate row values.
//! Native UNWIND CREATE/INSERT and CREATE RETURN keep their existing compiler.
//!
//! The admitted source is a nonempty list of maps (null rows and missing fields
//! yield null). Referenced fields must be scalar and have one exact non-null
//! kind per column; no numeric coercion is introduced. Nested field access,
//! alias rebinding, multiple statements and empty batches are not admitted.

use crate::algebra::GraphValue;
use crate::{
    BoundGraphWriteScriptBatch, GqlParameterError, GqlParameterType, GqlParameterValue,
    GqlParameters, GraphSymbol, GraphSymbolKind, GraphWriteScriptBatchError, GraphWriteScriptError,
    PreparedGraphWriteScript,
};
use fgdb_delta_types::RelationId;
use fgdb_types::{CanonicalScalar, CanonicalScalarKind};

/// A cap on the SUM of expanded argument transcripts, including repeated global
/// arguments. It is not an execution quota, allocator-byte bound or storage cap.
pub const MAX_UNWIND_BOUND_PARAMETER_BYTES: usize =
    crate::parameters::MAX_GQL_PARAMETER_TRANSCRIPT_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphUnwindRowError {
    ExpectedMap,
    ExpectedScalarField,
    IncompatibleFieldTypes,
}

/// Errors contain coordinates and typed causes, never source text or row values.
#[derive(Debug)]
pub enum GraphUnwindWriteError {
    Syntax(GraphWriteScriptError),
    /// The named UNWIND source is missing or is not a list parameter.
    SourceParameter,
    /// Supplied names must be exactly the names referenced by the original text.
    ArgumentNames,
    /// The ordinary atomic batch executor currently requires a nonempty batch.
    Empty,
    TooManyRows {
        limit: usize,
        observed: usize,
    },
    Row {
        row: usize,
        offset: usize,
        kind: GraphUnwindRowError,
    },
    Parameter {
        row: usize,
        source: GqlParameterError,
    },
    ExpandedParametersTooLarge {
        limit: usize,
        observed: u128,
    },
    Definition(GraphWriteScriptError),
    Binding(GraphWriteScriptBatchError),
}

impl core::fmt::Display for GraphUnwindWriteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Syntax(source) | Self::Definition(source) => source.fmt(f),
            Self::SourceParameter => f.write_str("UNWIND requires its named list parameter"),
            Self::ArgumentNames => f.write_str("UNWIND argument names do not match its definition"),
            Self::Empty => f.write_str("UNWIND write batch requires at least one input row"),
            Self::TooManyRows { limit, observed } => {
                write!(f, "UNWIND write batch has {observed} rows; limit {limit}")
            }
            Self::Row { row, offset, kind } => {
                write!(f, "UNWIND input row {row} at byte {offset}: {kind:?}")
            }
            Self::Parameter { row, source } => {
                write!(f, "UNWIND input row {row}: {source}")
            }
            Self::ExpandedParametersTooLarge { limit, observed } => write!(
                f,
                "UNWIND expanded argument transcripts use {observed} bytes; limit {limit}"
            ),
            Self::Binding(source) => source.fmt(f),
        }
    }
}
impl core::error::Error for GraphUnwindWriteError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Syntax(source) | Self::Definition(source) => Some(source),
            Self::Parameter { source, .. } => Some(source),
            Self::Binding(source) => Some(source),
            _ => None,
        }
    }
}

/// Admission failures are distinct from execution or publication failures.
/// An execution error keeps the batch's input coordinates and the original
/// commit outcome; this wrapper does not imply that a failed publish rolled back.
#[derive(Debug)]
pub enum GraphUnwindWriteExecutionError<E, A, C> {
    /// The complete input was refused before entering the program executor.
    Binding(GraphUnwindWriteError),
    /// The ordinary batch executor's error, without a partial success receipt.
    Execution(crate::GraphWriteScriptExecutionError<E, A, C>),
}

impl<E: core::fmt::Display, A: core::fmt::Display, C: core::fmt::Display> core::fmt::Display
    for GraphUnwindWriteExecutionError<E, A, C>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Binding(source) => source.fmt(f),
            Self::Execution(source) => source.fmt(f),
        }
    }
}

impl<
    E: core::error::Error + 'static,
    A: core::error::Error + 'static,
    C: core::error::Error + 'static,
> core::error::Error for GraphUnwindWriteExecutionError<E, A, C>
{
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::Binding(source) => Some(source),
            Self::Execution(source) => Some(source),
        }
    }
}

#[derive(Clone)]
pub(crate) struct UnwindField {
    pub(crate) key: String,
    pub(crate) parameter: String,
    pub(crate) offset: usize,
}

/// Lexically admitted UNWIND text. Parsing uses the native lexer. Binding also
/// runs the native write compiler: this object alone is not a validated query,
/// an authorization grant, or an executable program.
///
/// `parse_if_supported` leaves existing native CREATE/INSERT forms alone. The
/// explicit `parse` entrypoint refuses non-target forms. Bind with a relation
/// coordinate and the same symbol resolver used for ordinary write scripts.
#[derive(Clone)]
pub struct GraphUnwindWriteText {
    pub(crate) original: String,
    pub(crate) lowered: String,
    pub(crate) source_parameter: String,
    pub(crate) source_offset: usize,
    pub(crate) external_parameters: Box<[String]>,
    pub(crate) fields: Box<[UnwindField]>,
}
impl core::fmt::Debug for GraphUnwindWriteText {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GraphUnwindWriteText")
            .field("fields", &self.fields.len())
            .field("parameters", &(self.external_parameters.len() + 1))
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl GraphUnwindWriteText {
    #[must_use]
    pub fn text(&self) -> &str {
        &self.original
    }

    /// Admit and bind at most 64 rows into one native atomic write program.
    pub fn bind(
        &self,
        arguments: &GqlParameters,
        relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<BoundGraphWriteScriptBatch, GraphUnwindWriteError> {
        self.bind_with_limit(
            arguments,
            relation,
            crate::MAX_GRAPH_MUTATION_STATEMENTS,
            resolve,
        )
    }

    /// Bind a finite batch without per-row transactions, quota refreshes or
    /// retries. Row-count, shape, column-type and expanded-transcript admission
    /// all precede catalog resolution. Every row binds before a program escapes.
    ///
    /// Null does not fix a column's kind: a later non-null value does. An absent
    /// map key remains null. A list/map/identity used as a scalar field refuses.
    /// Global parameters retain their exact original types. The cap is clamped
    /// to the ordinary batch executor's 65,536-statement hard ceiling.
    ///
    /// Original UTF-8 byte offsets survive lowering, including comments and
    /// strings. Execution uses the ordinary program executor and its existing
    /// rollback, authorization, cancellation, allocation and commit semantics.
    pub fn bind_with_limit(
        &self,
        arguments: &GqlParameters,
        relation: RelationId,
        max_rows: usize,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<BoundGraphWriteScriptBatch, GraphUnwindWriteError> {
        let Some(GqlParameterValue::List(source)) = arguments.get(&self.source_parameter) else {
            return Err(GraphUnwindWriteError::SourceParameter);
        };
        let rows = source.values();
        if rows.is_empty() {
            return Err(GraphUnwindWriteError::Empty);
        }
        let limit = max_rows.min(PreparedGraphWriteScript::MAX_BATCH_STATEMENTS);
        if rows.len() > limit {
            return Err(GraphUnwindWriteError::TooManyRows {
                limit,
                observed: rows.len(),
            });
        }
        if arguments.len() != self.external_parameters.len() + 1
            || self
                .external_parameters
                .iter()
                .any(|name| arguments.get(name).is_none())
        {
            return Err(GraphUnwindWriteError::ArgumentNames);
        }

        // First inspect all shapes and exact kinds, without cloning field data.
        let mut kinds = vec![CanonicalScalarKind::Null; self.fields.len()];
        for (row_index, row) in rows.iter().enumerate() {
            if !matches!(
                row,
                GraphValue::Map { .. } | GraphValue::Scalar(CanonicalScalar::Null)
            ) {
                return Err(GraphUnwindWriteError::Row {
                    row: row_index,
                    offset: self.source_offset,
                    kind: GraphUnwindRowError::ExpectedMap,
                });
            }
            for (field, kind) in self.fields.iter().zip(kinds.iter_mut()) {
                if let Some(value) = scalar_field(row, field, row_index)? {
                    let actual = CanonicalScalarKind::of(value);
                    if actual == CanonicalScalarKind::Null {
                        continue;
                    }
                    if *kind != CanonicalScalarKind::Null && *kind != actual {
                        return Err(GraphUnwindWriteError::Row {
                            row: row_index,
                            offset: field.offset,
                            kind: GraphUnwindRowError::IncompatibleFieldTypes,
                        });
                    }
                    *kind = actual;
                }
            }
        }

        let mut globals = GqlParameters::new();
        for name in self.external_parameters.iter() {
            let value = arguments
                .get(name)
                .ok_or(GraphUnwindWriteError::ArgumentNames)?;
            globals
                .insert(name.clone(), value)
                .map_err(|source| GraphUnwindWriteError::Parameter { row: 0, source })?;
        }
        let mut declarations: Vec<_> = globals.parameter_types().collect();
        declarations.extend(
            self.fields
                .iter()
                .zip(&kinds)
                .map(|(field, kind)| (field.parameter.as_str(), GqlParameterType::Scalar(*kind))),
        );

        let mut sets = Vec::with_capacity(rows.len());
        let mut expanded_bytes = 0_u128;
        for (row_index, row) in rows.iter().enumerate() {
            let mut values = globals.clone();
            for field in self.fields.iter() {
                let value = scalar_field(row, field, row_index)?
                    .cloned()
                    .unwrap_or(CanonicalScalar::Null);
                values = values
                    .with_scalar(field.parameter.clone(), value)
                    .map_err(|source| GraphUnwindWriteError::Parameter {
                        row: row_index,
                        source,
                    })?;
            }
            expanded_bytes += values.canonical_byte_len() as u128;
            if expanded_bytes > MAX_UNWIND_BOUND_PARAMETER_BYTES as u128 {
                return Err(GraphUnwindWriteError::ExpandedParametersTooLarge {
                    limit: MAX_UNWIND_BOUND_PARAMETER_BYTES,
                    observed: expanded_bytes,
                });
            }
            sets.push(values);
        }
        let prepared = PreparedGraphWriteScript::prepare_with_parameter_types(
            &self.lowered,
            relation,
            &declarations,
            resolve,
        )
        .map_err(GraphUnwindWriteError::Definition)?;
        prepared
            .bind_parameter_sets_with_limit(&sets, limit)
            .map_err(GraphUnwindWriteError::Binding)
    }
}

fn scalar_field<'a>(
    row: &'a GraphValue,
    field: &UnwindField,
    row_index: usize,
) -> Result<Option<&'a CanonicalScalar>, GraphUnwindWriteError> {
    let Some((keys, values)) = row.as_map() else {
        // Row shape was admitted by the caller; null.field is null.
        return Ok(None);
    };
    let Ok(index) = keys.binary_search_by(|key| key.as_ref().cmp(field.key.as_str())) else {
        return Ok(None);
    };
    match &values[index] {
        GraphValue::Scalar(value) => Ok(Some(value)),
        _ => Err(GraphUnwindWriteError::Row {
            row: row_index,
            offset: field.offset,
            kind: GraphUnwindRowError::ExpectedScalarField,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GraphPatternTextErrorKind, GraphWriteScriptErrorKind};
    use fgdb_delta_types::{LabelId, PropertyKeyId};

    const QUERY: &str = "UNWIND $rows AS row MERGE (n:Entity {id:row.id}) SET n.name=row.name";

    fn resolve(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Label, "Entity") => Some(GraphSymbol::Label(LabelId(1))),
            (GraphSymbolKind::Property, "id") => Some(GraphSymbol::Property(PropertyKeyId(1))),
            (GraphSymbolKind::Property, "name") => Some(GraphSymbol::Property(PropertyKeyId(2))),
            (GraphSymbolKind::Property, "greeting") => {
                Some(GraphSymbol::Property(PropertyKeyId(3)))
            }
            _ => None,
        }
    }
    fn map(fields: &[(&str, CanonicalScalar)]) -> GraphValue {
        GraphValue::map(
            fields
                .iter()
                .map(|(name, value)| ((*name).into(), GraphValue::Scalar(value.clone())))
                .collect(),
        )
        .unwrap()
    }
    fn text(value: &str) -> CanonicalScalar {
        CanonicalScalar::ucs_basic_text(value).unwrap()
    }
    fn rows(values: Vec<GraphValue>) -> GqlParameters {
        GqlParameters::new().with_list("rows", values).unwrap()
    }

    #[test]
    fn binds_a_complete_map_batch_with_original_statement_coordinates() {
        let parsed = GraphUnwindWriteText::parse(QUERY).unwrap();
        let arguments = rows(vec![
            map(&[("id", CanonicalScalar::Int(1)), ("name", text("Ada"))]),
            map(&[("id", CanonicalScalar::Int(2)), ("name", text("Bob"))]),
            map(&[("id", CanonicalScalar::Int(1)), ("name", text("Updated"))]),
        ]);
        let frozen = arguments.canonical_bytes();
        let batch = parsed.bind(&arguments, RelationId(1), resolve).unwrap();
        assert_eq!(batch.argument_sets(), 3);
        assert_eq!(batch.program().statements().len(), 3);
        assert_eq!(batch.location(2).unwrap().argument_set, 2);
        assert_eq!(batch.location(2).unwrap().span, 0..QUERY.len());
        assert!(batch.location(3).is_none());
        assert_eq!(arguments.canonical_bytes(), frozen);
        assert_eq!(parsed.text(), QUERY);
    }

    #[test]
    fn null_and_missing_fields_do_not_freeze_column_type_to_null() {
        let parsed = GraphUnwindWriteText::parse(QUERY).unwrap();
        let arguments = rows(vec![
            map(&[("id", CanonicalScalar::Int(1))]),
            map(&[("id", CanonicalScalar::Int(2)), ("name", text("Ada"))]),
            map(&[
                ("id", CanonicalScalar::Int(3)),
                ("name", CanonicalScalar::Null),
            ]),
        ]);
        assert_eq!(
            parsed
                .bind(&arguments, RelationId(1), resolve)
                .unwrap()
                .argument_sets(),
            3
        );
    }

    #[test]
    fn mixed_column_kinds_refuse_before_catalog_callbacks() {
        let parsed = GraphUnwindWriteText::parse(QUERY).unwrap();
        let arguments = rows(vec![
            map(&[("id", CanonicalScalar::Int(1)), ("name", text("Ada"))]),
            map(&[
                ("id", CanonicalScalar::Int(2)),
                ("name", CanonicalScalar::Bool(true)),
            ]),
        ]);
        let mut calls = 0;
        let result = parsed.bind(&arguments, RelationId(1), |kind, name| {
            calls += 1;
            resolve(kind, name)
        });
        assert!(matches!(
            result,
            Err(GraphUnwindWriteError::Row {
                row: 1,
                kind: GraphUnwindRowError::IncompatibleFieldTypes,
                ..
            })
        ));
        assert_eq!(calls, 0);
    }

    #[test]
    fn count_and_row_shape_admission_precede_resolution() {
        let parsed = GraphUnwindWriteText::parse(QUERY).unwrap();
        let arguments = rows(vec![GraphValue::Scalar(CanonicalScalar::Int(1))]);
        let mut calls = 0;
        let mut catalog = |kind, name: &str| {
            calls += 1;
            resolve(kind, name)
        };
        assert!(matches!(
            parsed.bind_with_limit(&arguments, RelationId(1), 0, &mut catalog),
            Err(GraphUnwindWriteError::TooManyRows {
                limit: 0,
                observed: 1
            })
        ));
        assert!(matches!(
            parsed.bind(&arguments, RelationId(1), &mut catalog),
            Err(GraphUnwindWriteError::Row {
                kind: GraphUnwindRowError::ExpectedMap,
                ..
            })
        ));
        assert_eq!(calls, 0);
    }

    #[test]
    fn nested_fields_and_unused_arguments_refuse_without_catalog_access() {
        let parsed = GraphUnwindWriteText::parse(QUERY).unwrap();
        let composite = GraphValue::map(vec![
            ("id".into(), GraphValue::Scalar(CanonicalScalar::Int(1))),
            (
                "name".into(),
                GraphValue::List(Vec::new().into_boxed_slice()),
            ),
        ])
        .unwrap();
        assert!(matches!(
            parsed.bind(&rows(vec![composite]), RelationId(1), |_, _| panic!(
                "catalog"
            )),
            Err(GraphUnwindWriteError::Row {
                kind: GraphUnwindRowError::ExpectedScalarField,
                ..
            })
        ));
        let arguments = rows(vec![map(&[("id", CanonicalScalar::Int(1))])])
            .with_int64("unused", 9)
            .unwrap();
        assert!(matches!(
            parsed.bind(&arguments, RelationId(1), |_, _| panic!("catalog")),
            Err(GraphUnwindWriteError::ArgumentNames)
        ));
    }

    #[test]
    fn missing_source_and_empty_batches_are_explicit_refusals() {
        let parsed = GraphUnwindWriteText::parse(QUERY).unwrap();
        assert!(matches!(
            parsed.bind(&GqlParameters::new(), RelationId(1), resolve),
            Err(GraphUnwindWriteError::SourceParameter)
        ));
        assert!(matches!(
            parsed.bind(&rows(vec![]), RelationId(1), resolve),
            Err(GraphUnwindWriteError::Empty)
        ));
    }

    #[test]
    fn lowering_preserves_literals_comments_and_utf8_byte_offsets() {
        let query = "UNWIND /* é */ $rows AS row\nMERGE (n:Entity {id:row /* gap */ . id}) \
            SET n.name='row.name; $aa é'";
        let parsed = GraphUnwindWriteText::parse(query).unwrap();
        assert_eq!(parsed.lowered.len(), query.len());
        let literal = "'row.name; $aa é'";
        let start = query.find(literal).unwrap();
        assert_eq!(&parsed.lowered[start..start + literal.len()], literal);
        let merge = query.find("MERGE").unwrap();
        assert_eq!(&parsed.lowered[merge..merge + 5], "MERGE");
        assert_eq!(
            parsed.lowered.bytes().filter(|byte| *byte == b'\n').count(),
            query.bytes().filter(|byte| *byte == b'\n').count()
        );
    }

    #[test]
    fn synthetic_parameters_do_not_capture_user_parameters_or_repeat_fields() {
        // A MERGE's SET takes literal or parameter values (a computed value
        // such as `$aa + r.name` refuses typed), so the repeated field and the
        // user parameter each appear as a whole value here.
        let query = "UNWIND $rows AS r MERGE (n:Entity {id:r.id}) \
            ON CREATE SET n.name=r.name ON MATCH SET n.name=r.name SET n.greeting=$aa";
        let parsed = GraphUnwindWriteText::parse(query).unwrap();
        assert_eq!(parsed.fields.len(), 2);
        assert!(parsed.fields.iter().all(|field| field.parameter != "aa"));
        let arguments = rows(vec![map(&[
            ("id", CanonicalScalar::Int(1)),
            ("name", text("Ada")),
        ])])
        .with_text("aa", "Hello ")
        .unwrap();
        parsed.bind(&arguments, RelationId(1), resolve).unwrap();
    }

    #[test]
    fn map_values_are_field_uses_and_labels_spelled_like_the_alias_are_not() {
        let query = "UNWIND $rows AS row MERGE (n:row {id:row.id})-[:row]->(m:Entity {id:row.to})";
        let parsed = GraphUnwindWriteText::parse(query).unwrap();
        let keys: Vec<&str> = parsed
            .fields
            .iter()
            .map(|field| field.key.as_str())
            .collect();
        assert_eq!(keys, ["id", "to"]);
        assert!(!parsed.lowered.contains("row."));
        assert_eq!(parsed.lowered.matches(":row ").count(), 1);
        assert_eq!(parsed.lowered.matches(":row]").count(), 1);
    }

    #[test]
    fn native_create_and_create_return_are_not_intercepted() {
        for query in [
            "UNWIND $rows AS row CREATE (n {id:row.id})",
            "UNWIND $rows AS row MATCH (a) CREATE (n {id:row.id})",
            "UNWIND $rows AS row CREATE (n {id:row.id}) RETURN n",
            "CREATE (n)",
            "UNWIND [1,2] AS id CREATE (n {id:id})",
        ] {
            assert!(
                GraphUnwindWriteText::parse_if_supported(query)
                    .unwrap()
                    .is_none(),
                "{query}"
            );
        }
    }

    #[test]
    fn imported_row_cannot_be_rebound_or_used_as_an_identity() {
        for query in [
            "UNWIND $rows AS row MERGE (row:Entity {id:row.id})",
            "UNWIND $rows AS row MATCH (row) SET row.name='x'",
            "UNWIND $rows AS row MERGE (n:Entity {id:row})",
            "UNWIND $rows AS row MERGE (n:Entity {id:row.id.part})",
        ] {
            assert!(matches!(
                GraphUnwindWriteText::parse(query),
                Err(GraphUnwindWriteError::Syntax(_))
            ));
        }
    }

    #[test]
    fn native_preparation_errors_still_point_into_the_original_text() {
        let query = "UNWIND $rows AS row MERGE (n:Missing {id:row.id})";
        let parsed = GraphUnwindWriteText::parse(query).unwrap();
        let error = parsed
            .bind(
                &rows(vec![map(&[("id", CanonicalScalar::Int(1))])]),
                RelationId(1),
                resolve,
            )
            .unwrap_err();
        let GraphUnwindWriteError::Definition(source) = error else {
            panic!("{error}")
        };
        assert!(source.offset >= query.find("MERGE").unwrap());
        assert!(source.offset < query.len());
        assert!(!matches!(
            source.kind,
            GraphWriteScriptErrorKind::Syntax(GraphPatternTextErrorKind::Expected(
                "a supported graph write statement"
            ))
        ));
    }
}

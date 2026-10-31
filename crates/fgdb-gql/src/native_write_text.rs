//! Native no-RETURN text binding shared by embedded and command-line writers.
//!
//! The existing compilers own syntax, types and lowering. This boundary only
//! selects the bounded UNWIND mutation adapter or the ordinary write script,
//! splices a script's map arguments that stand for property maps
//! (GqlMapArgumentText), and retains input coordinates until
//! execution/completion has succeeded.

use crate::unwind_write::{GraphUnwindWriteError, GraphUnwindWriteText};
use crate::{
    BoundGraphWriteScriptBatch, GqlMapArgumentText, GqlParameterType, GqlParameters, GraphSymbol,
    GraphSymbolKind, GraphWriteProgramError, GraphWriteScriptError, GraphWriteScriptExecutionError,
    PreparedGraphWriteProgram, PreparedGraphWriteScript,
};
use fgdb_delta_types::RelationId;

#[derive(Debug)]
pub enum NativeGraphWriteBindError {
    ScriptPreparation(GraphWriteScriptError),
    ScriptBinding(GraphWriteScriptError),
    Unwind(GraphUnwindWriteError),
}
impl core::fmt::Display for NativeGraphWriteBindError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ScriptPreparation(error) | Self::ScriptBinding(error) => error.fmt(f),
            Self::Unwind(error) => error.fmt(f),
        }
    }
}
impl core::error::Error for NativeGraphWriteBindError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            Self::ScriptPreparation(error) | Self::ScriptBinding(error) => Some(error),
            Self::Unwind(error) => Some(error),
        }
    }
}

enum BoundInput {
    Script(PreparedGraphWriteProgram),
    Unwind(BoundGraphWriteScriptBatch),
}

/// One fully bound native no-RETURN write. This owns no transaction or authority.
///
/// A script retains its ordinary statement coordinates. A bounded UNWIND
/// mutation additionally retains its input-row coordinates; do not discard
/// this object before translating an execution or publication failure.
/// All parameters and all UNWIND rows bind before an executable program escapes.
/// CREATE/INSERT RETURN is a separate native result-query contract and must be
/// dispatched by that existing compiler before calling this no-RETURN binder.
/// The statement/parameter size caps of the underlying compilers still apply.
pub struct BoundNativeGraphWrite(BoundInput);

impl core::fmt::Debug for BoundNativeGraphWrite {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BoundNativeGraphWrite")
            .field("statements", &self.program().statements().len())
            .field("input_records", &self.input_records())
            .field("definition", &"[REDACTED]")
            .finish()
    }
}

impl BoundNativeGraphWrite {
    /// Probe a native no-RETURN definition without expanding argument rows.
    ///
    /// This is a dispatch probe, not a bound program or authority. Ordinary
    /// scripts, including computed-map UNWIND updates, are prepared but not
    /// bound; row-expanded UNWIND admission is lexical only. Values, row shapes,
    /// downstream types and execution budgets still
    /// require the ordinary binder. Collection contents are never inspected.
    /// Native reads should be admitted by their own compiler before this probe.
    /// Map arguments standing for property maps splice as `bind` splices
    /// them, so a statement the probe admits is one every native write
    /// executor binds (fgdb-0s2pq).
    #[allow(clippy::result_large_err)]
    pub fn probe_definition(
        text: &str,
        arguments: &GqlParameters,
        relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<(), NativeGraphWriteBindError> {
        let computed_maps = GraphUnwindWriteText::requires_computed_map_input(text)
            .map_err(NativeGraphWriteBindError::Unwind)?;
        if !computed_maps
            && GraphUnwindWriteText::parse_if_supported(text)
                .map_err(NativeGraphWriteBindError::Unwind)?
                .is_some()
        {
            return Ok(());
        }
        let lowered = if computed_maps {
            GqlMapArgumentText::lower_fields(text, arguments)
        } else {
            GqlMapArgumentText::lower(text, arguments)
        };
        let (text, arguments) = lowered
            .as_ref()
            .map_or((text, arguments), |(lowered, derived)| {
                (lowered.text(), derived)
            });
        let lowered = lowered.as_ref().map(|(lowered, _)| lowered);
        Self::prepare_script(text, arguments, relation, resolve)
            .map(|_| ())
            .map_err(|error| NativeGraphWriteBindError::ScriptPreparation(restored(lowered, error)))
    }

    /// Bind ordinary scripts or one `UNWIND $rows AS row` MERGE/MATCH mutation.
    ///
    /// Recognition uses the existing native lexer and inspects no values or
    /// catalog state. An admitted UNWIND is never retried as another statement
    /// kind after a bind error. Ordinary UNWIND CREATE/INSERT remains on its
    /// existing compiler, including its list/composite expression semantics.
    /// UNWIND expansion uses the existing atomic-batch hard ceiling (65,536
    /// statements), not the smaller limit on statements in a script definition.
    /// Execution budgets are separate and belong to the entire returned program.
    #[allow(clippy::result_large_err)] // one terminal native binding diagnostic
    pub fn bind(
        text: &str,
        arguments: &GqlParameters,
        relation: RelationId,
        resolve: impl FnMut(GraphSymbolKind, &str) -> Option<GraphSymbol>,
    ) -> Result<Self, NativeGraphWriteBindError> {
        Self::bind_with_resolver(text, arguments, relation, resolve)
    }

    /// As [`Self::bind`], with the host's own resolver rather than a closure
    /// over it, so the statements see its pinned tzdb (fgdb-txbg9).
    #[allow(clippy::result_large_err)]
    pub fn bind_with_resolver(
        text: &str,
        arguments: &GqlParameters,
        relation: RelationId,
        mut resolve: impl crate::GraphSymbolResolver,
    ) -> Result<Self, NativeGraphWriteBindError> {
        let computed_maps = GraphUnwindWriteText::requires_computed_map_input(text)
            .map_err(NativeGraphWriteBindError::Unwind)?;
        if !computed_maps
            && let Some(unwind) = GraphUnwindWriteText::parse_if_supported(text)
                .map_err(NativeGraphWriteBindError::Unwind)?
        {
            let batch = unwind
                .bind_with_limit_resolver(
                    arguments,
                    relation,
                    PreparedGraphWriteScript::MAX_BATCH_STATEMENTS,
                    crate::graph_text::LentResolver(&mut resolve),
                )
                .map_err(NativeGraphWriteBindError::Unwind)?;
            return Ok(Self(BoundInput::Unwind(batch)));
        }
        // Computed whole-map UNWIND updates keep one relational MATCH bag,
        // including heterogeneous maps and rekeys. As on authorized ingress,
        // only pattern fields are lowered; whole map operands stay values.
        // Other scripts retain their ordinary literal-map argument lowering.
        let lowered = if computed_maps {
            GqlMapArgumentText::lower_fields(text, arguments)
        } else {
            GqlMapArgumentText::lower(text, arguments)
        };
        let (text, arguments) = lowered
            .as_ref()
            .map_or((text, arguments), |(lowered, derived)| {
                (lowered.text(), derived)
            });
        let lowered = lowered.as_ref().map(|(lowered, _)| lowered);
        // A computed whole map names its keys through the reverse catalog,
        // which a function resolver lacks: add the argument maps' keys.
        let script = if computed_maps && resolve.reverse_catalog().is_none() {
            let resolve = ArgumentCatalog::new(resolve, text, arguments);
            Self::prepare_script(text, arguments, relation, resolve)
        } else {
            Self::prepare_script(text, arguments, relation, resolve)
        }
        .map_err(|error| NativeGraphWriteBindError::ScriptPreparation(restored(lowered, error)))?;
        let program = script
            .bind_parameters(arguments)
            .map_err(|error| NativeGraphWriteBindError::ScriptBinding(restored(lowered, error)))?;
        Ok(Self(BoundInput::Script(program)))
    }

    #[allow(clippy::result_large_err)]
    fn prepare_script(
        text: &str,
        arguments: &GqlParameters,
        relation: RelationId,
        mut resolve: impl crate::GraphSymbolResolver,
    ) -> Result<PreparedGraphWriteScript, GraphWriteScriptError> {
        // Preserve numeric inference while declaring collection/scalar roles.
        // Collections may occur outside UNWIND, which otherwise infers List.
        let declarations: Vec<_> = arguments
            .parameter_types()
            .filter(|(_, kind)| {
                matches!(
                    kind,
                    GqlParameterType::Scalar(_) | GqlParameterType::List | GqlParameterType::Map
                )
            })
            .collect();
        PreparedGraphWriteScript::prepare_with_parameter_types_and_resolver(
            text,
            relation,
            &declarations,
            &mut resolve,
        )
    }

    #[must_use]
    pub fn program(&self) -> &PreparedGraphWriteProgram {
        match &self.0 {
            BoundInput::Script(program) => program,
            BoundInput::Unwind(batch) => batch.program(),
        }
    }

    /// Some identifies a record-expanded UNWIND; None is an ordinary script.
    #[must_use]
    pub fn input_records(&self) -> Option<usize> {
        match &self.0 {
            BoundInput::Script(_) => None,
            BoundInput::Unwind(batch) => Some(batch.argument_sets()),
        }
    }

    /// Preserve the native program cause, adding row coordinates only when the
    /// batch can locate a failing executed step. Completion errors keep their
    /// original committed/unknown meaning and have no invented record location.
    #[must_use]
    pub fn execution_error<E, A, C>(
        &self,
        source: GraphWriteProgramError<E, A, C>,
    ) -> GraphWriteScriptExecutionError<E, A, C> {
        match &self.0 {
            BoundInput::Script(_) => GraphWriteScriptExecutionError::Program(source),
            BoundInput::Unwind(batch) => batch.execution_error(source),
        }
    }
}

/// A script error of a lowered statement, at the caller's bytes.
fn restored(
    lowered: Option<&GqlMapArgumentText>,
    mut error: GraphWriteScriptError,
) -> GraphWriteScriptError {
    if let Some(lowered) = lowered {
        lowered.restore(&mut error);
    }
    error
}

/// A resolver for a statement prepared with its arguments in hand. A function
/// resolver keeps no reverse catalog, and the one its text yields probes only
/// labels and relations, so a computed whole-map write (`SET n += row` over
/// rows with differing keys) could name none of its argument maps' keys
/// (UnmappedProperty), whichever write compiler takes the statement. This
/// resolves every key a list or map argument holds, once, as a property
/// through the resolver, beside that text catalog. A resolver with its own
/// reverse catalog, or arguments holding no map, pass through unchanged, and
/// every other method is the resolver's own.
///
/// Nothing is resolved until a compiler first asks for the reverse catalog,
/// so arguments that admission refuses never reach the resolver.
pub struct ArgumentCatalog<'a, R> {
    inner: core::cell::RefCell<R>,
    text: &'a str,
    arguments: &'a GqlParameters,
    catalog: core::cell::OnceCell<Option<crate::ReverseSymbolCatalog>>,
}

impl<'a, R: crate::GraphSymbolResolver> ArgumentCatalog<'a, R> {
    /// `inner`, naming the keys of the map values `arguments` hold.
    pub fn new(inner: R, text: &'a str, arguments: &'a GqlParameters) -> Self {
        Self {
            inner: core::cell::RefCell::new(inner),
            text,
            arguments,
            catalog: core::cell::OnceCell::new(),
        }
    }

    /// The text catalog plus every argument map key, or None when the
    /// resolver keeps its own catalog or no argument holds a map.
    fn argument_catalog(&self) -> Option<crate::ReverseSymbolCatalog> {
        let inner = &mut *self.inner.borrow_mut();
        if inner.reverse_catalog().is_some() {
            return None;
        }
        let mut named = std::collections::BTreeSet::new();
        let mut properties = Vec::new();
        let mut pending: Vec<&crate::algebra::GraphValue> =
            self.arguments.collection_values().collect();
        while let Some(value) = pending.pop() {
            if let Some(values) = value.as_list() {
                pending.extend(values);
            } else if let Some((keys, values)) = value.as_map() {
                for key in keys {
                    if named.insert(&**key)
                        && let Some(GraphSymbol::Property(id)) =
                            inner.resolve_symbol(GraphSymbolKind::Property, key)
                    {
                        properties.push((id, &**key));
                    }
                }
                pending.extend(values);
            }
        }
        (!named.is_empty()).then(|| {
            let mut catalog = crate::ReverseSymbolCatalog::from_resolver(inner, self.text);
            for (id, key) in properties {
                catalog.insert_property(id, key);
            }
            catalog
        })
    }
}

impl<R: crate::GraphSymbolResolver> crate::GraphSymbolResolver for ArgumentCatalog<'_, R> {
    fn resolve_symbol(&mut self, kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        self.inner.get_mut().resolve_symbol(kind, name)
    }

    fn time_zones(&self) -> Option<crate::GqlTimeZones> {
        self.inner.borrow().time_zones()
    }

    fn reverse_catalog(&self) -> Option<crate::ReverseSymbolCatalog> {
        self.catalog
            .get_or_init(|| self.argument_catalog())
            .clone()
            .or_else(|| self.inner.borrow().reverse_catalog())
    }

    fn reverse_label(&self, id: fgdb_delta_types::LabelId) -> Option<String> {
        self.inner.borrow().reverse_label(id)
    }

    fn reverse_relation(&self, id: RelationId) -> Option<String> {
        self.inner.borrow().reverse_relation(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_symbols(kind: GraphSymbolKind, name: &str) -> Option<GraphSymbol> {
        match (kind, name) {
            (GraphSymbolKind::Label, "Person") => {
                Some(GraphSymbol::Label(fgdb_delta_types::LabelId(1)))
            }
            (GraphSymbolKind::Property, "name") => {
                Some(GraphSymbol::Property(fgdb_delta_types::PropertyKeyId(1)))
            }
            (GraphSymbolKind::Property, "age") => {
                Some(GraphSymbol::Property(fgdb_delta_types::PropertyKeyId(2)))
            }
            _ => None,
        }
    }

    fn root_arguments() -> GqlParameters {
        use crate::algebra::GraphValue;
        let rows = [100, 200]
            .into_iter()
            .map(|age| {
                GraphValue::map(vec![
                    (
                        "age".into(),
                        GraphValue::Scalar(fgdb_types::CanonicalScalar::Int(age)),
                    ),
                    (
                        "friends".into(),
                        GraphValue::List(
                            vec![GraphValue::Scalar(
                                fgdb_types::CanonicalScalar::ucs_basic_text("Bob").unwrap(),
                            )]
                            .into_boxed_slice(),
                        ),
                    ),
                    (
                        "name".into(),
                        GraphValue::Scalar(
                            fgdb_types::CanonicalScalar::ucs_basic_text("Ada").unwrap(),
                        ),
                    ),
                ])
                .unwrap()
            })
            .collect();
        GqlParameters::new().with_list("rows", rows).unwrap()
    }

    #[test]
    fn completed_bag_binding_keeps_global_owner_and_explicit_batch_controls() {
        use crate::algebra::GraphValue;

        for carry in ["p,row", "row,p"] {
            let text = format!(
                "UNWIND $rows AS row MATCH (p:Person {{name:row.name}}) \
                WITH {carry} UNWIND row.friends AS friend MATCH (f:Person {{name:friend}}) \
                SET p.name='Renamed',f.age=row.age"
            );
            let arguments = root_arguments();
            let original_arguments = arguments.canonical_bytes();
            assert!(
                GraphUnwindWriteText::parse_if_supported(&text)
                    .unwrap()
                    .is_none(),
                "{text}"
            );
            let script = BoundNativeGraphWrite::prepare_script(
                &text,
                &arguments,
                RelationId(1),
                root_symbols,
            )
            .unwrap();
            // Keep the unequal duplicate roots in one completed relation.
            // Execution owners check their atomic conflict; binding must not
            // turn them into separately staged statements.
            let bound = BoundNativeGraphWrite::bind(&text, &arguments, RelationId(1), root_symbols)
                .unwrap();
            assert_eq!(bound.input_records(), None);
            assert_eq!(bound.program().statements().len(), 1);
            assert!(!bound.program().freezes_field_update_input());
            let expected_column = usize::from(carry == "p,row");
            for statement in bound.program().statements() {
                assert!(matches!(statement, crate::GraphWriteStatement::Mutation(_)));
                if let crate::GraphWriteStatement::Mutation(mutation) = statement {
                    assert_eq!(
                        mutation.input_relation().unwrap().columns()[expected_column],
                        "row"
                    );
                    assert!(!mutation.writes_only_constants());
                }
            }
            // Equal repeated values and distinct friend selectors retain the
            // same global owner. This unit checks binding, not graph effects.
            let crate::GqlParameterValue::List(rows) = arguments.get("rows").unwrap() else {
                panic!("fixture rows are a list");
            };
            let equal = GqlParameters::new()
                .with_list("rows", vec![rows.values()[0].clone(); 2])
                .unwrap();
            let mut distinct_rows = rows.values().to_vec();
            let (names, values) = distinct_rows[1].as_map().unwrap();
            let second = GraphValue::map(
                names
                    .iter()
                    .cloned()
                    .zip(values.iter().cloned())
                    .map(|(name, value)| {
                        let value = if name.as_ref() == "friends" {
                            GraphValue::List(
                                vec![GraphValue::Scalar(
                                    fgdb_types::CanonicalScalar::ucs_basic_text("Cy").unwrap(),
                                )]
                                .into_boxed_slice(),
                            )
                        } else {
                            value
                        };
                        (name, value)
                    })
                    .collect(),
            )
            .unwrap();
            distinct_rows[1] = second;
            let distinct = GqlParameters::new()
                .with_list("rows", distinct_rows)
                .unwrap();
            for values in [&equal, &distinct] {
                let frozen = values.canonical_bytes();
                let bound = BoundNativeGraphWrite::bind(&text, values, RelationId(1), root_symbols)
                    .unwrap();
                assert_eq!(bound.input_records(), None);
                assert_eq!(bound.program().statements().len(), 1);
                assert!(!bound.program().freezes_field_update_input());
                assert_eq!(values.canonical_bytes(), frozen);
            }

            // Two explicit parameter records are two statements, regardless
            // of how many UNWIND rows each record contains.
            let inputs = [arguments.clone(), arguments.clone()];
            let mut events = Vec::new();
            let batch = script
                .bind_parameter_sets_controlled(&inputs, 2, |at| {
                    events.push(at);
                    Ok::<_, usize>(())
                })
                .unwrap();
            assert_eq!(events, [None, Some(0), Some(1), None]);
            assert_eq!(batch.argument_sets(), 2);
            assert_eq!(batch.program().statements().len(), 2);
            assert!(!batch.program().freezes_field_update_input());
            let mut reached = 0;
            assert!(matches!(
                script.bind_parameter_sets_controlled(&inputs, 1, |_| {
                    reached += 1;
                    Ok::<_, usize>(())
                }),
                Err(crate::GraphWriteScriptBatchBindError::Binding(
                    crate::GraphWriteScriptBatchError::TooManyStatements {
                        limit: 1,
                        observed: 2
                    }
                ))
            ));
            assert_eq!(
                reached, 0,
                "cap admission precedes allocation and callbacks"
            );
            let three = [arguments.clone(), arguments.clone(), arguments.clone()];
            assert!(matches!(
                script.bind_parameter_sets_controlled(&three, 2, |_| {
                    reached += 1;
                    Ok::<_, usize>(())
                }),
                Err(crate::GraphWriteScriptBatchBindError::Binding(
                    crate::GraphWriteScriptBatchError::TooManyStatements {
                        limit: 2,
                        observed: 3
                    }
                ))
            ));
            assert_eq!(reached, 0);
            for stop in 0..events.len() {
                let mut visited = Vec::new();
                let result = script.bind_parameter_sets_controlled(&inputs, 2, |at| {
                    visited.push(at);
                    if visited.len() == stop + 1 {
                        Err(stop)
                    } else {
                        Ok(())
                    }
                });
                assert!(matches!(result,
                    Err(crate::GraphWriteScriptBatchBindError::Interrupted {
                        argument_set, source,
                    }) if argument_set == events[stop] && source == stop));
                assert_eq!(visited, events[..=stop]);
                assert_eq!(
                    script.bind_parameter_sets(&inputs).unwrap().argument_sets(),
                    2
                );
            }
            let extra = arguments.clone().with_int64("unused", 1).unwrap();
            assert!(
                BoundNativeGraphWrite::bind(&text, &extra, RelationId(1), root_symbols).is_err()
            );
            let late = [arguments.clone(), extra];
            let mut late_events = Vec::new();
            assert!(matches!(
                script.bind_parameter_sets_controlled(&late, 2, |at| {
                    late_events.push(at);
                    Ok::<_, ()>(())
                }),
                Err(crate::GraphWriteScriptBatchBindError::Binding(
                    crate::GraphWriteScriptBatchError::Arguments {
                        argument_set: 1,
                        ..
                    }
                ))
            ));
            assert_eq!(late_events, [None, Some(0), Some(1)]);
            let empty = GqlParameters::new().with_list("rows", vec![]).unwrap();
            let frozen_empty = empty.canonical_bytes();
            let bound =
                BoundNativeGraphWrite::bind(&text, &empty, RelationId(1), root_symbols).unwrap();
            assert_eq!(bound.input_records(), None);
            assert_eq!(bound.program().statements().len(), 1);
            assert!(!bound.program().freezes_field_update_input());
            assert_eq!(empty.canonical_bytes(), frozen_empty);
            let empty_extra = empty.with_int64("unused", 1).unwrap();
            assert!(
                BoundNativeGraphWrite::bind(&text, &empty_extra, RelationId(1), root_symbols)
                    .is_err()
            );
            let elements = GqlParameters::new()
                .with_list(
                    "rows",
                    vec![
                        crate::algebra::GraphValue::Vertex(fgdb_types::VId(1)),
                        crate::algebra::GraphValue::map(vec![(
                            "age".into(),
                            crate::algebra::GraphValue::Vertex(fgdb_types::VId(2)),
                        )])
                        .unwrap(),
                    ],
                )
                .unwrap();
            let frozen_elements = elements.canonical_bytes();
            let bound =
                BoundNativeGraphWrite::bind(&text, &elements, RelationId(1), root_symbols).unwrap();
            assert_eq!(bound.input_records(), None);
            assert_eq!(bound.program().statements().len(), 1);
            assert!(!bound.program().freezes_field_update_input());
            assert_eq!(elements.canonical_bytes(), frozen_elements);
            let mut missing = GqlParameters::new();
            missing
                .insert("unused", crate::GqlParameterValue::Int64(1))
                .unwrap();
            assert!(
                BoundNativeGraphWrite::bind(&text, &missing, RelationId(1), root_symbols).is_err()
            );

            // A single target column does not change completed-bag semantics.
            // Keep both unequal duplicate single-target forms, together with
            // the original literal and graph-reading RHS owner controls.
            for assignment in [
                "f.age=row.age",
                "p.age=row.age",
                "f.age=100",
                "f.age=f.age+row.age",
            ] {
                let other = text.replace("p.name='Renamed',f.age=row.age", assignment);
                let bound =
                    BoundNativeGraphWrite::bind(&other, &arguments, RelationId(1), root_symbols)
                        .unwrap();
                assert!(
                    GraphUnwindWriteText::parse_if_supported(&other)
                        .unwrap()
                        .is_none(),
                    "{other}"
                );
                assert_eq!(bound.input_records(), None, "{other}");
                assert_eq!(bound.program().statements().len(), 1, "{other}");
                assert!(!bound.program().freezes_field_update_input(), "{other}");
            }
            assert_eq!(arguments.canonical_bytes(), original_arguments);
        }
    }

    #[test]
    fn completed_bag_scopes_retain_the_scalar_adapter_boundary() {
        let arguments = root_arguments();
        let original_arguments = arguments.canonical_bytes();
        for prefix in [
            "WITH p,row AS kept UNWIND kept.friends AS friend",
            "WITH p,row.name AS row UNWIND row.friends AS friend",
            "WITH p UNWIND [1] AS friend",
            "WITH DISTINCT p,row UNWIND row.friends AS friend",
            "WITH p,row ORDER BY p UNWIND row.friends AS friend",
            "WITH p,row LIMIT 1 UNWIND row.friends AS friend",
            "WITH row,count(*) AS p UNWIND row.friends AS friend",
            "WITH p,row CALL { RETURN 1 AS x } UNWIND row.friends AS friend",
            "WITH p,row UNWIND ROW.friends AS friend",
        ] {
            let text = format!(
                "UNWIND $rows AS row MATCH (p:Person {{name:row.name}}) \
                {prefix} MATCH (f:Person {{name:friend}}) SET f.age=row.age"
            );
            assert!(
                GraphUnwindWriteText::parse_if_supported(&text)
                    .unwrap()
                    .is_none(),
                "{text}"
            );
        }
        for suffix in [
            " RETURN f",
            "; MATCH (n) SET n.age=1",
            " SET f.name='x'",
            ",f.name=$other",
        ] {
            let text = format!(
                "UNWIND $rows AS row MATCH (p) WITH p,row \
                UNWIND row.friends AS friend MATCH (f) SET f.age=row.age{suffix}"
            );
            assert!(
                GraphUnwindWriteText::parse_if_supported(&text)
                    .unwrap()
                    .is_none(),
                "{text}"
            );
        }
        for prefix in [
            "WITH DISTINCT p,row UNWIND row.friends AS friend",
            "WITH p,row ORDER BY p UNWIND row.friends AS friend",
            "WITH p,row LIMIT 1 UNWIND row.friends AS friend",
        ] {
            let text = format!(
                "UNWIND $rows AS row MATCH (p:Person {{name:row.name}}) \
                {prefix} MATCH (f:Person {{name:friend}}) SET f.age=row.age"
            );
            let bound = BoundNativeGraphWrite::bind(&text, &arguments, RelationId(1), root_symbols)
                .unwrap();
            assert_eq!(bound.input_records(), None, "{text}");
            assert_eq!(bound.program().statements().len(), 1, "{text}");
            assert!(!bound.program().freezes_field_update_input(), "{text}");
        }
        // These original suffixes are terminal no-RETURN/parameter binding
        // refusals, rather than alternate row-expansion opportunities.
        for suffix in [" RETURN f", ",f.name=$other"] {
            let text = format!(
                "UNWIND $rows AS row MATCH (p) WITH p,row \
                UNWIND row.friends AS friend MATCH (f) SET f.age=row.age{suffix}"
            );
            assert!(
                BoundNativeGraphWrite::bind(&text, &arguments, RelationId(1), root_symbols)
                    .is_err(),
                "{text}"
            );
        }
        let text = "UNWIND $rows AS row MATCH (p) WITH p,row \
            UNWIND row.friends AS friend MATCH (f) SET f.age=row.age; \
            MATCH (n) SET n.age=1";
        let bound =
            BoundNativeGraphWrite::bind(text, &arguments, RelationId(1), root_symbols).unwrap();
        assert_eq!(bound.input_records(), None);
        assert_eq!(bound.program().statements().len(), 2);
        assert!(!bound.program().freezes_field_update_input());
        assert_eq!(arguments.canonical_bytes(), original_arguments);
    }

    #[test]
    fn definition_probe_and_binder_share_ordinary_script_preparation() {
        let arguments = GqlParameters::new();
        let text = "CREATE (n); CREATE (m)";
        BoundNativeGraphWrite::probe_definition(text, &arguments, RelationId(1), |_, _| None)
            .unwrap();
        let bound =
            BoundNativeGraphWrite::bind(text, &arguments, RelationId(1), |_, _| None).unwrap();
        assert_eq!(bound.program().statements().len(), 2);
        assert_eq!(bound.input_records(), None);
    }

    #[test]
    fn definition_probe_does_not_bind_or_expand_unwind_rows() {
        let text = "UNWIND $rows AS row MERGE (n:Item {key: row.key})";
        for arguments in [
            GqlParameters::new(),
            GqlParameters::new().with_list("rows", Vec::new()).unwrap(),
        ] {
            BoundNativeGraphWrite::probe_definition(text, &arguments, RelationId(1), |_, _| {
                panic!("lexical UNWIND probing must not resolve row-dependent symbols")
            })
            .unwrap();
            // A successful probe is not a bound program: missing and empty
            // sources must still fail the ordinary all-rows binding boundary.
            assert!(matches!(
                BoundNativeGraphWrite::bind(text, &arguments, RelationId(1), |_, _| None),
                Err(NativeGraphWriteBindError::Unwind(_))
            ));
        }
    }

    #[test]
    fn definition_probe_refuses_reads_returning_writes_and_broken_scripts() {
        let arguments = GqlParameters::new();
        for text in ["RETURN 1", "CREATE (n) RETURN n", "CREATE ("] {
            assert!(
                BoundNativeGraphWrite::probe_definition(text, &arguments, RelationId(1), |_, _| {
                    None
                },)
                .is_err(),
                "{text}",
            );
        }
    }
}

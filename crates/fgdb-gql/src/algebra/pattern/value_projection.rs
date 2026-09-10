//! Value-column preparation and terminal multiplicity selection over the
//! existing connected-pattern compiler.

mod existence;

use super::super::{GraphColumn, GraphValueRow, ValueProjection};
use super::*;

impl<Row> PreparedGraphPattern<Row> {
    /// Preserve every complete matching occurrence instead of deduplicating
    /// projected rows. This applies to identity, binding and property outputs.
    /// Ordering still precedes occurrence-based offset/count pagination.
    /// Changing multiplicity changes logical bytes; the column schema and
    /// source/traversal plan are unchanged. Repeated calls are idempotent.
    #[must_use]
    pub fn with_duplicates(mut self) -> Self {
        if let Some(at) = self.logical.operators.len().checked_sub(3)
            && matches!(self.logical.operators.get(at), Some(GlaOperator::Distinct))
        {
            self.logical.operators.remove(at);
        }
        self
    }

    #[must_use]
    pub fn preserves_duplicates(&self) -> bool {
        !matches!(
            self.logical.operators.iter().rev().nth(2),
            Some(GlaOperator::Distinct)
        )
    }
}

impl GraphPatternBuilder {
    /// Project correlated vertex identities and canonical vertex properties.
    /// Aliases are unique column names; one vertex/property may appear under
    /// several different aliases. Missing properties become canonical nulls.
    /// Complete value rows are deduplicated and ordered before pagination
    /// unless the prepared result explicitly selects `with_duplicates()`.
    pub fn prepare_values(
        &self,
        columns: &[GraphColumn<'_>],
        offset: u64,
        count: Option<u64>,
    ) -> Result<PreparedGraphPattern<GraphValueRow>, PatternBuildError> {
        let variables = self.checked_value_columns(columns)?;
        let (mut operators, slots) = self.compile()?;
        let projection = columns
            .iter()
            .zip(variables)
            .map(|(column, variable)| {
                let slot = slots[variable];
                match column {
                    GraphColumn::Vertex { .. } => ValueProjection::Vertex { slot },
                    GraphColumn::Property { key, .. } => {
                        ValueProjection::Property { slot, key: *key }
                    }
                }
            })
            .collect();
        operators.extend([
            GlaOperator::ProjectValues {
                columns: projection,
            },
            GlaOperator::Distinct,
            GlaOperator::OrderByValues,
            GlaOperator::Limit { offset, count },
        ]);
        Ok(PreparedGraphPattern {
            logical: GlaPlan::from_operators(operators),
            variable_count: self.variables.len(),
            edge_count: self.edges.len(),
            columns: columns
                .iter()
                .map(|column| column.name().to_owned())
                .collect(),
        })
    }
}

impl PreparedGraphPattern<GraphValueRow> {
    /// Ordered expression schema. Property keys and slots are explicit exports;
    /// Debug output remains redacted. Aliases are available through columns().
    #[must_use]
    pub fn value_columns(&self) -> &[ValueProjection] {
        self.logical
            .operators()
            .iter()
            .find_map(|operator| match operator {
                GlaOperator::ProjectValues { columns } => Some(columns.as_slice()),
                _ => None,
            })
            .expect("the private value-pattern constructor owns its projection")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GqlQueryError, GqlQueryPolicy};
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_types::{CanonicalF64, CanonicalScalar};

    fn builder() -> GraphPatternBuilder {
        let mut b = GraphPatternBuilder::new();
        b.vertex("a").unwrap();
        b.vertex("b").unwrap();
        b.edge("a", RelationId(1), GlaDirection::Forward, "b")
            .unwrap();
        b
    }

    #[test]
    fn property_schema_is_checked_and_canonical_identity_preserves_expressions() {
        let b = builder();
        let key = PropertyKeyId(7);
        let columns = [
            GraphColumn::vertex("owner", "a"),
            GraphColumn::property("score", "b", key),
        ];
        let prepared = b.prepare_values(&columns, 0, Some(3)).unwrap();
        assert_eq!(
            prepared.columns(),
            &["owner".to_owned(), "score".to_owned()]
        );
        assert!(prepared.plan().projects_properties());
        assert!(prepared.plan().needs_vertex_values());
        assert_eq!(prepared.value_columns().len(), 2);
        assert!(
            matches!(prepared.value_columns()[1], ValueProjection::Property { key: actual, .. } if actual == key)
        );
        assert_eq!(
            b.prepare_values(&[], 0, None).unwrap_err(),
            PatternBuildError::EmptyProjection
        );
        assert_eq!(
            b.prepare_values(&[columns[0], columns[0]], 0, None)
                .unwrap_err(),
            PatternBuildError::DuplicateProjection
        );
        assert_eq!(
            b.prepare_values(&[GraphColumn::vertex("bad alias", "a")], 0, None)
                .unwrap_err(),
            PatternBuildError::InvalidColumnName
        );
        assert_eq!(
            b.prepare_values(&[GraphColumn::vertex("valid", "missing")], 0, None)
                .unwrap_err(),
            PatternBuildError::UnknownVariable
        );
        assert!(matches!(
            b.prepare_values(&[columns[0]; MAX_PATTERN_VERTICES + 1], 0, None),
            Err(PatternBuildError::LimitExceeded {
                dimension: PatternLimitDimension::Columns,
                ..
            })
        ));
        let renamed = b
            .prepare_values(
                &[
                    GraphColumn::vertex("x", "a"),
                    GraphColumn::property("y", "b", key),
                ],
                0,
                Some(3),
            )
            .unwrap();
        assert_eq!(prepared.canonical_bytes(), renamed.canonical_bytes());
        let changed = b
            .prepare_values(
                &[
                    columns[0],
                    GraphColumn::property("score", "b", PropertyKeyId(8)),
                ],
                0,
                Some(3),
            )
            .unwrap();
        assert_ne!(prepared.canonical_bytes(), changed.canonical_bytes());
        assert_ne!(
            prepared.canonical_bytes(),
            b.prepare_values(&[columns[1], columns[0]], 0, Some(3))
                .unwrap()
                .canonical_bytes()
        );
        assert_eq!(b.prepare_values(&columns, 0, Some(3)).unwrap(), prepared);
        assert!(!format!("{prepared:?} {columns:?}").contains("score"));
    }

    #[test]
    fn property_projection_uses_value_distinctness_order_and_shared_limits() {
        let b = builder();
        let key = PropertyKeyId(7);
        let columns = [GraphColumn::property("value", "b", key)];
        let pattern = b.prepare_values(&columns, 1, Some(3)).unwrap();
        let values = [
            CanonicalScalar::Int(9),
            CanonicalScalar::Int(-3),
            CanonicalScalar::Int(9),
            CanonicalScalar::Null,
            CanonicalScalar::Bool(true),
            CanonicalScalar::Float(CanonicalF64::new(9.0)),
        ];
        let edges: Vec<_> = (0..values.len())
            .map(|at| (VId(100), RelationId(1), VId(at as u128)))
            .collect();
        let run = |policy| {
            pattern.plan().execute_governed_with_properties(
                edges.len() as u64,
                [],
                edges.iter().copied(),
                |_, _| Ok::<_, &str>(true),
                |vid, property| Ok((property == key).then(|| &values[vid.0 as usize])),
                policy,
                || Ok::<_, usize>(()),
            )
        };
        let wide = run(GqlQueryPolicy::new(6, 3, u64::MAX, u64::MAX)).unwrap();
        assert_eq!(
            wide.value
                .iter()
                .map(|r| r.get(0).unwrap().as_scalar().unwrap())
                .collect::<Vec<_>>(),
            vec![&values[4], &values[1], &values[0]]
        );
        let exact = GqlQueryPolicy::new(
            6,
            3,
            wide.evaluator.work_units,
            wide.evaluator.scratch_entries,
        );
        assert_eq!(run(exact).unwrap(), wide);
        assert!(matches!(
            run(GqlQueryPolicy::new(6, 2, u64::MAX, u64::MAX)),
            Err(GqlQueryError::Rows(_))
        ));
        assert!(matches!(
            run(GqlQueryPolicy::new(
                6,
                3,
                exact.evaluator.max_work_units - 1,
                u64::MAX
            )),
            Err(GqlQueryError::Evaluator(_))
        ));
        assert!(matches!(
            run(GqlQueryPolicy::new(
                6,
                3,
                u64::MAX,
                exact.evaluator.max_scratch_entries - 1
            )),
            Err(GqlQueryError::Evaluator(_))
        ));
        let result = pattern.plan().execute_governed_with_properties(
            6,
            [],
            edges.iter().copied(),
            |_, _| Ok::<_, &str>(true),
            |vid, _| {
                if vid == VId(5) {
                    Err("late projected property")
                } else {
                    Ok(Some(&values[vid.0 as usize]))
                }
            },
            GqlQueryPolicy::new(6, 0, u64::MAX, u64::MAX),
            || Ok::<_, usize>(()),
        );
        assert!(matches!(
            result,
            Err(GqlQueryError::Source("late projected property"))
        ));
    }

    #[test]
    fn property_only_node_scan_keeps_nulls_and_cancellation_at_every_event() {
        let mut b = GraphPatternBuilder::new();
        b.vertex("n").unwrap();
        let pattern = b
            .prepare_values(
                &[GraphColumn::property("p", "n", PropertyKeyId(1))],
                0,
                None,
            )
            .unwrap();
        assert!(!pattern.plan().scans_edges());
        assert_eq!(pattern.required_vertex_label(), None);
        let scalar = CanonicalScalar::ucs_basic_text("value").unwrap();
        let mut calls = 0;
        let policy = GqlQueryPolicy::new(3, 2, u64::MAX, u64::MAX);
        let expected = pattern
            .plan()
            .execute_governed_with_properties(
                3,
                [VId(1), VId(2), VId(3)],
                [],
                |_, _| Ok::<_, ()>(true),
                |vid, _| Ok((vid == VId(2)).then_some(&scalar)),
                policy,
                || {
                    calls += 1;
                    Ok::<_, usize>(())
                },
            )
            .unwrap();
        assert_eq!(expected.value.len(), 2);
        assert!(expected.value[0].get(0).unwrap().is_null());
        for stop in 1..=calls {
            let mut at = 0;
            let result = pattern.plan().execute_governed_with_properties(
                3,
                [VId(1), VId(2), VId(3)],
                [],
                |_, _| Ok::<_, ()>(true),
                |vid, _| Ok((vid == VId(2)).then_some(&scalar)),
                policy,
                || {
                    at += 1;
                    if at == stop { Err(stop) } else { Ok(()) }
                },
            );
            assert!(matches!(result, Err(GqlQueryError::Interrupted(value)) if value == stop));
            assert_eq!(at, stop);
        }
    }

    #[test]
    fn value_bags_keep_equal_null_and_typed_numeric_occurrences_in_canonical_order() {
        let b = builder();
        let key = PropertyKeyId(7);
        let values = [
            CanonicalScalar::Int(9),
            CanonicalScalar::Null,
            CanonicalScalar::Int(9),
            CanonicalScalar::Float(CanonicalF64::new(9.0)),
        ];
        let distinct = b
            .prepare_values(&[GraphColumn::property("value", "b", key)], 0, None)
            .unwrap();
        let bag = distinct.clone().with_duplicates();
        assert!(bag.preserves_duplicates());
        assert!(!distinct.preserves_duplicates());
        assert_eq!(bag.clone().with_duplicates(), bag);
        assert_eq!(bag.columns(), distinct.columns());
        assert_ne!(bag.canonical_bytes(), distinct.canonical_bytes());
        let edges = [
            (VId(9), RelationId(1), VId(0)),
            (VId(9), RelationId(1), VId(0)),
            (VId(9), RelationId(1), VId(1)),
            (VId(9), RelationId(1), VId(2)),
            (VId(9), RelationId(1), VId(3)),
        ];
        let run = |pattern: &PreparedGraphPattern<GraphValueRow>, policy| {
            pattern.plan().execute_governed_with_properties(
                5,
                [],
                edges,
                |_, _| Ok::<_, ()>(true),
                |vid, _| Ok(Some(&values[vid.0 as usize])),
                policy,
                || Ok::<_, ()>(()),
            )
        };
        let all = run(&bag, GqlQueryPolicy::new(5, 5, u64::MAX, u64::MAX)).unwrap();
        assert_eq!(
            all.value
                .iter()
                .map(|row| row.get(0).unwrap().as_scalar().unwrap())
                .collect::<Vec<_>>(),
            vec![&values[1], &values[0], &values[0], &values[0], &values[3]]
        );
        assert_eq!(
            run(&distinct, GqlQueryPolicy::new(5, 3, u64::MAX, u64::MAX))
                .unwrap()
                .value
                .len(),
            3
        );
        assert!(
            matches!(run(&bag, GqlQueryPolicy::new(5, 2, u64::MAX, u64::MAX)),
            Err(GqlQueryError::Rows(error)) if error.observed == 3)
        );
        let exact = GqlQueryPolicy::new(
            5,
            5,
            all.evaluator.work_units,
            all.evaluator.scratch_entries,
        );
        assert_eq!(run(&bag, exact).unwrap(), all);
        let page = b
            .prepare_values(&[GraphColumn::property("value", "b", key)], 2, Some(2))
            .unwrap()
            .with_duplicates();
        let page = run(&page, GqlQueryPolicy::new(5, 2, u64::MAX, u64::MAX)).unwrap();
        assert_eq!(page.value.len(), 2);
        assert_eq!(page.value[0], page.value[1]);
        assert_eq!(page.value[0].get(0).unwrap().as_scalar(), Some(&values[0]));
    }
}

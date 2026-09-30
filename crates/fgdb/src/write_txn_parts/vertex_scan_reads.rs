// Membership witnesses for an admitted single-vertex selection prefix.
// Native predicates and Boolean evaluation own value/NULL/error semantics;
// these observations never replace the query's source or execution path.

struct VertexScanRead {
    // Only Select/SelectBoolean operators. Boolean programs share their checked
    // immutable encoding; the transaction never owns an evaluator callback.
    selections: Vec<fgdb_gql::algebra::GlaOperator>,
    // Only the originating call can complete this entry, AFTER execution and
    // all final quota checks. Refusal/unwind leaves a conservative vertex-domain
    // witness. A later successful call never completes an earlier failed read.
    complete: bool,
}

// A data exception is not FALSE: it may change a future query into a refusal.
// Controls must propagate instead of being mistaken for a possible match. The
// ordinary query remains responsible for reporting the original data exception.
enum VertexScanEvaluation<E> {
    Control(E),
    Data,
}

impl<E> From<fgdb_gql::GraphIntegerError> for VertexScanEvaluation<E> {
    fn from(_: fgdb_gql::GraphIntegerError) -> Self {
        Self::Data
    }
}

impl VertexScanRead {
    /// Borrow the checked selection prefix without copying its literals. A
    /// Boolean program may use OR/NOT, null tests, local property comparisons
    /// and scalar expressions, but every vertex operand must name slot zero.
    /// Joins, OPTIONAL, EXISTS, edge captures and additional roots stay broad.
    /// The sealed compiler already checks the terminal projection's shape.
    fn predicates<Row: fgdb_gql::algebra::GlaOutput>(
        logical: &fgdb_gql::algebra::GlaPlan<Row>,
    ) -> Option<&[fgdb_gql::algebra::GlaOperator]> {
        use fgdb_gql::algebra::GlaOperator;
        let [GlaOperator::ScanVertices, body @ ..] = logical.operators() else {
            return None;
        };
        let count = body
            .iter()
            .take_while(|operator| {
                matches!(operator, GlaOperator::Select { .. } | GlaOperator::SelectBoolean { .. })
            })
            .count();
        if count == 0 {
            return None;
        }
        let (selections, tail) = body.split_at(count);
        for selection in selections {
            match selection {
                GlaOperator::Select { slot, .. } if slot.ordinal() == 0 => {}
                GlaOperator::SelectBoolean { expression }
                    if expression.supports_vertex_bindings(1) => {}
                _ => return None,
            }
        }
        let terminal = tail.iter().all(|operator| match operator {
            GlaOperator::Project { slot } => slot.ordinal() == 0,
            GlaOperator::ProjectBindings { slots } => {
                slots.iter().all(|slot| slot.ordinal() == 0)
            }
            GlaOperator::ProjectValues { .. }
            | GlaOperator::Distinct
            | GlaOperator::OrderByVertexId
            | GlaOperator::OrderByBindings
            | GlaOperator::OrderByValues
            | GlaOperator::OrderByValueColumns { .. }
            | GlaOperator::Limit { .. } => true,
            _ => false,
        });
        terminal.then_some(selections)
    }

    /// Charge every retained operator and each separately copied predicate.
    /// BoundBooleanExpression::clone shares its immutable Arc program; it does
    /// not copy operand payloads or allocate a new instruction vector.
    fn charge_definition<E>(
        selections: &[fgdb_gql::algebra::GlaOperator],
        control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        use crate::gql_exec::source::SourceEvent;
        control(SourceEvent::ScratchEntry)?;
        for selection in selections {
            control(SourceEvent::ScratchEntry)?;
            if let fgdb_gql::algebra::GlaOperator::Select { predicates, .. } = selection {
                for _ in predicates {
                    control(SourceEvent::ScratchEntry)?;
                }
            }
        }
        Ok(())
    }

    fn target(row: &fgdb_delta_types::DeltaRow) -> Option<VId> {
        use fgdb_delta_types::DeltaRow;
        match row {
            DeltaRow::CreateVertex { vid, .. }
            | DeltaRow::DeleteVertex { vid, .. }
            | DeltaRow::LabelMembership { vid, .. }
            | DeltaRow::Property {
                elem: ElementId::Vertex(vid),
                ..
            } => Some(*vid),
            _ => None,
        }
    }

    fn relevant(
        &self,
        row: &fgdb_delta_types::DeltaRow,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<bool, WriteTxnError> {
        use fgdb_delta_types::DeltaRow;
        use fgdb_gql::algebra::{GlaOperator, VertexPredicate};
        if matches!(row, DeltaRow::CreateVertex { .. } | DeltaRow::DeleteVertex { .. }) {
            return Ok(true);
        }
        for selection in &self.selections {
            checkpoint()?;
            match selection {
                GlaOperator::Select { predicates, .. } => {
                    for predicate in predicates {
                        checkpoint()?;
                        let relevant = match row {
                            DeltaRow::Property { property, .. } => {
                                predicate.property_key() == Some(*property)
                            }
                            DeltaRow::LabelMembership { label, .. } => {
                                matches!(predicate, VertexPredicate::HasLabel(required) if required == label)
                            }
                            _ => true,
                        };
                        if relevant {
                            return Ok(true);
                        }
                    }
                }
                GlaOperator::SelectBoolean { expression } => {
                    // Include EVERY eager property input, not just the leaves
                    // that presently decide OR/AND or appear in the projection.
                    if let DeltaRow::Property { property, .. } = row {
                        for (_, key) in expression.referenced_vertex_properties() {
                            checkpoint()?;
                            if key == *property {
                                return Ok(true);
                            }
                        }
                    }
                }
                _ => return Ok(true),
            }
        }
        Ok(false)
    }

    /// Classify a basis/historical row for dependencies, not query output.
    /// FALSE/UNKNOWN need no full-row witness; data/profile refusals keep one.
    /// All actual query results and exceptions still come from native GLA.
    /// Staged identities are observed broadly by the source, independently.
    fn may_match<E>(
        selections: &[fgdb_gql::algebra::GlaOperator],
        row: &VertexRow,
        control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
    ) -> Result<bool, E> {
        use crate::gql_exec::source::SourceEvent;
        use fgdb_gql::algebra::{GlaOperator, VertexPredicate};
        let property = |key| {
            row.props
                .binary_search_by_key(&key, |(key, _)| *key)
                .ok()
                .map(|at| &row.props[at].1)
        };
        for selection in selections {
            control(SourceEvent::Work)?;
            match selection {
                GlaOperator::Select { predicates, .. } => {
                    for predicate in predicates {
                        control(SourceEvent::Work)?;
                        let label = match predicate {
                            VertexPredicate::HasLabel(label) => {
                                row.labels.binary_search(label).is_ok().then_some(*label)
                            }
                            _ => None,
                        };
                        let value = predicate
                            .property_key()
                            .and_then(|key| property(key).map(|value| (key, value)));
                        if !predicate.matches_borrowed(label, value) {
                            return Ok(false);
                        }
                    }
                }
                GlaOperator::SelectBoolean { expression } => {
                    let result = expression.evaluate_vertex_binding(
                        &[Some(row.vid)],
                        &mut |_, key| Ok::<_, VertexScanEvaluation<E>>(property(key)),
                        &mut |event| {
                            let event = match event {
                                fgdb_gql::GlaExecutionEvent::ScratchEntry => SourceEvent::ScratchEntry,
                                _ => SourceEvent::Work,
                            };
                            control(event).map_err(VertexScanEvaluation::Control)
                        },
                    );
                    match result {
                        Ok(Some(false)) => return Ok(false),
                        Ok(Some(true)) => {}
                        Ok(None) | Err(VertexScanEvaluation::Data) => return Ok(true),
                        Err(VertexScanEvaluation::Control(error)) => return Err(error),
                    }
                }
                _ => return Ok(true),
            }
        }
        Ok(true)
    }

    fn matches(
        &self,
        row: Option<&VertexRow>,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<bool, WriteTxnError> {
        match row {
            Some(row) => Self::may_match(&self.selections, row, &mut |_| checkpoint()),
            None => Ok(false),
        }
    }

    /// Preserve the label-domain witness of an incomplete scan. Only primitive
    /// selections BEFORE the first fallible Boolean program can bound the rows
    /// it may have examined: a later label must not hide an earlier data error.
    /// Without a leading label constraint, every existing vertex is in scope.
    fn matches_labels(&self, row: Option<&VertexRow>) -> bool {
        use fgdb_gql::algebra::{GlaOperator, VertexPredicate};
        let Some(row) = row else {
            return false;
        };
        for selection in &self.selections {
            let GlaOperator::Select { predicates, .. } = selection else {
                break;
            };
            for predicate in predicates {
                if let VertexPredicate::HasLabel(label) = predicate
                    && row.labels.binary_search(label).is_err()
                {
                    return false;
                }
            }
        }
        true
    }
}

impl PointReads {
    fn begin_vertex_scan(
        &mut self,
        predicates: &[fgdb_gql::algebra::GlaOperator],
    ) -> usize {
        let index = self.2.len();
        self.2.push(VertexScanRead {
            selections: predicates.to_vec(),
            complete: false,
        });
        index
    }

    fn complete_vertex_scan(&mut self, index: usize) {
        // The private source keeps the transaction immutably borrowed. Neither
        // refresh nor terminal cleanup can clear/reorder entries in its lifetime.
        self.2[index].complete = true;
    }

    fn vertex_scan_conflict<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        row: &fgdb_delta_types::DeltaRow,
        at: CommitSeq,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<Option<ElementId>, WriteTxnError> {
        if self.2.is_empty() {
            return Ok(None);
        }
        let Some(vid) = VertexScanRead::target(row) else {
            return Ok(None);
        };
        let mut images = None;
        for scan in &self.2 {
            checkpoint()?;
            // A refused or unwound scan keeps its label-domain witness, as the
            // pre-precise label phantom did; any change in that domain
            // conflicts, but a vertex outside it does not (fgdb-xwh00).
            if scan.complete && !scan.relevant(row, checkpoint)? {
                continue;
            }
            if images.is_none() {
                // Validation iterates the complete retained suffix. Its commit
                // sequences are positive; inspect both sides of EACH commit,
                // not just the current head (which would miss enter-then-leave).
                let before = crate::gql_exec::source::find_vertex(
                    &database.snapshot.patches,
                    vid,
                    CommitSeq(at.0 - 1),
                    &mut |_| checkpoint(),
                )?;
                let after = crate::gql_exec::source::find_vertex(
                    &database.snapshot.patches,
                    vid,
                    at,
                    &mut |_| checkpoint(),
                )?;
                images = Some((before, after));
            }
            let (before, after) = images.expect("historical images selected above");
            let hit = if scan.complete {
                scan.matches(before, checkpoint)? || scan.matches(after, checkpoint)?
            } else {
                scan.matches_labels(before) || scan.matches_labels(after)
            };
            if hit {
                return Ok(Some(ElementId::Vertex(vid)));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod vertex_scan_read_tests {
    include!("vertex_scan_read_tests.rs");
    include!("vertex_scan_boolean_tests.rs");
}

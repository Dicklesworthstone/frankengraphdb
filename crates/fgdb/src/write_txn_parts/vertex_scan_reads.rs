// Exact membership witnesses for the admitted single-vertex conjunction profile.
// The GQL predicate kernel and native historical row source remain the semantic
// owners. This records dependencies; it is not another matcher or SSI engine.

struct VertexScanRead {
    predicates: Vec<fgdb_gql::algebra::VertexPredicate>,
    // Only the originating call can complete this entry, AFTER execution and
    // all final quota checks. Refusal/unwind leaves a conservative vertex-domain
    // witness. A later successful call never completes an earlier failed read.
    complete: bool,
}

impl VertexScanRead {
    /// Only a single root followed by its conjunctive selection and terminal
    /// projection/order/page operators. Joins, OPTIONAL, EXISTS, Boolean data
    /// expressions, additional roots and unknown operators retain broad reads.
    /// The sealed compiler already checks the projection's binding shape.
    fn predicates<Row: fgdb_gql::algebra::GlaOutput>(
        logical: &fgdb_gql::algebra::GlaPlan<Row>,
    ) -> Option<&[fgdb_gql::algebra::VertexPredicate]> {
        use fgdb_gql::algebra::GlaOperator;
        let [
            GlaOperator::ScanVertices,
            GlaOperator::Select { slot, predicates },
            tail @ ..,
        ] = logical.operators()
        else {
            return None;
        };
        if slot.ordinal() != 0 || predicates.is_empty() {
            return None;
        }
        let terminal = tail.iter().all(|operator| match operator {
            GlaOperator::Project { slot } => slot.ordinal() == 0,
            GlaOperator::ProjectBindings { slots } => slots.iter().all(|slot| slot.ordinal() == 0),
            GlaOperator::ProjectValues { .. }
            | GlaOperator::Distinct
            | GlaOperator::OrderByVertexId
            | GlaOperator::OrderByBindings
            | GlaOperator::OrderByValues
            | GlaOperator::OrderByValueColumns { .. }
            | GlaOperator::Limit { .. } => true,
            _ => false,
        });
        terminal.then_some(predicates.as_slice())
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
        use fgdb_gql::algebra::VertexPredicate;
        for predicate in &self.predicates {
            checkpoint()?;
            let relevant = match row {
                DeltaRow::Property { property, .. } => predicate.property_key() == Some(*property),
                DeltaRow::LabelMembership { label, .. } => {
                    matches!(predicate, VertexPredicate::HasLabel(required) if required == label)
                }
                // Creation/retirement changes existence for every predicate.
                _ => true,
            };
            if relevant {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn matches(
        &self,
        row: Option<&VertexRow>,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<bool, WriteTxnError> {
        use fgdb_gql::algebra::VertexPredicate;
        let Some(row) = row else {
            return Ok(false);
        };
        for predicate in &self.predicates {
            checkpoint()?;
            let label = match predicate {
                VertexPredicate::HasLabel(label) => {
                    row.labels.binary_search(label).is_ok().then_some(*label)
                }
                _ => None,
            };
            let property = predicate.property_key().and_then(|key| {
                row.props
                    .binary_search_by_key(&key, |(key, _)| *key)
                    .ok()
                    .map(|at| (key, &row.props[at].1))
            });
            // Includes exact scalar kinds, registered collation and NULL laws.
            // Never compare encoded bytes or coerce values in the validator.
            if !predicate.matches_borrowed(label, property) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

impl PointReads {
    fn begin_vertex_scan(&mut self, predicates: &[fgdb_gql::algebra::VertexPredicate]) -> usize {
        let index = self.2.len();
        self.2.push(VertexScanRead {
            predicates: predicates.to_vec(),
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
            if !scan.complete {
                return Ok(Some(ElementId::Vertex(vid)));
            }
            if !scan.relevant(row, checkpoint)? {
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
            if scan.matches(before, checkpoint)? || scan.matches(after, checkpoint)? {
                return Ok(Some(ElementId::Vertex(vid)));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod vertex_scan_read_tests {
    include!("vertex_scan_read_tests.rs");
}

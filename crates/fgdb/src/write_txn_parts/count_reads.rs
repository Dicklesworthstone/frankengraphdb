// Exact count inputs read population membership, including absent members.
// The ordinary FCW validator must retain both arrivals AND departures without
// requiring a point witness for every member of that population (plan §7.3).

#[derive(Default)]
struct CountReads {
    vertices: bool,
    edges: bool,
    labels: std::collections::BTreeSet<LabelId>,
    relations: std::collections::BTreeSet<RelationId>,
}

impl CountReads {
    fn is_empty(&self) -> bool {
        !self.vertices && !self.edges && self.labels.is_empty() && self.relations.is_empty()
    }

    fn contains(&self, source: fgdb_gql::GraphCountSource) -> bool {
        use fgdb_gql::GraphCountSource;
        match source {
            GraphCountSource::Vertices => self.vertices,
            GraphCountSource::Edges => self.edges,
            GraphCountSource::Label(label) => self.labels.contains(&label),
            GraphCountSource::Relation(relation) => self.relations.contains(&relation),
        }
    }

    fn insert(&mut self, source: fgdb_gql::GraphCountSource) {
        use fgdb_gql::GraphCountSource;
        match source {
            GraphCountSource::Vertices => self.vertices = true,
            GraphCountSource::Edges => self.edges = true,
            GraphCountSource::Label(label) => {
                self.labels.insert(label);
            }
            GraphCountSource::Relation(relation) => {
                self.relations.insert(relation);
            }
        }
    }

    fn conflict<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        row: &fgdb_delta_types::DeltaRow,
        at: CommitSeq,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<Option<ElementId>, WriteTxnError> {
        use fgdb_delta_types::DeltaRow;
        if self.is_empty() {
            return Ok(None);
        }
        match row {
            DeltaRow::CreateVertex { vid, labels, .. } => {
                if self.vertices {
                    return Ok(Some(ElementId::Vertex(*vid)));
                }
                for label in labels {
                    checkpoint()?;
                    if self.labels.contains(label) {
                        return Ok(Some(ElementId::Vertex(*vid)));
                    }
                }
            }
            DeltaRow::LabelMembership {
                vid,
                label,
                before,
                after,
            } if before != after && self.labels.contains(label) => {
                return Ok(Some(ElementId::Vertex(*vid)));
            }
            DeltaRow::CreateEdge { eid, relation, .. }
                if self.edges || self.relations.contains(relation) =>
            {
                return Ok(Some(ElementId::Edge(*eid)));
            }
            DeltaRow::DeleteEdge { eid, .. } => {
                if self.retired_edge_conflicts(database, *eid, at, checkpoint)? {
                    return Ok(Some(ElementId::Edge(*eid)));
                }
            }
            DeltaRow::DeleteVertex {
                vid,
                sorted_retired_incident_edges,
                ..
            } => {
                if self.vertices {
                    return Ok(Some(ElementId::Vertex(*vid)));
                }
                if !self.labels.is_empty() {
                    // Check EACH commit's before-image: a vertex may enter
                    // and leave a label between this basis and today's head.
                    let before = crate::gql_exec::source::find_vertex(
                        &database.snapshot,
                        *vid,
                        CommitSeq(at.0 - 1),
                        &mut |_| checkpoint(),
                    )?;
                    let Some(before) = before else {
                        // An unavailable image cannot prove non-overlap.
                        return Ok(Some(ElementId::Vertex(*vid)));
                    };
                    for label in &before.labels {
                        checkpoint()?;
                        if self.labels.contains(label) {
                            return Ok(Some(ElementId::Vertex(*vid)));
                        }
                    }
                }
                for eid in sorted_retired_incident_edges {
                    checkpoint()?;
                    if self.retired_edge_conflicts(database, *eid, at, checkpoint)? {
                        return Ok(Some(ElementId::Edge(*eid)));
                    }
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn retired_edge_conflicts<V: Vfs + Clone>(
        &self,
        database: &Database<V>,
        eid: EId,
        at: CommitSeq,
        checkpoint: &mut impl FnMut() -> Result<(), WriteTxnError>,
    ) -> Result<bool, WriteTxnError> {
        if self.edges {
            return Ok(true);
        }
        if self.relations.is_empty() {
            return Ok(false);
        }
        let snapshot = &database.snapshot;
        let coordinate = snapshot.adjacency_index().statement_at_controlled(
            &snapshot.blocks,
            eid,
            CommitSeq(at.0 - 1),
            &mut |_| checkpoint(),
        )?;
        // A DeleteEdge's coordinate need not be its actual relation, and a
        // vertex cascade may cross many relations. Read immutable incidence
        // from the deletion's own before-cut, without copying any properties.
        Ok(coordinate.is_none_or(|(block, row)| {
            self.relations
                .contains(&snapshot.blocks[block][row].relation)
        }))
    }
}

impl WriteTxn {
    fn note_count_read<E>(
        &self,
        source: fgdb_gql::GraphCountSource,
        control: &mut impl FnMut(crate::gql_exec::source::SourceEvent) -> Result<(), E>,
    ) -> Result<(), E> {
        use crate::gql_exec::source::SourceEvent;
        if !self.count_reads.borrow().contains(source) {
            // Admission precedes allocation and the first metadata read. No
            // callback runs while the transaction's witness owner is borrowed.
            if matches!(
                source,
                fgdb_gql::GraphCountSource::Label(_) | fgdb_gql::GraphCountSource::Relation(_)
            ) {
                control(SourceEvent::ScratchEntry)?;
            }
            self.count_reads.borrow_mut().insert(source);
        }
        Ok(())
    }
}

#[cfg(test)]
mod count_basis_tests {
    use super::*;
    use crate::DatabaseKeys;
    use asupersync::lab::run_async_under_lab;
    use fgdb_delta_types::PropertyKeyId;
    use fgdb_gql::{GqlParameters, GqlQueryPolicy, GraphCountSource, PreparedGraphAggregateText};
    use fgdb_types::{DatabaseSecurityNamespaceId, PurposeContexts};

    #[test]
    fn every_private_rebase_acceptance_moves_counts_with_its_basis() {
        let ((), report) = run_async_under_lab(0xc071_1101, |root| async move {
            let contexts = PurposeContexts::narrow_runtime_root(&root);
            let commit = contexts.commit();
            let txcx = contexts.txn();
            let query = contexts.query();
            let aggregate =
                PreparedGraphAggregateText::prepare("MATCH (n) RETURN count(*) AS n", |_, _| None)
                    .unwrap()
                    .bind_parameters(&GqlParameters::new())
                    .unwrap();
            for mode in 0..4 {
                let keys = DatabaseKeys::new(
                    [0xc1; 32],
                    DatabaseSecurityNamespaceId([0xc2; 32]),
                    [0xc3; 32],
                );
                let mut database = Database::open_memory(&commit, keys).await.unwrap();
                let mut initial = WriteBatch::new(RelationId(1));
                for id in 1..=3 {
                    initial.create_vertex(VId(id), vec![], vec![]);
                }
                database.write(&commit, initial).await.unwrap();
                let mut transaction = database.begin(&txcx).unwrap();
                let mut staged = WriteBatch::new(RelationId(1));
                match mode {
                    0 => {
                        staged.add_edge(EId(50), VId(1), VId(2), vec![]);
                    }
                    1 => {
                        staged.set_vertex_property(
                            VId(1),
                            PropertyKeyId(1),
                            Some(CanonicalScalar::Int(5)),
                        );
                    }
                    2 => {
                        staged.create_vertex(VId(5), vec![], vec![]);
                        staged.set_vertex_property(
                            VId(1),
                            PropertyKeyId(1),
                            Some(CanonicalScalar::Int(5)),
                        );
                    }
                    _ => {
                        staged.ensure_vertex(VId(5), vec![], vec![]);
                    }
                }
                transaction.write(&mut database, staged).unwrap();
                assert_eq!(
                    transaction
                        .basis_counts
                        .lookup(GraphCountSource::Vertices, &mut |_| Ok::<_, ()>(()))
                        .unwrap(),
                    Some(3),
                );
                let mut concurrent = WriteBatch::new(RelationId(1));
                concurrent.create_vertex(VId(9), vec![], vec![]);
                database.write(&commit, concurrent).await.unwrap();
                // Public rebase methods are terminal. Inspect the accepted
                // private stage before that terminal guard publishes it, with
                // no prior query observation for the concurrent write to break.
                assert!(
                    transaction
                        .transaction_conflict_in(&database, ConflictScope::Reads, &mut || Ok(()))
                        .unwrap()
                        .is_none()
                );
                let merge = commit.merge_eval();
                match mode {
                    0 => transaction
                        .prepare_append_rebase(&mut database, 128, &merge, &mut || Ok(())),
                    1 => {
                        transaction.prepare_field_rebase(&mut database, 128, &merge, &mut || Ok(()))
                    }
                    2 => {
                        transaction.prepare_mixed_rebase(&mut database, 128, &merge, &mut || Ok(()))
                    }
                    _ => transaction.prepare_idempotent_rebase(
                        &mut database,
                        128,
                        &merge,
                        &mut || Ok(()),
                    ),
                }
                .unwrap();
                assert_eq!(transaction.basis(), database.frontier().unwrap());
                assert_eq!(
                    transaction
                        .basis_counts
                        .lookup(GraphCountSource::Vertices, &mut |_| Ok::<_, ()>(()))
                        .unwrap(),
                    Some(4),
                );
                let result = transaction
                    .execute_graph_aggregate_governed(
                        &database,
                        &query,
                        &aggregate,
                        GqlQueryPolicy::new(16, 8, 10_000, 1_000),
                    )
                    .unwrap();
                assert_eq!(
                    result.value[0].get(0).unwrap().as_count(),
                    Some(if mode < 2 { 4 } else { 5 }),
                );
                transaction.abort();
                assert_eq!(txcx.outstanding_obligations(), 0);
            }
        });
        assert!(report.lab_test_passed(), "{report:?}");
    }
}

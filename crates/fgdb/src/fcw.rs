//! First-committer-wins validation for the live `WriteBatch` write set.

use fgdb_chronicle::{CommitDraft, CommitValidator, ValidationRejection};
use fgdb_delta_types::{
    DeltaRow, ElementId, IndexError, LocalDeltaBatchIndex, LogicalDeltaTemplate,
};
use fgdb_types::{CommitSeq, VId};
use std::collections::{BTreeMap, BTreeSet};

const FCW_LAW: &str = "FG-LAW-FCW-01";

/// First-committer-wins over one explicit snapshot basis.
///
/// Default starts with no intervening commits. A caller committing an older
/// prepared write must use `from_history`, not a resettable validator left over
/// from some unrelated write. The constructor refuses incomplete retained cuts.
#[derive(Default)]
pub struct FirstCommitterWinsValidator {
    last_writer: BTreeMap<ElementId, CommitSeq>,
    adjacency_insertions: BTreeMap<VId, CommitSeq>,
    dependencies: BTreeSet<ElementId>,
    adjacency_dependencies: BTreeSet<VId>,
    scalar_resolver: Option<std::sync::Arc<dyn fgdb_types::CanonicalScalarResolver + Send + Sync>>,
}

impl core::fmt::Debug for FirstCommitterWinsValidator {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FirstCommitterWinsValidator")
            .field("last_writer", &self.last_writer)
            .field("adjacency_insertions", &self.adjacency_insertions)
            .field("dependencies", &self.dependencies)
            .field("adjacency_dependencies", &self.adjacency_dependencies)
            .finish_non_exhaustive()
    }
}

impl FirstCommitterWinsValidator {
    #[must_use]
    pub fn with_scalar_resolver(
        mut self,
        resolver: Option<std::sync::Arc<dyn fgdb_types::CanonicalScalarResolver + Send + Sync>>,
    ) -> Self {
        self.scalar_resolver = resolver;
        self
    }

    /// Reconstruct conflict state from the complete committed suffix after
    /// `basis`. Writes at or before that snapshot are deliberately excluded.
    /// A future basis or a retired prefix propagates the exact index error.
    pub fn from_history(
        basis: CommitSeq,
        history: &LocalDeltaBatchIndex,
    ) -> Result<Self, IndexError> {
        let mut validator = Self::default();
        for batch in history.since(basis)? {
            for coordinate in batch.coordinate_entries() {
                for row in &coordinate.rows {
                    validator.observe(row, batch.commit_seq());
                }
            }
        }
        Ok(validator)
    }

    /// Add compact dependencies observed during preparation, including guards
    /// that canonicalization reduced to no-ops and existing ensure aliases.
    /// Adjacency dependencies witness insertions, including previously absent
    /// triples. Existing incident edge identities witness updates/deletions.
    #[must_use]
    pub fn with_dependencies(
        mut self,
        elements: impl IntoIterator<Item = ElementId>,
        adjacency: impl IntoIterator<Item = VId>,
    ) -> Self {
        self.dependencies.extend(elements);
        self.adjacency_dependencies.extend(adjacency);
        self
    }

    fn observe(&mut self, row: &DeltaRow, at: CommitSeq) {
        let mut touched = BTreeSet::new();
        touched_elements(row, &mut touched);
        for element in touched {
            self.last_writer.insert(element, at);
        }
        if let DeltaRow::CreateEdge { src, dst, .. } = row {
            self.adjacency_insertions.insert(*src, at);
            self.adjacency_insertions.insert(*dst, at);
        }
    }
}

impl CommitValidator for FirstCommitterWinsValidator {
    fn validate(&mut self, draft: &CommitDraft<'_>) -> Result<(), ValidationRejection> {
        let decoded = match self.scalar_resolver.as_deref() {
            Some(resolver) => LogicalDeltaTemplate::decode_canonical_with_resolver(
                draft.capsule_plaintext,
                resolver,
            ),
            None => LogicalDeltaTemplate::decode_canonical(draft.capsule_plaintext),
        };
        let template = decoded.map_err(|error| ValidationRejection {
            law: FCW_LAW,
            detail: format!("malformed logical delta template: {error:?}"),
        })?;

        let mut touched = BTreeSet::new();
        let mut required_vertices = BTreeSet::new();
        let mut deleted_vertices = BTreeSet::new();
        for coordinate in template.coordinate_entries() {
            for row in &coordinate.rows {
                touched_elements(row, &mut touched);
                match row {
                    DeltaRow::CreateEdge { src, dst, .. } => {
                        required_vertices.insert(ElementId::Vertex(*src));
                        required_vertices.insert(ElementId::Vertex(*dst));
                    }
                    DeltaRow::DeleteVertex { vid, .. } => {
                        deleted_vertices.insert(*vid);
                    }
                    _ => {}
                }
            }
        }

        if let Some((element, previous_seq)) = touched
            .iter()
            .chain(required_vertices.iter())
            .chain(self.dependencies.iter())
            .find_map(|element| self.last_writer.get(element).map(|seq| (element, seq)))
        {
            return Err(ValidationRejection {
                law: FCW_LAW,
                detail: format!(
                    "prepared dependency {element:?} changed at {previous_seq:?}; draft {:?} loses first-committer-wins",
                    draft.commit_seq
                ),
            });
        }
        if let Some((vertex, previous_seq)) = deleted_vertices
            .iter()
            .chain(self.adjacency_dependencies.iter())
            .find_map(|vertex| {
                self.adjacency_insertions
                    .get(vertex)
                    .map(|seq| (vertex, seq))
            })
        {
            return Err(ValidationRejection {
                law: FCW_LAW,
                detail: format!(
                    "prepared adjacency at {vertex:?} gained an edge at {previous_seq:?}; draft {:?} must be prepared again",
                    draft.commit_seq
                ),
            });
        }

        // A rejection above cannot partially install any write or adjacency.
        for coordinate in template.coordinate_entries() {
            for row in &coordinate.rows {
                self.observe(row, draft.commit_seq);
            }
        }
        Ok(())
    }
}

/// Element-level conflicts include every canonical family that mutates an
/// element, including families not yet exposed by the `WriteBatch` surface.
/// A counter's algebra profile does not authorize a merge through this FCW
/// validator; such a merge needs its own validated lane.
fn touched_elements(row: &DeltaRow, touched: &mut BTreeSet<ElementId>) {
    match row {
        DeltaRow::CreateVertex { vid, .. } => {
            touched.insert(ElementId::Vertex(*vid));
        }
        DeltaRow::CreateEdge { eid, .. } => {
            touched.insert(ElementId::Edge(*eid));
        }
        DeltaRow::DeleteVertex {
            vid,
            sorted_retired_incident_edges,
            ..
        } => {
            touched.insert(ElementId::Vertex(*vid));
            for eid in sorted_retired_incident_edges {
                touched.insert(ElementId::Edge(*eid));
            }
        }
        DeltaRow::DeleteEdge { eid, .. } => {
            touched.insert(ElementId::Edge(*eid));
        }
        DeltaRow::LabelMembership { vid, .. } => {
            touched.insert(ElementId::Vertex(*vid));
        }
        DeltaRow::Property { elem, .. }
        | DeltaRow::ValidTime { elem, .. }
        | DeltaRow::Counter { elem, .. } => {
            touched.insert(*elem);
        }
        DeltaRow::Escrow { subject, .. } => {
            touched.insert(*subject);
        }
        // Non-element state has separate validation requirements. Keep this
        // exhaustive so a new row family cannot silently escape FCW review.
        DeltaRow::Sketch { .. } | DeltaRow::Schema { .. } | DeltaRow::Constraint { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_chronicle::{CommitMarker, EffectSource};
    use fgdb_crypto::Digest;
    use fgdb_delta_types::{
        CoordinateEntry, EscrowDomainId, LabelId, OperationKey, PropertyKeyId, RelationId,
        SchemaEpoch, ValidTimePeriod,
    };
    use fgdb_types::{BranchId, CanonicalScalar, EId, GraphId, ObjectId};

    fn rows_template(rows: Vec<DeltaRow>) -> Vec<u8> {
        LogicalDeltaTemplate::build(
            ObjectId([0x11; 32]),
            [0x22; 32],
            vec![CoordinateEntry {
                graph: GraphId(1),
                branch: BranchId(1),
                relation: RelationId(1),
                schema_epoch: SchemaEpoch(0),
                schema_transition: None,
                rows,
            }],
        )
        .expect("test template is canonical")
        .canonical_bytes()
        .expect("test template encodes")
    }

    fn template(vertices: &[u128]) -> Vec<u8> {
        rows_template(
            vertices
                .iter()
                .map(|vertex| DeltaRow::LabelMembership {
                    vid: VId(*vertex),
                    label: LabelId(1),
                    before: false,
                    after: true,
                })
                .collect(),
        )
    }

    fn edge(eid: u128, src: u128, dst: u128) -> DeltaRow {
        DeltaRow::CreateEdge {
            eid: EId(eid),
            birth_ordinal: 1,
            src: VId(src),
            relation: RelationId(1),
            dst: VId(dst),
            canonical_key: None,
            props: vec![],
            valid_time: None,
        }
    }

    fn marker(seq: u64) -> CommitMarker {
        CommitMarker {
            logical_command_seq: seq,
            commit_seq: seq,
            effect_source: EffectSource::Local {
                capsule_ref: ObjectId([0x31; 32]),
                logical_delta_template_digest: Digest([0x32; 32]),
            },
            prev_global: None,
            head_updates: Vec::new(),
            merge_record_oid: None,
            coordinate_schema_transition_digest: Digest([0x33; 32]),
            topology_epoch: 1,
            policy_epoch: 1,
            revocation_index: 1,
            txn_token: [0x34; 16],
            commit_hlc: seq,
            final_effect_digest: Digest([0x35; 32]),
            authorization_decision_digest: Digest([0x36; 32]),
            resource_effect_digest: Digest([0x37; 32]),
            payload_availability_certificate_oid: None,
            flags: 0,
        }
    }

    fn validate(
        validator: &mut FirstCommitterWinsValidator,
        plaintext: &[u8],
        seq: u64,
    ) -> Result<(), ValidationRejection> {
        let marker = marker(seq);
        validator.validate(&CommitDraft {
            commit_seq: CommitSeq(seq),
            capsule_oid: ObjectId([0x41; 32]),
            capsule_plaintext: plaintext,
            marker: &marker,
        })
    }

    #[test]
    fn overlapping_templates_reject_the_second_writer() {
        let mut validator = FirstCommitterWinsValidator::default();
        assert_eq!(validate(&mut validator, &template(&[7]), 1), Ok(()));
        let rejection = validate(&mut validator, &template(&[7]), 2)
            .expect_err("second writer to one element must lose");
        assert_eq!(rejection.law, FCW_LAW);
    }

    #[test]
    fn disjoint_templates_are_accepted() {
        let mut validator = FirstCommitterWinsValidator::default();
        assert_eq!(validate(&mut validator, &template(&[7]), 1), Ok(()));
        assert_eq!(validate(&mut validator, &template(&[8]), 2), Ok(()));
    }

    #[test]
    fn rejected_overlap_does_not_mutate_last_writer() {
        let mut validator = FirstCommitterWinsValidator::default();
        assert_eq!(validate(&mut validator, &template(&[7]), 1), Ok(()));
        validate(&mut validator, &template(&[7, 8]), 2)
            .expect_err("overlap rejects the whole draft");
        assert_eq!(
            validator.last_writer.get(&ElementId::Vertex(VId(7))),
            Some(&CommitSeq(1))
        );
        assert_eq!(
            validator.last_writer.get(&ElementId::Vertex(VId(8))),
            None,
            "a rejected draft must not partially install disjoint keys"
        );
    }

    #[test]
    fn history_constructor_refuses_a_future_basis_instead_of_empty_state() {
        let history = LocalDeltaBatchIndex::new();
        assert!(FirstCommitterWinsValidator::from_history(CommitSeq(0), &history).is_ok());
        assert!(matches!(
            FirstCommitterWinsValidator::from_history(CommitSeq(1), &history),
            Err(IndexError::BeyondFrontier {
                asked: CommitSeq(1),
                frontier: CommitSeq(0)
            })
        ));
    }

    #[test]
    fn retained_dependency_conflicts_even_when_absent_from_the_template() {
        let mut validator = FirstCommitterWinsValidator::default()
            .with_dependencies([ElementId::Vertex(VId(7))], []);
        validator.observe(
            &DeltaRow::LabelMembership {
                vid: VId(7),
                label: LabelId(1),
                before: false,
                after: true,
            },
            CommitSeq(2),
        );
        let before = validator.last_writer.clone();
        assert!(validate(&mut validator, &template(&[9]), 3).is_err());
        assert_eq!(validator.last_writer, before);
    }

    #[test]
    fn an_edge_requires_unchanged_endpoints_but_parallel_inserts_do_not_conflict() {
        let mut validator = FirstCommitterWinsValidator::default();
        assert!(validate(&mut validator, &rows_template(vec![edge(10, 1, 2)]), 1).is_ok());
        assert!(validate(&mut validator, &rows_template(vec![edge(11, 1, 2)]), 2).is_ok());
        validator.observe(
            &DeltaRow::DeleteVertex {
                vid: VId(2),
                before_version: ObjectId([1; 32]),
                sorted_retired_incident_edges: vec![EId(10), EId(11)],
            },
            CommitSeq(3),
        );
        assert!(validate(&mut validator, &rows_template(vec![edge(12, 1, 2)]), 4).is_err());
        assert!(
            !validator
                .last_writer
                .contains_key(&ElementId::Edge(EId(12)))
        );
    }

    #[test]
    fn cascade_and_absent_ensure_witness_incoming_insertions() {
        let mut validator = FirstCommitterWinsValidator::default();
        validator.observe(&edge(10, 2, 1), CommitSeq(2));
        assert!(
            validate(
                &mut validator,
                &rows_template(vec![DeltaRow::DeleteVertex {
                    vid: VId(1),
                    before_version: ObjectId([1; 32]),
                    sorted_retired_incident_edges: vec![],
                }]),
                3
            )
            .is_err()
        );
        let mut ensure = validator.with_dependencies([], [VId(1)]);
        assert!(validate(&mut ensure, &template(&[9]), 3).is_err());
    }

    fn property(elem: ElementId) -> DeltaRow {
        DeltaRow::Property {
            elem,
            property: PropertyKeyId(3),
            before: Some(CanonicalScalar::Int(10)),
            after: Some(CanonicalScalar::Int(15)),
        }
    }

    // Exercise actual canonical payloads through the production decoder, not
    // synthetic entries inserted straight into the conflict map.
    fn typed_element_updates(elem: ElementId) -> [DeltaRow; 4] {
        [
            DeltaRow::ValidTime {
                elem,
                contract_id: ObjectId([4; 32]),
                before: None,
                after: Some(ValidTimePeriod {
                    start_micros: 10,
                    end_micros: Some(20),
                }),
            },
            DeltaRow::Counter {
                operation_key: OperationKey([5; 32]),
                elem,
                property: PropertyKeyId(3),
                algebra_profile: ObjectId([6; 32]),
                delta: 5,
                before: 10,
                after: 15,
            },
            DeltaRow::Escrow {
                domain_id: EscrowDomainId(1),
                epoch: 1,
                operation_key: OperationKey([7; 32]),
                subject: elem,
                subject_property: Some(PropertyKeyId(3)),
                delta: -3,
                before_value: 10,
                after_value: 7,
            },
            DeltaRow::Escrow {
                domain_id: EscrowDomainId(1),
                epoch: 1,
                operation_key: OperationKey([8; 32]),
                subject: elem,
                subject_property: None,
                delta: -3,
                before_value: 10,
                after_value: 7,
            },
        ]
    }

    #[test]
    fn typed_element_updates_conflict_with_property_writes_in_both_orders() {
        for elem in [ElementId::Vertex(VId(7)), ElementId::Edge(EId(7))] {
            for row in typed_element_updates(elem) {
                for (first, second) in
                    [(row.clone(), property(elem)), (property(elem), row.clone())]
                {
                    let mut validator = FirstCommitterWinsValidator::default();
                    assert_eq!(
                        validate(&mut validator, &rows_template(vec![first]), 1),
                        Ok(())
                    );
                    let rejection = validate(&mut validator, &rows_template(vec![second]), 2)
                        .expect_err("all element-changing families must participate in FCW");
                    assert_eq!(rejection.law, FCW_LAW);
                    assert_eq!(validator.last_writer.get(&elem), Some(&CommitSeq(1)));
                }
            }
        }
    }

    #[test]
    fn typed_element_updates_conflict_with_each_other() {
        for elem in [ElementId::Vertex(VId(7)), ElementId::Edge(EId(7))] {
            for first in typed_element_updates(elem) {
                for second in typed_element_updates(elem) {
                    let mut validator = FirstCommitterWinsValidator::default();
                    assert_eq!(
                        validate(&mut validator, &rows_template(vec![first.clone()]), 1),
                        Ok(())
                    );
                    let rejection = validate(&mut validator, &rows_template(vec![second]), 2)
                        .expect_err("typed writes must participate in FCW");
                    assert_eq!(rejection.law, FCW_LAW);
                }
            }
        }
    }

    #[test]
    fn typed_element_updates_preserve_disjointness_and_element_kind() {
        for (left, right) in [
            (ElementId::Vertex(VId(7)), ElementId::Vertex(VId(8))),
            (ElementId::Edge(EId(7)), ElementId::Edge(EId(8))),
            (ElementId::Vertex(VId(7)), ElementId::Edge(EId(7))),
        ] {
            for first in typed_element_updates(left) {
                for second in typed_element_updates(right) {
                    let mut validator = FirstCommitterWinsValidator::default();
                    assert_eq!(
                        validate(&mut validator, &rows_template(vec![first.clone()]), 1),
                        Ok(())
                    );
                    assert_eq!(
                        validate(&mut validator, &rows_template(vec![second]), 2),
                        Ok(())
                    );
                    assert_eq!(validator.last_writer.get(&left), Some(&CommitSeq(1)));
                    assert_eq!(validator.last_writer.get(&right), Some(&CommitSeq(2)));
                }
            }
        }
    }

    #[test]
    fn typed_element_updates_invalidate_retained_read_dependencies() {
        for elem in [ElementId::Vertex(VId(7)), ElementId::Edge(EId(7))] {
            for row in typed_element_updates(elem) {
                let mut validator = FirstCommitterWinsValidator::default();
                assert_eq!(
                    validate(&mut validator, &rows_template(vec![row]), 1),
                    Ok(())
                );
                let mut validator = validator.with_dependencies([elem], []);
                let before = validator.last_writer.clone();
                let rejection = validate(&mut validator, &template(&[9]), 2)
                    .expect_err("retained reads must notice post-snapshot typed updates");
                assert_eq!(rejection.law, FCW_LAW);
                assert_eq!(validator.last_writer, before);
            }
        }
    }

    #[test]
    fn rejected_typed_updates_install_neither_disjoint_writes_nor_adjacency() {
        for elem in [ElementId::Vertex(VId(7)), ElementId::Edge(EId(7))] {
            for row in typed_element_updates(elem) {
                let mut validator = FirstCommitterWinsValidator::default();
                assert_eq!(
                    validate(&mut validator, &rows_template(vec![property(elem)]), 1),
                    Ok(())
                );
                let writers_before = validator.last_writer.clone();
                let adjacency_before = validator.adjacency_insertions.clone();
                let disjoint = property(ElementId::Vertex(VId(9)));
                let rejection = validate(
                    &mut validator,
                    &rows_template(vec![disjoint.clone(), edge(10, 1, 2), row]),
                    2,
                )
                .expect_err("the overlapping typed row rejects the complete draft");
                assert_eq!(rejection.law, FCW_LAW);
                assert_eq!(validator.last_writer, writers_before);
                assert_eq!(validator.adjacency_insertions, adjacency_before);
                assert_eq!(
                    validate(
                        &mut validator,
                        &rows_template(vec![disjoint, edge(10, 1, 2)]),
                        2
                    ),
                    Ok(()),
                    "a refused draft must not poison later disjoint validation"
                );
            }
        }
    }

    #[test]
    fn typed_vertex_updates_invalidate_edge_endpoint_dependencies() {
        for endpoint in [1, 2] {
            for row in typed_element_updates(ElementId::Vertex(VId(endpoint))) {
                let mut validator = FirstCommitterWinsValidator::default();
                assert_eq!(
                    validate(&mut validator, &rows_template(vec![row]), 1),
                    Ok(())
                );
                let writers_before = validator.last_writer.clone();
                let rejection = validate(&mut validator, &rows_template(vec![edge(10, 1, 2)]), 2)
                    .expect_err("edge endpoints must retain their prepared state");
                assert_eq!(rejection.law, FCW_LAW);
                assert_eq!(validator.last_writer, writers_before);
                assert!(validator.adjacency_insertions.is_empty());
            }
        }
    }
}

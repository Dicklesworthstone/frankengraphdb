//! First-committer-wins validation for the live `WriteBatch` write set.

use fgdb_chronicle::{CommitDraft, CommitValidator, ValidationRejection};
use fgdb_delta_types::{DeltaRow, ElementId, LogicalDeltaTemplate};
use fgdb_types::CommitSeq;
use std::collections::{BTreeMap, BTreeSet};

const FCW_LAW: &str = "FG-LAW-FCW-01";

/// Accept the first draft to touch an element and reject later overlapping
/// drafts.
///
/// This deliberately implements only the bounded live `WriteBatch` rule. It
/// has no snapshot-basis input, SSI graph, or merge-ladder behavior: with the
/// current [`CommitValidator`] seam, an already-recorded element is the exact
/// conflict predicate.
#[derive(Debug, Default)]
pub struct FirstCommitterWinsValidator {
    last_writer: BTreeMap<ElementId, CommitSeq>,
}

impl CommitValidator for FirstCommitterWinsValidator {
    fn validate(&mut self, draft: &CommitDraft<'_>) -> Result<(), ValidationRejection> {
        let template = LogicalDeltaTemplate::decode_canonical(draft.capsule_plaintext).map_err(
            |error| ValidationRejection {
                law: FCW_LAW,
                detail: format!("malformed logical delta template: {error:?}"),
            },
        )?;

        let mut touched = BTreeSet::new();
        for coordinate in template.coordinate_entries() {
            for row in &coordinate.rows {
                touched_elements(row, &mut touched);
            }
        }

        if let Some((element, previous_seq)) = touched
            .iter()
            .find_map(|element| self.last_writer.get(element).map(|seq| (element, seq)))
        {
            return Err(ValidationRejection {
                law: FCW_LAW,
                detail: format!(
                    "write-set element {element:?} was first committed at {previous_seq:?}; draft {:?} loses first-committer-wins",
                    draft.commit_seq
                ),
            });
        }

        for element in touched {
            self.last_writer.insert(element, draft.commit_seq);
        }
        Ok(())
    }
}

/// Keep this match identical to `touched_elements` in `lib.rs`: it is the
/// product write-set definition shared by version advancement and FCW.
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
        DeltaRow::Property { elem, .. } => {
            touched.insert(*elem);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fgdb_chronicle::{CommitMarker, EffectSource};
    use fgdb_crypto::Digest;
    use fgdb_delta_types::{CoordinateEntry, LabelId, RelationId, SchemaEpoch};
    use fgdb_types::{BranchId, GraphId, ObjectId, VId};

    fn template(vertices: &[u128]) -> Vec<u8> {
        let rows = vertices
            .iter()
            .map(|vertex| DeltaRow::LabelMembership {
                vid: VId(*vertex),
                label: LabelId(1),
                before: false,
                after: true,
            })
            .collect();
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
}

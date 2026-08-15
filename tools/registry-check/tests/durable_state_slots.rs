//! Mutation suite for the §5.1 durable-state-slot constitution (fgdb-96rj).

use registry_check::command_contracts::{ContractRegistry, load_from_repo as load_contracts};
use registry_check::durable_state_slots::{
    BackingRegistry, SlotRegistry, load_from_repo, parse_backing_registry, parse_slot_registry,
    validate,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn fixture() -> (
    SlotRegistry,
    BTreeMap<String, BackingRegistry>,
    ContractRegistry,
) {
    let root = repo_root();
    let (slots, backings) = load_from_repo(&root).expect("state-slot registries load");
    let contracts = load_contracts(&root).expect("command contracts load");
    (slots, backings, contracts)
}

fn codes(
    slots: &SlotRegistry,
    backings: &BTreeMap<String, BackingRegistry>,
    contracts: &ContractRegistry,
) -> BTreeSet<String> {
    validate(slots, backings, contracts)
        .into_iter()
        .map(|violation| violation.code)
        .collect()
}

#[test]
fn shipped_slot_and_backing_registries_are_exact_and_clean() {
    let (slots, backings, contracts) = fixture();
    let violations = validate(&slots, &backings, &contracts);
    assert!(
        violations.is_empty(),
        "unexpected violations: {violations:#?}"
    );

    assert_eq!(slots.slots.len(), 52, "the frozen reservation inventory");
    let mut plane_counts = BTreeMap::new();
    for slot in &slots.slots {
        *plane_counts.entry(slot.plane.as_str()).or_insert(0usize) += 1;
    }
    assert_eq!(plane_counts.get("SemanticPayload"), Some(&48));
    assert_eq!(plane_counts.get("Protocol"), Some(&1));
    assert_eq!(plane_counts.get("PreparedOwnership"), None);
    assert_eq!(plane_counts.get("Consensus"), Some(&1));
    assert_eq!(plane_counts.get("Bootstrap"), Some(&2));

    assert_eq!(backings["state_payload_fields.toml"].fields.len(), 48);
    assert_eq!(backings["protocol_state_fields.toml"].fields.len(), 1);
    assert!(backings["prepared_state_fields.toml"].fields.is_empty());
    assert_eq!(backings["consensus_state_fields.toml"].fields.len(), 1);

    let key_state = slots
        .slots
        .iter()
        .find(|slot| slot.plane == "SemanticPayload" && slot.slot_tag == "key_lifecycle_state")
        .expect("F13 key lifecycle slot");
    assert_eq!(key_state.role, "Local");
    assert_eq!(key_state.backing_registry, "state_payload_fields.toml");
    assert_eq!(key_state.status, "reserved");
    assert_eq!(
        key_state.transition_writer_contract_ids,
        [
            "cc:local:key-destroy-authorize-spec",
            "cc:local:key-destroy-certificate-publish-spec",
            "cc:local:key-destroy-finalize-spec",
        ]
    );

    let gc_state = slots
        .slots
        .iter()
        .find(|slot| slot.plane == "SemanticPayload" && slot.slot_tag == "gc_semantic_state")
        .expect("F14 semantic GC slot");
    assert_eq!(gc_state.role, "Local");
    assert_eq!(gc_state.backing_registry, "state_payload_fields.toml");
    assert_eq!(gc_state.status, "reserved");
    assert_eq!(
        gc_state.transition_writer_contract_ids,
        [
            "cc:local:gc-physical-disposition-import-spec:cancelled",
            "cc:local:gc-physical-disposition-import-spec:completed",
            "cc:local:local-gc-apply-quarantine-spec",
            "cc:local:local-gc-authorize-spec",
            "cc:local:local-gc-cancellation-authorize-spec",
        ]
    );

    let backup_state = slots
        .slots
        .iter()
        .find(|slot| slot.plane == "SemanticPayload" && slot.slot_tag == "backup_registry_root")
        .expect("F15A Local backup registry slot");
    assert_eq!(backup_state.role, "Local");
    assert_eq!(backup_state.backing_registry, "state_payload_fields.toml");
    assert_eq!(backup_state.stable_name, "backup_registry_root");
    assert_eq!(backup_state.status, "reserved");
    assert_eq!(
        backup_state.transition_writer_contract_ids,
        [
            "cc:local:archive-source-release-completion-import-spec",
            "cc:local:local-backup-abort-spec",
            "cc:local:local-backup-artifact-verify-spec",
            "cc:local:local-backup-barrier-spec",
            "cc:local:local-backup-closure-publish-spec",
            "cc:local:local-backup-grant-issue-import-spec",
            "cc:local:local-backup-publication-authorize-spec",
            "cc:local:local-backup-publication-receipt-import-spec",
            "cc:local:local-backup-release-spec",
            "cc:local:local-backup-seal-spec",
        ]
    );

    let restore_state = slots
        .slots
        .iter()
        .find(|slot| slot.plane == "SemanticPayload" && slot.slot_tag == "restore_registry_root")
        .expect("F15B Local restore registry slot");
    assert_eq!(restore_state.role, "Local");
    assert_eq!(restore_state.backing_registry, "state_payload_fields.toml");
    assert_eq!(restore_state.stable_name, "restore_registry_root");
    assert_eq!(restore_state.status, "reserved");
    assert_eq!(
        restore_state.transition_writer_contract_ids,
        [
            "cc:local:local-restore-abandon-finalize-spec",
            "cc:local:local-restore-abandonment-pin-install-spec",
            "cc:local:local-restore-activation-spec",
            "cc:local:local-restore-service-completion-spec",
            "cc:local:local-restore-service-prepare-spec",
            "cc:local:local-restore-service-promotion-spec",
            "cc:local:restore-abandon-spec:local",
        ]
    );
}

#[test]
fn parsers_reject_unknown_schema_keys() {
    let root = repo_root();
    let mut slots_text = fs::read_to_string(root.join("registries/durable_state_slots.toml"))
        .expect("slot registry source");
    slots_text.push_str("\nunknown_top_level = 1\n");
    let error = parse_slot_registry(&slots_text).expect_err("unknown top-level key must fail");
    assert!(error.contains("unknown key \"unknown_top_level\""));

    let mut backing_text = fs::read_to_string(root.join("registries/protocol_state_fields.toml"))
        .expect("backing registry source");
    backing_text.push_str("\nunknown_top_level = 1\n");
    let error = parse_backing_registry(&backing_text, "protocol_state_fields.toml")
        .expect_err("unknown backing key must fail");
    assert!(error.contains("unknown key \"unknown_top_level\""));
}

#[test]
fn command_reference_bijection_rejects_missing_and_extra_rows() {
    let (slots, backings, contracts) = fixture();

    let mut missing = slots.clone();
    missing
        .slots
        .retain(|slot| slot.slot_tag != "statement_index");
    assert!(codes(&missing, &backings, &contracts).contains("contract_slot_ref_missing_row"));

    let mut extra = slots.clone();
    let mut row = extra.slots[0].clone();
    row.slot_tag = "unreferenced_state".into();
    row.stable_name = row.slot_tag.clone();
    extra.slots.push(row);
    assert!(codes(&extra, &backings, &contracts).contains("slot_row_without_contract_ref"));
}

#[test]
fn writer_set_is_derived_not_advisory() {
    let (mut slots, backings, contracts) = fixture();
    let row = slots
        .slots
        .iter_mut()
        .find(|slot| slot.slot_tag == "logical_state_root")
        .expect("logical root row");
    row.transition_writer_contract_ids.clear();
    assert!(codes(&slots, &backings, &contracts).contains("slot_writer_set_mismatch"));
}

#[test]
fn active_rows_cannot_retain_reserved_sentinels_or_zero_writers() {
    let (mut slots, backings, contracts) = fixture();
    let row = slots
        .slots
        .iter_mut()
        .find(|slot| slot.slot_tag == "checkpoint_floor")
        .expect("checkpoint floor row");
    row.status = "active".into();
    let found = codes(&slots, &backings, &contracts);
    assert!(found.contains("slot_active_with_reserved_sentinel"));
    assert!(found.contains("slot_active_without_writer"));
}

#[test]
fn plane_backing_mapping_and_projection_are_fail_closed() {
    let (mut slots, mut backings, contracts) = fixture();
    let protocol = slots
        .slots
        .iter_mut()
        .find(|slot| slot.plane == "Protocol")
        .expect("protocol row");
    protocol.backing_registry = "state_payload_fields.toml".into();
    assert!(codes(&slots, &backings, &contracts).contains("slot_backing_registry_mismatch"));

    let (slots, shipped_backings, contracts) = fixture();
    backings = shipped_backings;
    backings
        .get_mut("protocol_state_fields.toml")
        .expect("protocol backing")
        .fields
        .clear();
    assert!(codes(&slots, &backings, &contracts).contains("slot_backing_field_missing"));

    let (slots, mut backings, contracts) = fixture();
    let field = backings["state_payload_fields.toml"].fields[0].clone();
    backings
        .get_mut("protocol_state_fields.toml")
        .expect("protocol backing")
        .fields
        .push(field);
    assert!(codes(&slots, &backings, &contracts).contains("slot_backing_field_extra"));
}

#[test]
fn cross_plane_status_drift_cannot_hide_in_a_projection() {
    let (slots, mut backings, contracts) = fixture();
    backings
        .get_mut("consensus_state_fields.toml")
        .expect("consensus backing")
        .fields[0]
        .status = "active".into();
    let found = codes(&slots, &backings, &contracts);
    assert!(found.contains("slot_backing_field_missing"));
    assert!(found.contains("slot_backing_field_extra"));
}

#[test]
fn missing_per_plane_registry_is_a_typed_violation() {
    let (slots, mut backings, contracts) = fixture();
    backings.remove("prepared_state_fields.toml");
    assert!(codes(&slots, &backings, &contracts).contains("slot_backing_registry_missing"));
}

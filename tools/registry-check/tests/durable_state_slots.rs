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

    assert_eq!(slots.slots.len(), 79, "the frozen reservation inventory");
    let mut plane_counts = BTreeMap::new();
    for slot in &slots.slots {
        *plane_counts.entry(slot.plane.as_str()).or_insert(0usize) += 1;
    }
    assert_eq!(plane_counts.get("SemanticPayload"), Some(&70));
    assert_eq!(plane_counts.get("Protocol"), Some(&2));
    assert_eq!(plane_counts.get("PreparedOwnership"), Some(&1));
    assert_eq!(plane_counts.get("Consensus"), Some(&2));
    assert_eq!(plane_counts.get("Bootstrap"), Some(&4));

    assert_eq!(backings["state_payload_fields.toml"].fields.len(), 70);
    assert_eq!(backings["protocol_state_fields.toml"].fields.len(), 2);
    assert_eq!(backings["prepared_state_fields.toml"].fields.len(), 1);
    assert_eq!(backings["consensus_state_fields.toml"].fields.len(), 2);

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
        .find(|slot| {
            slot.plane == "SemanticPayload"
                && slot.role == "Local"
                && slot.slot_tag == "gc_semantic_state"
        })
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

    let meta_gc_state = slots
        .slots
        .iter()
        .find(|slot| {
            slot.plane == "SemanticPayload"
                && slot.role == "Meta"
                && slot.slot_tag == "gc_semantic_state"
        })
        .expect("Meta F17A distributed-GC semantic state slot");
    assert_eq!(meta_gc_state.stable_name, "gc_semantic_state");
    assert_eq!(meta_gc_state.backing_registry, "state_payload_fields.toml");
    assert_eq!(meta_gc_state.status, "reserved");
    assert_eq!(
        meta_gc_state.transition_writer_contract_ids,
        [
            "cc:meta:gc-physical-disposition-import-spec:cancelled",
            "cc:meta:gc-physical-disposition-import-spec:completed",
            "cc:meta:global-gc-authorization-spec",
            "cc:meta:meta-gc-apply-quarantine-spec",
        ]
    );
    let projected_meta_gc_state = backings["state_payload_fields.toml"]
        .fields
        .iter()
        .filter(|field| {
            field.role == "Meta"
                && field.slot_tag == "gc_semantic_state"
                && field.status == "reserved"
        })
        .count();
    assert_eq!(
        projected_meta_gc_state, 1,
        "Meta F17A GC backing projection must be unique"
    );

    let retention_map = slots
        .slots
        .iter()
        .find(|slot| slot.plane == "SemanticPayload" && slot.slot_tag == "retention_map")
        .expect("retention map slot");
    assert_eq!(retention_map.role, "Local");
    assert_eq!(retention_map.backing_registry, "state_payload_fields.toml");
    assert_eq!(retention_map.status, "reserved");
    assert_eq!(
        retention_map.transition_writer_contract_ids,
        [
            "cc:local:branch-retire-finalize-spec",
            "cc:local:checkpoint-install-spec",
            "cc:local:history-cut-activation-spec",
            "cc:local:restore-source-lease-release-finalize-spec",
            "cc:local:restore-terminal-pin-release-finalize-spec",
        ]
    );
    let remote_retention = slots
        .slots
        .iter()
        .find(|slot| {
            slot.plane == "SemanticPayload" && slot.slot_tag == "remote_retention_obligation_root"
        })
        .expect("remote retention obligation slot");
    assert_eq!(
        remote_retention.transition_writer_contract_ids,
        [
            "cc:local:begin-role-transition-spec",
            "cc:local:remote-retention-control-spec:acquire-grant",
            "cc:local:remote-retention-control-spec:apply-authority-release",
            "cc:local:remote-retention-control-spec:publish-authority-release-ack",
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
        .expect("F15B-F15G Local restore registry slot");
    assert_eq!(restore_state.role, "Local");
    assert_eq!(restore_state.backing_registry, "state_payload_fields.toml");
    assert_eq!(restore_state.stable_name, "restore_registry_root");
    assert_eq!(restore_state.status, "reserved");
    assert_eq!(
        restore_state.transition_writer_contract_ids,
        [
            "cc:local:directory-bound-abandon-apply-spec",
            "cc:local:directory-bound-abandon-receipt-import-spec",
            "cc:local:directory-bound-enter-promotion-pending-spec",
            "cc:local:directory-bound-finalize-operational-authority-spec",
            "cc:local:local-restore-abandon-finalize-spec",
            "cc:local:local-restore-abandonment-pin-install-spec",
            "cc:local:local-restore-activation-spec",
            "cc:local:local-restore-service-completion-spec",
            "cc:local:local-restore-service-prepare-spec",
            "cc:local:local-restore-service-promotion-spec",
            "cc:local:restore-abandon-spec:local",
            "cc:local:restore-source-key-access-cleanup-authorize-spec",
            "cc:local:restore-source-key-access-cleanup-finalize-spec",
            "cc:local:restore-source-key-access-cleanup-import-spec",
            "cc:local:restore-source-lease-release-authorized-never-armed-finalize-spec",
            "cc:local:restore-source-lease-release-finalize-spec",
            "cc:local:restore-source-lease-release-no-effect-finalize-spec",
            "cc:local:restore-source-lease-release-spec",
            "cc:local:restore-source-lease-renew-authorize-spec",
            "cc:local:restore-source-lease-renew-authorized-never-armed-finalize-spec",
            "cc:local:restore-source-lease-renew-finalize-spec",
            "cc:local:restore-source-lease-renew-no-effect-finalize-spec",
            "cc:local:restore-terminal-pin-release-finalize-spec",
        ]
    );

    let sharding_state = slots
        .slots
        .iter()
        .find(|slot| slot.plane == "SemanticPayload" && slot.slot_tag == "sharding_migration_state")
        .expect("F16 sharding migration state slot");
    assert_eq!(sharding_state.role, "Local");
    assert_eq!(sharding_state.backing_registry, "state_payload_fields.toml");
    assert_eq!(sharding_state.stable_name, "sharding_migration_state");
    assert_eq!(sharding_state.status, "reserved");
    assert_eq!(
        sharding_state.transition_writer_contract_ids,
        [
            "cc:local:begin-role-transition-spec",
            "cc:local:sharding-freeze-spec",
            "cc:local:sharding-unfreeze-spec",
        ]
    );

    let meta_guard = slots
        .slots
        .iter()
        .find(|slot| {
            slot.plane == "Bootstrap"
                && slot.role == "Meta"
                && slot.slot_tag == "pending_restore_creation_guard"
        })
        .expect("Meta F1 pending restore creation guard slot");
    assert!(meta_guard.transition_writer_contract_ids.is_empty());
    assert_eq!(meta_guard.backing_registry, "bootstrap_frames.toml");
    assert_eq!(meta_guard.status, "reserved");

    let meta_origin = slots
        .slots
        .iter()
        .find(|slot| {
            slot.plane == "Bootstrap"
                && slot.role == "Meta"
                && slot.slot_tag == "restore_bridge_origin"
        })
        .expect("Meta F1 recovery origin slot");
    assert_eq!(
        meta_origin.transition_writer_contract_ids,
        ["cc:meta:recovery-bridge-spec"]
    );
    assert_eq!(meta_origin.backing_registry, "bootstrap_frames.toml");

    let meta_root = slots
        .slots
        .iter()
        .find(|slot| {
            slot.plane == "SemanticPayload"
                && slot.role == "Meta"
                && slot.slot_tag == "global_state_root"
        })
        .expect("Meta F1 global state root slot");
    assert_eq!(meta_root.stable_name, "global_state_root");
    assert_eq!(meta_root.backing_registry, "state_payload_fields.toml");
    assert_eq!(
        meta_root.transition_writer_contract_ids,
        ["cc:meta:recovery-bridge-spec"]
    );
    assert_eq!(meta_root.status, "reserved");

    let projected_meta_root = backings["state_payload_fields.toml"]
        .fields
        .iter()
        .filter(|field| field.role == "Meta" && field.slot_tag == "global_state_root")
        .count();
    assert_eq!(projected_meta_root, 1);

    for slot_tag in ["topology_state_ref", "meta_config_payload_floor_ref"] {
        let slot = slots
            .slots
            .iter()
            .find(|slot| {
                slot.plane == "SemanticPayload" && slot.role == "Meta" && slot.slot_tag == slot_tag
            })
            .expect("Meta F15 configuration state slot");
        assert_eq!(slot.stable_name, slot_tag);
        assert_eq!(slot.backing_registry, "state_payload_fields.toml");
        let expected_writers = if slot_tag == "topology_state_ref" {
            vec![
                "cc:meta:meta-configuration-transition-spec",
                "cc:meta:shard-configuration-adoption-spec",
            ]
        } else {
            vec!["cc:meta:meta-configuration-transition-spec"]
        };
        assert_eq!(slot.transition_writer_contract_ids, expected_writers);
        assert_eq!(slot.status, "reserved");
        let projected = backings["state_payload_fields.toml"]
            .fields
            .iter()
            .filter(|field| field.role == "Meta" && field.slot_tag == slot_tag)
            .count();
        assert_eq!(projected, 1, "{slot_tag} backing projection must be unique");
    }

    let meta_remote_trust = slots
        .slots
        .iter()
        .find(|slot| {
            slot.plane == "SemanticPayload"
                && slot.role == "Meta"
                && slot.slot_tag == "remote_configuration_trust_root"
        })
        .expect("Meta F16 imported remote configuration trust selector");
    assert_eq!(
        meta_remote_trust.stable_name,
        "remote_configuration_trust_root"
    );
    assert_eq!(
        meta_remote_trust.backing_registry,
        "state_payload_fields.toml"
    );
    assert_eq!(
        meta_remote_trust.transition_writer_contract_ids,
        ["cc:meta:shard-configuration-adoption-spec"]
    );
    assert_eq!(meta_remote_trust.status, "reserved");
    let projected_meta_remote_trust = backings["state_payload_fields.toml"]
        .fields
        .iter()
        .filter(|field| field.role == "Meta" && field.slot_tag == "remote_configuration_trust_root")
        .count();
    assert_eq!(
        projected_meta_remote_trust, 1,
        "Meta F16 trust selector backing projection must be unique"
    );

    for slot_tag in [
        "global_begin_idempotency_index_root",
        "global_outcome_directory_root",
    ] {
        let slot = slots
            .slots
            .iter()
            .find(|slot| {
                slot.plane == "SemanticPayload" && slot.role == "Meta" && slot.slot_tag == slot_tag
            })
            .expect("Meta F2/F3 shared state slot");
        assert_eq!(slot.stable_name, slot_tag);
        assert_eq!(slot.backing_registry, "state_payload_fields.toml");
        assert_eq!(slot.status, "reserved");
        let expected_writers = if slot_tag == "global_outcome_directory_root" {
            vec![
                "cc:meta:global-attempt-cancel-spec",
                "cc:meta:global-attempt-registration-spec",
                "cc:meta:global-begin-reservation-spec",
                "cc:meta:global-begin-terminal-spec",
                "cc:meta:global-closed-attempt-floor-publish-spec",
                "cc:meta:global-outcome-expiry-spec",
                "cc:meta:global-prepare-admission-spec",
                "cc:meta:global-read-close-spec",
                "cc:meta:global-statement-publication-spec",
                "cc:meta:global-terminal-completion-spec",
                "cc:meta:never-registered-floor-spec",
                "cc:meta:txn-ownership-expiry-abort-spec",
            ]
        } else {
            vec![
                "cc:meta:global-attempt-registration-spec",
                "cc:meta:global-begin-reservation-spec",
                "cc:meta:global-begin-terminal-spec",
                "cc:meta:global-closed-attempt-floor-publish-spec",
                "cc:meta:global-outcome-expiry-spec",
                "cc:meta:never-registered-floor-spec",
            ]
        };
        assert_eq!(slot.transition_writer_contract_ids, expected_writers);

        let projected = backings["state_payload_fields.toml"]
            .fields
            .iter()
            .filter(|field| field.role == "Meta" && field.slot_tag == slot_tag)
            .count();
        assert_eq!(projected, 1, "{slot_tag} backing projection must be unique");
    }

    for (plane, slot_tag, backing_registry, writers) in [
        (
            "SemanticPayload",
            "global_attempt_compaction_floor_ref",
            "state_payload_fields.toml",
            vec![
                "cc:meta:global-closed-attempt-floor-publish-spec",
                "cc:meta:never-registered-floor-spec",
            ],
        ),
        (
            "PreparedOwnership",
            "meta_prepared_payload_root",
            "prepared_state_fields.toml",
            vec![],
        ),
        (
            "Consensus",
            "meta_certificate_ledger",
            "consensus_state_fields.toml",
            vec![],
        ),
    ] {
        let slot = slots
            .slots
            .iter()
            .find(|slot| slot.plane == plane && slot.role == "Meta" && slot.slot_tag == slot_tag)
            .expect("Meta F11 state authority");
        assert_eq!(slot.stable_name, slot_tag);
        assert_eq!(slot.backing_registry, backing_registry);
        assert_eq!(slot.transition_writer_contract_ids, writers);
        assert_eq!(slot.status, "reserved");
        let projected = backings[backing_registry]
            .fields
            .iter()
            .filter(|field| field.role == "Meta" && field.slot_tag == slot_tag)
            .count();
        assert_eq!(
            projected, 1,
            "{plane}|Meta|{slot_tag} backing must be unique"
        );
    }

    let pending_compaction = slots
        .slots
        .iter()
        .find(|slot| {
            slot.plane == "SemanticPayload"
                && slot.role == "Meta"
                && slot.slot_tag == "global_attempt_compaction_pending_root"
        })
        .expect("Meta F13 pending compaction root slot");
    assert_eq!(
        pending_compaction.transition_writer_contract_ids,
        [
            "cc:meta:closed-attempt-compaction-spec",
            "cc:meta:global-closed-attempt-floor-publish-spec",
        ]
    );
    assert_eq!(pending_compaction.stable_name, pending_compaction.slot_tag);
    assert_eq!(
        pending_compaction.backing_registry,
        "state_payload_fields.toml"
    );
    assert_eq!(pending_compaction.status, "reserved");
    let projected_pending_compaction = backings["state_payload_fields.toml"]
        .fields
        .iter()
        .filter(|field| {
            field.role == "Meta" && field.slot_tag == "global_attempt_compaction_pending_root"
        })
        .count();
    assert_eq!(projected_pending_compaction, 1);

    let audit_ticket_index = slots
        .slots
        .iter()
        .find(|slot| {
            slot.plane == "SemanticPayload"
                && slot.role == "Meta"
                && slot.slot_tag == "audit_ticket_index_root"
        })
        .expect("Meta F2/F3 audit ticket index slot");
    assert_eq!(audit_ticket_index.stable_name, "audit_ticket_index_root");
    assert_eq!(
        audit_ticket_index.transition_writer_contract_ids,
        [
            "cc:meta:global-attempt-cancel-spec",
            "cc:meta:global-attempt-registration-spec",
            "cc:meta:global-begin-terminal-spec",
            "cc:meta:global-read-close-spec",
            "cc:meta:global-statement-abort-spec",
            "cc:meta:global-statement-publication-spec",
            "cc:meta:global-statement-registration-spec",
            "cc:meta:global-terminal-completion-spec",
            "cc:meta:txn-ownership-expiry-abort-spec",
        ]
    );
    assert_eq!(audit_ticket_index.status, "reserved");
    let projected_audit_ticket_index = backings["state_payload_fields.toml"]
        .fields
        .iter()
        .filter(|field| field.role == "Meta" && field.slot_tag == "audit_ticket_index_root")
        .count();
    assert_eq!(projected_audit_ticket_index, 1);

    for slot_tag in ["global_attempt_index_root", "global_conflict_index_ref"] {
        let slot = slots
            .slots
            .iter()
            .find(|slot| {
                slot.plane == "SemanticPayload" && slot.role == "Meta" && slot.slot_tag == slot_tag
            })
            .expect("Meta F3 registration state slot");
        assert_eq!(slot.stable_name, slot_tag);
        assert_eq!(slot.backing_registry, "state_payload_fields.toml");
        let expected_writers = if slot_tag == "global_conflict_index_ref" {
            vec![
                "cc:meta:global-attempt-cancel-spec",
                "cc:meta:global-attempt-registration-spec",
                "cc:meta:global-closed-attempt-floor-publish-spec",
                "cc:meta:global-prepare-admission-spec",
                "cc:meta:global-read-close-spec",
                "cc:meta:global-statement-publication-spec",
                "cc:meta:global-terminal-completion-spec",
                "cc:meta:txn-ownership-expiry-abort-spec",
            ]
        } else {
            vec![
                "cc:meta:global-attempt-cancel-spec",
                "cc:meta:global-attempt-registration-spec",
                "cc:meta:global-closed-attempt-floor-publish-spec",
                "cc:meta:global-prepare-admission-spec",
                "cc:meta:global-read-close-spec",
                "cc:meta:global-terminal-completion-spec",
                "cc:meta:txn-ownership-expiry-abort-spec",
            ]
        };
        assert_eq!(slot.transition_writer_contract_ids, expected_writers);
        assert_eq!(slot.status, "reserved");

        let projected = backings["state_payload_fields.toml"]
            .fields
            .iter()
            .filter(|field| field.role == "Meta" && field.slot_tag == slot_tag)
            .count();
        assert_eq!(projected, 1, "{slot_tag} backing projection must be unique");
    }

    for (slot_tag, writers) in [
        (
            "global_statement_index_root",
            vec![
                "cc:meta:global-statement-abort-spec",
                "cc:meta:global-statement-publication-spec",
                "cc:meta:global-statement-registration-spec",
                "cc:meta:txn-ownership-expiry-abort-spec",
            ],
        ),
        (
            "global_txn_capability_lineage_root",
            vec![
                "cc:meta:txn-ownership-expiry-abort-spec",
                "cc:meta:txn-ownership-transition-spec:reattach",
                "cc:meta:txn-ownership-transition-spec:renew",
            ],
        ),
        (
            "global_txn_ownership_directory_root",
            vec![
                "cc:meta:txn-ownership-expiry-abort-spec",
                "cc:meta:txn-ownership-transition-spec:reattach",
                "cc:meta:txn-ownership-transition-spec:renew",
            ],
        ),
        (
            "resource_ledger_root",
            vec![
                "cc:meta:global-attempt-cancel-spec",
                "cc:meta:global-prepare-admission-spec",
                "cc:meta:global-read-close-spec",
                "cc:meta:global-statement-abort-spec",
                "cc:meta:global-statement-publication-spec",
                "cc:meta:global-terminal-completion-spec",
                "cc:meta:txn-ownership-expiry-abort-spec",
            ],
        ),
    ] {
        let slot = slots
            .slots
            .iter()
            .find(|slot| {
                slot.plane == "SemanticPayload" && slot.role == "Meta" && slot.slot_tag == slot_tag
            })
            .expect("Meta F3 ownership state slot");
        assert_eq!(slot.stable_name, slot_tag);
        assert_eq!(slot.backing_registry, "state_payload_fields.toml");
        assert_eq!(slot.transition_writer_contract_ids, writers);
        assert_eq!(slot.status, "reserved");

        let projected = backings["state_payload_fields.toml"]
            .fields
            .iter()
            .filter(|field| field.role == "Meta" && field.slot_tag == slot_tag)
            .count();
        assert_eq!(projected, 1, "{slot_tag} backing projection must be unique");
    }

    for slot_tag in [
        "global_constraint_reservation_index_root",
        "terminal_admission_fence",
    ] {
        let slot = slots
            .slots
            .iter()
            .find(|slot| {
                slot.plane == "SemanticPayload" && slot.role == "Meta" && slot.slot_tag == slot_tag
            })
            .expect("Meta F6 prepare-admission state slot");
        assert_eq!(slot.stable_name, slot_tag);
        assert_eq!(slot.backing_registry, "state_payload_fields.toml");
        assert_eq!(
            slot.transition_writer_contract_ids,
            ["cc:meta:global-prepare-admission-spec"]
        );
        assert_eq!(slot.status, "reserved");

        let projected = backings["state_payload_fields.toml"]
            .fields
            .iter()
            .filter(|field| field.role == "Meta" && field.slot_tag == slot_tag)
            .count();
        assert_eq!(projected, 1, "{slot_tag} backing projection must be unique");
    }

    for slot_tag in [
        "terminal_certification_reservation_root",
        "terminal_coordinate_hold_root",
        "terminal_obligation_hold_root",
    ] {
        let slot = slots
            .slots
            .iter()
            .find(|slot| {
                slot.plane == "SemanticPayload" && slot.role == "Meta" && slot.slot_tag == slot_tag
            })
            .expect("Meta F8 final-certification state slot");
        assert_eq!(slot.stable_name, slot_tag);
        assert_eq!(slot.backing_registry, "state_payload_fields.toml");
        assert_eq!(
            slot.transition_writer_contract_ids,
            [
                "cc:meta:global-final-certification-cancel-spec",
                "cc:meta:global-final-certification-reserve-spec",
            ]
        );
        assert_eq!(slot.status, "reserved");

        let projected = backings["state_payload_fields.toml"]
            .fields
            .iter()
            .filter(|field| field.role == "Meta" && field.slot_tag == slot_tag)
            .count();
        assert_eq!(projected, 1, "{slot_tag} backing projection must be unique");
    }

    let result_retention = slots
        .slots
        .iter()
        .find(|slot| {
            slot.plane == "Protocol"
                && slot.role == "Meta"
                && slot.slot_tag == "result_independent_retention_index"
        })
        .expect("Meta F4 Protocol result-retention slot");
    assert_eq!(
        result_retention.transition_writer_contract_ids,
        [
            "cc:meta:global-read-close-spec",
            "cc:meta:global-statement-publication-spec",
            "cc:meta:global-terminal-completion-spec",
        ]
    );
    assert_eq!(
        result_retention.backing_registry,
        "protocol_state_fields.toml"
    );
    assert_eq!(result_retention.status, "reserved");
    let projected_result_retention = backings["protocol_state_fields.toml"]
        .fields
        .iter()
        .filter(|field| {
            field.role == "Meta" && field.slot_tag == "result_independent_retention_index"
        })
        .count();
    assert_eq!(projected_result_retention, 1);
    assert_eq!(
        backings["state_payload_fields.toml"]
            .fields
            .iter()
            .filter(|field| field.slot_tag == "result_independent_retention_index")
            .count(),
        0,
        "the Meta result owner must not leak into GlobalStatePayload"
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

    let (mut slots, backings, contracts) = fixture();
    let meta_gc = slots
        .slots
        .iter_mut()
        .find(|slot| {
            slot.plane == "SemanticPayload"
                && slot.role == "Meta"
                && slot.slot_tag == "gc_semantic_state"
        })
        .expect("Meta F17A GC slot");
    meta_gc
        .transition_writer_contract_ids
        .retain(|writer| !writer.ends_with("global-gc-authorization-spec"));
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

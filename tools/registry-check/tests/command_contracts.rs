//! Mutation suite for the command-contract registry (fgdb-5uw2).
//!
//! Every validation rule in `registry_check::command_contracts` gets a test
//! that takes a well-formed row, breaks exactly one thing, and asserts the
//! exact violation code. The registry shipped deliberately empty until the
//! owner-confirmed v1 tag freeze opened Phase B; it now carries the landed
//! F1-F16 tranche rows (all `reserved` — see the registry header). The single-defect
//! mutations still run against a synthetic row whose own clean baseline is
//! asserted first: a fixture control only proves the reader works on the
//! fixture, so the baseline assert is what licenses the mutations.

use registry_check::command_contracts::{
    Contract, ContractRegistry, load_contracts, load_from_repo, validate_contracts,
};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn registry() -> ContractRegistry {
    load_from_repo(&repo_root()).expect("command_contracts.toml loads")
}

fn codes(registry: &ContractRegistry) -> Vec<String> {
    validate_contracts(registry)
        .into_iter()
        .map(|violation| violation.code)
        .collect()
}

/// A fully well-formed reserved row. Every mutation below starts from this and
/// breaks exactly one thing.
fn synthetic_row() -> Contract {
    Contract {
        command_contract_id: "cc-local-branch-retire-v1".into(),
        role: "Local".into(),
        outer_command_union: "SequenceNeutralSpec".into(),
        outer_wire_tag: 0x0001,
        input_schema_id: "BranchRetireSpec".into(),
        input_wire_tag: 0x0001,
        inner_wire_tag: None,
        body_schema_id: "BranchRetireSpecBody".into(),
        result_schema_id: "BranchRetireResult".into(),
        applied_record_schema_id: "BranchRetireTombstone".into(),
        handler_symbol: "fgdb_chronicle::apply_branch_retire".into(),
        transition_class: "Semantic".into(),
        sequence_effects: "advances LogicalCommandSeq and HLC, never CommitSeq".into(),
        expected_state_schema_id: "ExpectedStateCondition".into(),
        authority_arm: "BranchAuthority".into(),
        authority_evidence_target_schema_id: None,
        terminal_audit_freeze_arm: "LocalControl".into(),
        terminal_audit_gate_arm: "Required".into(),
        payload_availability_rule: "None".into(),
        publication_mode: "SinglePlane".into(),
        construction_dag_recipe_id: "cdr-branch-retire-v1".into(),
        consumed_state_slots: vec!["SemanticPayload|Local|branch_registry".into()],
        written_state_slots: vec!["SemanticPayload|Local|branch_registry".into()],
        checkpoint_floor_classes: vec!["branch-registry".into()],
        backup_restore_gc_classes: vec!["branch-registry".into()],
        posture_feature_predicate: "role-local".into(),
        format_epoch_range: "1..".into(),
        status: "reserved".into(),
    }
}

fn with_row(f: impl FnOnce(&mut Contract)) -> ContractRegistry {
    let mut row = synthetic_row();
    f(&mut row);
    ContractRegistry {
        registry_epoch: 1,
        contracts: vec![row],
    }
}

/// The control. Every mutation test below is meaningless if the shipped
/// registry is not clean.
#[test]
fn real_registry_is_clean() {
    let violations = validate_contracts(&registry());
    assert!(
        violations.is_empty(),
        "the shipped contract registry must validate clean, found: {violations:?}"
    );
}

/// Phase B floor, replacing the Phase A deliberate-empty pin in the same
/// commit that landed the first rows (the point of pinning it): the landed
/// tranche rows of the owner-confirmed v1 freeze, by id. The population may
/// only grow from here — a missing row is either an illegal deletion (released
/// tags are permanent, plan line 290) or a gutted file, and both deserve a red.
#[test]
fn phase_b_seed_rows_are_present() {
    let registry = registry();
    for id in [
        // F1/F2 (87cf892)
        "cc:local:recovery-bridge-spec",
        // Meta F1 begins the separate GlobalSequenceNeutralSpec<Tag> tag
        // namespace at its own frozen ordinal 0x0001.
        "cc:meta:recovery-bridge-spec",
        // Meta F2 continues that namespace with the two exact BEGIN lineage
        // bodies at frozen ordinals 0x0002-0x0003.
        "cc:meta:global-begin-reservation-spec",
        "cc:meta:global-begin-terminal-spec",
        // Meta F3 starts the registered-attempt lineage at Global ordinal 4.
        "cc:meta:global-attempt-registration-spec",
        "cc:meta:txn-ownership-transition-spec:reattach",
        "cc:meta:txn-ownership-transition-spec:renew",
        "cc:meta:txn-ownership-expiry-abort-spec",
        "cc:meta:global-statement-registration-spec",
        "cc:meta:global-statement-publication-spec",
        "cc:meta:global-statement-abort-spec",
        "cc:meta:global-attempt-cancel-spec",
        "cc:meta:global-prepare-admission-spec",
        "cc:meta:global-read-close-spec",
        "cc:local:local-begin-reservation-spec",
        "cc:local:local-begin-terminal-spec",
        // F3 attempt-lifecycle (frozen ordinals 0x0004-0x0011)
        "cc:local:local-attempt-registration-spec:explicit-begin",
        "cc:local:local-attempt-registration-spec:autocommit",
        "cc:local:local-autocommit-write-spec",
        "cc:local:txn-ownership-transition-spec:reattach",
        "cc:local:txn-ownership-transition-spec:renew",
        "cc:local:txn-ownership-expiry-abort-spec",
        "cc:local:local-statement-registration-spec",
        "cc:local:local-statement-publication-spec",
        "cc:local:local-statement-abort-spec",
        "cc:local:local-prepare-admission-spec",
        "cc:local:local-read-close-spec",
        "cc:local:local-terminal-completion-spec",
        "cc:local:txn-abort-spec",
        "cc:local:local-outcome-compaction-spec:never-registered",
        "cc:local:local-outcome-compaction-spec:terminal-ready",
        "cc:local:local-outcome-expiry-spec",
        "cc:local:local-conflict-compaction-spec",
        // F4 allocation-escape-and-finalization (frozen ordinals 0x0012-0x0015)
        "cc:local:allocation-reservation-transition-spec:escaping-reserve",
        "cc:local:allocation-reservation-transition-spec:binding-compaction",
        "cc:local:local-final-certification-reserve-spec",
        "cc:local:local-final-certification-cancel-spec",
        "cc:local:finalization-allocation-disposition-spec:consumed",
        "cc:local:finalization-allocation-disposition-spec:abandoned-spent",
        // F5a checkpoint/floor/history-cut (frozen ordinals 0x0016-0x0018)
        "cc:local:checkpoint-install-spec",
        "cc:local:initial-config-floor-install-spec",
        "cc:local:history-cut-activation-spec",
        // F5b configuration transition (frozen ordinal 0x0019)
        "cc:local:configuration-transition-spec:propose-joint",
        "cc:local:configuration-transition-spec:commit-joint",
        "cc:local:configuration-transition-spec:commit-new",
        "cc:local:configuration-transition-spec:commit-retirement-floor",
        // F5c durable-format transition (frozen ordinal 0x001a). Five arms of one
        // member, inner tags minted from L1218 source order — no wire_types.toml
        // reservation constrains this family, unlike the F3 armed members.
        "cc:local:format-transition-spec:advertise-target",
        "cc:local:format-transition-spec:activate-write-epoch",
        "cc:local:format-transition-spec:record-first-target-write",
        "cc:local:format-transition-spec:rewrite-equivalent",
        "cc:local:format-transition-spec:retire-old-decoder",
        // F5d delta-batch retention cut (frozen ordinal 0x001b). The last F5
        // member per ruling C2 (round 10); struct body unspelled, T6 sentinels.
        "cc:local:local-delta-batch-retention-cut-spec",
        // F6 remote-retention-and-trust (frozen ordinals 0x001c-0x001e). The
        // eight RemoteRetentionControlSpec inner tags are FORCED by the
        // wire_types.toml union_variant reservations 0x0101-0x0108, which
        // agree with L1582 source order.
        "cc:local:remote-retention-control-spec:acquire-grant",
        "cc:local:remote-retention-control-spec:register-consumer-grant",
        "cc:local:remote-retention-control-spec:request-consumer-release",
        "cc:local:remote-retention-control-spec:publish-consumer-release-evidence",
        "cc:local:remote-retention-control-spec:apply-authority-release",
        "cc:local:remote-retention-control-spec:publish-authority-release-ack",
        "cc:local:remote-retention-control-spec:consume-release-ack",
        "cc:local:remote-retention-control-spec:adopt-legacy-authority-transfer",
        "cc:local:advance-remote-configuration-evidence-spec",
        "cc:local:validate-remote-configuration-anchor-spec",
        // F7 branches-and-merge (frozen ordinals 0x001f-0x0028). Ten armless
        // rows, fresh dense mint (measured 0 wire_types.toml hits);
        // MergePrepareSpec is ordered semantic per ruling I-5.
        "cc:local:branch-epoch-boundary-reserve-spec",
        "cc:local:branch-fork-spec",
        "cc:local:branch-grant-spec",
        "cc:local:branch-epoch-boundary-cancel-spec",
        "cc:local:branch-epoch-boundary-abandon-spec",
        "cc:local:branch-retire-spec",
        "cc:local:branch-retire-finalize-spec",
        "cc:local:merge-reject-spec",
        "cc:local:merge-prepare-spec",
        "cc:local:merge-execute-spec",
        // F8 id-validtime-constraint-resource-escrow (frozen ordinals
        // 0x0029-0x002b). EscrowRightsTransitionSpec's five inner tags are a
        // fresh mint from L256 source order (measured 0 wire_types.toml
        // hits); ExpiryEpochAdvanceSpec is included per ruling C4.
        "cc:local:resource-ledger-transition-spec",
        "cc:local:escrow-rights-transition-spec:transfer-atomic",
        "cc:local:escrow-rights-transition-spec:reconfigure-domain",
        "cc:local:escrow-rights-transition-spec:advance-quota-epoch",
        "cc:local:escrow-rights-transition-spec:retire-branch-overlay",
        "cc:local:escrow-rights-transition-spec:compact-terminal-evidence",
        "cc:local:expiry-epoch-advance-spec",
        // F9 policy-revocation-timeauthority-privacy-dp (frozen ordinals
        // 0x002c-0x0036): eleven members, SEVEN of them time-authority
        // controls. DpTransitionSpec's eight inner tags are a fresh mint
        // (measured 0 wire_types.toml hits) that L936 independently forces:
        // "its u8 arm tags follow that source order".
        "cc:local:policy-transition-spec",
        "cc:local:revocation-transition-spec",
        "cc:local:time-issuance-admission-freeze-spec",
        "cc:local:time-authority-rotation-intent-spec",
        "cc:local:time-authority-issuance-close-spec",
        "cc:local:time-authority-issuance-fence-authorize-spec",
        "cc:local:time-authority-registry-transition-authorize-spec",
        "cc:local:time-authority-profile-transition-spec",
        "cc:local:time-authority-profile-retirement-spec",
        "cc:local:privacy-continuity-import-spec",
        "cc:local:dp-transition-spec:prepare",
        "cc:local:dp-transition-spec:abandon",
        "cc:local:dp-transition-spec:arm-charge",
        "cc:local:dp-transition-spec:start",
        "cc:local:dp-transition-spec:yield",
        "cc:local:dp-transition-spec:reclaim",
        "cc:local:dp-transition-spec:commit-result",
        "cc:local:dp-transition-spec:compact",
        // F10 audit (frozen ordinals 0x0037-0x003c). AuditTerminalFreezeSpec's
        // three inner tags are a fresh mint from the L1834 closed-union source
        // order Arm|BeginRelease|FinalizeRelease (measured: no wire_types
        // union_variant reservation names the family).
        "cc:local:audit-ticket-admission-spec",
        "cc:local:audit-terminal-freeze-spec:arm",
        "cc:local:audit-terminal-freeze-spec:begin-release",
        "cc:local:audit-terminal-freeze-spec:finalize-release",
        "cc:local:audit-terminal-plan-abandon-spec",
        "cc:local:audit-terminal-spec",
        "cc:local:audit-recovery-spec",
        "cc:local:audit-completeness-transition-spec",
        // F11 bulk-load-staging (frozen ordinal 0x003d). Five inner tags,
        // fresh mint that L637 independently forces to the same source order
        // Reserve|AppendChunk|Seal|PrepareCommit|Abort; Committed is
        // intent-carried per the x2ar-3 ruling — no arm.
        "cc:local:bulk-load-transition-spec:reserve",
        "cc:local:bulk-load-transition-spec:append-chunk",
        "cc:local:bulk-load-transition-spec:seal",
        "cc:local:bulk-load-transition-spec:prepare-commit",
        "cc:local:bulk-load-transition-spec:abort",
        // F12 derived-build (frozen ordinal 0x003e). Six inner tags, fresh
        // mint from the L643 source order (no tag-order sentence in source —
        // singly determined, unlike DP/bulk-load); schema activation
        // contributes zero rows per I-9.
        "cc:local:derived-build-transition-spec:reserve",
        "cc:local:derived-build-transition-spec:publish-snapshot-progress",
        "cc:local:derived-build-transition-spec:publish-catchup-progress",
        "cc:local:derived-build-transition-spec:begin-validation",
        "cc:local:derived-build-transition-spec:publish-ready",
        "cc:local:derived-build-transition-spec:abort",
        // F13 keys (frozen ordinals 0x003f-0x0041). Three armless ordered
        // semantic transitions; proposal construction and physical dispatch
        // remain pre-order/Protocol work, respectively.
        "cc:local:key-destroy-authorize-spec",
        "cc:local:key-destroy-finalize-spec",
        "cc:local:key-destroy-certificate-publish-spec",
        // F14 semantic GC authorization (frozen ordinals 0x0042-0x0045).
        // Proposal construction is pre-order; physical work is Protocol.
        "cc:local:local-gc-authorize-spec",
        "cc:local:local-gc-apply-quarantine-spec",
        "cc:local:local-gc-cancellation-authorize-spec",
        "cc:local:gc-physical-disposition-import-spec:completed",
        "cc:local:gc-physical-disposition-import-spec:cancelled",
        // F15A Local semantic backup (frozen ordinals 0x0046-0x004f).
        // External dispatch remains Protocol work; later F15 restore and
        // structural Role members retain their frozen positions.
        "cc:local:local-backup-barrier-spec",
        "cc:local:local-backup-closure-publish-spec",
        "cc:local:local-backup-seal-spec",
        "cc:local:local-backup-publication-authorize-spec",
        "cc:local:local-backup-publication-receipt-import-spec",
        "cc:local:local-backup-grant-issue-import-spec",
        "cc:local:local-backup-artifact-verify-spec",
        "cc:local:local-backup-release-spec",
        "cc:local:archive-source-release-completion-import-spec",
        "cc:local:local-backup-abort-spec",
        // F15B Local semantic restore (frozen ordinals 0x0050-0x0056).
        // RestoreAbandonSpec is armed; this role-valid projection contains
        // exactly its first/source-ordered Local arm.
        "cc:local:local-restore-activation-spec",
        "cc:local:local-restore-service-prepare-spec",
        "cc:local:local-restore-service-promotion-spec",
        "cc:local:local-restore-service-completion-spec",
        "cc:local:local-restore-abandon-finalize-spec",
        "cc:local:local-restore-abandonment-pin-install-spec",
        "cc:local:restore-abandon-spec:local",
        // F15C DirectoryBound restore authority (frozen ordinals
        // 0x0057-0x005a). All four members are armless and Local-only.
        "cc:local:directory-bound-enter-promotion-pending-spec",
        "cc:local:directory-bound-finalize-operational-authority-spec",
        "cc:local:directory-bound-abandon-apply-spec",
        "cc:local:directory-bound-abandon-receipt-import-spec",
        // F15D first authority-owning restore/lease cleanup cohort (frozen
        // ordinals 0x005b-0x005d). These structurally Local instantiations
        // are armless; later C5 members retain their source-order positions.
        "cc:local:restore-source-key-access-cleanup-finalize-spec",
        "cc:local:restore-source-lease-renew-authorized-never-armed-finalize-spec",
        "cc:local:restore-source-lease-release-authorized-never-armed-finalize-spec",
        // F15E terminal-pin release and source-access cleanup cohort (frozen
        // ordinals 94-96).
        "cc:local:restore-terminal-pin-release-finalize-spec",
        "cc:local:restore-source-key-access-cleanup-authorize-spec",
        "cc:local:restore-source-key-access-cleanup-import-spec",
        // F15F source-lease renewal (frozen ordinals 97-99). Authorization
        // remains recipe-only; applied and no-effect terminal paths are
        // separate armless transitions.
        "cc:local:restore-source-lease-renew-authorize-spec",
        "cc:local:restore-source-lease-renew-finalize-spec",
        "cc:local:restore-source-lease-renew-no-effect-finalize-spec",
        // F15G source-lease release (frozen ordinals 100-102). The physical
        // dispatch initializer is Protocol-only and deliberately absent.
        "cc:local:restore-source-lease-release-spec",
        "cc:local:restore-source-lease-release-finalize-spec",
        "cc:local:restore-source-lease-release-no-effect-finalize-spec",
        // F16 sharding freeze/unfreeze/role-transition initiation (frozen
        // ordinals 103-105). Audit visibility remains Protocol maintenance.
        "cc:local:sharding-freeze-spec",
        "cc:local:sharding-unfreeze-spec",
        "cc:local:begin-role-transition-spec",
    ] {
        assert!(
            registry
                .contracts
                .iter()
                .any(|row| row.command_contract_id == id && row.status == "reserved"),
            "confirmed seed row {id:?} is missing"
        );
    }
    assert!(
        registry.contracts.len() >= 159,
        "the population may only grow from the landed Local F1-F16 plus Meta F1-F7 rows"
    );
}

/// Meta F1 is the role-specialized recovery bridge, not a copy of the Local
/// result/state triple. It starts the independent Global tag namespace and
/// consumes only the Meta pending-creation guard before publishing the first
/// Global root and RecoveryBridge origin.
#[test]
fn meta_f1_guarded_recovery_contract_is_exact() {
    let registry = registry();
    let row = registry
        .contracts
        .iter()
        .find(|row| row.command_contract_id == "cc:meta:recovery-bridge-spec")
        .expect("Meta F1 recovery bridge row");

    assert_eq!(row.role, "Meta");
    assert_eq!(row.outer_command_union, "GlobalSequenceNeutralSpec<Tag>");
    assert_eq!(row.outer_wire_tag, 0x0001);
    assert_eq!(row.input_wire_tag, 0x0001);
    assert_eq!(row.inner_wire_tag, None);
    assert_eq!(row.input_schema_id, "RecoveryBridgeSpec<Meta>");
    assert_eq!(row.body_schema_id, "RecoveryBridgeSpec<Meta>");
    assert_eq!(row.result_schema_id, "GlobalControlRecord");
    assert_eq!(row.applied_record_schema_id, "GlobalStateRoot");
    assert_eq!(row.handler_symbol, "fgdb_apply::meta::recovery_bridge_spec");
    assert_eq!(row.transition_class, "Semantic");
    assert_eq!(
        row.expected_state_schema_id,
        "PendingRestoreCreationGuard<Meta>"
    );
    assert_eq!(row.authority_arm, "RecoveryBridgeAuthority<Meta>");
    assert_eq!(row.authority_evidence_target_schema_id, None);
    assert_eq!(row.terminal_audit_freeze_arm, "Forbidden");
    assert_eq!(
        row.terminal_audit_gate_arm,
        "StructurallyInapplicable{RecoverySystemAuthority}"
    );
    assert_eq!(row.publication_mode, "SinglePlane");
    assert_eq!(
        row.consumed_state_slots,
        ["Bootstrap|Meta|pending_restore_creation_guard"]
    );
    assert_eq!(
        row.written_state_slots,
        [
            "Bootstrap|Meta|restore_bridge_origin",
            "SemanticPayload|Meta|global_state_root",
        ]
    );
    assert_eq!(
        row.checkpoint_floor_classes,
        ["genesis-or-restore-first-root"]
    );
    assert_eq!(row.backup_restore_gc_classes, ["restore-bridge"]);
    assert_eq!(row.posture_feature_predicate, "sharded");
    assert_eq!(row.status, "reserved");

    let global_tag_one: Vec<_> = registry
        .contracts
        .iter()
        .filter(|candidate| {
            candidate.role == "Meta"
                && candidate.outer_command_union == "GlobalSequenceNeutralSpec<Tag>"
                && candidate.outer_wire_tag == 0x0001
        })
        .collect();
    assert_eq!(global_tag_one.len(), 1, "Meta tag 1 must be unambiguous");

    let local = registry
        .contracts
        .iter()
        .find(|candidate| candidate.command_contract_id == "cc:local:recovery-bridge-spec")
        .expect("Local recovery bridge remains present");
    assert_eq!(local.outer_wire_tag, 0x0001);
    assert_ne!(local.outer_command_union, row.outer_command_union);
    assert_ne!(local.result_schema_id, row.result_schema_id);
    assert_ne!(local.applied_record_schema_id, row.applied_record_schema_id);
}

/// Meta F2 is a two-member armless family in the independent Global tag
/// namespace. Both members update the same begin-index/outcome-directory
/// lineage, while terminalization alone carries the required prebuilt audit
/// freeze and must never create an attempt.
#[test]
fn meta_f2_begin_lineage_contracts_are_exact() {
    let registry = registry();
    let expected = [
        (
            "cc:meta:global-begin-reservation-spec",
            0x0002,
            "GlobalBeginReservationSpec",
            "GlobalBeginReservationRecord",
            "WeakAbsenceProof",
            "Forbidden",
            "LifecycleScaffoldingNotRequired",
            "fgdb_apply::meta::global_begin_reservation_spec",
        ),
        (
            "cc:meta:global-begin-terminal-spec",
            0x0003,
            "GlobalBeginTerminalSpec",
            "NeverRegisteredTerminalRecord",
            "GlobalBeginReservationRecord",
            "AuditFreezeField::Required<MetaBeginTerminal>",
            "TerminalAuditGate",
            "fgdb_apply::meta::global_begin_terminal_spec",
        ),
    ];

    for (id, tag, input, result, expected_state, freeze, gate, handler) in expected {
        let row = registry
            .contracts
            .iter()
            .find(|row| row.command_contract_id == id)
            .expect("Meta F2 row");
        assert_eq!(row.role, "Meta", "{id} role drifted");
        assert_eq!(
            row.outer_command_union, "GlobalSequenceNeutralSpec<Tag>",
            "{id} union drifted"
        );
        assert_eq!(row.outer_wire_tag, tag, "{id} outer tag drifted");
        assert_eq!(row.input_wire_tag, tag, "{id} input tag drifted");
        assert_eq!(row.inner_wire_tag, None, "{id} invented an inner arm");
        assert_eq!(row.input_schema_id, input, "{id} input drifted");
        assert_eq!(row.body_schema_id, input, "{id} body drifted");
        assert_eq!(row.result_schema_id, result, "{id} result drifted");
        assert_eq!(
            row.applied_record_schema_id, "GlobalBeginIdempotencyIndex",
            "{id} applied index drifted"
        );
        assert_eq!(
            row.expected_state_schema_id, expected_state,
            "{id} expected state drifted"
        );
        assert_eq!(row.authority_arm, "AuthorityBoundHeader<Meta>");
        assert_eq!(row.terminal_audit_freeze_arm, freeze);
        assert_eq!(row.terminal_audit_gate_arm, gate);
        assert_eq!(row.handler_symbol, handler);
        assert_eq!(row.transition_class, "Semantic");
        assert_eq!(row.publication_mode, "SinglePlane");
        if id.ends_with("reservation-spec") {
            assert_eq!(
                row.consumed_state_slots,
                ["SemanticPayload|Meta|global_begin_idempotency_index_root"]
            );
            assert_eq!(
                row.written_state_slots,
                [
                    "SemanticPayload|Meta|global_begin_idempotency_index_root",
                    "SemanticPayload|Meta|global_outcome_directory_root",
                ]
            );
        } else {
            assert_eq!(
                row.consumed_state_slots,
                [
                    "SemanticPayload|Meta|audit_ticket_index_root",
                    "SemanticPayload|Meta|global_begin_idempotency_index_root",
                ]
            );
            assert_eq!(
                row.written_state_slots,
                [
                    "SemanticPayload|Meta|audit_ticket_index_root",
                    "SemanticPayload|Meta|global_begin_idempotency_index_root",
                    "SemanticPayload|Meta|global_outcome_directory_root",
                ]
            );
        }
        assert_eq!(row.checkpoint_floor_classes, ["txn-attempt"]);
        assert_eq!(row.backup_restore_gc_classes, ["txn-lifecycle"]);
        assert_eq!(row.posture_feature_predicate, "sharded");
        assert_eq!(row.status, "reserved");
    }

    let meta_f2_tags: Vec<_> = registry
        .contracts
        .iter()
        .filter(|row| {
            row.role == "Meta"
                && row.outer_command_union == "GlobalSequenceNeutralSpec<Tag>"
                && (row.outer_wire_tag == 0x0002 || row.outer_wire_tag == 0x0003)
        })
        .map(|row| (row.outer_wire_tag, row.command_contract_id.as_str()))
        .collect();
    assert_eq!(
        meta_f2_tags,
        [
            (0x0002, "cc:meta:global-begin-reservation-spec"),
            (0x0003, "cc:meta:global-begin-terminal-spec"),
        ]
    );

    let terminal = registry
        .contracts
        .iter()
        .find(|row| row.command_contract_id == "cc:meta:global-begin-terminal-spec")
        .expect("Meta terminal row");
    assert!(
        terminal
            .sequence_effects
            .contains("resolves the exact pending operation ticket")
    );
    assert!(
        terminal
            .sequence_effects
            .contains("without creating an attempt")
    );
}

/// Meta F3 begins with one armless registration member. The body contains
/// several predecessor-state choices, but none is a command-union arm: the
/// frozen Global tag remains uniquely owned by this one contract row.
#[test]
fn meta_f3_attempt_registration_contract_is_exact_and_atomic() {
    let registry = registry();
    let row = registry
        .contracts
        .iter()
        .find(|row| row.command_contract_id == "cc:meta:global-attempt-registration-spec")
        .expect("Meta F3 attempt-registration row");

    assert_eq!(row.role, "Meta");
    assert_eq!(row.outer_command_union, "GlobalSequenceNeutralSpec<Tag>");
    assert_eq!(row.outer_wire_tag, 0x0004);
    assert_eq!(row.input_wire_tag, 0x0004);
    assert_eq!(row.inner_wire_tag, None);
    assert_eq!(row.input_schema_id, "GlobalAttemptRegistrationSpec");
    assert_eq!(row.body_schema_id, "GlobalAttemptRegistrationSpec");
    assert_eq!(row.result_schema_id, "GlobalAttemptRegistration");
    assert_eq!(row.applied_record_schema_id, "GlobalBeginIdempotencyIndex");
    assert_eq!(row.expected_state_schema_id, "GlobalBeginReservationRecord");
    assert_eq!(row.authority_arm, "AuthorityBoundHeader<Meta>");
    assert_eq!(row.terminal_audit_freeze_arm, "Forbidden");
    assert_eq!(
        row.terminal_audit_gate_arm,
        "LifecycleScaffoldingNotRequired"
    );
    assert_eq!(
        row.handler_symbol,
        "fgdb_apply::meta::global_attempt_registration_spec"
    );
    assert_eq!(row.transition_class, "Semantic");
    assert_eq!(row.publication_mode, "SinglePlane");
    assert_eq!(
        row.consumed_state_slots,
        [
            "SemanticPayload|Meta|audit_ticket_index_root",
            "SemanticPayload|Meta|global_attempt_index_root",
            "SemanticPayload|Meta|global_begin_idempotency_index_root",
            "SemanticPayload|Meta|global_conflict_index_ref",
            "SemanticPayload|Meta|global_outcome_directory_root",
        ]
    );
    assert_eq!(row.written_state_slots, row.consumed_state_slots);
    assert_eq!(row.checkpoint_floor_classes, ["txn-attempt"]);
    assert_eq!(row.backup_restore_gc_classes, ["txn-lifecycle"]);
    assert_eq!(row.posture_feature_predicate, "sharded");
    assert_eq!(row.status, "reserved");
    assert!(row.sequence_effects.contains("PendingUnclaimed"));
    assert!(row.sequence_effects.contains("workspace generation zero"));
    assert!(row.sequence_effects.contains("no participant read"));

    let global_tag_four: Vec<_> = registry
        .contracts
        .iter()
        .filter(|candidate| {
            candidate.role == "Meta"
                && candidate.outer_command_union == "GlobalSequenceNeutralSpec<Tag>"
                && candidate.outer_wire_tag == 0x0004
        })
        .map(|candidate| candidate.command_contract_id.as_str())
        .collect();
    assert_eq!(
        global_tag_four,
        ["cc:meta:global-attempt-registration-spec"]
    );
}

/// The shared ownership machine specializes into the independent Global
/// command namespace without duplicating its input class. Reattach/Renew own
/// one armed outer tag; expiry is the next armless member and closes every
/// exact Meta attempt-lifecycle index named by the terminalization law.
#[test]
fn meta_f3_ownership_contracts_are_exact_and_atomic() {
    let registry = registry();
    let transition_slots = [
        "SemanticPayload|Meta|global_txn_capability_lineage_root".to_owned(),
        "SemanticPayload|Meta|global_txn_ownership_directory_root".to_owned(),
    ];
    for (id, inner, handler, effect) in [
        (
            "cc:meta:txn-ownership-transition-spec:reattach",
            0x0001,
            "fgdb_apply::meta::txn_ownership_transition_spec::reattach",
            "changes session only through one exact open-statement disposition",
        ),
        (
            "cc:meta:txn-ownership-transition-spec:renew",
            0x0002,
            "fgdb_apply::meta::txn_ownership_transition_spec::renew",
            "cannot transfer session or control mode",
        ),
    ] {
        let row = registry
            .contracts
            .iter()
            .find(|row| row.command_contract_id == id)
            .expect("Meta ownership transition arm");
        assert_eq!(row.role, "Meta");
        assert_eq!(row.outer_command_union, "GlobalSequenceNeutralSpec<Tag>");
        assert_eq!(row.outer_wire_tag, 0x0005);
        assert_eq!(row.input_wire_tag, 0x0005);
        assert_eq!(row.inner_wire_tag, Some(inner));
        assert_eq!(row.input_schema_id, "TxnOwnershipTransitionSpec<Meta>");
        assert_eq!(row.body_schema_id, "TxnOwnershipTransitionSpec<Meta>");
        assert_eq!(row.result_schema_id, "TxnOwnershipLease<Meta>");
        assert_eq!(row.applied_record_schema_id, "TxnOwnershipLease<Meta>");
        assert_eq!(row.expected_state_schema_id, "TxnOwnershipLease<Meta>");
        assert_eq!(row.authority_arm, "AuthorityBoundHeader<Meta>");
        assert_eq!(
            row.authority_evidence_target_schema_id.as_deref(),
            Some("DurableCapabilityValidationEvidence")
        );
        assert_eq!(row.terminal_audit_freeze_arm, "Forbidden");
        assert_eq!(row.terminal_audit_gate_arm, "TerminalAuditGate");
        assert_eq!(row.handler_symbol, handler);
        assert_eq!(row.consumed_state_slots, transition_slots);
        assert_eq!(row.written_state_slots, transition_slots);
        assert!(row.sequence_effects.contains(effect));
        assert!(row.sequence_effects.contains("capability lineage"));
        assert_eq!(row.posture_feature_predicate, "sharded");
        assert_eq!(row.status, "reserved");
    }

    let expiry = registry
        .contracts
        .iter()
        .find(|row| row.command_contract_id == "cc:meta:txn-ownership-expiry-abort-spec")
        .expect("Meta ownership expiry row");
    assert_eq!(expiry.outer_command_union, "GlobalSequenceNeutralSpec<Tag>");
    assert_eq!(expiry.outer_wire_tag, 0x0006);
    assert_eq!(expiry.input_wire_tag, 0x0006);
    assert_eq!(expiry.inner_wire_tag, None);
    assert_eq!(expiry.input_schema_id, "TxnOwnershipExpiryAbortSpec<Meta>");
    assert_eq!(expiry.body_schema_id, "TxnOwnershipExpiryAbortSpec<Meta>");
    assert_eq!(expiry.result_schema_id, "GlobalTxnOutcomeRecord");
    assert_eq!(expiry.applied_record_schema_id, "TxnOwnershipLease<Meta>");
    assert_eq!(expiry.authority_arm, "TimeValidationEvidence");
    assert_eq!(
        expiry.authority_evidence_target_schema_id.as_deref(),
        Some("TimeValidationEvidence")
    );
    assert_eq!(
        expiry.consumed_state_slots,
        [
            "SemanticPayload|Meta|audit_ticket_index_root",
            "SemanticPayload|Meta|global_attempt_index_root",
            "SemanticPayload|Meta|global_conflict_index_ref",
            "SemanticPayload|Meta|global_outcome_directory_root",
            "SemanticPayload|Meta|global_statement_index_root",
            "SemanticPayload|Meta|global_txn_capability_lineage_root",
            "SemanticPayload|Meta|global_txn_ownership_directory_root",
            "SemanticPayload|Meta|resource_ledger_root",
        ]
    );
    assert_eq!(expiry.written_state_slots, expiry.consumed_state_slots);
    for required in [
        "Expired TimeValidationEvidence",
        "structured cancellation",
        "open-statement terminalization",
        "result detachment",
        "workspace-resource",
        "conflict evidence",
        "before PrepareFenced",
    ] {
        assert!(
            expiry.sequence_effects.contains(required),
            "expiry law lost {required:?}"
        );
    }
    assert_eq!(expiry.status, "reserved");

    let tag_five: Vec<_> = registry
        .contracts
        .iter()
        .filter(|row| {
            row.role == "Meta"
                && row.outer_command_union == "GlobalSequenceNeutralSpec<Tag>"
                && row.outer_wire_tag == 0x0005
        })
        .map(|row| row.inner_wire_tag)
        .collect();
    assert_eq!(tag_five, [Some(0x0001), Some(0x0002)]);
}

/// Registration, publication, and abort are three distinct Global semantic
/// inputs. Publication alone crosses the Semantic/Protocol boundary, and its
/// Protocol owner is independent rather than reachable from the statement
/// value after detachment.
#[test]
fn meta_f4_statement_contracts_are_exact_and_cross_plane_safe() {
    let registry = registry();

    let registration = registry
        .contracts
        .iter()
        .find(|row| row.command_contract_id == "cc:meta:global-statement-registration-spec")
        .expect("Meta statement registration row");
    assert_eq!(
        registration.outer_command_union,
        "GlobalSequenceNeutralSpec<Tag>"
    );
    assert_eq!(registration.outer_wire_tag, 0x0007);
    assert_eq!(registration.input_wire_tag, 0x0007);
    assert_eq!(registration.inner_wire_tag, None);
    assert_eq!(
        registration.input_schema_id,
        "GlobalStatementRegistrationSpec"
    );
    assert_eq!(registration.result_schema_id, "GlobalStatementRegistration");
    assert_eq!(
        registration.applied_record_schema_id,
        "GlobalStatementIndex"
    );
    assert_eq!(
        registration.expected_state_schema_id,
        "GlobalTxnWorkspaceGeneration"
    );
    assert_eq!(registration.authority_arm, "AuthorityBoundHeader<Meta>");
    assert_eq!(registration.terminal_audit_freeze_arm, "Forbidden");
    assert_eq!(
        registration.terminal_audit_gate_arm,
        "LifecycleScaffoldingNotRequired"
    );
    assert_eq!(registration.publication_mode, "SinglePlane");
    assert_eq!(
        registration.consumed_state_slots,
        [
            "SemanticPayload|Meta|audit_ticket_index_root",
            "SemanticPayload|Meta|global_attempt_index_root",
            "SemanticPayload|Meta|global_conflict_index_ref",
            "SemanticPayload|Meta|global_statement_index_root",
        ]
    );
    assert_eq!(
        registration.written_state_slots,
        [
            "SemanticPayload|Meta|audit_ticket_index_root",
            "SemanticPayload|Meta|global_statement_index_root",
        ]
    );
    for required in [
        "before any shard observation",
        "exact retry joins",
        "every drift fails",
    ] {
        assert!(registration.sequence_effects.contains(required));
    }

    let publication = registry
        .contracts
        .iter()
        .find(|row| row.command_contract_id == "cc:meta:global-statement-publication-spec")
        .expect("Meta statement publication row");
    assert_eq!(publication.outer_wire_tag, 0x0008);
    assert_eq!(publication.input_wire_tag, 0x0008);
    assert_eq!(publication.inner_wire_tag, None);
    assert_eq!(
        publication.input_schema_id,
        "GlobalStatementPublicationSpec"
    );
    assert_eq!(publication.result_schema_id, "StatementPublishedOutput");
    assert_eq!(publication.applied_record_schema_id, "GlobalStatementIndex");
    assert_eq!(publication.expected_state_schema_id, "GlobalStatementIndex");
    assert_eq!(publication.terminal_audit_freeze_arm, "Forbidden");
    assert_eq!(publication.terminal_audit_gate_arm, "TerminalAuditGate");
    assert_eq!(publication.publication_mode, "AtomicProtocolDetach");
    assert_eq!(
        publication.consumed_state_slots,
        [
            "Protocol|Meta|result_independent_retention_index",
            "SemanticPayload|Meta|audit_ticket_index_root",
            "SemanticPayload|Meta|global_attempt_index_root",
            "SemanticPayload|Meta|global_conflict_index_ref",
            "SemanticPayload|Meta|global_outcome_directory_root",
            "SemanticPayload|Meta|global_statement_index_root",
            "SemanticPayload|Meta|resource_ledger_root",
        ]
    );
    assert_eq!(
        publication.written_state_slots,
        [
            "Protocol|Meta|result_independent_retention_index",
            "SemanticPayload|Meta|audit_ticket_index_root",
            "SemanticPayload|Meta|global_conflict_index_ref",
            "SemanticPayload|Meta|global_outcome_directory_root",
            "SemanticPayload|Meta|global_statement_index_root",
            "SemanticPayload|Meta|resource_ledger_root",
        ]
    );
    for required in [
        "ResultIndependentRetentionRecord<Meta>",
        "without a Semantic reference",
        "succeeding Registered outcome",
    ] {
        assert!(publication.sequence_effects.contains(required));
    }

    let abort = registry
        .contracts
        .iter()
        .find(|row| row.command_contract_id == "cc:meta:global-statement-abort-spec")
        .expect("Meta statement abort row");
    assert_eq!(abort.outer_wire_tag, 0x0009);
    assert_eq!(abort.input_wire_tag, 0x0009);
    assert_eq!(abort.inner_wire_tag, None);
    assert_eq!(abort.input_schema_id, "GlobalStatementAbortSpec");
    assert_eq!(abort.result_schema_id, "GlobalStatementIndex");
    assert_eq!(abort.applied_record_schema_id, "GlobalStatementIndex");
    assert_eq!(abort.authority_arm, "AuthorizationDecisionRecord<Meta>");
    assert_eq!(
        abort.authority_evidence_target_schema_id.as_deref(),
        Some("AuthorizationDecisionRecord<Meta>")
    );
    assert_eq!(abort.publication_mode, "SinglePlane");
    assert_eq!(
        abort.consumed_state_slots,
        [
            "SemanticPayload|Meta|audit_ticket_index_root",
            "SemanticPayload|Meta|global_attempt_index_root",
            "SemanticPayload|Meta|global_conflict_index_ref",
            "SemanticPayload|Meta|global_statement_index_root",
            "SemanticPayload|Meta|resource_ledger_root",
        ]
    );
    assert_eq!(
        abort.written_state_slots,
        [
            "SemanticPayload|Meta|audit_ticket_index_root",
            "SemanticPayload|Meta|global_statement_index_root",
            "SemanticPayload|Meta|resource_ledger_root",
        ]
    );
    for required in [
        "ContinueAttempt or AbortAttempt",
        "no result or workspace effect",
        "operation ticket and attempt outcome unresolved",
    ] {
        assert!(abort.sequence_effects.contains(required));
    }

    let exact_meta_tags: Vec<_> = registry
        .contracts
        .iter()
        .filter(|row| {
            row.role == "Meta"
                && row.outer_command_union == "GlobalSequenceNeutralSpec<Tag>"
                && (0x0007..=0x0009).contains(&row.outer_wire_tag)
        })
        .map(|row| (row.outer_wire_tag, row.command_contract_id.as_str()))
        .collect();
    assert_eq!(
        exact_meta_tags,
        [
            (0x0007, "cc:meta:global-statement-registration-spec"),
            (0x0008, "cc:meta:global-statement-publication-spec"),
            (0x0009, "cc:meta:global-statement-abort-spec"),
        ]
    );
}

/// Freeze the next source-ordered Meta member without crossing the later
/// ReadClose/Prepare boundary. Cancellation is one armless semantic row: the
/// lifecycle-operation discriminator lives in its body and must not be minted
/// as a second command tag.
#[test]
fn meta_f5_active_attempt_cancel_contract_is_exact() {
    let registry = registry();
    let rows: Vec<_> = registry
        .contracts
        .iter()
        .filter(|row| row.command_contract_id == "cc:meta:global-attempt-cancel-spec")
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "Meta attempt cancellation must classify once"
    );
    let row = rows[0];
    assert_eq!(row.role, "Meta");
    assert_eq!(row.outer_command_union, "GlobalSequenceNeutralSpec<Tag>");
    assert_eq!(row.outer_wire_tag, 0x000a);
    assert_eq!(row.input_wire_tag, 0x000a);
    assert_eq!(row.inner_wire_tag, None);
    assert_eq!(row.input_schema_id, "GlobalAttemptCancelSpec");
    assert_eq!(row.body_schema_id, "GlobalAttemptCancelSpec");
    assert_eq!(row.result_schema_id, "GlobalTxnOutcomeRecord");
    assert_eq!(row.applied_record_schema_id, "GlobalTxnOutcomeRecord");
    assert_eq!(row.expected_state_schema_id, "GlobalAttemptIndex");
    assert_eq!(row.authority_arm, "AuthorityBoundHeader<Meta>");
    assert_eq!(
        row.authority_evidence_target_schema_id.as_deref(),
        Some("TerminalAbortAuthority<Meta>")
    );
    assert_eq!(row.terminal_audit_freeze_arm, "Forbidden");
    assert_eq!(row.terminal_audit_gate_arm, "TerminalAuditGate");
    assert_eq!(row.publication_mode, "SinglePlane");
    assert_eq!(
        row.handler_symbol,
        "fgdb_apply::meta::global_attempt_cancel_spec"
    );
    assert_eq!(
        row.consumed_state_slots,
        [
            "SemanticPayload|Meta|audit_ticket_index_root",
            "SemanticPayload|Meta|global_attempt_index_root",
            "SemanticPayload|Meta|global_conflict_index_ref",
            "SemanticPayload|Meta|global_outcome_directory_root",
            "SemanticPayload|Meta|global_statement_index_root",
            "SemanticPayload|Meta|resource_ledger_root",
        ]
    );
    assert_eq!(
        row.written_state_slots,
        [
            "SemanticPayload|Meta|audit_ticket_index_root",
            "SemanticPayload|Meta|global_attempt_index_root",
            "SemanticPayload|Meta|global_conflict_index_ref",
            "SemanticPayload|Meta|global_outcome_directory_root",
            "SemanticPayload|Meta|resource_ledger_root",
        ]
    );
    for required in [
        "only before prepare admission",
        "every Open statement is Failed or Abandoned",
        "AppliedAbortRef::MetaControl",
        "without releasing an independent result owner",
    ] {
        assert!(row.sequence_effects.contains(required));
    }
    assert_eq!(row.status, "reserved");

    let exact_ordinal: Vec<_> = registry
        .contracts
        .iter()
        .filter(|candidate| {
            candidate.role == "Meta"
                && candidate.outer_command_union == "GlobalSequenceNeutralSpec<Tag>"
                && candidate.outer_wire_tag == 0x000a
        })
        .map(|candidate| candidate.command_contract_id.as_str())
        .collect();
    assert_eq!(exact_ordinal, ["cc:meta:global-attempt-cancel-spec"]);
}

/// The next dense Global ordinal is the armless ReadWrite admission member.
/// Pin its complete state/authority/result tuple so a Local analogue, a future
/// terminal order, or a partial reservation set cannot masquerade as Meta
/// prepare admission.
#[test]
fn meta_f6_prepare_admission_contract_is_exact() {
    let registry = registry();
    let rows: Vec<_> = registry
        .contracts
        .iter()
        .filter(|row| row.command_contract_id == "cc:meta:global-prepare-admission-spec")
        .collect();
    assert_eq!(rows.len(), 1, "Meta prepare admission must classify once");
    let row = rows[0];
    assert_eq!(row.role, "Meta");
    assert_eq!(row.outer_command_union, "GlobalSequenceNeutralSpec<Tag>");
    assert_eq!(row.outer_wire_tag, 0x000b);
    assert_eq!(row.input_wire_tag, 0x000b);
    assert_eq!(row.inner_wire_tag, None);
    assert_eq!(row.input_schema_id, "GlobalPrepareAdmissionSpec");
    assert_eq!(row.body_schema_id, "GlobalPrepareAdmissionSpec");
    assert_eq!(row.result_schema_id, "MetaTerminalAdmissionFence");
    assert_eq!(row.applied_record_schema_id, "GlobalTxnOutcomeRecord");
    assert_eq!(row.expected_state_schema_id, "GlobalAttemptIndex");
    assert_eq!(row.authority_arm, "GlobalAuthorizationDecisionRecord");
    assert_eq!(
        row.authority_evidence_target_schema_id.as_deref(),
        Some("GlobalAuthorizationDecisionRecord")
    );
    assert_eq!(row.terminal_audit_freeze_arm, "Forbidden");
    assert_eq!(
        row.terminal_audit_gate_arm,
        "LifecycleScaffoldingNotRequired"
    );
    assert_eq!(row.publication_mode, "SinglePlane");
    assert_eq!(
        row.handler_symbol,
        "fgdb_apply::meta::global_prepare_admission_spec"
    );
    assert_eq!(
        row.consumed_state_slots,
        [
            "SemanticPayload|Meta|global_attempt_index_root",
            "SemanticPayload|Meta|global_conflict_index_ref",
            "SemanticPayload|Meta|global_constraint_reservation_index_root",
            "SemanticPayload|Meta|global_outcome_directory_root",
            "SemanticPayload|Meta|global_statement_index_root",
            "SemanticPayload|Meta|resource_ledger_root",
        ]
    );
    assert_eq!(
        row.written_state_slots,
        [
            "SemanticPayload|Meta|global_attempt_index_root",
            "SemanticPayload|Meta|global_conflict_index_ref",
            "SemanticPayload|Meta|global_constraint_reservation_index_root",
            "SemanticPayload|Meta|global_outcome_directory_root",
            "SemanticPayload|Meta|resource_ledger_root",
            "SemanticPayload|Meta|terminal_admission_fence",
        ]
    );
    for required in [
        "one contiguous terminal statement and delivery frontier",
        "no prior admission or terminal",
        "the Armed terminal fence",
        "without assigning a future sequence or HLC",
    ] {
        assert!(row.sequence_effects.contains(required));
    }
    assert_eq!(row.status, "reserved");

    let exact_ordinal: Vec<_> = registry
        .contracts
        .iter()
        .filter(|candidate| {
            candidate.role == "Meta"
                && candidate.outer_command_union == "GlobalSequenceNeutralSpec<Tag>"
                && candidate.outer_wire_tag == 0x000b
        })
        .map(|candidate| candidate.command_contract_id.as_str())
        .collect();
    assert_eq!(exact_ordinal, ["cc:meta:global-prepare-admission-spec"]);
}

/// The Global read-close member is armless even though its body has two modes.
/// Pin both its terminal state effects and the autocommit cross-plane detach so
/// a Local row, a write terminal, or an explicit-close-only implementation
/// cannot occupy ordinal 12.
#[test]
fn meta_f7_read_close_contract_is_exact() {
    let registry = registry();
    let rows: Vec<_> = registry
        .contracts
        .iter()
        .filter(|row| row.command_contract_id == "cc:meta:global-read-close-spec")
        .collect();
    assert_eq!(rows.len(), 1, "Meta read close must classify once");
    let row = rows[0];
    assert_eq!(row.role, "Meta");
    assert_eq!(row.outer_command_union, "GlobalSequenceNeutralSpec<Tag>");
    assert_eq!(row.outer_wire_tag, 0x000c);
    assert_eq!(row.input_wire_tag, 0x000c);
    assert_eq!(row.inner_wire_tag, None);
    assert_eq!(row.input_schema_id, "GlobalReadCloseSpec");
    assert_eq!(row.body_schema_id, "GlobalReadCloseSpec");
    assert_eq!(row.result_schema_id, "GlobalTxnOutcomeRecord");
    assert_eq!(row.applied_record_schema_id, "GlobalTxnOutcomeRecord");
    assert_eq!(row.expected_state_schema_id, "WeakStateIdentity");
    assert_eq!(row.authority_arm, "AuthorityBoundHeader<Meta>");
    assert_eq!(row.authority_evidence_target_schema_id, None);
    assert_eq!(row.terminal_audit_freeze_arm, "Forbidden");
    assert_eq!(row.terminal_audit_gate_arm, "TerminalAuditGate");
    assert_eq!(row.publication_mode, "AtomicProtocolDetach");
    assert_eq!(
        row.handler_symbol,
        "fgdb_apply::meta::global_read_close_spec"
    );
    assert_eq!(
        row.consumed_state_slots,
        [
            "Protocol|Meta|result_independent_retention_index",
            "SemanticPayload|Meta|audit_ticket_index_root",
            "SemanticPayload|Meta|global_attempt_index_root",
            "SemanticPayload|Meta|global_conflict_index_ref",
            "SemanticPayload|Meta|global_outcome_directory_root",
            "SemanticPayload|Meta|global_statement_index_root",
            "SemanticPayload|Meta|resource_ledger_root",
        ]
    );
    assert_eq!(
        row.written_state_slots,
        [
            "Protocol|Meta|result_independent_retention_index",
            "SemanticPayload|Meta|audit_ticket_index_root",
            "SemanticPayload|Meta|global_attempt_index_root",
            "SemanticPayload|Meta|global_conflict_index_ref",
            "SemanticPayload|Meta|global_outcome_directory_root",
            "SemanticPayload|Meta|resource_ledger_root",
        ]
    );
    assert_eq!(
        row.checkpoint_floor_classes,
        ["txn-attempt", "result-delivery"]
    );
    assert_eq!(
        row.backup_restore_gc_classes,
        ["txn-lifecycle", "protocol-result-retention"]
    );
    for required in [
        "operation_class ReadOnly",
        "advances GlobalLogicalCommandSeq and HLC but never GlobalCommitSeq",
        "installs ReadClosed",
        "without waiting for activation or ACK",
    ] {
        assert!(row.sequence_effects.contains(required));
    }
    assert_eq!(row.status, "reserved");

    let exact_ordinal: Vec<_> = registry
        .contracts
        .iter()
        .filter(|candidate| {
            candidate.role == "Meta"
                && candidate.outer_command_union == "GlobalSequenceNeutralSpec<Tag>"
                && candidate.outer_wire_tag == 0x000c
        })
        .map(|candidate| candidate.command_contract_id.as_str())
        .collect();
    assert_eq!(exact_ordinal, ["cc:meta:global-read-close-spec"]);
}

/// Freeze the complete F13 reservation, not merely its population. These
/// literals are independent of the TOML rows: deleting a member, moving one
/// to another tag/plane, weakening its authority/result, inventing an inner
/// arm, or routing it through another state class must red this test.
#[test]
fn f13_key_destroy_contracts_are_exact() {
    let registry = registry();
    let expected = [
        (
            "cc:local:key-destroy-authorize-spec",
            0x003f,
            "KeyDestroyAuthorizeSpec",
            "KeyReferenceQuarantine",
            "ExternalKeyDestructionOperationRecord",
            "WeakStateIdentity",
            "KeyDestructionAuthorization",
            "fgdb_apply::local::key_destroy_authorize_spec",
        ),
        (
            "cc:local:key-destroy-finalize-spec",
            0x0040,
            "KeyDestroyFinalizeSpec",
            "KeyDestroyRecord",
            "KeyDestroyRecord",
            "KeyReferenceQuarantine",
            "PortableKeyDestructionDispatchTerminalEvidence",
            "fgdb_apply::local::key_destroy_finalize_spec",
        ),
        (
            "cc:local:key-destroy-certificate-publish-spec",
            0x0041,
            "KeyDestroyCertificatePublishSpec",
            "SourceUnspelled",
            "SourceUnspelled",
            "WeakStateIdentity",
            "SameGroupCertificateHeader",
            "fgdb_apply::local::key_destroy_certificate_publish_spec",
        ),
    ];

    for (id, tag, input, result, applied, state, authority, handler) in expected {
        let row = registry
            .contracts
            .iter()
            .find(|row| row.command_contract_id == id)
            .expect("exact F13 table must resolve every contract row");
        assert_eq!(row.role, "Local", "{id} role drifted");
        assert_eq!(
            row.outer_command_union, "SequenceNeutralSpec<Tag>",
            "{id} union drifted"
        );
        assert_eq!(row.outer_wire_tag, tag, "{id} outer tag drifted");
        assert_eq!(row.input_wire_tag, tag, "{id} input tag drifted");
        assert_eq!(row.inner_wire_tag, None, "{id} must remain armless");
        assert_eq!(row.input_schema_id, input, "{id} input drifted");
        assert_eq!(row.body_schema_id, input, "{id} body drifted");
        assert_eq!(row.result_schema_id, result, "{id} result drifted");
        assert_eq!(
            row.applied_record_schema_id, applied,
            "{id} applied record drifted"
        );
        assert_eq!(
            row.expected_state_schema_id, state,
            "{id} expected state drifted"
        );
        assert_eq!(row.authority_arm, authority, "{id} authority drifted");
        assert_eq!(row.handler_symbol, handler, "{id} handler drifted");
        assert_eq!(row.transition_class, "Semantic", "{id} plane drifted");
        assert_eq!(row.publication_mode, "SinglePlane", "{id} mode drifted");
        assert_eq!(row.terminal_audit_gate_arm, "TerminalAuditGate");
        assert_eq!(
            row.consumed_state_slots,
            ["SemanticPayload|Local|key_lifecycle_state"]
        );
        assert_eq!(
            row.written_state_slots,
            ["SemanticPayload|Local|key_lifecycle_state"]
        );
        assert_eq!(row.status, "reserved", "{id} activated prematurely");
    }
}

/// Freeze the complete F14 reservation, including the Appendix-pinned arm
/// tags. The literal table is independent of the TOML so deleting an arm,
/// swapping Completed/Cancelled, laundering physical work into Semantic, or
/// weakening the Local cancellation authority reds this test.
#[test]
fn f14_semantic_gc_contracts_are_exact() {
    let registry = registry();
    let expected = [
        (
            "cc:local:local-gc-authorize-spec",
            0x0042,
            None,
            "LocalGcAuthorizeSpec",
            "GcDecisionRecord",
            "GcDecisionRecord",
            "WeakStateIdentity",
            "SourceUnspelled",
            None,
            "fgdb_apply::local::local_gc_authorize_spec",
        ),
        (
            "cc:local:local-gc-apply-quarantine-spec",
            0x0043,
            None,
            "LocalGcApplyQuarantineSpec",
            "LocalGcReclaimAuthorizationRecord",
            "GcSemanticState<Local>",
            "WeakStateIdentity",
            "GcDecisionRecordDecision::Accepted",
            Some("GcDecisionRecord"),
            "fgdb_apply::local::local_gc_apply_quarantine_spec",
        ),
        (
            "cc:local:local-gc-cancellation-authorize-spec",
            0x0044,
            None,
            "LocalGcCancellationAuthorizeSpec",
            "LocalGcCancellationAuthorizationRecord",
            "LocalGcCancellationAuthorizationRecord",
            "GcSemanticState<Local>",
            "PortableGcPhysicalTerminalEvidence",
            Some("PortableGcPhysicalTerminalEvidence"),
            "fgdb_apply::local::local_gc_cancellation_authorize_spec",
        ),
        (
            "cc:local:gc-physical-disposition-import-spec:completed",
            0x0045,
            Some(0x0001),
            "GcPhysicalDispositionImportSpec<Local>",
            "GcSemanticState<Local>",
            "GcSemanticState<Local>",
            "GcSemanticState<Local>",
            "PortableGcPhysicalTerminalEvidence",
            Some("PortableGcPhysicalTerminalEvidence"),
            "fgdb_apply::local::gc_physical_disposition_import_spec::completed",
        ),
        (
            "cc:local:gc-physical-disposition-import-spec:cancelled",
            0x0045,
            Some(0x0002),
            "GcPhysicalDispositionImportSpec<Local>",
            "GcSemanticState<Local>",
            "GcSemanticState<Local>",
            "GcSemanticState<Local>",
            "GcCancellationAuthorityFor<Local>::LocalStandalone",
            Some("LocalGcCancellationAuthorizationRecord"),
            "fgdb_apply::local::gc_physical_disposition_import_spec::cancelled",
        ),
    ];

    for (id, outer, inner, input, result, applied, state, authority, target, handler) in expected {
        let row = registry
            .contracts
            .iter()
            .find(|row| row.command_contract_id == id)
            .expect("exact F14 table must resolve every contract row");
        assert_eq!(row.role, "Local", "{id} role drifted");
        assert_eq!(row.outer_command_union, "SequenceNeutralSpec<Tag>");
        assert_eq!(row.outer_wire_tag, outer, "{id} outer tag drifted");
        assert_eq!(row.input_wire_tag, outer, "{id} input tag drifted");
        assert_eq!(row.inner_wire_tag, inner, "{id} inner tag drifted");
        assert_eq!(row.input_schema_id, input, "{id} input drifted");
        assert_eq!(row.body_schema_id, input, "{id} body drifted");
        assert_eq!(row.result_schema_id, result, "{id} result drifted");
        assert_eq!(
            row.applied_record_schema_id, applied,
            "{id} applied drifted"
        );
        assert_eq!(row.expected_state_schema_id, state, "{id} state drifted");
        assert_eq!(row.authority_arm, authority, "{id} authority drifted");
        assert_eq!(
            row.authority_evidence_target_schema_id.as_deref(),
            target,
            "{id} authority target drifted"
        );
        assert_eq!(row.handler_symbol, handler, "{id} handler drifted");
        assert_eq!(row.transition_class, "Semantic", "{id} plane drifted");
        assert_eq!(row.publication_mode, "SinglePlane", "{id} mode drifted");
        assert_eq!(
            row.consumed_state_slots,
            ["SemanticPayload|Local|gc_semantic_state"]
        );
        assert_eq!(
            row.written_state_slots,
            ["SemanticPayload|Local|gc_semantic_state"]
        );
        assert_eq!(row.status, "reserved", "{id} activated prematurely");
    }
}

/// Freeze the first Local backup cohort as one exact family increment. The
/// table is intentionally independent of the TOML: deleting/reordering a
/// member, inventing an inner arm, crossing Semantic into Protocol dispatch,
/// or losing the sole backup-registry writer binding must red this test.
#[test]
fn f15a_local_backup_contracts_are_exact() {
    let registry = registry();
    let expected = [
        (
            "cc:local:local-backup-barrier-spec",
            0x0046,
            "LocalBackupBarrierSpec",
            "LocalBackupPinRecord",
            "LocalBackupPinRecord",
            "LogicalStatePayload",
            "SourceUnspelled",
            None,
            "TerminalAuditGate",
            "fgdb_apply::local::local_backup_barrier_spec",
        ),
        (
            "cc:local:local-backup-closure-publish-spec",
            0x0047,
            "LocalBackupClosurePublishSpec",
            "LocalBackupClosureCertificate",
            "LocalBackupClosureCertificate",
            "SourceUnspelled",
            "SameGroupCertificateHeader",
            Some("LocalBackupClosureCertificate"),
            "SourceUnspelled",
            "fgdb_apply::local::local_backup_closure_publish_spec",
        ),
        (
            "cc:local:local-backup-seal-spec",
            0x0048,
            "LocalBackupSealSpec",
            "BackupManifest",
            "BackupManifest",
            "SourceUnspelled",
            "BackupManifestSignatureSet",
            Some("BackupManifest"),
            "SourceUnspelled",
            "fgdb_apply::local::local_backup_seal_spec",
        ),
        (
            "cc:local:local-backup-publication-authorize-spec",
            0x0049,
            "LocalBackupPublicationAuthorizeSpec",
            "BackupExternalOperationRecord<Local>",
            "BackupExternalOperationRecord<Local>",
            "SourceUnspelled",
            "AuthorityBoundHeader<Local>",
            Some("BackupExternalOperationRecord<Local>"),
            "TerminalAuditGate",
            "fgdb_apply::local::local_backup_publication_authorize_spec",
        ),
        (
            "cc:local:local-backup-publication-receipt-import-spec",
            0x004a,
            "LocalBackupPublicationReceiptImportSpec",
            "BackupPublicationReceipt",
            "SourceUnspelled",
            "SourceUnspelled",
            "PortableBackupExternalOperationTerminalEvidence",
            Some("BackupPublicationReceipt"),
            "SourceUnspelled",
            "fgdb_apply::local::local_backup_publication_receipt_import_spec",
        ),
        (
            "cc:local:local-backup-grant-issue-import-spec",
            0x004b,
            "LocalBackupGrantIssueImportSpec",
            "ArchiveGrantAuthorityProjection<Local>",
            "ArchiveGrantAuthorityProjection<Local>",
            "SourceUnspelled",
            "PortableBackupExternalOperationTerminalEvidence",
            Some("ArchiveRetentionGrant"),
            "TerminalAuditGate",
            "fgdb_apply::local::local_backup_grant_issue_import_spec",
        ),
        (
            "cc:local:local-backup-artifact-verify-spec",
            0x004c,
            "LocalBackupArtifactVerifySpec",
            "BackupArtifactReopenProof<Local>",
            "SourceUnspelled",
            "SourceUnspelled",
            "ArchiveGrantUseGuard<Local>",
            Some("BackupArtifactReopenProof<Local>"),
            "SourceUnspelled",
            "fgdb_apply::local::local_backup_artifact_verify_spec",
        ),
        (
            "cc:local:local-backup-release-spec",
            0x004d,
            "LocalBackupReleaseSpec",
            "ArchiveSourceReleaseEvidence::Local",
            "ArchiveSourceReleaseCompletionOperationRecord",
            "SourceUnspelled",
            "ArchiveGrantAuthorityObservationImport<Local>",
            Some("ArchiveSourceReleaseHold"),
            "SourceUnspelled",
            "fgdb_apply::local::local_backup_release_spec",
        ),
        (
            "cc:local:archive-source-release-completion-import-spec",
            0x004e,
            "ArchiveSourceReleaseCompletionImportSpec<Local>",
            "ArchiveGrantAuthorityProjection<Local>",
            "ArchiveGrantAuthorityProjection<Local>",
            "SourceUnspelled",
            "PortableArchiveSourceReleaseCompletionTerminalEvidence",
            Some("ArchiveSourceReleaseCompletionReceipt"),
            "TerminalAuditGate",
            "fgdb_apply::local::archive_source_release_completion_import_spec",
        ),
        (
            "cc:local:local-backup-abort-spec",
            0x004f,
            "LocalBackupAbortSpec",
            "SourceUnspelled",
            "SourceUnspelled",
            "SourceUnspelled",
            "BackupNoPublicationProof<Local>",
            Some("BackupStagingCleanupCompletion<Local>"),
            "TerminalAuditGate",
            "fgdb_apply::local::local_backup_abort_spec",
        ),
    ];

    for (id, tag, input, result, applied, state, authority, target, gate, handler) in expected {
        let row = registry
            .contracts
            .iter()
            .find(|row| row.command_contract_id == id)
            .expect("exact F15A table must resolve every contract row");
        assert_eq!(row.role, "Local", "{id} role drifted");
        assert_eq!(row.outer_command_union, "SequenceNeutralSpec<Tag>");
        assert_eq!(row.outer_wire_tag, tag, "{id} outer tag drifted");
        assert_eq!(row.input_wire_tag, tag, "{id} input tag drifted");
        assert_eq!(row.inner_wire_tag, None, "{id} must remain armless");
        assert_eq!(row.input_schema_id, input, "{id} input drifted");
        assert_eq!(row.body_schema_id, input, "{id} body drifted");
        assert_eq!(row.result_schema_id, result, "{id} result drifted");
        assert_eq!(
            row.applied_record_schema_id, applied,
            "{id} applied drifted"
        );
        assert_eq!(row.expected_state_schema_id, state, "{id} state drifted");
        assert_eq!(row.authority_arm, authority, "{id} authority drifted");
        assert_eq!(
            row.authority_evidence_target_schema_id.as_deref(),
            target,
            "{id} authority target drifted"
        );
        assert_eq!(row.terminal_audit_gate_arm, gate, "{id} gate drifted");
        assert_eq!(row.handler_symbol, handler, "{id} handler drifted");
        assert_eq!(row.transition_class, "Semantic", "{id} plane drifted");
        assert_eq!(row.publication_mode, "SinglePlane", "{id} mode drifted");
        assert_eq!(
            row.consumed_state_slots,
            ["SemanticPayload|Local|backup_registry_root"]
        );
        assert_eq!(
            row.written_state_slots,
            ["SemanticPayload|Local|backup_registry_root"]
        );
        assert_eq!(row.checkpoint_floor_classes, ["semantic-backup"]);
        assert_eq!(row.backup_restore_gc_classes, ["semantic-backup"]);
        assert_eq!(row.posture_feature_predicate, "local");
        assert_eq!(row.status, "reserved", "{id} activated prematurely");
    }
}

/// Freeze the exact Local restore cohort after F15A. This table independently
/// pins dense order, the single role-valid RestoreAbandonSpec arm, semantic
/// plane, state root, authority evidence, and forward-declared handler seam.
#[test]
fn f15b_local_restore_contracts_are_exact() {
    let registry = registry();
    assert_eq!(
        registry
            .contracts
            .iter()
            .filter(|row| {
                row.role == "Local" && (0x0050..=0x0056).contains(&row.outer_wire_tag)
            })
            .count(),
        7,
        "the F15B outer-tag interval must contain exactly its seven role-valid rows"
    );
    let expected = [
        (
            "cc:local:local-restore-activation-spec",
            0x0050,
            None,
            "LocalRestoreActivationSpec",
            "LocalRestoreActivationCertificate",
            "LocalRestoreRegistryValue",
            "WeakStateIdentity",
            "SameGroupCertificateHeader",
            Some("LocalRestoreReadyCertificate"),
            "TerminalAuditGate",
            "fgdb_apply::local::local_restore_activation_spec",
        ),
        (
            "cc:local:local-restore-service-prepare-spec",
            0x0051,
            None,
            "LocalRestoreServicePrepareSpec",
            "LocalRestoreServicePrepareCertificate",
            "LocalRestoreRegistryValue",
            "WeakStateIdentity",
            "SameGroupCertificateHeader",
            Some("LocalRestoreActivationCertificate"),
            "TerminalAuditGate",
            "fgdb_apply::local::local_restore_service_prepare_spec",
        ),
        (
            "cc:local:local-restore-service-promotion-spec",
            0x0052,
            None,
            "LocalRestoreServicePromotionSpec",
            "RestorePromotionRootSeal<Local>",
            "LocalRestoreRegistryValue",
            "WeakStateIdentity",
            "RestoreServicePromotionReceipt",
            Some("RestoreServicePromotionManifest"),
            "TerminalAuditGate",
            "fgdb_apply::local::local_restore_service_promotion_spec",
        ),
        (
            "cc:local:local-restore-service-completion-spec",
            0x0053,
            None,
            "LocalRestoreServiceCompletionSpec",
            "PortableSemanticVisibilityCertificate<Local>",
            "LocalRestoreRegistryValue",
            "LocalRestoreRegistryValue",
            "LocalRestoreIndependentReopenProof",
            Some("RestorePromotionRootSeal<Local>"),
            "TerminalAuditGate",
            "fgdb_apply::local::local_restore_service_completion_spec",
        ),
        (
            "cc:local:local-restore-abandon-finalize-spec",
            0x0054,
            None,
            "LocalRestoreAbandonFinalizeSpec",
            "AuthorityOwningRestoreAbandonmentTombstone<Local>",
            "LocalRestoreRegistryValue",
            "LocalRestoreRegistryValue",
            "RestoreAbandonOperationRecord<Local>",
            Some("RestoreAbandonmentReceipt"),
            "TerminalAuditGate",
            "fgdb_apply::local::local_restore_abandon_finalize_spec",
        ),
        (
            "cc:local:local-restore-abandonment-pin-install-spec",
            0x0055,
            None,
            "LocalRestoreAbandonmentPinInstallSpec",
            "RestoreTerminalPinBasis<Local>",
            "LocalRestoreRegistryValue",
            "LocalRestoreRegistryValue",
            "RestoreTerminalPinDurabilityReceipt<Local,Abandoned>",
            Some("RestoreTerminalPhysicalInventory<Local,Abandoned>"),
            "SourceUnspelled",
            "fgdb_apply::local::local_restore_abandonment_pin_install_spec",
        ),
        (
            "cc:local:restore-abandon-spec:local",
            0x0056,
            Some(0x0001),
            "RestoreAbandonSpec",
            "RestoreAbandonOperationRecord<Local>",
            "LocalRestoreRegistryValue",
            "WeakStateIdentity",
            "RestoreAbandonAuthorityProfile<Local>",
            Some("RestoreNoTargetObservationProof<Local>"),
            "TerminalAuditGate",
            "fgdb_apply::local::restore_abandon_spec::local",
        ),
    ];

    for (id, tag, inner, input, result, applied, state, authority, target, gate, handler) in
        expected
    {
        let row = registry
            .contracts
            .iter()
            .find(|row| row.command_contract_id == id)
            .expect("exact F15B table must resolve every contract row");
        assert_eq!(row.role, "Local", "{id} role drifted");
        assert_eq!(row.outer_command_union, "SequenceNeutralSpec<Tag>");
        assert_eq!(row.outer_wire_tag, tag, "{id} outer tag drifted");
        assert_eq!(row.input_wire_tag, tag, "{id} input tag drifted");
        assert_eq!(row.inner_wire_tag, inner, "{id} inner tag drifted");
        assert_eq!(row.input_schema_id, input, "{id} input drifted");
        assert_eq!(
            row.body_schema_id,
            if inner.is_some() {
                "RestoreAbandonSpec::Local"
            } else {
                input
            },
            "{id} body drifted"
        );
        assert_eq!(row.result_schema_id, result, "{id} result drifted");
        assert_eq!(
            row.applied_record_schema_id, applied,
            "{id} applied drifted"
        );
        assert_eq!(row.expected_state_schema_id, state, "{id} state drifted");
        assert_eq!(row.authority_arm, authority, "{id} authority drifted");
        assert_eq!(
            row.authority_evidence_target_schema_id.as_deref(),
            target,
            "{id} authority target drifted"
        );
        assert_eq!(row.terminal_audit_gate_arm, gate, "{id} gate drifted");
        assert_eq!(row.handler_symbol, handler, "{id} handler drifted");
        assert_eq!(row.transition_class, "Semantic", "{id} plane drifted");
        assert_eq!(row.publication_mode, "SinglePlane", "{id} mode drifted");
        assert_eq!(
            row.consumed_state_slots,
            ["SemanticPayload|Local|restore_registry_root"]
        );
        assert_eq!(
            row.written_state_slots,
            ["SemanticPayload|Local|restore_registry_root"]
        );
        assert_eq!(row.checkpoint_floor_classes, ["semantic-restore"]);
        assert_eq!(row.backup_restore_gc_classes, ["semantic-restore"]);
        assert_eq!(row.posture_feature_predicate, "local");
        assert_eq!(row.status, "reserved", "{id} activated prematurely");
    }
}

/// Freeze the four armless DirectoryBound members after F15B. Besides dense
/// tags, this pins the two post-publication receipt types and the asymmetric
/// abandonment law: apply cannot claim its future receipt/tombstone, whereas
/// receipt import alone may materialize the tombstone.
#[test]
fn f15c_directory_bound_restore_contracts_are_exact() {
    let registry = registry();
    assert_eq!(
        registry
            .contracts
            .iter()
            .filter(|row| {
                row.role == "Local" && (0x0057..=0x005a).contains(&row.outer_wire_tag)
            })
            .count(),
        4,
        "the F15C outer-tag interval must contain exactly its four armless rows"
    );
    let expected = [
        (
            "cc:local:directory-bound-enter-promotion-pending-spec",
            0x0057,
            "DirectoryBoundEnterPromotionPendingSpec",
            "DirectoryBoundPromotionPendingReceipt",
            "LocalRestoreRegistryValue",
            "WeakStateIdentity",
            "DirectoryBoundCreationEvidence<Local>",
            Some("DirectoryBoundCreationEvidence<Local>"),
            "SourceUnspelled",
            "fgdb_apply::local::directory_bound_enter_promotion_pending_spec",
        ),
        (
            "cc:local:directory-bound-finalize-operational-authority-spec",
            0x0058,
            "DirectoryBoundFinalizeOperationalAuthoritySpec",
            "DirectoryBoundOperationalAuthorityReceipt",
            "LocalRestoreRegistryValue",
            "LocalRestoreRegistryValue",
            "DirectoryBoundCreationEvidence<Local>",
            Some("DirectoryBoundPromotionPendingReceipt"),
            "SourceUnspelled",
            "fgdb_apply::local::directory_bound_finalize_operational_authority_spec",
        ),
        (
            "cc:local:directory-bound-abandon-apply-spec",
            0x0059,
            "DirectoryBoundAbandonApplySpec",
            "SourceUnspelled",
            "LocalRestoreRegistryValue",
            "LocalRestoreRegistryValue",
            "AuthorityBoundHeader<Local>",
            Some("RestoreAbandonOperationRecord<Local>"),
            "TerminalAuditGate",
            "fgdb_apply::local::directory_bound_abandon_apply_spec",
        ),
        (
            "cc:local:directory-bound-abandon-receipt-import-spec",
            0x005a,
            "DirectoryBoundAbandonReceiptImportSpec",
            "AuthorityOwningRestoreAbandonmentTombstone<Local>",
            "LocalRestoreRegistryValue",
            "LocalRestoreRegistryValue",
            "DirectoryBoundAbandonmentReceipt",
            Some("DirectoryBoundAbandonmentReceipt"),
            "SourceUnspelled",
            "fgdb_apply::local::directory_bound_abandon_receipt_import_spec",
        ),
    ];

    for (id, tag, input, result, applied, state, authority, target, gate, handler) in expected {
        let row = registry
            .contracts
            .iter()
            .find(|row| row.command_contract_id == id)
            .expect("exact F15C table must resolve every contract row");
        assert_eq!(row.role, "Local", "{id} role drifted");
        assert_eq!(row.outer_command_union, "SequenceNeutralSpec<Tag>");
        assert_eq!(row.outer_wire_tag, tag, "{id} outer tag drifted");
        assert_eq!(row.input_wire_tag, tag, "{id} input tag drifted");
        assert_eq!(row.inner_wire_tag, None, "{id} invented an inner arm");
        assert_eq!(row.input_schema_id, input, "{id} input drifted");
        assert_eq!(row.body_schema_id, input, "{id} body drifted");
        assert_eq!(row.result_schema_id, result, "{id} result drifted");
        assert_eq!(
            row.applied_record_schema_id, applied,
            "{id} applied drifted"
        );
        assert_eq!(row.expected_state_schema_id, state, "{id} state drifted");
        assert_eq!(row.authority_arm, authority, "{id} authority drifted");
        assert_eq!(
            row.authority_evidence_target_schema_id.as_deref(),
            target,
            "{id} authority target drifted"
        );
        assert_eq!(row.terminal_audit_gate_arm, gate, "{id} gate drifted");
        assert_eq!(row.handler_symbol, handler, "{id} handler drifted");
        assert_eq!(row.transition_class, "Semantic", "{id} plane drifted");
        assert_eq!(row.publication_mode, "SinglePlane", "{id} mode drifted");
        assert_eq!(
            row.consumed_state_slots,
            ["SemanticPayload|Local|restore_registry_root"]
        );
        assert_eq!(
            row.written_state_slots,
            ["SemanticPayload|Local|restore_registry_root"]
        );
        assert_eq!(row.checkpoint_floor_classes, ["semantic-restore"]);
        assert_eq!(row.backup_restore_gc_classes, ["semantic-restore"]);
        assert_eq!(row.posture_feature_predicate, "local-directory-bound");
        assert_eq!(row.status, "reserved", "{id} activated prematurely");
    }

    let apply = registry
        .contracts
        .iter()
        .find(|row| row.command_contract_id == "cc:local:directory-bound-abandon-apply-spec")
        .expect("DirectoryBound abandon apply row exists");
    assert_eq!(apply.result_schema_id, "SourceUnspelled");
    assert!(
        !apply
            .sequence_effects
            .contains("materializes the Local authority-owning tombstone")
    );
    assert!(
        apply
            .sequence_effects
            .contains("never the future tombstone")
    );

    let import = registry
        .contracts
        .iter()
        .find(|row| {
            row.command_contract_id == "cc:local:directory-bound-abandon-receipt-import-spec"
        })
        .expect("DirectoryBound abandon receipt-import row exists");
    assert_eq!(
        import.result_schema_id,
        "AuthorityOwningRestoreAbandonmentTombstone<Local>"
    );
    assert!(
        import
            .sequence_effects
            .contains("materializes the Local authority-owning tombstone")
    );
}

/// Freeze the first structurally Local authority-owning cleanup cohort after
/// F15C. The literal table pins the dense tag order, authority/evidence
/// pairing, and distinct cleanup versus pre-Arm renew/release terminal laws.
#[test]
fn f15d_authority_owning_restore_lease_contracts_are_exact() {
    let registry = registry();
    assert_eq!(
        registry
            .contracts
            .iter()
            .filter(|row| {
                row.role == "Local" && (0x005b..=0x005d).contains(&row.outer_wire_tag)
            })
            .count(),
        3,
        "the F15D outer-tag interval must contain exactly its three armless rows"
    );
    let expected = [
        (
            "cc:local:restore-source-key-access-cleanup-finalize-spec",
            0x005b,
            "RestoreSourceKeyAccessCleanupFinalizeSpec<Local>",
            "RestoreSourceKeyAccessCleanupRecord<Local>",
            "RestoreSourceKeyAccessCleanupProgress<Local>",
            "fgdb_apply::local::restore_source_key_access_cleanup_finalize_spec",
        ),
        (
            "cc:local:restore-source-lease-renew-authorized-never-armed-finalize-spec",
            0x005c,
            "RestoreSourceLeaseRenewAuthorizedNeverArmedFinalizeSpec<Local>",
            "RestoreLeaseOperationTerminalRecord<Local,Renew>",
            "RestoreSourceLeaseRenewOperationRecord<Local>",
            "fgdb_apply::local::restore_source_lease_renew_authorized_never_armed_finalize_spec",
        ),
        (
            "cc:local:restore-source-lease-release-authorized-never-armed-finalize-spec",
            0x005d,
            "RestoreSourceLeaseReleaseAuthorizedNeverArmedFinalizeSpec<Local>",
            "RestoreLeaseOperationTerminalRecord<Local,Release>",
            "RestoreSourceLeaseReleaseOperationRecord<Local>",
            "fgdb_apply::local::restore_source_lease_release_authorized_never_armed_finalize_spec",
        ),
    ];

    for (id, tag, input, result, authority_target, handler) in expected {
        let row = registry
            .contracts
            .iter()
            .find(|row| row.command_contract_id == id)
            .expect("exact F15D table must resolve every contract row");
        assert_eq!(row.role, "Local", "{id} role drifted");
        assert_eq!(row.outer_command_union, "SequenceNeutralSpec<Tag>");
        assert_eq!(row.outer_wire_tag, tag, "{id} outer tag drifted");
        assert_eq!(row.input_wire_tag, tag, "{id} input tag drifted");
        assert_eq!(row.inner_wire_tag, None, "{id} invented an inner arm");
        assert_eq!(row.input_schema_id, input, "{id} input drifted");
        assert_eq!(row.body_schema_id, input, "{id} body drifted");
        assert_eq!(row.result_schema_id, result, "{id} result drifted");
        assert_eq!(
            row.applied_record_schema_id, "LocalRestoreRegistryValue",
            "{id} applied record drifted"
        );
        assert_eq!(
            row.expected_state_schema_id, "LocalRestoreRegistryValue",
            "{id} expected state drifted"
        );
        assert_eq!(row.authority_arm, "AuthorityBoundHeader<Local>");
        assert_eq!(
            row.authority_evidence_target_schema_id.as_deref(),
            Some(authority_target),
            "{id} authority target drifted"
        );
        assert_eq!(row.terminal_audit_freeze_arm, "SourceUnspelled");
        assert_eq!(row.terminal_audit_gate_arm, "SourceUnspelled");
        assert_eq!(row.handler_symbol, handler, "{id} handler drifted");
        assert_eq!(row.transition_class, "Semantic", "{id} plane drifted");
        assert_eq!(row.publication_mode, "SinglePlane", "{id} mode drifted");
        assert_eq!(
            row.consumed_state_slots,
            ["SemanticPayload|Local|restore_registry_root"]
        );
        assert_eq!(
            row.written_state_slots,
            ["SemanticPayload|Local|restore_registry_root"]
        );
        assert_eq!(row.checkpoint_floor_classes, ["semantic-restore"]);
        assert_eq!(row.backup_restore_gc_classes, ["semantic-restore"]);
        assert_eq!(row.posture_feature_predicate, "local");
        assert_eq!(row.status, "reserved", "{id} activated prematurely");
    }

    let cleanup = registry
        .contracts
        .iter()
        .find(|row| {
            row.command_contract_id == "cc:local:restore-source-key-access-cleanup-finalize-spec"
        })
        .expect("cleanup finalizer row exists");
    for required in [
        "zero unresolved renewal",
        "cleanup accumulator",
        "cleanup record",
        "AwaitingSourceRelease",
        "RenewalClosed",
        "terminal pin basis",
    ] {
        assert!(
            cleanup.sequence_effects.contains(required),
            "cleanup law lost {required:?}"
        );
    }

    let renew = registry
        .contracts
        .iter()
        .find(|row| {
            row.command_contract_id
                == "cc:local:restore-source-lease-renew-authorized-never-armed-finalize-spec"
        })
        .expect("renew AuthorizedNeverArmed row exists");
    for required in [
        "zero-attempt-membership proof",
        "AuthorizedNeverArmed",
        "unchanged lease to Current",
        "without inventing DispatchNeverSentProof",
    ] {
        assert!(
            renew.sequence_effects.contains(required),
            "renew law lost {required:?}"
        );
    }

    let release = registry
        .contracts
        .iter()
        .find(|row| {
            row.command_contract_id
                == "cc:local:restore-source-lease-release-authorized-never-armed-finalize-spec"
        })
        .expect("release AuthorizedNeverArmed row exists");
    for required in [
        "zero-attempt-membership proof",
        "AuthorizedNeverArmed",
        "unchanged RenewalClosed lease state",
        "lease expiry alone never forces this path",
    ] {
        assert!(
            release.sequence_effects.contains(required),
            "release law lost {required:?}"
        );
    }
}

/// F15E continues the owner-frozen structural Local cohort in exact source
/// order. Pin the complete three-row interval and the load-bearing distinction
/// between releasing a terminal pin and authorizing/importing one source-key
/// revocation. Deletion, tag drift, slot elision, or treating authorization as
/// terminal evidence must all red this selector.
#[test]
fn f15e_terminal_pin_and_source_cleanup_contracts_are_exact() {
    let registry = registry();
    let interval: Vec<_> = registry
        .contracts
        .iter()
        .filter(|row| (0x005e..=0x0060).contains(&row.outer_wire_tag))
        .collect();
    assert_eq!(
        interval.len(),
        3,
        "the F15E outer-tag interval must contain exactly three armless rows"
    );

    let expected = [
        (
            "cc:local:restore-terminal-pin-release-finalize-spec",
            0x005e,
            "RestoreTerminalPinReleaseFinalizeSpec<Local>",
            "RestoreTerminalPinIndex<Local>",
            "RestoreTerminalPinReleaseCompletion<Local,RestoreTerminalDisposition>",
            "fgdb_apply::local::restore_terminal_pin_release_finalize_spec",
        ),
        (
            "cc:local:restore-source-key-access-cleanup-authorize-spec",
            0x005f,
            "RestoreSourceKeyAccessCleanupAuthorizeSpec<Local>",
            "RestoreSourceAccessRevocationOperationRecord<Local,ExactResourceKind>",
            "RestoreSourceKeyAccessCleanupProgress<Local>",
            "fgdb_apply::local::restore_source_key_access_cleanup_authorize_spec",
        ),
        (
            "cc:local:restore-source-key-access-cleanup-import-spec",
            0x0060,
            "RestoreSourceKeyAccessCleanupImportSpec<Local>",
            "RestoreSourceKeyAccessCleanupProgress<Local>",
            "RestoreSourceAccessRevocationOperationRecord<Local,ExactResourceKind>",
            "fgdb_apply::local::restore_source_key_access_cleanup_import_spec",
        ),
    ];

    for (id, tag, input, result, authority_target, handler) in expected {
        let row = registry
            .contracts
            .iter()
            .find(|row| row.command_contract_id == id)
            .expect("exact F15E table must resolve every contract row");
        assert_eq!(row.role, "Local", "{id} role drifted");
        assert_eq!(row.outer_command_union, "SequenceNeutralSpec<Tag>");
        assert_eq!(row.outer_wire_tag, tag, "{id} outer tag drifted");
        assert_eq!(row.input_wire_tag, tag, "{id} input tag drifted");
        assert_eq!(row.inner_wire_tag, None, "{id} invented an inner arm");
        assert_eq!(row.input_schema_id, input, "{id} input drifted");
        assert_eq!(row.body_schema_id, input, "{id} body drifted");
        assert_eq!(row.result_schema_id, result, "{id} result drifted");
        assert_eq!(row.applied_record_schema_id, "LocalRestoreRegistryValue");
        assert_eq!(row.expected_state_schema_id, "LocalRestoreRegistryValue");
        assert_eq!(row.authority_arm, "AuthorityBoundHeader<Local>");
        assert_eq!(
            row.authority_evidence_target_schema_id.as_deref(),
            Some(authority_target),
            "{id} authority target drifted"
        );
        assert_eq!(row.terminal_audit_freeze_arm, "SourceUnspelled");
        assert_eq!(row.terminal_audit_gate_arm, "SourceUnspelled");
        assert_eq!(row.handler_symbol, handler, "{id} handler drifted");
        assert_eq!(row.transition_class, "Semantic", "{id} plane drifted");
        assert_eq!(row.publication_mode, "SinglePlane", "{id} mode drifted");
        assert_eq!(row.checkpoint_floor_classes, ["semantic-restore"]);
        assert_eq!(row.backup_restore_gc_classes, ["semantic-restore"]);
        assert_eq!(row.posture_feature_predicate, "local");
        assert_eq!(row.status, "reserved", "{id} activated prematurely");
    }

    let pin = registry
        .contracts
        .iter()
        .find(|row| {
            row.command_contract_id == "cc:local:restore-terminal-pin-release-finalize-spec"
        })
        .expect("terminal-pin finalizer exists");
    assert_eq!(
        pin.consumed_state_slots,
        [
            "SemanticPayload|Local|restore_registry_root",
            "SemanticPayload|Local|retention_map",
        ]
    );
    assert_eq!(pin.written_state_slots, pin.consumed_state_slots);
    for required in [
        "pin-release completion",
        "no-other-owner proof",
        "selects Released",
        "only then permits",
        "never treating authorization or timeout as release completion",
    ] {
        assert!(
            pin.sequence_effects.contains(required),
            "terminal-pin law lost {required:?}"
        );
    }

    for id in [
        "cc:local:restore-source-key-access-cleanup-authorize-spec",
        "cc:local:restore-source-key-access-cleanup-import-spec",
    ] {
        let row = registry
            .contracts
            .iter()
            .find(|row| row.command_contract_id == id)
            .expect("source cleanup row exists");
        assert_eq!(
            row.consumed_state_slots,
            ["SemanticPayload|Local|restore_registry_root"]
        );
        assert_eq!(row.written_state_slots, row.consumed_state_slots);
    }

    let authorize = registry
        .contracts
        .iter()
        .find(|row| {
            row.command_contract_id == "cc:local:restore-source-key-access-cleanup-authorize-spec"
        })
        .expect("cleanup authorize row exists");
    for required in [
        "ExplicitRevocationPending",
        "ExplicitRevocationAuthorized",
        "without inventing an attempt, receipt, provider result or successor progress",
    ] {
        assert!(
            authorize.sequence_effects.contains(required),
            "cleanup-authorize law lost {required:?}"
        );
    }

    let import = registry
        .contracts
        .iter()
        .find(|row| {
            row.command_contract_id == "cc:local:restore-source-key-access-cleanup-import-spec"
        })
        .expect("cleanup import row exists");
    for required in [
        "terminal evidence",
        "ExplicitRevocationTerminal",
        "complete source-resource and required-Shard-closure bijection",
        "timeout, missing import and digest-only done states remain nonterminal",
    ] {
        assert!(
            import.sequence_effects.contains(required),
            "cleanup-import law lost {required:?}"
        );
    }
}

/// F15F continues the owner-frozen structural Local cohort with the complete
/// renewal state machine from plan lines 2393-2395. Pin the distinct recipe,
/// applied-successor, and no-effect results so a generic "renew finalize" row
/// cannot erase the fresh-observation law, mint a successor before dispatch,
/// or turn a zero-effect status into generation g+1.
#[test]
fn f15f_source_lease_renewal_contracts_are_exact() {
    let registry = registry();
    let interval: Vec<_> = registry
        .contracts
        .iter()
        .filter(|row| (0x0061..=0x0063).contains(&row.outer_wire_tag))
        .collect();
    assert_eq!(
        interval.len(),
        3,
        "the F15F outer-tag interval must contain exactly three armless rows"
    );

    let expected = [
        (
            "cc:local:restore-source-lease-renew-authorize-spec",
            0x0061,
            "RestoreSourceLeaseRenewAuthorizeSpec<Local>",
            "RestoreSourceLeaseRenewOperationRecord<Local>",
            "RestoreSourceLeaseAuthorityObservationImport<Local>",
            "TerminalAuditGate",
            "fgdb_apply::local::restore_source_lease_renew_authorize_spec",
        ),
        (
            "cc:local:restore-source-lease-renew-finalize-spec",
            0x0062,
            "RestoreSourceLeaseRenewFinalizeSpec<Local>",
            "RestoreSourceLeaseRecord<Local>",
            "RestoreSourceLeaseRenewOperationRecord<Local>",
            "SourceUnspelled",
            "fgdb_apply::local::restore_source_lease_renew_finalize_spec",
        ),
        (
            "cc:local:restore-source-lease-renew-no-effect-finalize-spec",
            0x0063,
            "RestoreSourceLeaseRenewNoEffectFinalizeSpec<Local>",
            "RestoreLeaseOperationTerminalRecord<Local,Renew>",
            "RestoreSourceLeaseRenewOperationRecord<Local>",
            "SourceUnspelled",
            "fgdb_apply::local::restore_source_lease_renew_no_effect_finalize_spec",
        ),
    ];

    for (id, tag, input, result, authority_target, audit_gate, handler) in expected {
        let row = registry
            .contracts
            .iter()
            .find(|row| row.command_contract_id == id)
            .expect("exact F15F table must resolve every contract row");
        assert_eq!(row.role, "Local", "{id} role drifted");
        assert_eq!(row.outer_command_union, "SequenceNeutralSpec<Tag>");
        assert_eq!(row.outer_wire_tag, tag, "{id} outer tag drifted");
        assert_eq!(row.input_wire_tag, tag, "{id} input tag drifted");
        assert_eq!(row.inner_wire_tag, None, "{id} invented an inner arm");
        assert_eq!(row.input_schema_id, input, "{id} input drifted");
        assert_eq!(row.body_schema_id, input, "{id} body drifted");
        assert_eq!(row.result_schema_id, result, "{id} result drifted");
        assert_eq!(row.applied_record_schema_id, "LocalRestoreRegistryValue");
        assert_eq!(row.expected_state_schema_id, "LocalRestoreRegistryValue");
        assert_eq!(row.authority_arm, "AuthorityBoundHeader<Local>");
        assert_eq!(
            row.authority_evidence_target_schema_id.as_deref(),
            Some(authority_target),
            "{id} authority target drifted"
        );
        assert_eq!(row.terminal_audit_freeze_arm, "SourceUnspelled");
        assert_eq!(row.terminal_audit_gate_arm, audit_gate);
        assert_eq!(row.handler_symbol, handler, "{id} handler drifted");
        assert_eq!(row.transition_class, "Semantic", "{id} plane drifted");
        assert_eq!(row.publication_mode, "SinglePlane", "{id} mode drifted");
        assert_eq!(
            row.consumed_state_slots,
            ["SemanticPayload|Local|restore_registry_root"]
        );
        assert_eq!(row.written_state_slots, row.consumed_state_slots);
        assert_eq!(row.checkpoint_floor_classes, ["semantic-restore"]);
        assert_eq!(row.backup_restore_gc_classes, ["semantic-restore"]);
        assert_eq!(row.posture_feature_predicate, "local");
        assert_eq!(row.status, "reserved", "{id} activated prematurely");
    }

    let authorize = registry
        .contracts
        .iter()
        .find(|row| row.command_contract_id == "cc:local:restore-source-lease-renew-authorize-spec")
        .expect("renew authorize row exists");
    for required in [
        "fresh challenged action-bound observation imports",
        "AuthorizedUndispatchedRecipe",
        "names no successor, attempt, full request, receipt or provider result",
        "no lineage proof without a fresh current grant observation",
    ] {
        assert!(
            authorize.sequence_effects.contains(required),
            "renew-authorize law lost {required:?}"
        );
    }

    let finalize = registry
        .contracts
        .iter()
        .find(|row| row.command_contract_id == "cc:local:restore-source-lease-renew-finalize-spec")
        .expect("renew finalize row exists");
    for required in [
        "Applied or AlreadyApplied",
        "fresh grant observation/import lineage",
        "generation g+1",
        "restores the frozen phase with Current",
    ] {
        assert!(
            finalize.sequence_effects.contains(required),
            "renew-finalize law lost {required:?}"
        );
    }

    let no_effect = registry
        .contracts
        .iter()
        .find(|row| {
            row.command_contract_id == "cc:local:restore-source-lease-renew-no-effect-finalize-spec"
        })
        .expect("renew no-effect row exists");
    for required in [
        "expired-NotRegistered",
        "no successor lease was installed",
        "preserves the old Current lease record",
        "permanently forbids resend",
    ] {
        assert!(
            no_effect.sequence_effects.contains(required),
            "renew-no-effect law lost {required:?}"
        );
    }
}

/// F15G pins the semantic release state machine without laundering its
/// Protocol dispatch initializer into SequenceNeutralSpec. Authorization must
/// remain an undispatched stable recipe, success must prove the provider's
/// terminal successor before cutting the lease, and no-effect finalization
/// must preserve the active predecessor and terminal pin.
#[test]
fn f15g_source_lease_release_contracts_are_exact() {
    let registry = registry();
    let interval: Vec<_> = registry
        .contracts
        .iter()
        .filter(|row| (0x0064..=0x0066).contains(&row.outer_wire_tag))
        .collect();
    assert_eq!(
        interval.len(),
        3,
        "the F15G outer-tag interval must contain exactly three armless rows"
    );
    for row in registry.contracts.iter().filter(|row| {
        row.input_schema_id == "RestoreSourceLeaseReleaseDispatchInitializeSpec<Local>"
    }) {
        assert_ne!(
            row.outer_command_union, "SequenceNeutralSpec<Tag>",
            "the Protocol-only release dispatch initializer entered SequenceNeutralSpec"
        );
        assert_eq!(
            row.transition_class, "Maintenance",
            "a future dispatch-initializer row must retain its Protocol maintenance plane"
        );
    }

    let expected = [
        (
            "cc:local:restore-source-lease-release-spec",
            0x0064,
            "RestoreSourceLeaseReleaseSpec<Local>",
            "RestoreSourceLeaseReleaseOperationRecord<Local>",
            "RestoreLeaseReleaseEligibility<Local>",
            "TerminalAuditGate",
            "fgdb_apply::local::restore_source_lease_release_spec",
        ),
        (
            "cc:local:restore-source-lease-release-finalize-spec",
            0x0065,
            "RestoreSourceLeaseReleaseFinalizeSpec<Local>",
            "LocalRestoreTerminalTombstone",
            "RestoreSourceLeaseReleaseOperationRecord<Local>",
            "SourceUnspelled",
            "fgdb_apply::local::restore_source_lease_release_finalize_spec",
        ),
        (
            "cc:local:restore-source-lease-release-no-effect-finalize-spec",
            0x0066,
            "RestoreSourceLeaseReleaseNoEffectFinalizeSpec<Local>",
            "RestoreLeaseOperationTerminalRecord<Local,Release>",
            "RestoreSourceLeaseReleaseOperationRecord<Local>",
            "SourceUnspelled",
            "fgdb_apply::local::restore_source_lease_release_no_effect_finalize_spec",
        ),
    ];

    for (id, tag, input, result, authority_target, audit_gate, handler) in expected {
        let row = registry
            .contracts
            .iter()
            .find(|row| row.command_contract_id == id)
            .expect("exact F15G table must resolve every contract row");
        assert_eq!(row.role, "Local", "{id} role drifted");
        assert_eq!(row.outer_command_union, "SequenceNeutralSpec<Tag>");
        assert_eq!(row.outer_wire_tag, tag, "{id} outer tag drifted");
        assert_eq!(row.input_wire_tag, tag, "{id} input tag drifted");
        assert_eq!(row.inner_wire_tag, None, "{id} invented an inner arm");
        assert_eq!(row.input_schema_id, input, "{id} input drifted");
        assert_eq!(row.body_schema_id, input, "{id} body drifted");
        assert_eq!(row.result_schema_id, result, "{id} result drifted");
        assert_eq!(row.applied_record_schema_id, "LocalRestoreRegistryValue");
        assert_eq!(row.expected_state_schema_id, "LocalRestoreRegistryValue");
        assert_eq!(row.authority_arm, "AuthorityBoundHeader<Local>");
        assert_eq!(
            row.authority_evidence_target_schema_id.as_deref(),
            Some(authority_target),
            "{id} authority target drifted"
        );
        assert_eq!(row.terminal_audit_freeze_arm, "SourceUnspelled");
        assert_eq!(row.terminal_audit_gate_arm, audit_gate);
        assert_eq!(row.handler_symbol, handler, "{id} handler drifted");
        assert_eq!(row.transition_class, "Semantic", "{id} plane drifted");
        assert_eq!(row.publication_mode, "SinglePlane", "{id} mode drifted");
        let expected_slots = if id == "cc:local:restore-source-lease-release-finalize-spec" {
            vec![
                "SemanticPayload|Local|restore_registry_root".to_owned(),
                "SemanticPayload|Local|retention_map".to_owned(),
            ]
        } else {
            vec!["SemanticPayload|Local|restore_registry_root".to_owned()]
        };
        assert_eq!(row.consumed_state_slots, expected_slots);
        assert_eq!(row.written_state_slots, row.consumed_state_slots);
        assert_eq!(row.checkpoint_floor_classes, ["semantic-restore"]);
        assert_eq!(row.backup_restore_gc_classes, ["semantic-restore"]);
        assert_eq!(row.posture_feature_predicate, "local");
        assert_eq!(row.status, "reserved", "{id} activated prematurely");
    }

    let authorize = registry
        .contracts
        .iter()
        .find(|row| row.command_contract_id == "cc:local:restore-source-lease-release-spec")
        .expect("release authorize row exists");
    for required in [
        "Live-or-Expired action-bound release eligibility",
        "AuthorizedUndispatchedRecipe",
        "stable action body excludes eligibility",
        "names no future publication evidence, full request, attempt or receipt",
    ] {
        assert!(
            authorize.sequence_effects.contains(required),
            "release-authorize law lost {required:?}"
        );
    }

    let finalize = registry
        .contracts
        .iter()
        .find(|row| {
            row.command_contract_id == "cc:local:restore-source-lease-release-finalize-spec"
        })
        .expect("release finalize row exists");
    for required in [
        "Released, AlreadyReleased or AlreadyExpired",
        "full signed no-successor proof",
        "removes the canonical lease edge only with audit-visible apply",
        "enters ReleasedTerminal",
    ] {
        assert!(
            finalize.sequence_effects.contains(required),
            "release-finalize law lost {required:?}"
        );
    }

    let no_effect = registry
        .contracts
        .iter()
        .find(|row| {
            row.command_contract_id
                == "cc:local:restore-source-lease-release-no-effect-finalize-spec"
        })
        .expect("release no-effect row exists");
    for required in [
        "archive head remains the exact Active predecessor",
        "preserves AwaitingSourceRelease plus the old Current lease",
        "fresh operation ID plus current eligibility",
        "never releases a terminal pin or pretends the source lease ended",
    ] {
        assert!(
            no_effect.sequence_effects.contains(required),
            "release-no-effect law lost {required:?}"
        );
    }
}

/// F16 is the final Local semantic cohort. It binds freeze to an empty audit
/// predecessor and a future-free candidate, keeps unfreeze legal only before
/// Begin, and makes Begin terminal at Raft acceptance without inventing a
/// consensus apply slot or admitting Protocol audit advancement as semantic.
#[test]
fn f16_sharding_role_transition_contracts_are_exact() {
    let registry = registry();
    let interval: Vec<_> = registry
        .contracts
        .iter()
        .filter(|row| (0x0067..=0x0069).contains(&row.outer_wire_tag))
        .collect();
    assert_eq!(
        interval.len(),
        3,
        "the F16 outer-tag interval must contain exactly three armless rows"
    );
    assert!(
        registry
            .contracts
            .iter()
            .all(|row| row.input_schema_id != "AuditVisibilityAdvanceSpec<Local>"),
        "Protocol audit advancement entered the Local semantic union"
    );

    let expected = [
        (
            "cc:local:sharding-freeze-spec",
            0x0067,
            "ShardingFreezeSpec",
            "ShardingFreezeSpec",
            "ShardingFreezeRecord",
            "LogicalStatePayload",
            "TerminalAuditGate",
            "fgdb_apply::local::sharding_freeze_spec",
            vec!["SemanticPayload|Local|sharding_migration_state"],
        ),
        (
            "cc:local:sharding-unfreeze-spec",
            0x0068,
            "ShardingUnfreezeSpec",
            "SourceUnspelled",
            "SourceUnspelled",
            "ShardingMigrationState",
            "SourceUnspelled",
            "fgdb_apply::local::sharding_unfreeze_spec",
            vec!["SemanticPayload|Local|sharding_migration_state"],
        ),
        (
            "cc:local:begin-role-transition-spec",
            0x0069,
            "BeginRoleTransitionSpec",
            "BeginRoleTransitionSpec",
            "BeginRoleTransitionRecord",
            "ShardingMigrationState",
            "StructurallyInapplicable{ShardingRoleTransitionAuthority}",
            "fgdb_apply::local::begin_role_transition_spec",
            vec![
                "SemanticPayload|Local|remote_retention_obligation_root",
                "SemanticPayload|Local|sharding_migration_state",
            ],
        ),
    ];

    for (id, tag, input, body, result, state, audit_gate, handler, slots) in expected {
        let row = registry
            .contracts
            .iter()
            .find(|row| row.command_contract_id == id)
            .expect("exact F16 table must resolve every contract row");
        assert_eq!(row.role, "Local", "{id} role drifted");
        assert_eq!(row.outer_command_union, "SequenceNeutralSpec<Tag>");
        assert_eq!(row.outer_wire_tag, tag, "{id} outer tag drifted");
        assert_eq!(row.input_wire_tag, tag, "{id} input tag drifted");
        assert_eq!(row.inner_wire_tag, None, "{id} invented an inner arm");
        assert_eq!(row.input_schema_id, input, "{id} input drifted");
        assert_eq!(row.body_schema_id, body, "{id} body drifted");
        assert_eq!(row.result_schema_id, result, "{id} result drifted");
        assert_eq!(row.applied_record_schema_id, "ShardingMigrationState");
        assert_eq!(row.expected_state_schema_id, state, "{id} state drifted");
        assert_eq!(row.authority_arm, "SourceUnspelled");
        assert_eq!(row.authority_evidence_target_schema_id, None);
        assert_eq!(row.terminal_audit_freeze_arm, "SourceUnspelled");
        assert_eq!(row.terminal_audit_gate_arm, audit_gate);
        assert_eq!(row.handler_symbol, handler, "{id} handler drifted");
        assert_eq!(row.transition_class, "Semantic", "{id} plane drifted");
        assert_eq!(row.publication_mode, "SinglePlane", "{id} mode drifted");
        let expected_slots: Vec<String> = slots.into_iter().map(str::to_owned).collect();
        assert_eq!(row.consumed_state_slots, expected_slots);
        assert_eq!(row.written_state_slots, row.consumed_state_slots);
        assert_eq!(row.checkpoint_floor_classes, ["role-transition"]);
        assert_eq!(row.backup_restore_gc_classes, ["role-transition"]);
        assert_eq!(row.posture_feature_predicate, "local");
        assert_eq!(row.status, "reserved", "{id} activated prematurely");
    }

    let freeze = registry
        .contracts
        .iter()
        .find(|row| row.command_contract_id == "cc:local:sharding-freeze-spec")
        .expect("freeze row exists");
    for required in [
        "empty predecessor audit pipeline",
        "drains every active transaction",
        "names no future payload/root",
        "AuditVisibilityAdvanceSpec<Local> makes the candidate FrozenVisible",
    ] {
        assert!(
            freeze.sequence_effects.contains(required),
            "freeze law lost {required:?}"
        );
    }

    let unfreeze = registry
        .contracts
        .iter()
        .find(|row| row.command_contract_id == "cc:local:sharding-unfreeze-spec")
        .expect("unfreeze row exists");
    for required in [
        "only from FrozenVisible before BeginRoleTransitionSpec",
        "invalidates the current ShardingRoleTransitionPlan generation",
        "newly audited plan and ShardingFreezeSpec",
    ] {
        assert!(
            unfreeze.sequence_effects.contains(required),
            "unfreeze law lost {required:?}"
        );
    }

    let begin = registry
        .contracts
        .iter()
        .find(|row| row.command_contract_id == "cc:local:begin-role-transition-spec")
        .expect("begin row exists");
    for required in [
        "before any voter acknowledges Begin AppendEntries at index I",
        "terminal RaftRoleTransitionSeal",
        "quorum acceptance is roll-forward-only",
        "installs RoleTransitionLocked",
        "old-Local side of every authority transfer",
        "quorum-durable seals plus quorum apply through Begin",
    ] {
        assert!(
            begin.sequence_effects.contains(required),
            "begin law lost {required:?}"
        );
    }
}

/// An armed member's rows share the member's outer tag and differ only by
/// inner_wire_tag: the arm-slot law keys on (role, union, outer, inner), so
/// this shape validates clean while a same-inner duplicate or an
/// armless/armed mix on one outer tag reds contract_arm_slot_duplicate.
/// Assert the landed shape explicitly: shared outer tag, distinct inner tags.
#[test]
fn armed_member_rows_share_outer_tag_with_distinct_inner_tags() {
    let registry = registry();
    for (root, arms) in [
        ("cc:local:local-attempt-registration-spec", 2usize),
        ("cc:local:txn-ownership-transition-spec", 2),
        ("cc:meta:txn-ownership-transition-spec", 2),
        ("cc:local:local-outcome-compaction-spec", 2),
        ("cc:local:allocation-reservation-transition-spec", 2),
        ("cc:local:finalization-allocation-disposition-spec", 2),
        ("cc:local:configuration-transition-spec", 4),
        ("cc:local:format-transition-spec", 5),
        ("cc:local:remote-retention-control-spec", 8),
        ("cc:local:escrow-rights-transition-spec", 5),
        ("cc:local:dp-transition-spec", 8),
        ("cc:local:audit-terminal-freeze-spec", 3),
        ("cc:local:bulk-load-transition-spec", 5),
        ("cc:local:derived-build-transition-spec", 6),
        ("cc:local:gc-physical-disposition-import-spec", 2),
        ("cc:local:restore-abandon-spec", 1),
    ] {
        let family: Vec<_> = registry
            .contracts
            .iter()
            .filter(|row| row.command_contract_id.starts_with(&format!("{root}:")))
            .collect();
        assert_eq!(
            family.len(),
            arms,
            "family {root:?} has the wrong arm count"
        );
        let outer: std::collections::BTreeSet<i64> =
            family.iter().map(|row| row.outer_wire_tag).collect();
        assert_eq!(outer.len(), 1, "family {root:?} must share one outer tag");
        let inner: std::collections::BTreeSet<Option<i64>> =
            family.iter().map(|row| row.inner_wire_tag).collect();
        assert_eq!(
            inner.len(),
            arms,
            "family {root:?} must carry distinct inner tags"
        );
        assert!(
            !inner.contains(&None),
            "an armed row must carry an inner_wire_tag"
        );
    }
}

/// The synthetic baseline must itself be clean, or every mutation below tests
/// the fixture rather than the law.
#[test]
fn synthetic_baseline_is_clean() {
    let r = with_row(|_| {});
    let violations = validate_contracts(&r);
    assert!(
        violations.is_empty(),
        "the synthetic baseline row must validate clean, found: {violations:?}"
    );
}

#[test]
fn epoch_below_one_is_rejected() {
    let r = ContractRegistry {
        registry_epoch: 0,
        contracts: Vec::new(),
    };
    assert!(
        codes(&r).contains(&"contract_registry_epoch_invalid".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn empty_id_is_rejected() {
    let r = with_row(|c| c.command_contract_id = "  ".into());
    assert!(
        codes(&r).contains(&"contract_id_empty".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn duplicate_id_is_rejected() {
    let mut r = with_row(|_| {});
    let mut second = synthetic_row();
    second.outer_wire_tag = 0x0002;
    r.contracts.push(second);
    assert!(
        codes(&r).contains(&"contract_id_duplicate".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn invalid_role_is_rejected() {
    let r = with_row(|c| c.role = "Global".into());
    assert!(
        codes(&r).contains(&"contract_role_invalid".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn invalid_transition_class_is_rejected() {
    let r = with_row(|c| c.transition_class = "Housekeeping".into());
    assert!(
        codes(&r).contains(&"contract_transition_class_invalid".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn invalid_status_is_rejected() {
    let r = with_row(|c| c.status = "active".into());
    assert!(
        codes(&r).contains(&"contract_status_invalid".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn zero_wire_tag_is_rejected() {
    let r = with_row(|c| c.outer_wire_tag = 0x0000);
    assert!(
        codes(&r).contains(&"contract_wire_tag_out_of_space".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn sentinel_wire_tag_is_rejected() {
    let r = with_row(|c| c.input_wire_tag = 0xffff);
    assert!(
        codes(&r).contains(&"contract_wire_tag_out_of_space".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn optional_inner_tag_is_bounded_too() {
    let r = with_row(|c| c.inner_wire_tag = Some(0xffff));
    assert!(
        codes(&r).contains(&"contract_wire_tag_out_of_space".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn duplicate_arm_slot_is_rejected() {
    let mut r = with_row(|_| {});
    let mut second = synthetic_row();
    second.command_contract_id = "cc-local-branch-retire-v2".into();
    // Same (role, outer_command_union, outer_wire_tag), both armless: a
    // second command encoded under one tag.
    r.contracts.push(second);
    assert!(
        codes(&r).contains(&"contract_arm_slot_duplicate".to_string()),
        "{:?}",
        codes(&r)
    );
}

/// Two arm rows may share the member's outer tag ONLY through distinct inner
/// tags; the same (outer, inner) pair is a second command under one tag.
#[test]
fn same_outer_and_same_inner_tag_is_rejected() {
    let mut r = with_row(|c| c.inner_wire_tag = Some(0x0001));
    let mut second = synthetic_row();
    second.command_contract_id = "cc-local-branch-retire-v2".into();
    second.inner_wire_tag = Some(0x0001);
    r.contracts.push(second);
    assert!(
        codes(&r).contains(&"contract_arm_slot_duplicate".to_string()),
        "{:?}",
        codes(&r)
    );
}

/// Distinct inner tags under one outer tag are the SPECIFIED armed-family
/// shape (plan line 294) and must validate clean — the differential control
/// for the two rejections beside it.
#[test]
fn distinct_inner_tags_under_one_outer_tag_are_clean() {
    let mut r = with_row(|c| c.inner_wire_tag = Some(0x0001));
    let mut second = synthetic_row();
    second.command_contract_id = "cc-local-branch-retire-v2".into();
    second.inner_wire_tag = Some(0x0002);
    r.contracts.push(second);
    let violations = validate_contracts(&r);
    assert!(violations.is_empty(), "{violations:?}");
}

/// One outer tag is one armless command or one armed family, never both: an
/// armless row and an armed row sharing the outer tag hide an open subcommand.
#[test]
fn armless_and_armed_rows_on_one_outer_tag_are_rejected() {
    let mut r = with_row(|_| {});
    let mut second = synthetic_row();
    second.command_contract_id = "cc-local-branch-retire-v2".into();
    second.inner_wire_tag = Some(0x0001);
    r.contracts.push(second);
    assert!(
        codes(&r).contains(&"contract_arm_slot_duplicate".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn malformed_state_slot_is_rejected() {
    let r = with_row(|c| c.written_state_slots = vec!["branch_registry".into()]);
    assert!(
        codes(&r).contains(&"contract_state_slot_malformed".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn unregistered_slot_plane_is_rejected() {
    let r = with_row(|c| c.consumed_state_slots = vec!["Payload|Local|branch_registry".into()]);
    assert!(
        codes(&r).contains(&"contract_state_slot_malformed".to_string()),
        "{:?}",
        codes(&r)
    );
}

/// The plan-line-296 bijection quantifies over live rows; the enumerators that
/// discharge a live row's obligations do not exist yet, so `live` is refused
/// rather than silently accepted (green-over-unchecked is the failure class
/// this registry exists to end).
#[test]
fn live_row_is_refused_until_bijection_enumerators_exist() {
    let r = with_row(|c| c.status = "live".into());
    assert!(
        codes(&r).contains(&"contract_live_row_unverifiable".to_string()),
        "{:?}",
        codes(&r)
    );
}

/// Parse-level closed-key law: a row carrying an unknown key is a load error,
/// not a violation. Uses a process-unique temp path so a concurrent pane
/// cannot race this fixture.
#[test]
fn unknown_key_fails_the_load() {
    let dir = std::env::temp_dir().join(format!(
        "fgdb-command-contracts-test-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("registries").join("command_contracts.toml");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("temp registries dir");
    std::fs::write(
        &path,
        "registry_epoch = 1\n\n[[contract]]\nwildcard_body = \"*\"\n",
    )
    .expect("fixture written");
    let error = load_contracts(&path).expect_err("unknown key must fail the load");
    assert!(
        error.message.contains("unknown key"),
        "load error must name the closed-key law, got: {error}"
    );
    std::fs::remove_dir_all(&dir).ok();
}

//! Mutation suite for the input-looking type classification registry
//! (fgdb-5uw2).
//!
//! Every validation rule in `registry_check::command_type_classification` gets
//! a test that takes the real registry, breaks exactly one thing, and asserts
//! the exact violation code. The load-bearing test is
//! `every_anchor_names_its_type_at_the_cited_line`: a classification row's
//! whole claim is that its class is FORCED at the cited plan line, so the
//! suite opens the line and requires the classified type to be named there —
//! without that, the registry would be a place to write choices wearing
//! citations, the defect §5.1 pairs this registry against.

use registry_check::command_contracts::{ContractRegistry, load_from_repo as load_contracts};
use registry_check::command_type_classification::{
    ClassificationRegistry, load_from_repo, validate_classifications,
};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn registry() -> ClassificationRegistry {
    load_from_repo(&repo_root()).expect("command_type_classification.toml loads")
}

fn contracts() -> ContractRegistry {
    load_contracts(&repo_root()).expect("command_contracts.toml loads")
}

fn codes(registry: &ClassificationRegistry) -> Vec<String> {
    validate_classifications(registry, &contracts())
        .into_iter()
        .map(|violation| violation.code)
        .collect()
}

/// The control. Every mutation test below is meaningless if the unmutated
/// registry is not clean.
#[test]
fn real_registry_is_clean() {
    let violations = validate_classifications(&registry(), &contracts());
    assert!(
        violations.is_empty(),
        "the shipped classification registry must validate clean, found: {violations:?}"
    );
}

/// Non-vacuity floor: the source-forced seed population, by name. A row
/// leaving this set is either Phase B progress (add, never remove) or a gutted
/// file — both deserve a red here.
#[test]
fn source_forced_seed_population_is_present() {
    let registry = registry();
    for name in [
        "TxnSubmissionSpec",
        "AuditExternalResolutionBatch",
        "AuditExternalResolutionReceipt",
        "AllocationReservationTransitionSpec",
        "LocalFinalCertificationReserveSpec",
        "LocalFinalCertificationCancelSpec",
        "FinalizationAllocationDispositionSpec",
        "CheckpointInstallSpec",
        "InitialConfigFloorInstallSpec",
        "HistoryCutActivationSpec",
        "ConfigurationTransitionSpec",
        "LocalDeltaBatchRetentionCutSpec",
        "RemoteRetentionControlSpec",
        "AdvanceRemoteConfigurationEvidenceSpec",
        "ValidateRemoteConfigurationAnchorSpec",
        "RemoteRetentionGrantSpec",
        "RetentionAuthorityTransferAdoptionSpec",
        "BranchEpochBoundaryReserveSpec",
        "BranchForkSpec",
        "BranchGrantSpec",
        "BranchRetireSpec",
        "BranchRetireFinalizeSpec",
        "MergeRejectSpec",
        "MergePrepareSpec",
        "MergeExecuteSpec",
        "ResourceLedgerTransitionSpec",
        "EscrowRightsTransitionSpec",
        "ExpiryEpochAdvanceSpec",
        "PolicyTransitionSpec",
        "RevocationTransitionSpec",
        "TimeIssuanceAdmissionFreezeSpec",
        "TimeAuthorityRotationIntentSpec",
        "TimeAuthorityIssuanceCloseSpec",
        "TimeAuthorityIssuanceFenceAuthorizeSpec",
        "TimeAuthorityRegistryTransitionAuthorizeSpec",
        "TimeAuthorityProfileTransitionSpec",
        "TimeAuthorityProfileRetirementSpec",
        "PrivacyContinuityImportSpec",
        "DpTransitionSpec",
        "AuditTicketAdmissionSpec",
        "AuditTerminalFreezeSpec",
        "AuditTerminalPlanAbandonSpec",
        "AuditTerminalSpec",
        "AuditRecoverySpec",
        "AuditCompletenessTransitionSpec",
        "BulkLoadTransitionSpec",
        "DerivedBuildTransitionSpec",
        "KeyDestroyAuthorizeSpec",
        "KeyDestroyFinalizeSpec",
        "KeyDestroyCertificatePublishSpec",
        "LocalGcAuthorizeSpec",
        "LocalGcApplyQuarantineSpec",
        "LocalGcCancellationAuthorizeSpec",
        "GcPhysicalDispositionImportSpec",
        "LocalBackupBarrierSpec",
        "LocalBackupClosurePublishSpec",
        "LocalBackupSealSpec",
        "LocalBackupPublicationAuthorizeSpec",
        "LocalBackupPublicationReceiptImportSpec",
        "LocalBackupGrantIssueImportSpec",
        "LocalBackupArtifactVerifySpec",
        "LocalBackupReleaseSpec",
        "ArchiveSourceReleaseCompletionImportSpec",
        "LocalBackupAbortSpec",
        "LocalRestoreActivationSpec",
        "LocalRestoreServicePrepareSpec",
        "LocalRestoreServicePromotionSpec",
        "LocalRestoreServiceCompletionSpec",
        "LocalRestoreAbandonFinalizeSpec",
        "LocalRestoreAbandonmentPinInstallSpec",
        "RestoreAbandonSpec",
        "DirectoryBoundEnterPromotionPendingSpec",
        "DirectoryBoundFinalizeOperationalAuthoritySpec",
        "DirectoryBoundAbandonApplySpec",
        "DirectoryBoundAbandonReceiptImportSpec",
        "RestoreSourceKeyAccessCleanupFinalizeSpec",
        "RestoreSourceLeaseRenewAuthorizedNeverArmedFinalizeSpec",
        "RestoreSourceLeaseReleaseAuthorizedNeverArmedFinalizeSpec",
        "RestoreTerminalPinReleaseFinalizeSpec",
        "RestoreSourceKeyAccessCleanupAuthorizeSpec",
        "RestoreSourceKeyAccessCleanupImportSpec",
        "RestoreSourceLeaseRenewAuthorizeSpec",
        "RestoreSourceLeaseRenewFinalizeSpec",
        "RestoreSourceLeaseRenewNoEffectFinalizeSpec",
        "RestoreSourceLeaseReleaseSpec",
        "RestoreSourceLeaseReleaseFinalizeSpec",
        "RestoreSourceLeaseReleaseNoEffectFinalizeSpec",
        "ShardingFreezeSpec",
        "ShardingUnfreezeSpec",
        "BeginRoleTransitionSpec",
        "GlobalBeginReservationSpec",
        "GlobalBeginTerminalSpec",
        "GlobalAttemptRegistrationSpec",
        "GlobalStatementRegistrationSpec",
        "GlobalStatementPublicationSpec",
        "GlobalStatementAbortSpec",
        "GlobalAttemptCancelSpec",
        "GlobalPrepareAdmissionSpec",
        "GlobalReadCloseSpec",
        "GlobalFinalCertificationReserveSpec",
        "GlobalFinalCertificationCancelSpec",
        "GlobalTerminalCompletionSpec",
        "NeverRegisteredFloorSpec",
    ] {
        assert!(
            registry
                .classifications
                .iter()
                .any(|row| row.type_name == name && row.status == "registered"),
            "source-forced seed row {name:?} is missing"
        );
    }
    assert!(
        registry.classifications.len() >= 125,
        "the population may only grow from the landed Local F1-F16 plus Meta F1-F11 rows"
    );
}

/// F13's type-to-contract map is the classification authority. Pin it as a
/// complete literal table so a suffix-based guess, wrong command root, stale
/// anchor, or status rewrite cannot hide behind the generic resolver.
#[test]
fn f13_key_destroy_classifications_are_exact() {
    let registry = registry();
    for (type_name, command_contract_id, source_location) in [
        (
            "KeyDestroyAuthorizeSpec",
            "cc:local:key-destroy-authorize-spec",
            "a15:2059",
        ),
        (
            "KeyDestroyFinalizeSpec",
            "cc:local:key-destroy-finalize-spec",
            "a15:2067",
        ),
        (
            "KeyDestroyCertificatePublishSpec",
            "cc:local:key-destroy-certificate-publish-spec",
            "a15:2069",
        ),
    ] {
        let row = registry
            .classifications
            .iter()
            .find(|row| row.type_name == type_name)
            .expect("exact F13 table must resolve every classification row");
        assert_eq!(row.class, "RegisteredCommandInput");
        assert_eq!(
            row.command_contract_id.as_deref(),
            Some(command_contract_id),
            "{type_name} command binding drifted"
        );
        assert_eq!(
            row.source_location, source_location,
            "{type_name} source anchor drifted"
        );
        assert_eq!(row.status, "registered");
    }
}

/// F14's four member roots are the exact command-classification authority.
/// The disposition union classifies once at its member root; its two inner
/// contracts remain the command registry's responsibility.
#[test]
fn f14_semantic_gc_classifications_are_exact() {
    let registry = registry();
    for (type_name, command_contract_id, source_location) in [
        (
            "LocalGcAuthorizeSpec",
            "cc:local:local-gc-authorize-spec",
            "a14:2045",
        ),
        (
            "LocalGcApplyQuarantineSpec",
            "cc:local:local-gc-apply-quarantine-spec",
            "a14:2047",
        ),
        (
            "LocalGcCancellationAuthorizeSpec",
            "cc:local:local-gc-cancellation-authorize-spec",
            "a14:2055",
        ),
        (
            "GcPhysicalDispositionImportSpec",
            "cc:local:gc-physical-disposition-import-spec",
            "a14:2055",
        ),
    ] {
        let row = registry
            .classifications
            .iter()
            .find(|row| row.type_name == type_name)
            .expect("exact F14 table must resolve every classification row");
        assert_eq!(row.class, "RegisteredCommandInput");
        assert_eq!(
            row.command_contract_id.as_deref(),
            Some(command_contract_id),
            "{type_name} command binding drifted"
        );
        assert_eq!(row.source_location, source_location);
        assert_eq!(row.status, "registered");
    }
}

/// F15A's ten explicit Local backup members are independently named inputs,
/// in the owner-confirmed dense order. Pin each input to its exact contract
/// and source anchor so a suffix guess or cross-role alias cannot substitute.
#[test]
fn f15a_local_backup_classifications_are_exact() {
    let registry = registry();
    for (type_name, command_contract_id, source_location) in [
        (
            "LocalBackupBarrierSpec",
            "cc:local:local-backup-barrier-spec",
            "a15:2117",
        ),
        (
            "LocalBackupClosurePublishSpec",
            "cc:local:local-backup-closure-publish-spec",
            "a15:2121",
        ),
        (
            "LocalBackupSealSpec",
            "cc:local:local-backup-seal-spec",
            "a15:2121",
        ),
        (
            "LocalBackupPublicationAuthorizeSpec",
            "cc:local:local-backup-publication-authorize-spec",
            "a15:2121",
        ),
        (
            "LocalBackupPublicationReceiptImportSpec",
            "cc:local:local-backup-publication-receipt-import-spec",
            "a15:2121",
        ),
        (
            "LocalBackupGrantIssueImportSpec",
            "cc:local:local-backup-grant-issue-import-spec",
            "a15:2123",
        ),
        (
            "LocalBackupArtifactVerifySpec",
            "cc:local:local-backup-artifact-verify-spec",
            "a15:2123",
        ),
        (
            "LocalBackupReleaseSpec",
            "cc:local:local-backup-release-spec",
            "a15:2123",
        ),
        (
            "ArchiveSourceReleaseCompletionImportSpec",
            "cc:local:archive-source-release-completion-import-spec",
            "a15:2107",
        ),
        (
            "LocalBackupAbortSpec",
            "cc:local:local-backup-abort-spec",
            "a15:2139",
        ),
    ] {
        let row = registry
            .classifications
            .iter()
            .find(|row| row.type_name == type_name)
            .expect("exact F15A table must resolve every classification row");
        assert_eq!(row.class, "RegisteredCommandInput");
        assert_eq!(
            row.command_contract_id.as_deref(),
            Some(command_contract_id),
            "{type_name} command binding drifted"
        );
        assert_eq!(row.source_location, source_location);
        assert_eq!(row.status, "registered");
    }
}

/// F15B's seven Local restore members are exact registered inputs. The final
/// closed union binds its member-root contract id; only its Local arm is
/// inhabitable in SequenceNeutralSpec<Tag>.
#[test]
fn f15b_local_restore_classifications_are_exact() {
    let registry = registry();
    for (type_name, command_contract_id, source_location) in [
        (
            "LocalRestoreActivationSpec",
            "cc:local:local-restore-activation-spec",
            "a20:2603",
        ),
        (
            "LocalRestoreServicePrepareSpec",
            "cc:local:local-restore-service-prepare-spec",
            "a20:2603",
        ),
        (
            "LocalRestoreServicePromotionSpec",
            "cc:local:local-restore-service-promotion-spec",
            "a20:2605",
        ),
        (
            "LocalRestoreServiceCompletionSpec",
            "cc:local:local-restore-service-completion-spec",
            "a20:2605",
        ),
        (
            "LocalRestoreAbandonFinalizeSpec",
            "cc:local:local-restore-abandon-finalize-spec",
            "a18:2453",
        ),
        (
            "LocalRestoreAbandonmentPinInstallSpec",
            "cc:local:local-restore-abandonment-pin-install-spec",
            "a18:2381",
        ),
        (
            "RestoreAbandonSpec",
            "cc:local:restore-abandon-spec",
            "a18:2433",
        ),
    ] {
        let row = registry
            .classifications
            .iter()
            .find(|row| row.type_name == type_name)
            .expect("exact F15B table must resolve every classification row");
        assert_eq!(row.class, "RegisteredCommandInput");
        assert_eq!(
            row.command_contract_id.as_deref(),
            Some(command_contract_id),
            "{type_name} command binding drifted"
        );
        assert_eq!(row.source_location, source_location);
        assert_eq!(row.status, "registered");
    }
}

/// F15C's four DirectoryBound members are separately addressable ordered
/// inputs, not certificate/result shapes. Pin their exact contract roots and
/// the two normative plan anchors so suffix inference cannot classify them.
#[test]
fn f15c_directory_bound_restore_classifications_are_exact() {
    let registry = registry();
    for (type_name, command_contract_id, source_location) in [
        (
            "DirectoryBoundEnterPromotionPendingSpec",
            "cc:local:directory-bound-enter-promotion-pending-spec",
            "a20:2588",
        ),
        (
            "DirectoryBoundFinalizeOperationalAuthoritySpec",
            "cc:local:directory-bound-finalize-operational-authority-spec",
            "a20:2588",
        ),
        (
            "DirectoryBoundAbandonApplySpec",
            "cc:local:directory-bound-abandon-apply-spec",
            "a18:2446",
        ),
        (
            "DirectoryBoundAbandonReceiptImportSpec",
            "cc:local:directory-bound-abandon-receipt-import-spec",
            "a18:2446",
        ),
    ] {
        let row = registry
            .classifications
            .iter()
            .find(|row| row.type_name == type_name)
            .expect("exact F15C table must resolve every classification row");
        assert_eq!(row.class, "RegisteredCommandInput");
        assert_eq!(
            row.command_contract_id.as_deref(),
            Some(command_contract_id),
            "{type_name} command binding drifted"
        );
        assert_eq!(row.source_location, source_location);
        assert_eq!(row.status, "registered");
    }
}

/// F15D's first three structurally Local authority-owning members remain
/// distinct registered inputs. Pin their exact roots and first-occurrence
/// plan anchors so suffix inference or renew/release aliasing cannot pass.
#[test]
fn f15d_authority_owning_restore_lease_classifications_are_exact() {
    let registry = registry();
    for (type_name, command_contract_id, source_location) in [
        (
            "RestoreSourceKeyAccessCleanupFinalizeSpec",
            "cc:local:restore-source-key-access-cleanup-finalize-spec",
            "a18:2351",
        ),
        (
            "RestoreSourceLeaseRenewAuthorizedNeverArmedFinalizeSpec",
            "cc:local:restore-source-lease-renew-authorized-never-armed-finalize-spec",
            "a18:2353",
        ),
        (
            "RestoreSourceLeaseReleaseAuthorizedNeverArmedFinalizeSpec",
            "cc:local:restore-source-lease-release-authorized-never-armed-finalize-spec",
            "a18:2353",
        ),
    ] {
        let row = registry
            .classifications
            .iter()
            .find(|row| row.type_name == type_name)
            .expect("exact F15D table must resolve every classification row");
        assert_eq!(row.class, "RegisteredCommandInput");
        assert_eq!(
            row.command_contract_id.as_deref(),
            Some(command_contract_id),
            "{type_name} command binding drifted"
        );
        assert_eq!(
            row.source_location, source_location,
            "{type_name} source anchor drifted"
        );
        assert_eq!(row.status, "registered");
    }
}

/// F15E's next three structurally Local members must remain distinct command
/// inputs with exact first-occurrence anchors. This kills suffix inference,
/// cleanup authorize/import aliasing, and pin-finalizer reassignment.
#[test]
fn f15e_terminal_pin_and_source_cleanup_classifications_are_exact() {
    let registry = registry();
    for (type_name, command_contract_id, source_location) in [
        (
            "RestoreTerminalPinReleaseFinalizeSpec",
            "cc:local:restore-terminal-pin-release-finalize-spec",
            "a18:2371",
        ),
        (
            "RestoreSourceKeyAccessCleanupAuthorizeSpec",
            "cc:local:restore-source-key-access-cleanup-authorize-spec",
            "a18:2381",
        ),
        (
            "RestoreSourceKeyAccessCleanupImportSpec",
            "cc:local:restore-source-key-access-cleanup-import-spec",
            "a18:2381",
        ),
    ] {
        let row = registry
            .classifications
            .iter()
            .find(|row| row.type_name == type_name)
            .expect("exact F15E table must resolve every classification row");
        assert_eq!(row.class, "RegisteredCommandInput");
        assert_eq!(
            row.command_contract_id.as_deref(),
            Some(command_contract_id),
            "{type_name} command binding drifted"
        );
        assert_eq!(
            row.source_location, source_location,
            "{type_name} source anchor drifted"
        );
        assert_eq!(row.status, "registered");
    }
}

/// F15F's exact type-to-contract map keeps the three renewal phases distinct.
/// The anchors deliberately split authorize from both terminal paths so a
/// suffix guess or one generic finalize binding cannot pass.
#[test]
fn f15f_source_lease_renewal_classifications_are_exact() {
    let registry = registry();
    for (type_name, command_contract_id, source_location) in [
        (
            "RestoreSourceLeaseRenewAuthorizeSpec",
            "cc:local:restore-source-lease-renew-authorize-spec",
            "a18:2393",
        ),
        (
            "RestoreSourceLeaseRenewFinalizeSpec",
            "cc:local:restore-source-lease-renew-finalize-spec",
            "a18:2395",
        ),
        (
            "RestoreSourceLeaseRenewNoEffectFinalizeSpec",
            "cc:local:restore-source-lease-renew-no-effect-finalize-spec",
            "a18:2395",
        ),
    ] {
        let row = registry
            .classifications
            .iter()
            .find(|row| row.type_name == type_name)
            .expect("exact F15F table must resolve every classification row");
        assert_eq!(row.class, "RegisteredCommandInput");
        assert_eq!(
            row.command_contract_id.as_deref(),
            Some(command_contract_id),
            "{type_name} command binding drifted"
        );
        assert_eq!(
            row.source_location, source_location,
            "{type_name} source anchor drifted"
        );
        assert_eq!(row.status, "registered");
    }
}

/// F15G's exact map binds only the three semantic release transitions. The
/// dispatch initializer is a physical/Protocol input and must not be inferred
/// into this table merely because its stable name ends in Spec.
#[test]
fn f15g_source_lease_release_classifications_are_exact() {
    let registry = registry();
    if let Some(row) = registry
        .classifications
        .iter()
        .find(|row| row.type_name == "RestoreSourceLeaseReleaseDispatchInitializeSpec")
    {
        let contract_id = row
            .command_contract_id
            .as_deref()
            .expect("a future dispatch-initializer classification must bind a contract");
        let contracts = contracts();
        let contract = contracts
            .contracts
            .iter()
            .find(|contract| contract.command_contract_id == contract_id)
            .expect("a future dispatch-initializer classification must resolve");
        assert_eq!(
            contract.transition_class, "Maintenance",
            "the Protocol-only dispatch initializer must retain its maintenance plane"
        );
    }
    for (type_name, command_contract_id, source_location) in [
        (
            "RestoreSourceLeaseReleaseSpec",
            "cc:local:restore-source-lease-release-spec",
            "a18:2405",
        ),
        (
            "RestoreSourceLeaseReleaseFinalizeSpec",
            "cc:local:restore-source-lease-release-finalize-spec",
            "a18:2413",
        ),
        (
            "RestoreSourceLeaseReleaseNoEffectFinalizeSpec",
            "cc:local:restore-source-lease-release-no-effect-finalize-spec",
            "a18:2413",
        ),
    ] {
        let row = registry
            .classifications
            .iter()
            .find(|row| row.type_name == type_name)
            .expect("exact F15G table must resolve every classification row");
        assert_eq!(row.class, "RegisteredCommandInput");
        assert_eq!(
            row.command_contract_id.as_deref(),
            Some(command_contract_id),
            "{type_name} command binding drifted"
        );
        assert_eq!(
            row.source_location, source_location,
            "{type_name} source anchor drifted"
        );
        assert_eq!(row.status, "registered");
    }
}

/// F16 closes the frozen Local type-to-contract map without classifying the
/// Protocol-only audit advancement as a semantic command.
#[test]
fn f16_sharding_role_transition_classifications_are_exact() {
    let registry = registry();
    assert!(
        registry
            .classifications
            .iter()
            .all(|row| row.type_name != "AuditVisibilityAdvanceSpec"),
        "Protocol audit advancement entered the Local semantic classification tranche"
    );
    for (type_name, command_contract_id, source_location) in [
        (
            "ShardingFreezeSpec",
            "cc:local:sharding-freeze-spec",
            "a04:1594",
        ),
        (
            "ShardingUnfreezeSpec",
            "cc:local:sharding-unfreeze-spec",
            "a04:1594",
        ),
        (
            "BeginRoleTransitionSpec",
            "cc:local:begin-role-transition-spec",
            "a04:1598",
        ),
    ] {
        let row = registry
            .classifications
            .iter()
            .find(|row| row.type_name == type_name)
            .expect("exact F16 table must resolve every classification row");
        assert_eq!(row.class, "RegisteredCommandInput");
        assert_eq!(
            row.command_contract_id.as_deref(),
            Some(command_contract_id),
            "{type_name} command binding drifted"
        );
        assert_eq!(
            row.source_location, source_location,
            "{type_name} source anchor drifted"
        );
        assert_eq!(row.status, "registered");
    }
}

/// RecoveryBridgeSpec<Role> is one generic normative input-looking type, so
/// §5.1 permits exactly one classification row even though the two generated
/// command unions carry distinct Local and Meta concrete contract rows. This
/// pins the cross-role extension without laundering a duplicate type class.
#[test]
fn recovery_bridge_generic_classification_covers_both_role_contracts_once() {
    let registry = registry();
    let rows: Vec<_> = registry
        .classifications
        .iter()
        .filter(|row| row.type_name == "RecoveryBridgeSpec")
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "generic type must be classified exactly once"
    );
    assert_eq!(rows[0].class, "RegisteredCommandInput");
    assert_eq!(
        rows[0].command_contract_id.as_deref(),
        Some("cc:local:recovery-bridge-spec")
    );

    let contracts = contracts();
    for (id, input, union) in [
        (
            "cc:local:recovery-bridge-spec",
            "RecoveryBridgeSpec<Local>",
            "SequenceNeutralSpec<Tag>",
        ),
        (
            "cc:meta:recovery-bridge-spec",
            "RecoveryBridgeSpec<Meta>",
            "GlobalSequenceNeutralSpec<Tag>",
        ),
    ] {
        let contract = contracts
            .contracts
            .iter()
            .find(|contract| contract.command_contract_id == id)
            .expect("both concrete role contracts must exist");
        assert_eq!(contract.input_schema_id, input);
        assert_eq!(contract.outer_command_union, union);
    }
}

/// Unlike generic RecoveryBridgeSpec<Role>, the two Meta F2 bodies are
/// independently addressable Global types. Pin the exact type-to-contract and
/// source map so neither can be aliased to its similarly named Local body.
#[test]
fn meta_f2_begin_classifications_are_exact_and_role_distinct() {
    let registry = registry();
    for (type_name, contract_id, source_location) in [
        (
            "GlobalBeginReservationSpec",
            "cc:meta:global-begin-reservation-spec",
            "a07:1710",
        ),
        (
            "GlobalBeginTerminalSpec",
            "cc:meta:global-begin-terminal-spec",
            "a07:1714",
        ),
    ] {
        let rows: Vec<_> = registry
            .classifications
            .iter()
            .filter(|row| row.type_name == type_name)
            .collect();
        assert_eq!(rows.len(), 1, "{type_name} must classify exactly once");
        let row = rows[0];
        assert_eq!(row.class, "RegisteredCommandInput");
        assert_eq!(row.command_contract_id.as_deref(), Some(contract_id));
        assert_eq!(row.source_location, source_location);
        assert_eq!(row.status, "registered");

        let contract = contracts()
            .contracts
            .into_iter()
            .find(|contract| contract.command_contract_id == contract_id)
            .expect("classified Meta contract");
        assert_eq!(contract.role, "Meta");
        assert_eq!(contract.input_schema_id, type_name);
        assert_eq!(
            contract.outer_command_union,
            "GlobalSequenceNeutralSpec<Tag>"
        );
        assert!(!contract.command_contract_id.contains("cc:local:"));
    }
}

#[test]
fn meta_f3_attempt_registration_classification_is_exact() {
    let registry = registry();
    let rows: Vec<_> = registry
        .classifications
        .iter()
        .filter(|row| row.type_name == "GlobalAttemptRegistrationSpec")
        .collect();
    assert_eq!(rows.len(), 1, "the Global registration type is singular");
    let row = rows[0];
    assert_eq!(row.class, "RegisteredCommandInput");
    assert_eq!(
        row.command_contract_id.as_deref(),
        Some("cc:meta:global-attempt-registration-spec")
    );
    assert_eq!(row.source_location, "a09:1716");
    assert_eq!(row.status, "registered");

    let contract = contracts()
        .contracts
        .into_iter()
        .find(|contract| contract.command_contract_id == "cc:meta:global-attempt-registration-spec")
        .expect("classified Meta F3 contract");
    assert_eq!(contract.role, "Meta");
    assert_eq!(contract.input_schema_id, "GlobalAttemptRegistrationSpec");
    assert_eq!(
        contract.outer_command_union,
        "GlobalSequenceNeutralSpec<Tag>"
    );
    assert_eq!(contract.outer_wire_tag, 0x0004);
    assert_eq!(contract.inner_wire_tag, None);
}

#[test]
fn meta_f4_statement_classifications_are_exact() {
    let registry = registry();
    let contracts = contracts();
    for (type_name, contract_id, source_location, outer_tag) in [
        (
            "GlobalStatementRegistrationSpec",
            "cc:meta:global-statement-registration-spec",
            "a09:1726",
            0x0007,
        ),
        (
            "GlobalStatementPublicationSpec",
            "cc:meta:global-statement-publication-spec",
            "a09:1732",
            0x0008,
        ),
        (
            "GlobalStatementAbortSpec",
            "cc:meta:global-statement-abort-spec",
            "a09:1736",
            0x0009,
        ),
    ] {
        let rows: Vec<_> = registry
            .classifications
            .iter()
            .filter(|row| row.type_name == type_name)
            .collect();
        assert_eq!(rows.len(), 1, "{type_name} must classify exactly once");
        let row = rows[0];
        assert_eq!(row.class, "RegisteredCommandInput");
        assert_eq!(row.command_contract_id.as_deref(), Some(contract_id));
        assert_eq!(row.source_location, source_location);
        assert_eq!(row.status, "registered");

        let contract = contracts
            .contracts
            .iter()
            .find(|contract| contract.command_contract_id == contract_id)
            .expect("classified Meta F4 contract");
        assert_eq!(contract.role, "Meta");
        assert_eq!(contract.input_schema_id, type_name);
        assert_eq!(
            contract.outer_command_union,
            "GlobalSequenceNeutralSpec<Tag>"
        );
        assert_eq!(contract.outer_wire_tag, outer_tag);
        assert_eq!(contract.input_wire_tag, outer_tag);
        assert_eq!(contract.inner_wire_tag, None);
    }
}

#[test]
fn meta_f5_attempt_cancel_classification_is_exact() {
    let registry = registry();
    let rows: Vec<_> = registry
        .classifications
        .iter()
        .filter(|row| row.type_name == "GlobalAttemptCancelSpec")
        .collect();
    assert_eq!(rows.len(), 1, "GlobalAttemptCancelSpec must classify once");
    let row = rows[0];
    assert_eq!(row.class, "RegisteredCommandInput");
    assert_eq!(
        row.command_contract_id.as_deref(),
        Some("cc:meta:global-attempt-cancel-spec")
    );
    assert_eq!(row.source_location, "a09:1762");
    assert_eq!(row.status, "registered");

    let contracts = contracts();
    let contract = contracts
        .contracts
        .iter()
        .find(|contract| contract.command_contract_id == "cc:meta:global-attempt-cancel-spec")
        .expect("classified Meta F5 contract");
    assert_eq!(contract.role, "Meta");
    assert_eq!(contract.input_schema_id, "GlobalAttemptCancelSpec");
    assert_eq!(
        contract.outer_command_union,
        "GlobalSequenceNeutralSpec<Tag>"
    );
    assert_eq!(contract.outer_wire_tag, 0x000a);
    assert_eq!(contract.input_wire_tag, 0x000a);
    assert_eq!(contract.inner_wire_tag, None);
}

#[test]
fn meta_f6_prepare_admission_classification_is_exact() {
    let registry = registry();
    let rows: Vec<_> = registry
        .classifications
        .iter()
        .filter(|row| row.type_name == "GlobalPrepareAdmissionSpec")
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "GlobalPrepareAdmissionSpec must classify once"
    );
    let row = rows[0];
    assert_eq!(row.class, "RegisteredCommandInput");
    assert_eq!(
        row.command_contract_id.as_deref(),
        Some("cc:meta:global-prepare-admission-spec")
    );
    assert_eq!(row.source_location, "a09:1740");
    assert_eq!(row.status, "registered");

    let contracts = contracts();
    let contract = contracts
        .contracts
        .iter()
        .find(|contract| contract.command_contract_id == "cc:meta:global-prepare-admission-spec")
        .expect("classified Meta F6 contract");
    assert_eq!(contract.role, "Meta");
    assert_eq!(contract.input_schema_id, "GlobalPrepareAdmissionSpec");
    assert_eq!(
        contract.outer_command_union,
        "GlobalSequenceNeutralSpec<Tag>"
    );
    assert_eq!(contract.outer_wire_tag, 0x000b);
    assert_eq!(contract.input_wire_tag, 0x000b);
    assert_eq!(contract.inner_wire_tag, None);
}

#[test]
fn meta_f7_read_close_classification_is_exact() {
    let registry = registry();
    let rows: Vec<_> = registry
        .classifications
        .iter()
        .filter(|row| row.type_name == "GlobalReadCloseSpec")
        .collect();
    assert_eq!(rows.len(), 1, "GlobalReadCloseSpec must classify once");
    let row = rows[0];
    assert_eq!(row.class, "RegisteredCommandInput");
    assert_eq!(
        row.command_contract_id.as_deref(),
        Some("cc:meta:global-read-close-spec")
    );
    assert_eq!(row.source_location, "a09:1766");
    assert_eq!(row.status, "registered");

    let contracts = contracts();
    let contract = contracts
        .contracts
        .iter()
        .find(|contract| contract.command_contract_id == "cc:meta:global-read-close-spec")
        .expect("classified Meta F7 contract");
    assert_eq!(contract.role, "Meta");
    assert_eq!(contract.input_schema_id, "GlobalReadCloseSpec");
    assert_eq!(
        contract.outer_command_union,
        "GlobalSequenceNeutralSpec<Tag>"
    );
    assert_eq!(contract.outer_wire_tag, 0x000c);
    assert_eq!(contract.input_wire_tag, 0x000c);
    assert_eq!(contract.inner_wire_tag, None);
}

#[test]
fn meta_f8_final_certification_reserve_classification_is_exact() {
    let registry = registry();
    let rows: Vec<_> = registry
        .classifications
        .iter()
        .filter(|row| row.type_name == "GlobalFinalCertificationReserveSpec")
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "GlobalFinalCertificationReserveSpec must classify once"
    );
    let row = rows[0];
    assert_eq!(row.class, "RegisteredCommandInput");
    assert_eq!(
        row.command_contract_id.as_deref(),
        Some("cc:meta:global-final-certification-reserve-spec")
    );
    assert_eq!(row.source_location, "a09:1748");
    assert_eq!(row.status, "registered");

    let contracts = contracts();
    let contract = contracts
        .contracts
        .iter()
        .find(|contract| {
            contract.command_contract_id == "cc:meta:global-final-certification-reserve-spec"
        })
        .expect("classified Meta F8 contract");
    assert_eq!(contract.role, "Meta");
    assert_eq!(
        contract.input_schema_id,
        "GlobalFinalCertificationReserveSpec"
    );
    assert_eq!(
        contract.outer_command_union,
        "GlobalSequenceNeutralSpec<Tag>"
    );
    assert_eq!(contract.outer_wire_tag, 0x000d);
    assert_eq!(contract.input_wire_tag, 0x000d);
    assert_eq!(contract.inner_wire_tag, None);
}

#[test]
fn meta_f9_final_certification_cancel_classification_is_exact() {
    let registry = registry();
    let rows: Vec<_> = registry
        .classifications
        .iter()
        .filter(|row| row.type_name == "GlobalFinalCertificationCancelSpec")
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "GlobalFinalCertificationCancelSpec must classify once"
    );
    let row = rows[0];
    assert_eq!(row.class, "RegisteredCommandInput");
    assert_eq!(
        row.command_contract_id.as_deref(),
        Some("cc:meta:global-final-certification-cancel-spec")
    );
    assert_eq!(row.source_location, "a09:1754");
    assert_eq!(row.status, "registered");

    let contracts = contracts();
    let contract = contracts
        .contracts
        .iter()
        .find(|contract| {
            contract.command_contract_id == "cc:meta:global-final-certification-cancel-spec"
        })
        .expect("classified Meta F9 contract");
    assert_eq!(contract.role, "Meta");
    assert_eq!(
        contract.input_schema_id,
        "GlobalFinalCertificationCancelSpec"
    );
    assert_eq!(
        contract.outer_command_union,
        "GlobalSequenceNeutralSpec<Tag>"
    );
    assert_eq!(contract.outer_wire_tag, 0x000e);
    assert_eq!(contract.input_wire_tag, 0x000e);
    assert_eq!(contract.inner_wire_tag, None);
}

#[test]
fn meta_f10_terminal_completion_classification_is_exact() {
    let registry = registry();
    let rows: Vec<_> = registry
        .classifications
        .iter()
        .filter(|row| row.type_name == "GlobalTerminalCompletionSpec")
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "GlobalTerminalCompletionSpec must classify once"
    );
    let row = rows[0];
    assert_eq!(row.class, "RegisteredCommandInput");
    assert_eq!(
        row.command_contract_id.as_deref(),
        Some("cc:meta:global-terminal-completion-spec")
    );
    assert_eq!(row.source_location, "a09:1790");
    assert_eq!(row.status, "registered");

    let contracts = contracts();
    let contract = contracts
        .contracts
        .iter()
        .find(|contract| contract.command_contract_id == "cc:meta:global-terminal-completion-spec")
        .expect("classified Meta F10 contract");
    assert_eq!(contract.role, "Meta");
    assert_eq!(contract.input_schema_id, "GlobalTerminalCompletionSpec");
    assert_eq!(
        contract.outer_command_union,
        "GlobalSequenceNeutralSpec<Tag>"
    );
    assert_eq!(contract.outer_wire_tag, 0x000f);
    assert_eq!(contract.input_wire_tag, 0x000f);
    assert_eq!(contract.inner_wire_tag, None);
}

#[test]
fn meta_f11_never_registered_floor_classification_is_exact() {
    let registry = registry();
    let rows: Vec<_> = registry
        .classifications
        .iter()
        .filter(|row| row.type_name == "NeverRegisteredFloorSpec")
        .collect();
    assert_eq!(rows.len(), 1, "NeverRegisteredFloorSpec must classify once");
    let row = rows[0];
    assert_eq!(row.class, "RegisteredCommandInput");
    assert_eq!(
        row.command_contract_id.as_deref(),
        Some("cc:meta:never-registered-floor-spec")
    );
    assert_eq!(row.source_location, "a08:1796");
    assert_eq!(row.status, "registered");

    let contracts = contracts();
    let contract = contracts
        .contracts
        .iter()
        .find(|contract| contract.command_contract_id == "cc:meta:never-registered-floor-spec")
        .expect("classified Meta F11 contract");
    assert_eq!(contract.role, "Meta");
    assert_eq!(contract.input_schema_id, "NeverRegisteredFloorSpec");
    assert_eq!(
        contract.outer_command_union,
        "GlobalSequenceNeutralSpec<Tag>"
    );
    assert_eq!(contract.outer_wire_tag, 0x0010);
    assert_eq!(contract.input_wire_tag, 0x0010);
    assert_eq!(contract.inner_wire_tag, None);
}

#[test]
fn meta_f12_global_outcome_expiry_classification_is_exact() {
    let registry = registry();
    let rows: Vec<_> = registry
        .classifications
        .iter()
        .filter(|row| row.type_name == "GlobalOutcomeExpirySpec")
        .collect();
    assert_eq!(rows.len(), 1, "GlobalOutcomeExpirySpec must classify once");
    let row = rows[0];
    assert_eq!(row.class, "RegisteredCommandInput");
    assert_eq!(
        row.command_contract_id.as_deref(),
        Some("cc:meta:global-outcome-expiry-spec")
    );
    assert_eq!(row.source_location, "a08:1798");
    assert_eq!(row.status, "registered");

    let contracts = contracts();
    let contract = contracts
        .contracts
        .iter()
        .find(|contract| contract.command_contract_id == "cc:meta:global-outcome-expiry-spec")
        .expect("classified Meta F12 contract");
    assert_eq!(contract.role, "Meta");
    assert_eq!(contract.input_schema_id, "GlobalOutcomeExpirySpec");
    assert_eq!(
        contract.outer_command_union,
        "GlobalSequenceNeutralSpec<Tag>"
    );
    assert_eq!(contract.outer_wire_tag, 0x0011);
    assert_eq!(contract.input_wire_tag, 0x0011);
    assert_eq!(contract.inner_wire_tag, None);
}

#[test]
fn meta_f13_closed_attempt_compaction_classification_is_exact() {
    let registry = registry();
    let rows: Vec<_> = registry
        .classifications
        .iter()
        .filter(|row| row.type_name == "ClosedAttemptCompactionSpec")
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "ClosedAttemptCompactionSpec must classify once"
    );
    let row = rows[0];
    assert_eq!(row.class, "RegisteredCommandInput");
    assert_eq!(
        row.command_contract_id.as_deref(),
        Some("cc:meta:closed-attempt-compaction-spec")
    );
    assert_eq!(row.source_location, "a08:1800");
    assert_eq!(row.status, "registered");

    let contracts = contracts();
    let contract = contracts
        .contracts
        .iter()
        .find(|contract| contract.command_contract_id == "cc:meta:closed-attempt-compaction-spec")
        .expect("classified Meta F13 contract");
    assert_eq!(contract.role, "Meta");
    assert_eq!(contract.input_schema_id, "ClosedAttemptCompactionSpec");
    assert_eq!(
        contract.outer_command_union,
        "GlobalSequenceNeutralSpec<Tag>"
    );
    assert_eq!(contract.outer_wire_tag, 0x0012);
    assert_eq!(contract.input_wire_tag, 0x0012);
    assert_eq!(contract.inner_wire_tag, None);
}

/// Role specialization creates concrete Local and Meta contract families, not
/// duplicate input-looking types. Pin the singular generic classifications
/// and require both role-valid command roots to exist with their exact unions.
#[test]
fn meta_f3_ownership_uses_singular_generic_classifications() {
    let registry = registry();
    let contracts = contracts();
    for (type_name, classification_root, local_root, meta_root, armed) in [
        (
            "TxnOwnershipTransitionSpec",
            "cc:local:txn-ownership-transition-spec",
            "cc:local:txn-ownership-transition-spec",
            "cc:meta:txn-ownership-transition-spec",
            true,
        ),
        (
            "TxnOwnershipExpiryAbortSpec",
            "cc:local:txn-ownership-expiry-abort-spec",
            "cc:local:txn-ownership-expiry-abort-spec",
            "cc:meta:txn-ownership-expiry-abort-spec",
            false,
        ),
    ] {
        let rows: Vec<_> = registry
            .classifications
            .iter()
            .filter(|row| row.type_name == type_name)
            .collect();
        assert_eq!(rows.len(), 1, "{type_name} must classify exactly once");
        assert_eq!(rows[0].class, "RegisteredCommandInput");
        assert_eq!(
            rows[0].command_contract_id.as_deref(),
            Some(classification_root)
        );

        for (role, root, union, input) in [
            (
                "Local",
                local_root,
                "SequenceNeutralSpec<Tag>",
                format!("{type_name}<Local>"),
            ),
            (
                "Meta",
                meta_root,
                "GlobalSequenceNeutralSpec<Tag>",
                format!("{type_name}<Meta>"),
            ),
        ] {
            let concrete: Vec<_> = contracts
                .contracts
                .iter()
                .filter(|contract| {
                    contract.command_contract_id == root
                        || contract
                            .command_contract_id
                            .starts_with(&format!("{root}:"))
                })
                .collect();
            assert_eq!(concrete.len(), if armed { 2 } else { 1 });
            assert!(concrete.iter().all(|contract| {
                contract.role == role
                    && contract.outer_command_union == union
                    && contract.input_schema_id == input
            }));
        }
    }
}

/// The load-bearing test: open every cited plan line and require the
/// classified type to be named there. A class whose anchor does not name the
/// type is a choice, not a derivation.
#[test]
fn every_anchor_names_its_type_at_the_cited_line() {
    let plan = std::fs::read_to_string(
        repo_root().join("COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md"),
    )
    .expect("plan is readable");
    let lines: Vec<&str> = plan.lines().collect();
    let registry = registry();
    let mut checked = 0usize;
    for row in &registry.classifications {
        let anchor = row.source_location.split_once(':');
        assert!(anchor.is_some(), "{} has no aNN:LINE anchor", row.type_name);
        let (_slice, digits) = anchor.expect("anchor presence asserted above");
        let parsed: Result<usize, _> = digits.parse();
        assert!(
            parsed.is_ok(),
            "{} anchor line is not a number",
            row.type_name
        );
        let line_no = parsed.expect("digits asserted above");
        assert!(
            line_no >= 1 && line_no <= lines.len(),
            "{} cites line {line_no}, past the end of the plan ({} lines)",
            row.type_name,
            lines.len()
        );
        let text = lines[line_no - 1];
        // AuditExternalResolutionReceipt is cited via the "Batch/receipt"
        // contraction at a07:1770; the family stem is the named form.
        let stem = row
            .type_name
            .strip_suffix("Receipt")
            .filter(|_| !text.contains(&row.type_name))
            .unwrap_or(&row.type_name);
        assert!(
            text.contains(stem),
            "{} cites plan line {line_no}, but that line does not name it",
            row.type_name
        );
        checked += 1;
    }
    assert!(checked > 0, "no anchors were checked");
}

fn mutate(
    f: impl FnOnce(&mut registry_check::command_type_classification::Classification),
) -> ClassificationRegistry {
    let mut registry = registry();
    f(&mut registry.classifications[0]);
    registry
}

#[test]
fn empty_registry_is_rejected() {
    let r = ClassificationRegistry {
        registry_epoch: 1,
        classifications: Vec::new(),
    };
    assert!(
        codes(&r).contains(&"classification_registry_empty".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn epoch_below_one_is_rejected() {
    let mut r = registry();
    r.registry_epoch = 0;
    assert!(
        codes(&r).contains(&"classification_registry_epoch_invalid".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn empty_type_name_is_rejected() {
    let r = mutate(|row| row.type_name = " ".into());
    assert!(
        codes(&r).contains(&"classification_type_empty".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn duplicate_type_is_rejected() {
    let mut r = registry();
    let dup = r.classifications[0].type_name.clone();
    r.classifications[1].type_name = dup;
    assert!(
        codes(&r).contains(&"classification_type_duplicate".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn invalid_class_is_rejected() {
    let r = mutate(|row| row.class = "CommandInput".into());
    assert!(
        codes(&r).contains(&"classification_class_invalid".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn invalid_status_is_rejected() {
    let r = mutate(|row| row.status = "proposed".into());
    assert!(
        codes(&r).contains(&"classification_status_invalid".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn registered_command_input_without_contract_id_is_rejected() {
    let r = mutate(|row| row.class = "RegisteredCommandInput".into());
    assert!(
        codes(&r).contains(&"classification_contract_id_missing".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn dangling_contract_id_is_rejected() {
    let r = mutate(|row| {
        row.class = "RegisteredCommandInput".into();
        row.command_contract_id = Some("cc-does-not-exist-v1".into());
    });
    assert!(
        codes(&r).contains(&"classification_contract_unresolved".to_string()),
        "{:?}",
        codes(&r)
    );
}

/// Family resolution (round-12 T4), the differential: the armed members'
/// classification rows bind member-root ids that are NOT exact contract row
/// ids — only "{root}:{arm}" rows exist — and the registry validates clean,
/// so resolution can only have come from the family rule. If someone later
/// lands an exact member-root row, this assert flags the shape change.
#[test]
fn family_root_binding_resolves_through_arm_rows_not_an_exact_row() {
    let contracts = contracts();
    let ids: std::collections::BTreeSet<&str> = contracts
        .contracts
        .iter()
        .map(|c| c.command_contract_id.as_str())
        .collect();
    for root in [
        "cc:local:local-attempt-registration-spec",
        "cc:local:txn-ownership-transition-spec",
        "cc:local:local-outcome-compaction-spec",
        "cc:local:allocation-reservation-transition-spec",
        "cc:local:finalization-allocation-disposition-spec",
    ] {
        assert!(!ids.contains(root), "{root:?} must not be an exact row id");
        let prefix = format!("{root}:");
        assert!(
            ids.iter().any(|id| id.starts_with(prefix.as_str())),
            "{root:?} must have arm rows"
        );
    }
    let violations = validate_classifications(&registry(), &contracts);
    assert!(violations.is_empty(), "{violations:?}");
}

/// The ":"-boundary law: a bare prefix of a longer id (no arm delimiter at
/// the cut) is NOT a member root and must stay unresolved.
#[test]
fn bare_prefix_without_arm_boundary_is_rejected() {
    let r = mutate(|row| {
        row.class = "RegisteredCommandInput".into();
        // A strict prefix of "cc:local:local-attempt-registration-spec:..."
        // that does not end at a ":" boundary.
        row.command_contract_id = Some("cc:local:local-attempt".into());
    });
    assert!(
        codes(&r).contains(&"classification_contract_unresolved".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn contract_id_on_non_command_class_is_rejected() {
    let r = mutate(|row| row.command_contract_id = Some("cc-does-not-exist-v1".into()));
    assert!(
        codes(&r).contains(&"classification_contract_id_forbidden".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn malformed_anchor_is_rejected() {
    let r = mutate(|row| row.source_location = "section 5.1".into());
    assert!(
        codes(&r).contains(&"classification_source_anchor_malformed".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn empty_statement_is_rejected() {
    let r = mutate(|row| row.statement = "".into());
    assert!(
        codes(&r).contains(&"classification_statement_empty".to_string()),
        "{:?}",
        codes(&r)
    );
}

//! Cross-validator mutation witnesses for high-blast-radius laws.
//!
//! A string census can say that a violation code exists, but only a paired
//! control and mutation show that the production validator can emit it.  Each
//! test below first proves that its unmodified owner is accepted, changes one
//! load-bearing fact, and then requires the exact violation code.  Keeping the
//! control and mutation in the same harness prevents a malformed fixture from
//! masquerading as a witnessed law.
//!
//! # What this file is for
//!
//! `topology.rs`'s own docstring states the standard: *a law nobody has watched
//! fire is a law nobody knows works.*  The census behind
//! `fgdb-validator-laws-never-witnessed-firing-xnxy` measured how far the
//! repository was from it — at `dec248a`, 426 distinct violation codes, of which
//! 234 were production-reachable and had never been asserted by any test or e2e
//! script.  Green from those 234 was not evidence of anything.
//!
//! 122 of the 234 belong to `topology`, `threat`, `validate` and
//! `unsafe_ledger`.  The tables below close every one of them that an input can
//! reach; the `unsafe_ledger` share lives in `tests/unsafe_ledger.rs`, beside the
//! synthetic-workspace builder those laws need.  The remaining 112 are
//! Appendix A's, and are not touched here.
//!
//! # The shape every row keeps
//!
//! * the control is the REAL registry, and it must produce **zero** violations —
//!   so a row that fires without its mutation is impossible, not merely unlikely;
//! * the mutation changes ONE load-bearing fact, through the same public owner
//!   the production binary calls;
//! * the assertion names the exact violation code.  A string-only or
//!   count-only assertion would pass against the wrong law.
//!
//! A table rather than one function per law is deliberate: the control is loaded
//! once and each row is applied to a fresh clone, so the suite cannot drift into
//! asserting against an already-mutated registry, and a failure reports every
//! law that stopped firing rather than only the first.
//!
//! # The laws that CANNOT fire, and why that is a finding rather than a gap
//!
//! Three codes in the residue are unreachable from any input, by construction:
//!
//! * `site_scanner_self_test_failed` and `safe_facing_self_test_failed`
//!   (`unsafe_ledger::check_workspace`), and
//! * `checker_liveness_self_test_failed` (`validate::validate_all`).
//!
//! Each guards a reader against ITSELF: the predicate compares a reader's answer
//! on a compiled-in fixture against a compiled-in constant, and no argument to
//! the checker reaches it.  They are the opposite of vacuous — they are the
//! controls that license every zero the readers below them report — but no
//! registry, manifest or source tree can make one fire, so no input-driven
//! witness for them can exist.  What CAN be witnessed is that the guard
//! predicate is live rather than constant-true, and
//! [`the_liveness_self_test_guard_can_be_false`] here (with its two siblings in
//! `tests/unsafe_ledger.rs`) does exactly that: same public reader, perturbed
//! fixture, answer moves.  The remaining step — the violation itself — is
//! reachable only by breaking the reader's source, and is proved that way in the
//! bead rather than checked in.

use registry_check::model::{self, Registries, ScriptDisposition};
use registry_check::threat::{self, ThreatRegistry};
use registry_check::topology::{self, TopologyRegistry};
use registry_check::{appendix_a, liveness, unsafe_ledger, validate};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repository root resolves")
}

fn appendix_catalog() -> appendix_a::Catalog {
    let text = std::fs::read_to_string(repo_root().join(appendix_a::CATALOG_PATH))
        .expect("Appendix A catalog is readable");
    appendix_a::parse_catalog(&text).expect("unmodified Appendix A catalog is valid")
}

fn assert_code(codes: &[String], expected: &str) {
    assert!(
        codes.iter().any(|code| code == expected),
        "expected violation {expected:?}, got {codes:?}"
    );
}

#[test]
fn missing_appendix_projection_is_seen_to_fire() {
    let catalog = appendix_catalog();
    // SCOPED, not global (fgdb-guard-disabled-by-its-own-trigger-70q9). This test is
    // the ONLY witness that `projection_read` can fire, and a global emptiness
    // precondition made it unavailable whenever ANY unrelated violation was present --
    // including that code itself, so the guard was absent in exactly the circumstance it
    // was built for. The differential keeps what the global form was protecting: a
    // validator that silently returns nothing still fails the assertion below, because
    // the code must be ADDED by the mutation rather than merely present.
    let baseline = appendix_a::verify_projections(&repo_root(), &catalog);
    assert!(
        !baseline
            .iter()
            .any(|violation| violation.code == "projection_read"),
        "projection_read is already present without the mutation, so the witness below would pass \
         without the planted defect contributing anything: {baseline:?}"
    );

    let absent_root = std::env::temp_dir().join(format!(
        "fgdb-xnxy-uncreated-projection-root-{}",
        std::process::id()
    ));
    assert!(
        !absent_root.exists(),
        "mutation root must be absent so the read failure is deliberate"
    );
    let codes: Vec<String> = appendix_a::verify_projections(&absent_root, &catalog)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert_code(&codes, "projection_read");
}

#[test]
fn appendix_target_identity_mismatch_is_seen_to_fire() {
    let mut catalog = appendix_catalog();
    // SCOPED, not global (fgdb-guard-disabled-by-its-own-trigger-70q9). This test is
    // the ONLY witness that `catalog_target_identity_mismatch` can fire, and a global emptiness
    // precondition made it unavailable whenever ANY unrelated violation was present --
    // including that code itself, so the guard was absent in exactly the circumstance it
    // was built for. The differential keeps what the global form was protecting: a
    // validator that silently returns nothing still fails the assertion below, because
    // the code must be ADDED by the mutation rather than merely present.
    let baseline = appendix_a::validate_catalog(&catalog);
    assert!(
        !baseline
            .iter()
            .any(|violation| violation.code == "catalog_target_identity_mismatch"),
        "catalog_target_identity_mismatch is already present without the mutation, so the witness below would pass \
         without the planted defect contributing anything: {baseline:?}"
    );

    catalog
        .targets
        .first_mut()
        .expect("Appendix catalog has targets")
        .slice_id
        .push_str("-wrong");
    let codes: Vec<String> = appendix_a::validate_catalog(&catalog)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert_code(&codes, "catalog_target_identity_mismatch");
}

#[test]
fn unexpected_appendix_projection_class_is_seen_to_fire() {
    let mut catalog = appendix_catalog();
    // SCOPED, not global (fgdb-guard-disabled-by-its-own-trigger-70q9). This test is
    // the ONLY witness that `catalog_projection_unexpected` can fire, and a global emptiness
    // precondition made it unavailable whenever ANY unrelated violation was present --
    // including that code itself, so the guard was absent in exactly the circumstance it
    // was built for. The differential keeps what the global form was protecting: a
    // validator that silently returns nothing still fails the assertion below, because
    // the code must be ADDED by the mutation rather than merely present.
    let baseline = appendix_a::validate_catalog(&catalog);
    assert!(
        !baseline
            .iter()
            .any(|violation| violation.code == "catalog_projection_unexpected"),
        "catalog_projection_unexpected is already present without the mutation, so the witness below would pass \
         without the planted defect contributing anything: {baseline:?}"
    );

    let row = catalog
        .projection_rows
        .iter_mut()
        .find(|row| row.slice_id == "a02")
        .expect("A02 has projection rows");
    row.projection = "logical_object_kinds".to_owned();
    let codes: Vec<String> = appendix_a::validate_catalog(&catalog)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert_code(&codes, "catalog_projection_unexpected");
}

#[test]
fn workspace_unsafe_lint_drift_is_seen_to_fire() {
    let root = repo_root();
    let mut registry = topology::load_from_repo(&root).expect("unmodified topology registry loads");
    // SCOPED, not global (fgdb-guard-disabled-by-its-own-trigger-70q9). This test is
    // the ONLY witness that `workspace_unsafe_lint_drift` can fire, and a global emptiness
    // precondition made it unavailable whenever ANY unrelated violation was present --
    // including that code itself, so the guard was absent in exactly the circumstance it
    // was built for. The differential keeps what the global form was protecting: a
    // validator that silently returns nothing still fails the assertion below, because
    // the code must be ADDED by the mutation rather than merely present.
    let baseline = topology::validate_topology(&registry, &root);
    assert!(
        !baseline
            .iter()
            .any(|violation| violation.code == "workspace_unsafe_lint_drift"),
        "workspace_unsafe_lint_drift is already present without the mutation, so the witness below would pass \
         without the planted defect contributing anything: {baseline:?}"
    );

    registry.registry.workspace_unsafe_lint = "deny".to_owned();
    let codes: Vec<String> = topology::validate_topology(&registry, &root)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert_code(&codes, "workspace_unsafe_lint_drift");
}

#[test]
fn security_identity_wire_tag_collision_is_seen_to_fire() {
    let root = repo_root();
    let mut registry = threat::load_from_repo(&root).expect("unmodified threat registry loads");
    // SCOPED, not global (fgdb-guard-disabled-by-its-own-trigger-70q9). This test is
    // the ONLY witness that `identity_wire_tag_collision` can fire, and a global emptiness
    // precondition made it unavailable whenever ANY unrelated violation was present --
    // including that code itself, so the guard was absent in exactly the circumstance it
    // was built for. The differential keeps what the global form was protecting: a
    // validator that silently returns nothing still fails the assertion below, because
    // the code must be ADDED by the mutation rather than merely present.
    let baseline = threat::validate_threat(&registry, &root);
    assert!(
        !baseline
            .iter()
            .any(|violation| violation.code == "identity_wire_tag_collision"),
        "identity_wire_tag_collision is already present without the mutation, so the witness below would pass \
         without the planted defect contributing anything: {baseline:?}"
    );

    let duplicate_tag = registry
        .identities
        .first()
        .expect("threat model has identities")
        .wire_tag
        .clone();
    registry
        .identities
        .get_mut(1)
        .expect("threat model has a second identity")
        .wire_tag = duplicate_tag;
    let codes: Vec<String> = threat::validate_threat(&registry, &root)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert_code(&codes, "identity_wire_tag_collision");
}

#[test]
fn unreadable_workspace_manifest_is_seen_to_fire() {
    let root = repo_root();
    let (_, control) = unsafe_ledger::check_workspace(&root);
    assert!(
        control.is_empty(),
        "unmodified unsafe-ledger control must pass: {control:?}"
    );

    let absent_root = std::env::temp_dir().join(format!(
        "fgdb-xnxy-uncreated-workspace-root-{}",
        std::process::id()
    ));
    assert!(
        !absent_root.exists(),
        "mutation root must be absent so the manifest read failure is deliberate"
    );
    let codes: Vec<String> = unsafe_ledger::check_workspace(&absent_root)
        .1
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    assert_code(&codes, "workspace_manifest_unreadable");
}

// ===========================================================================
// workspace_topology.toml — `topology::validate_topology`
// ===========================================================================

/// One law, one fact, one expected code.
struct TopologyWitness {
    code: &'static str,
    /// The single load-bearing fact the mutation changes, in prose. It appears
    /// in the failure message, so a law that stops firing names what was done
    /// to it rather than only what was expected.
    fact: &'static str,
    mutate: fn(&mut TopologyRegistry),
}

fn crate_row_mut<'a>(r: &'a mut TopologyRegistry, name: &str) -> &'a mut topology::CrateRow {
    r.crates
        .iter_mut()
        .find(|row| row.name == name)
        .unwrap_or_else(|| panic!("crate row {name:?} exists"))
}

fn capability_mut<'a>(r: &'a mut TopologyRegistry, id: &str) -> &'a mut topology::Capability {
    r.capabilities
        .iter_mut()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("capability {id:?} exists"))
}

fn project_mut<'a>(r: &'a mut TopologyRegistry, id: &str) -> &'a mut topology::FoundationProject {
    r.foundation_projects
        .iter_mut()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("foundation project {id:?} exists"))
}

fn layer_mut<'a>(r: &'a mut TopologyRegistry, id: &str) -> &'a mut topology::Layer {
    r.layers
        .iter_mut()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("layer {id:?} exists"))
}

fn source_block_mut<'a>(r: &'a mut TopologyRegistry, id: &str) -> &'a mut topology::SourceBlock {
    r.source_blocks
        .iter_mut()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("source block {id:?} exists"))
}

#[rustfmt::skip]
fn topology_header_witnesses() -> Vec<TopologyWitness> {
    vec![
    TopologyWitness { code: "registry_name", fact: "registry.name is renamed", mutate: |r| r.registry.name = "not_workspace_topology".into() },
    TopologyWitness { code: "replay_command_drift", fact: "registry.replay_command is rewritten", mutate: |r| r.registry.replay_command = "cargo run -p registry-check --bin something-else".into() },
    TopologyWitness { code: "id_table_hash_drift", fact: "registry.id_table_hash is repinned", mutate: |r| r.registry.id_table_hash = "0000000000000000".into() },
    TopologyWitness { code: "embedded_block_unresolved", fact: "embedded_source_blocks names a block that does not exist", mutate: |r| r.registry.embedded_source_blocks.push("plan-no-such-block-v1".into()) },
    TopologyWitness { code: "required_edge_floor_vacuous", fact: "the monotone live-edge floor is emptied", mutate: |r| r.registry.required_dependency_live_floor.clear() },
    TopologyWitness { code: "required_edge_floor_regression", fact: "the live-edge floor is repointed at an edge that is still deferred", mutate: |r| r.registry.required_dependency_live_floor = vec!["txn-over-chronicle".into()] },
    TopologyWitness { code: "toolchain_channel_drift", fact: "registry.toolchain_channel is repinned", mutate: |r| r.registry.toolchain_channel = "nightly-1999-01-01".into() },
    TopologyWitness { code: "layer_order_not_dense", fact: "one layer's source_order leaves the dense range", mutate: |r| layer_mut(r, "foundation").source_order = 99 },
    TopologyWitness { code: "layer_edge_unresolved", fact: "allowed_outgoing_layers names an unknown layer", mutate: |r| layer_mut(r, "chronicle").allowed_outgoing_layers.push("no_such_layer".into()) },
    TopologyWitness { code: "reciprocal_pair_drift", fact: "layer_law.reciprocal_pair is repointed", mutate: |r| r.layer_law.reciprocal_pair = vec!["chronicle".into(), "strata".into()] },
    TopologyWitness { code: "layer_position_not_dense", fact: "one crate's layer_position leaves the dense range", mutate: |r| crate_row_mut(r, "fgdb-types").layer_position = 99 },
    TopologyWitness { code: "crate_duplicate", fact: "a crate row is repeated", mutate: |r| { let duplicate = r.crates[0].clone(); r.crates.push(duplicate); } },
    ]
}

#[rustfmt::skip]
fn topology_crate_witnesses() -> Vec<TopologyWitness> {
    vec![
    TopologyWitness { code: "crate_layer_unresolved", fact: "a crate row names an unknown layer", mutate: |r| crate_row_mut(r, "fgdb-types").layer = "no_such_layer".into() },
    TopologyWitness { code: "active_without_owner_bead", fact: "an active crate loses its owner bead", mutate: |r| crate_row_mut(r, "fgdb-types").owner_bead.clear() },
    TopologyWitness { code: "island_layer_drift", fact: "an unsafe island is moved off the unsafe_islands layer", mutate: |r| crate_row_mut(r, "fgdb-unsafe-simd").layer = "foundation".into() },
    TopologyWitness { code: "entry_crate_unclaimed", fact: "a posture's entry_crate is repointed, orphaning an entry_ row", mutate: |r| r.postures[0].entry_crate = "fgdb-cli".into() },
    TopologyWitness { code: "posture_entry_unresolved", fact: "a posture names an unregistered entry crate", mutate: |r| r.postures[0].entry_crate = "no-such-crate".into() },
    TopologyWitness { code: "design_only_pinned", fact: "the design-only donor gains a pinned revision", mutate: |r| project_mut(r, "frankensqlite").pinned_rev = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".into() },
    TopologyWitness { code: "linkable_without_pin", fact: "a linked foundation project loses its pinned revision", mutate: |r| project_mut(r, "asupersync").pinned_rev.clear() },
    TopologyWitness { code: "foundation_without_prefix", fact: "a foundation project loses its package prefixes", mutate: |r| project_mut(r, "asupersync").package_prefixes.clear() },
    TopologyWitness { code: "foundation_source_endpoint", fact: "a required edge is sourced from a foundation project", mutate: |r| r.required_dependencies[0].from_kind = "foundation".into() },
    TopologyWitness { code: "required_edge_endpoint_unresolved", fact: "a required edge names an unresolvable source", mutate: |r| r.required_dependencies[0].from = "no-such-layer".into() },
    ]
}

#[rustfmt::skip]
fn topology_capability_witnesses() -> Vec<TopologyWitness> {
    vec![
    TopologyWitness { code: "capability_owner_unresolved", fact: "a capability names an unregistered owner crate", mutate: |r| capability_mut(r, "compression-codecs").owner_crate = "no-such-crate".into() },
    TopologyWitness { code: "build_here_with_foundation", fact: "a build_here capability names a foundation project", mutate: |r| capability_mut(r, "compression-codecs").foundation_project = "asupersync".into() },
    TopologyWitness { code: "consume_from_unresolved", fact: "a consume_from capability names an unregistered project", mutate: |r| capability_mut(r, "async-runtime").foundation_project = "no-such-project".into() },
    TopologyWitness { code: "consume_from_design_only", fact: "a consume_from capability is sourced from the design-only donor", mutate: |r| capability_mut(r, "async-runtime").foundation_project = "frankensqlite".into() },
    TopologyWitness { code: "consume_from_without_asset", fact: "a consume_from capability loses its asset evidence", mutate: |r| capability_mut(r, "async-runtime").foundation_asset.clear() },
    TopologyWitness { code: "design_only_wrong_project", fact: "a design_only capability is sourced from a linked project", mutate: |r| capability_mut(r, "design-ssi").foundation_project = "asupersync".into() },
    TopologyWitness { code: "design_only_with_asset", fact: "a design_only capability names a consumed asset", mutate: |r| capability_mut(r, "design-ssi").foundation_asset = "some asset".into() },
    TopologyWitness { code: "foundation_asset_unresolved", fact: "a consume_from asset phrase stops resolving in the asset block", mutate: |r| capability_mut(r, "async-runtime").foundation_asset = "an asset phrase that appears nowhere".into() },
    TopologyWitness { code: "asset_gap_unresolved", fact: "an evidence gap names an unregistered capability", mutate: |r| r.asset_evidence_gaps[0].capability_id = "no-such-capability".into() },
    TopologyWitness { code: "asset_gap_wrong_disposition", fact: "an evidence gap is moved onto a build_here capability", mutate: |r| r.asset_evidence_gaps[0].capability_id = "compression-codecs".into() },
    TopologyWitness { code: "asset_gap_with_asset", fact: "the gapped capability gains an asset row", mutate: |r| capability_mut(r, "queue-clients").foundation_asset = "some asset".into() },
    TopologyWitness { code: "asset_gap_block_unresolved", fact: "an evidence gap names an absent source block", mutate: |r| r.asset_evidence_gaps[0].verified_absent_from = "plan-no-such-block-v1".into() },
    ]
}

#[rustfmt::skip]
fn topology_derivation_witnesses() -> Vec<TopologyWitness> {
    vec![
    TopologyWitness { code: "narrowing_crate_unresolved", fact: "a narrowing names an unregistered crate", mutate: |r| r.dependency_narrowings[0].crate_name = "no-such-crate".into() },
    TopologyWitness { code: "narrowing_layer_unresolved", fact: "a narrowing allows an unknown layer", mutate: |r| r.dependency_narrowings[0].allowed_layers.push("no_such_layer".into()) },
    TopologyWitness { code: "narrowing_exception_unresolved", fact: "a narrowing allows an unregistered crate", mutate: |r| r.dependency_narrowings[0].allowed_crates.push("no-such-crate".into()) },
    TopologyWitness { code: "source_block_missing", fact: "the §18.1 crate-layer source block is removed", mutate: |r| r.source_blocks.retain(|block| block.id != "plan-crate-layer-table-v1") },
    TopologyWitness { code: "layer_table_unparsable", fact: "the §18.1 block is repointed at prose", mutate: |r| { let block = source_block_mut(r, "plan-crate-layer-table-v1"); block.plan_start_line = 1; block.plan_end_line = 1; } },
    TopologyWitness { code: "layer_row_count_drift", fact: "the §18.1 block is truncated to fewer rows", mutate: |r| source_block_mut(r, "plan-crate-layer-table-v1").plan_end_line = 1290 },
    TopologyWitness { code: "layer_title_position_drift", fact: "a layer title stops matching its §18.1 row", mutate: |r| layer_mut(r, "foundation").title = "Not Foundation".into() },
    TopologyWitness { code: "workstream_title_drift", fact: "a workstream title stops matching §19", mutate: |r| r.owner_scopes.iter_mut().find(|scope| scope.id == "W1").expect("W1 is registered").title = "Not Bedrock".into() },
    TopologyWitness { code: "design_row_count_drift", fact: "one design_only capability is dropped", mutate: |r| { let id = r.capabilities.iter().rev().find(|row| row.disposition == "design_only").expect("a design_only row exists").id.clone(); r.capabilities.retain(|row| row.id != id); } },
    TopologyWitness { code: "design_phrase_unresolved", fact: "a design capability's source phrase matches no donor row", mutate: |r| capability_mut(r, "design-ssi").source_phrase = "a phrase no donor row contains".into() },
    TopologyWitness { code: "design_phrase_ambiguous", fact: "a design capability's source phrase matches every donor row", mutate: |r| capability_mut(r, "design-ssi").source_phrase = String::new() },
    TopologyWitness { code: "island_roster_unreadable", fact: "the island roster path points at a file that does not exist", mutate: |r| r.registry.unsafe_ledger_registry = "registries/no-such-ledger.toml".into() },
    TopologyWitness { code: "island_roster_unparsable", fact: "the island roster path points at a file that is not TOML", mutate: |r| r.registry.unsafe_ledger_registry = "AGENTS.md".into() },
    ]
}

#[rustfmt::skip]
fn topology_live_tree_witnesses() -> Vec<TopologyWitness> {
    vec![
    TopologyWitness { code: "active_not_a_member", fact: "an active row's manifest_dir is repointed off the workspace", mutate: |r| crate_row_mut(r, "fgdb-types").manifest_dir = "crates/not-a-member".into() },
    TopologyWitness { code: "active_manifest_missing", fact: "an active row's manifest_dir is repointed at an absent manifest", mutate: |r| crate_row_mut(r, "fgdb-types").manifest_dir = "crates/not-a-member".into() },
    TopologyWitness { code: "package_name_drift", fact: "an active row is renamed away from its manifest package", mutate: |r| crate_row_mut(r, "fgdb-types").name = "fgdb-types-renamed".into() },
    TopologyWitness { code: "island_inherits_forbid", fact: "an ordinary crate's unsafe_policy is relabelled deny_ledgered", mutate: |r| crate_row_mut(r, "fgdb-claim").unsafe_policy = "deny_ledgered".into() },
    TopologyWitness { code: "island_root_missing_deny", fact: "an ordinary crate's unsafe_policy is relabelled deny_ledgered", mutate: |r| crate_row_mut(r, "fgdb-claim").unsafe_policy = "deny_ledgered".into() },
    TopologyWitness { code: "internal_dependency_not_path", fact: "a reserved crate row is renamed onto a git-sourced package", mutate: |r| crate_row_mut(r, "fgdb-shard").name = "asupersync".into() },
    TopologyWitness { code: "dependency_on_inactive_crate", fact: "a depended-on crate row is demoted to planned", mutate: |r| crate_row_mut(r, "fgdb-types").activation_status = "planned".into() },
    TopologyWitness { code: "forbidden_dependency", fact: "a forbidden package prefix is repointed at a linked foundation package", mutate: |r| r.forbidden_dependencies[0].package_prefix = "asupersync".into() },
    TopologyWitness { code: "design_only_linked", fact: "a linked foundation project is relabelled design_only", mutate: |r| project_mut(r, "asupersync").linkage = "design_only".into() },
    TopologyWitness { code: "foundation_source_drift", fact: "a foundation project's git_url is repointed", mutate: |r| project_mut(r, "asupersync").git_url = "https://example.invalid/elsewhere".into() },
    ]
}

#[rustfmt::skip]
fn topology_live_graph_witnesses() -> Vec<TopologyWitness> {
    vec![
    TopologyWitness { code: "layer_inversion", fact: "the foundation layer stops allowing its own outgoing edges", mutate: |r| layer_mut(r, "foundation").allowed_outgoing_layers.clear() },
    TopologyWitness { code: "narrowing_violated", fact: "the narrowing row is repointed at a live crate it does not admit", mutate: |r| r.dependency_narrowings[0].crate_name = "fgdb-calibrate".into() },
    TopologyWitness { code: "required_edge_missing", fact: "a required edge is repointed at a project its source does not depend on", mutate: |r| r.required_dependencies.iter_mut().find(|edge| edge.id == "calibrate-over-asupersync").expect("the calibrate edge is registered").to = "franken_networkx".into() },
    TopologyWitness { code: "posture_status_drift", fact: "a deferred posture is declared live", mutate: |r| r.postures[0].status = "live".into() },
    TopologyWitness { code: "tooling_dependency", fact: "a crate that has dependencies is registered as G0 tooling", mutate: |r| r.registry.tooling_members.push("crates/fgdb-calibrate".into()) },
    // The only two-fact row in the file, and it is two because the law cannot
    // be reached with one: with the whole composition layer planned, every
    // posture closure is `deferred` and holds nothing. Making a posture live on
    // an active crate is one fact; putting a forbidden participation inside its
    // closure is the other. Both are stated here rather than hidden behind a
    // single prose label.
    TopologyWitness { code: "posture_closure_violation", fact: "a posture is made live on a crate whose closure holds a test-only crate", mutate: |r| { r.postures[0].entry_crate = "fgdb-calibrate".into(); crate_row_mut(r, "fgdb-types").posture_participation = "test_only".into(); } },
    ]
}

fn topology_codes(registry: &TopologyRegistry, root: &Path) -> BTreeSet<String> {
    topology::validate_topology(registry, root)
        .into_iter()
        .map(|violation| violation.code)
        .collect()
}

fn run_topology_witnesses(rows: Vec<TopologyWitness>) {
    let root = repo_root();
    let base = topology::load_from_repo(&root).expect("unmodified topology registry loads");
    let control = topology_codes(&base, &root);
    assert!(
        control.is_empty(),
        "the control must be clean, or no row below is a witness: {control:?}"
    );

    let mut silent: Vec<String> = Vec::new();
    for row in rows {
        let mut mutated = base.clone();
        (row.mutate)(&mut mutated);
        let codes = topology_codes(&mutated, &root);
        if !codes.contains(row.code) {
            silent.push(format!("{} [{}] -> {codes:?}", row.code, row.fact));
        }
    }
    assert!(
        silent.is_empty(),
        "{} topology laws did not fire on the fact that violates them:\n{}",
        silent.len(),
        silent.join("\n")
    );
}

#[test]
fn topology_header_laws_are_seen_to_fire() {
    run_topology_witnesses(topology_header_witnesses());
}

#[test]
fn topology_crate_laws_are_seen_to_fire() {
    run_topology_witnesses(topology_crate_witnesses());
}

#[test]
fn topology_capability_laws_are_seen_to_fire() {
    run_topology_witnesses(topology_capability_witnesses());
}

#[test]
fn topology_derivation_laws_are_seen_to_fire() {
    run_topology_witnesses(topology_derivation_witnesses());
}

#[test]
fn topology_live_tree_laws_are_seen_to_fire() {
    run_topology_witnesses(topology_live_tree_witnesses());
}

#[test]
fn topology_live_graph_laws_are_seen_to_fire() {
    run_topology_witnesses(topology_live_graph_witnesses());
}

/// The one topology law whose subject is the root itself rather than a registry
/// row: a workspace that cannot be scanned must fail, never be skipped.
#[test]
fn topology_workspace_scan_failure_is_seen_to_fire() {
    let root = repo_root();
    let base = topology::load_from_repo(&root).expect("unmodified topology registry loads");
    assert!(
        topology_codes(&base, &root).is_empty(),
        "the control must be clean"
    );

    let absent = std::env::temp_dir().join(format!(
        "fgdb-xnxy1-uncreated-topology-root-{}",
        std::process::id()
    ));
    assert!(
        !absent.exists(),
        "the mutation root must be absent so the scan failure is deliberate"
    );
    let codes = topology_codes(&base, &absent);
    assert!(
        codes.contains("workspace_scan_failed"),
        "expected workspace_scan_failed, got {codes:?}"
    );
}

// ===========================================================================
// threat_model.toml — `threat::validate_threat`
// ===========================================================================

struct ThreatWitness {
    code: &'static str,
    fact: &'static str,
    mutate: fn(&mut ThreatRegistry),
}

fn identity_mut<'a>(r: &'a mut ThreatRegistry, name: &str) -> &'a mut threat::Identity {
    r.identities
        .iter_mut()
        .find(|row| row.name == name)
        .unwrap_or_else(|| panic!("identity {name:?} exists"))
}

fn dimension_mut<'a>(r: &'a mut ThreatRegistry, id: &str) -> &'a mut threat::AuthorityDimension {
    r.authority_dimensions
        .iter_mut()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("authority dimension {id:?} exists"))
}

fn threat_posture_mut<'a>(r: &'a mut ThreatRegistry, id: &str) -> &'a mut threat::Posture {
    r.postures
        .iter_mut()
        .find(|row| row.id == id)
        .unwrap_or_else(|| panic!("posture {id:?} exists"))
}

#[rustfmt::skip]
fn threat_witnesses() -> Vec<ThreatWitness> {
    vec![
    // header and source blocks
    ThreatWitness { code: "document_path", fact: "registry.document_path is repointed", mutate: |r| r.registry.document_path = "docs/SOMEWHERE_ELSE.md".into() },
    ThreatWitness { code: "source_block_path", fact: "a source block's plan_path is repointed off the plan", mutate: |r| r.source_blocks[0].plan_path = "AGENTS.md".into() },
    ThreatWitness { code: "source_block_unreadable", fact: "a source block's line range runs past the end of the plan", mutate: |r| { r.source_blocks[0].plan_start_line = 10_000_000; r.source_blocks[0].plan_end_line = 10_000_001; } },
    // actors, assets, assumptions, registered rejections
    ThreatWitness { code: "actor_unsummarized", fact: "an actor loses its summary", mutate: |r| r.actors[0].summary.clear() },
    ThreatWitness { code: "asset_unbound_claim", fact: "an asset loses its primary claim reference", mutate: |r| r.assets[0].primary_claim_ref.clear() },
    ThreatWitness { code: "assumption_unbounded", fact: "an assumption loses its bounds", mutate: |r| r.assumptions[0].bounds.clear() },
    ThreatWitness { code: "out_of_scope_unreasoned", fact: "a registered rejection loses its rationale", mutate: |r| r.out_of_scope[0].rationale.clear() },
    ThreatWitness { code: "ordinal_gap", fact: "an actor's source_order leaves the dense range", mutate: |r| r.actors[0].source_order = 99 },
    // exposure matrix
    ThreatWitness { code: "exposure_malformed", fact: "an exposure entry loses its asset:assumption shape", mutate: |r| r.actors[0].defended_assets[0] = "no-binding-separator".into() },
    ThreatWitness { code: "exposure_unknown_asset", fact: "an exposure entry names an unregistered asset", mutate: |r| r.actors[0].defended_assets[0] = "no_such_asset:A-PROCESS-TRUSTED".into() },
    ThreatWitness { code: "exposure_duplicated", fact: "one asset is dispositioned twice for the same actor", mutate: |r| { let duplicate = r.actors[0].defended_assets[0].clone(); r.actors[0].conditional_assets.push(duplicate); } },
    // identity and epoch lattice
    ThreatWitness { code: "epoch_domains_unify", fact: "the adaptive epoch is moved into the security domain", mutate: |r| identity_mut(r, "DecisionPolicyEpoch").epoch_domain = "security".into() },
    ThreatWitness { code: "epoch_identity_absent", fact: "the security epoch identity is renamed away", mutate: |r| identity_mut(r, "SecurityPolicyEpoch").name = "RetiredEpoch".into() },
    // presentation bindings
    ThreatWitness { code: "binding_rank_not_dense", fact: "a presentation binding's rank leaves the dense range", mutate: |r| r.presentation_bindings[0].rank = 9 },
    ThreatWitness { code: "binding_transition_duplicated", fact: "a transition cell is declared twice", mutate: |r| { let duplicate = r.binding_transitions[0].clone(); r.binding_transitions.push(duplicate); } },
    ThreatWitness { code: "binding_transition_law", fact: "a transition's declared law contradicts the rank order", mutate: |r| r.binding_transitions[0].law = "weakened_binding".into() },
    ThreatWitness { code: "binding_transition_missing", fact: "one cell of the transition matrix is dropped", mutate: |r| { r.binding_transitions.pop(); } },
    ThreatWitness { code: "binding_transition_unknown_class", fact: "a transition names an unregistered presentation binding", mutate: |r| r.binding_transitions[0].from = "NoSuchBinding".into() },
    // attenuation laws
    ThreatWitness { code: "attenuation_law_ungoverned", fact: "an attenuation law governs no authority dimension", mutate: |r| r.attenuation_laws[0].dimension_ids.clear() },
    ThreatWitness { code: "attenuation_law_no_fixture", fact: "an attenuation law names no negative fixture", mutate: |r| r.attenuation_laws[0].negative_fixture.clear() },
    ThreatWitness { code: "attenuation_law_one_sided", fact: "every prohibition is dropped, leaving only permissions", mutate: |r| r.attenuation_laws.retain(|law| law.class == "permitted") },
    ThreatWitness { code: "prohibition_operator_mismatch", fact: "a governed dimension's narrowing_operator stops matching its prohibition", mutate: |r| dimension_mut(r, "redaction_profile").narrowing_operator = "intersect".into() },
    // product space and postures
    ThreatWitness { code: "product_space_cell_count", fact: "the declared product-space cell count drifts", mutate: |r| r.product_space.cell_count += 1 },
    ThreatWitness { code: "posture_off_axis", fact: "a posture is placed off a declared axis", mutate: |r| r.postures[0].service_class = "NotOnAnyAxis".into() },
    ThreatWitness { code: "posture_duplicated", fact: "a posture id is declared twice", mutate: |r| { let duplicate = r.postures[0].clone(); r.postures.push(duplicate); } },
    ThreatWitness { code: "posture_empty_unjustified", fact: "an empty-footprint posture loses its justification", mutate: |r| threat_posture_mut(r, "local_directory_bound").empty_justification.clear() },
    ThreatWitness { code: "posture_complete_with_justification", fact: "a complete-footprint posture gains an empty justification", mutate: |r| threat_posture_mut(r, "local_external_cas").empty_justification = "not applicable here".into() },
    ThreatWitness { code: "posture_empty_with_rows", fact: "a posture carrying footprint rows is relabelled empty", mutate: |r| threat_posture_mut(r, "local_external_cas").footprint_declaration = "empty".into() },
    ThreatWitness { code: "exclusion_law_vacuous", fact: "an exclusion law's id stops resolving any excluded cell", mutate: |r| r.exclusion_laws[0].id = "PX-UNREFERENCED".into() },
    // deferred postures
    ThreatWitness { code: "deferred_posture_unowned", fact: "a deferred posture loses its owner bead", mutate: |r| r.deferred_postures[0].owner_bead.clear() },
    ThreatWitness { code: "deferred_posture_unexplained", fact: "a deferred posture loses its reason", mutate: |r| r.deferred_postures[0].reason.clear() },
    ThreatWitness { code: "deferred_posture_also_registered", fact: "a deferred posture takes a registered posture's id", mutate: |r| r.deferred_postures[0].id = "local_directory_bound".into() },
    // authority footprint
    ThreatWitness { code: "footprint_unknown_posture", fact: "a footprint row names an unregistered posture", mutate: |r| r.footprints[0].posture_id = "no_such_posture".into() },
    ThreatWitness { code: "footprint_unknown_authority", fact: "a footprint row names an unregistered authority", mutate: |r| r.footprints[0].authority_id = "no_such_authority".into() },
    ThreatWitness { code: "footprint_duplicated", fact: "a footprint cell is declared twice", mutate: |r| { let duplicate = r.footprints[0].clone(); r.footprints.push(duplicate); } },
    ThreatWitness { code: "footprint_empty_named_classes", fact: "a cell naming no operation class is relabelled named_in_source", mutate: |r| r.footprints[0].operation_class_basis = "named_in_source".into() },
    ThreatWitness { code: "footprint_unknown_operation_class", fact: "a named_in_source cell names a class outside the closed sixteen", mutate: |r| { let mut row = r.footprints[0].clone(); row.authority_id = "transparency_witness".into(); row.operation_class_basis = "named_in_source".into(); row.operation_classes = vec!["not_an_operation_class".into()]; r.footprints.push(row); } },
    ThreatWitness { code: "footprint_unowned_deferral", fact: "a trigger-site-only cell loses its deferred binding owner", mutate: |r| r.footprints[0].deferred_binding_owner.clear() },
    ThreatWitness { code: "footprint_position_absent", fact: "a synchronous-path cell loses its position", mutate: |r| r.footprints[0].sync_path_position.clear() },
    ThreatWitness { code: "footprint_zero_touches", fact: "a synchronous-path cell drops to zero touches", mutate: |r| r.footprints[0].touch_count = 0 },
    ]
}

#[test]
fn threat_model_laws_are_seen_to_fire() {
    let root = repo_root();
    let base = threat::load_from_repo(&root).expect("unmodified threat registry loads");
    let control: BTreeSet<String> = threat::validate_threat(&base, &root)
        .into_iter()
        .map(|violation| violation.code)
        .collect();
    // SCOPED PER ROW, not global (fgdb-guard-disabled-by-its-own-trigger-70q9). One
    // global "the control must be clean" gated this ENTIRE witness table: a single
    // ambient violation anywhere disabled every row, including rows whose code was
    // nowhere near it. The baseline is now tolerated, and only the rows it actually
    // collides with are withheld -- and those are REPORTED as UNRUN rather than
    // silently skipped, because a witness that did not run must read as neither passed
    // nor failed (fgdb-1nqb, and the PASS/FAIL/RED/UNRUN vocabulary of
    // scripts/lib/gate_verdict.sh, fgdb-udco).

    let mut silent: Vec<String> = Vec::new();
    let mut unrun: Vec<String> = Vec::new();
    for row in threat_witnesses() {
        if control.contains(row.code) {
            unrun.push(format!("{} [{}]", row.code, row.fact));
            continue;
        }
        let mut mutated = base.clone();
        (row.mutate)(&mut mutated);
        let codes: BTreeSet<String> = threat::validate_threat(&mutated, &root)
            .into_iter()
            .map(|violation| violation.code)
            .collect();
        if !codes.contains(row.code) {
            silent.push(format!("{} [{}] -> {codes:?}", row.code, row.fact));
        }
    }
    assert!(
        silent.is_empty(),
        "{} threat-model laws did not fire on the fact that violates them:\n{}",
        silent.len(),
        silent.join("\n")
    );
    assert!(
        unrun.is_empty(),
        "UNRUN: {} witness row(s) were not exercised because their code is already \
         present in the baseline, so mutating for them would prove nothing. UNRUN is \
         neither pass nor fail:\n{}",
        unrun.len(),
        unrun.join("\n")
    );
}

// ===========================================================================
// the claim registries — `validate::validate_all`
// ===========================================================================

struct RegistryWitness {
    code: &'static str,
    fact: &'static str,
    mutate: fn(&mut Registries),
}

#[rustfmt::skip]
fn registry_witnesses() -> Vec<RegistryWitness> {
    vec![
    RegistryWitness { code: "unregistered_justifier", fact: "a clause justifies itself by an unregistered row", mutate: |r| r.invariants.invariants[0].clauses[0].justified_by.push("no-such-registry-row".into()) },
    RegistryWitness { code: "script_undeclared", fact: "one on-disk script loses its disposition row", mutate: |r| { r.script_dispositions.pop(); } },
    RegistryWitness { code: "script_disposition_dangling", fact: "a disposition names a script that does not exist", mutate: |r| r.script_dispositions.push(ScriptDisposition { path: "scripts/no_such_script.sh".into(), role: "advisory".into(), reason: "witness fixture".into() }) },
    RegistryWitness { code: "script_disposition_conflict", fact: "a registered gate also declares a non-gate disposition", mutate: |r| { let artifact = r.checker_index.iter().map(|row| row.artifact.clone()).find(|artifact| artifact.starts_with("scripts/")).expect("a registered scripts/ artifact exists"); r.script_dispositions.push(ScriptDisposition { path: artifact, role: "advisory".into(), reason: "witness fixture".into() }); } },
    ]
}

fn registry_codes(registries: &Registries, root: &Path) -> BTreeSet<String> {
    validate::validate_all(registries, root)
        .into_iter()
        .map(|violation| violation.code)
        .collect()
}

#[test]
fn claim_registry_laws_are_seen_to_fire() {
    let root = repo_root();
    let base = model::load_registries(&root.join("registries")).expect("registries load");
    let control = registry_codes(&base, &root);
    // SCOPED PER ROW, not global (fgdb-guard-disabled-by-its-own-trigger-70q9). One
    // global "the control must be clean" gated this ENTIRE witness table: a single
    // ambient violation anywhere disabled every row, including rows whose code was
    // nowhere near it. The baseline is now tolerated, and only the rows it actually
    // collides with are withheld -- and those are REPORTED as UNRUN rather than
    // silently skipped, because a witness that did not run must read as neither passed
    // nor failed (fgdb-1nqb, and the PASS/FAIL/RED/UNRUN vocabulary of
    // scripts/lib/gate_verdict.sh, fgdb-udco).

    let mut silent: Vec<String> = Vec::new();
    let mut unrun: Vec<String> = Vec::new();
    for row in registry_witnesses() {
        if control.contains(row.code) {
            unrun.push(format!("{} [{}]", row.code, row.fact));
            continue;
        }
        let mut mutated = base.clone();
        (row.mutate)(&mut mutated);
        let codes = registry_codes(&mutated, &root);
        if !codes.contains(row.code) {
            silent.push(format!("{} [{}] -> {codes:?}", row.code, row.fact));
        }
    }
    assert!(
        silent.is_empty(),
        "{} claim-registry laws did not fire on the fact that violates them:\n{}",
        silent.len(),
        silent.join("\n")
    );
    assert!(
        unrun.is_empty(),
        "UNRUN: {} witness row(s) were not exercised because their code is already \
         present in the baseline, so mutating for them would prove nothing. UNRUN is \
         neither pass nor fail:\n{}",
        unrun.len(),
        unrun.join("\n")
    );
}

// --- the laws whose subject is the tree, not a registry row -----------------

fn scratch_root(tag: &str) -> PathBuf {
    // Process-scoped: a bare name lets a concurrent run of this binary delete
    // the fixture out from under this one, which fails a DIFFERENT test each
    // round and reads exactly like a real defect.
    let dir = std::env::temp_dir().join(format!("fgdb-xnxy1-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("fixture root");
    dir
}

fn write_fixture(root: &Path, relative: &str, text: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("fixture parent")).expect("fixture directory");
    fs::write(&path, text).expect("fixture file");
}

const WITNESS_REFS: &str = "active_logical_object_kinds! {\n    Alpha = 0x0001 => \"Alpha\",\n}\n";
const WITNESS_KINDS: &str =
    "schema_version = 1\n\n[[kind]]\nobject_kind = 0x0001\nname = \"Alpha\"\nstatus = \"active\"\n";

#[test]
fn claim_registry_tree_laws_are_seen_to_fire() {
    let repo = repo_root();
    let base = model::load_registries(&repo.join("registries")).expect("registries load");
    assert!(
        registry_codes(&base, &repo).is_empty(),
        "the control must be clean, or no row below is a witness"
    );

    let mut silent: Vec<String> = Vec::new();
    let mut witness = |tag: &str, code: &str, build: &dyn Fn(&Path)| {
        let root = scratch_root(tag);
        build(&root);
        let codes = registry_codes(&base, &root);
        if !codes.contains(code) {
            silent.push(format!("{code} [{tag}] -> {codes:?}"));
        }
    };

    witness("scan-failed", "script_scan_failed", &|_| {});
    witness("scan-empty", "script_scan_empty", &|root| {
        fs::create_dir_all(root.join("scripts")).expect("empty scripts directory");
    });
    witness(
        "refs-unreadable",
        "active_logical_kind_source_unreadable",
        &|_| {},
    );
    witness(
        "macro-absent",
        "active_logical_kind_macro_absent",
        &|root| {
            write_fixture(
                root,
                "crates/fgdb-types/src/refs.rs",
                "// no active_logical_object_kinds! invocation here\n",
            );
            write_fixture(root, "registries/logical_object_kinds.toml", WITNESS_KINDS);
        },
    );
    witness(
        "arm-duplicate",
        "active_logical_kind_arm_duplicate",
        &|root| {
            write_fixture(
                root,
                "crates/fgdb-types/src/refs.rs",
                "active_logical_object_kinds! {\n    Alpha = 0x0001 => \"Alpha\",\n    Beta = 0x0001 => \"Beta\",\n}\n",
            );
            write_fixture(root, "registries/logical_object_kinds.toml", WITNESS_KINDS);
        },
    );
    witness(
        "kinds-unparseable",
        "active_logical_kind_registry_unparseable",
        &|root| {
            write_fixture(root, "crates/fgdb-types/src/refs.rs", WITNESS_REFS);
            write_fixture(
                root,
                "registries/logical_object_kinds.toml",
                "this is not = = toml\n",
            );
        },
    );
    witness(
        "row-unreadable",
        "active_logical_kind_row_unreadable",
        &|root| {
            write_fixture(root, "crates/fgdb-types/src/refs.rs", WITNESS_REFS);
            write_fixture(
                root,
                "registries/logical_object_kinds.toml",
                "schema_version = 1\n\n[[kind]]\nobject_kind = 0x0001\nstatus = \"active\"\n",
            );
        },
    );

    assert!(
        silent.is_empty(),
        "{} claim-registry tree laws did not fire on the fact that violates them:\n{}",
        silent.len(),
        silent.join("\n")
    );
}

// ===========================================================================
// The law that cannot fire, and its live-guard witness
// ===========================================================================

/// `checker_liveness_self_test_failed` is unreachable from any input.
///
/// `liveness::self_test()` takes no arguments: it asks the liveness readers a
/// fixed set of questions whose answers are compiled in, so no registry, no
/// manifest and no source tree can move its verdict. That is the point of it —
/// it is the control that licenses every clean liveness verdict below it — but
/// it means the code is emitted only by a reader regression, never by an input,
/// and an input-driven witness for it cannot exist.
///
/// What is witnessable is that the guard is a live predicate rather than a
/// constant: the same public readers give a DIFFERENT answer on perturbed text.
/// If this stops holding, `self_test().licensed()` has become true by
/// construction and the control it provides is worth nothing.
#[test]
fn the_liveness_self_test_guard_can_be_false() {
    let control = liveness::self_test();
    assert!(
        control.licensed(),
        "the shipped liveness readers must answer their own fixture correctly: {control:?}"
    );
    assert!(
        control.cases > 0,
        "a self-test over zero cases is licensed by construction and licenses nothing"
    );

    // The readers are input-sensitive, and specifically comment-aware, which is
    // what half the self-test's cases assert. `mask_shell` is the public reader
    // under the shell half of it: a reader that had stopped reading would return
    // its input unchanged and pass every "the comment was ignored" case for the
    // wrong reason.
    let commented = "#!/usr/bin/env bash\n# exit 1\nexit 0\n";
    let live = "#!/usr/bin/env bash\nexit 1\n";
    assert!(
        !liveness::mask_shell(commented).contains("exit 1"),
        "the shell reader must blank a commented exit"
    );
    assert!(
        liveness::mask_shell(live).contains("exit 1"),
        "the shell reader must leave live text in place"
    );
}

// ===========================================================================
// The suite's own guard
// ===========================================================================

/// Every table row must name a distinct law, and the total must be the pinned
/// one.
///
/// Without this a row can be deleted, or two rows can collapse onto one code
/// after a rename, and the suite still passes — the exact failure mode the whole
/// file exists to remove, reached from inside. The pin is a local fact about
/// this file, not a census of the corpus, so it moves only when a row is
/// deliberately added or removed.
#[test]
fn every_witness_row_names_a_distinct_law() {
    let mut codes: Vec<&'static str> = Vec::new();
    for group in [
        topology_header_witnesses(),
        topology_crate_witnesses(),
        topology_capability_witnesses(),
        topology_derivation_witnesses(),
        topology_live_tree_witnesses(),
        topology_live_graph_witnesses(),
    ] {
        codes.extend(group.iter().map(|row| row.code));
    }
    codes.extend(threat_witnesses().iter().map(|row| row.code));
    codes.extend(registry_witnesses().iter().map(|row| row.code));

    assert_eq!(codes.len(), 107, "table row count moved");
    let distinct: BTreeSet<&str> = codes.iter().copied().collect();
    // `active_not_a_member`/`active_manifest_missing` and
    // `island_inherits_forbid`/`island_root_missing_deny` are two laws each
    // reached by one fact, so they are two rows with two codes; nothing here
    // may share a code.
    assert_eq!(
        distinct.len(),
        codes.len(),
        "two rows name the same law, so one of them is proving nothing"
    );
    assert!(
        codes.iter().all(|code| !code.is_empty()),
        "a row with an empty code asserts nothing"
    );
}

//! registry-check CLI — the claims-lint / registry-validation CI job.
//!
//! Subcommands (all emit deterministic JSONL events on stdout; human
//! diagnostics go to stderr; exit 0 = clean, 1 = violations, 2 = usage or
//! load error):
//!
//!   registry-check validate --root <repo-root>
//!   registry-check lint     --root <repo-root>
//!   registry-check closure  --root <repo-root> --manifest <path>
//!   registry-check hash     --root <repo-root>
//!   registry-check identity --root <repo-root>
//!   registry-check appendix --root <repo-root>
//!   registry-check appendix-generate --root <repo-root>
//!   registry-check all      --root <repo-root> [--manifest <path>]

use registry_check::appendix_a;
use registry_check::closure;
use registry_check::hash::id_table_hash;
use registry_check::identity;
use registry_check::jsonl::{JsonValue, arr, b, event, n, s};
use registry_check::lint;
use registry_check::model::{self, Registries};
use registry_check::validate::{self, Violation, expected_invariant_ids};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

struct Args {
    command: String,
    root: PathBuf,
    manifest: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    let mut argv = std::env::args().skip(1);
    let command = argv.next().ok_or_else(usage)?;
    let mut root: Option<PathBuf> = None;
    let mut manifest: Option<PathBuf> = None;
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--root" => {
                root = Some(PathBuf::from(argv.next().ok_or("--root requires a value")?));
            }
            "--manifest" => {
                manifest = Some(PathBuf::from(
                    argv.next().ok_or("--manifest requires a value")?,
                ));
            }
            other => return Err(format!("unknown flag {other:?}\n{}", usage())),
        }
    }
    Ok(Args {
        command,
        root: root.unwrap_or_else(|| PathBuf::from(".")),
        manifest,
    })
}

fn usage() -> String {
    concat!(
        "usage: registry-check ",
        "<validate|lint|closure|hash|identity|appendix|appendix-generate|all> ",
        "--root <repo-root> [--manifest <path>]\n",
        "  appendix           verify the Appendix A catalog, source, and checked-in projections\n",
        "  appendix-generate  render in memory and byte-verify the six Appendix A projections"
    )
    .to_string()
}

fn identity_row_faults(violations: &[Violation], row_id: &str) -> usize {
    violations
        .iter()
        .filter(|violation| violation.row_id == row_id)
        .count()
}

fn numeric_array(values: &[i64]) -> JsonValue {
    JsonValue::Array(values.iter().copied().map(JsonValue::Int).collect())
}

fn appendix_violation_message(violation: &appendix_a::Violation) -> String {
    match violation.code.as_str() {
        "catalog_read" => "cannot read the canonical Appendix A catalog".to_string(),
        "source_read" => "cannot read the canonical Appendix A plan source".to_string(),
        "projection_read" => "cannot read a checked-in Appendix A projection".to_string(),
        _ => "Appendix A contract check failed".to_string(),
    }
}

fn appendix_violation_row_id(violation: &appendix_a::Violation) -> &str {
    let row_id = violation.row_id.as_str();
    let fixed_row_id = matches!(
        row_id,
        "catalog"
            | "catalog_rows"
            | "source_manifest"
            | "reference_manifest"
            | "slice_manifest"
            | "projection_rows"
            | "projection_files"
            | "reservation"
            | "annotation"
            | "maintenance_proof"
            | "top_level_candidate"
            | "target"
            | "semantic_binding"
            | "evidence"
            | "source_symbol_disposition"
            | "g0"
            | "plan"
            | "a01"
            | "a02"
            | "a03"
            | "a04"
            | "a05"
            | "a06"
            | "a07"
            | "a08"
            | "a09"
            | "a10"
            | "a11"
            | "a12"
            | "a13"
            | "a14"
            | "a15"
            | "a16"
            | "a17"
            | "a18"
            | "a19"
            | "a20"
            | "a21"
    );
    let fixed_projection = appendix_a::PROJECTION_FILES
        .iter()
        .any(|(registry, file)| row_id == *registry || row_id == *file);
    if fixed_row_id || fixed_projection {
        row_id
    } else {
        "catalog_row"
    }
}

fn appendix_has_structural_error(violations: &[appendix_a::Violation]) -> bool {
    violations.iter().any(|violation| {
        matches!(
            violation.code.as_str(),
            "catalog_read"
                | "catalog_encoding"
                | "catalog_toml_parse"
                | "catalog_schema"
                | "catalog_unknown_key"
                | "catalog_projection_schema"
                | "source_read"
                | "projection_read"
        )
    })
}

fn emit_appendix_violations(violations: &[appendix_a::Violation]) {
    for violation in violations {
        let msg = appendix_violation_message(violation);
        let row_id = appendix_violation_row_id(violation);
        println!(
            "{}",
            event(&[
                ("event", s("violation")),
                ("code", s(&violation.code)),
                ("registry", s(appendix_a::CATALOG_NAME)),
                ("row_id", s(row_id)),
                ("msg", s(&msg)),
            ])
        );
        eprintln!(
            "violation[{}] {}::{}: {}",
            violation.code,
            appendix_a::CATALOG_NAME,
            row_id,
            msg
        );
    }
}

fn finish_appendix_load_failure(
    completion_event: &str,
    violations: &[appendix_a::Violation],
) -> Result<usize, String> {
    emit_appendix_violations(violations);
    let structural = appendix_has_structural_error(violations);
    println!(
        "{}",
        event(&[
            ("event", s(completion_event)),
            ("slices", n(0)),
            ("projection_rows", n(0)),
            ("projection_files", n(0)),
            ("reservations", n(0)),
            ("source_dispositions", n(0)),
            ("top_level_candidates", n(0)),
            ("targets", n(0)),
            ("semantic_bindings", n(0)),
            ("evidence_rows", n(0)),
            ("violations", n(violations.len() as i64)),
            ("outcome", s(if structural { "error" } else { "fail" })),
        ])
    );
    if structural {
        Err("Appendix A structural load failed; see redacted violation events".to_string())
    } else {
        Ok(violations.len())
    }
}

fn emit_appendix_catalog(
    catalog: &appendix_a::Catalog,
    projection_violations: &[appendix_a::Violation],
) {
    let manifest = &catalog.source_manifest;
    println!(
        "{}",
        event(&[
            ("event", s("appendix_source_manifest")),
            ("catalog", s(appendix_a::CATALOG_PATH)),
            ("plan_path", s(&manifest.plan_path)),
            ("start_line", n(manifest.start_line)),
            ("end_line", n(manifest.end_line)),
            ("line_count", n(manifest.line_count)),
            ("byte_count", n(manifest.byte_count)),
            ("sha256", s(&manifest.sha256)),
            ("heading", s(&manifest.heading)),
            ("next_heading", s(&manifest.next_heading)),
            ("source_encoding", s(&catalog.source_encoding)),
            ("hash_algorithm", s(&catalog.hash_algorithm)),
            ("outcome", s("pass")),
        ])
    );

    let reference = &catalog.reference_manifest;
    println!(
        "{}",
        event(&[
            ("event", s("appendix_reference_manifest")),
            ("target_count", n(reference.target_count)),
            ("target_ids_sha256", s(&reference.target_ids_sha256)),
            ("occurrence_count", n(reference.occurrence_count)),
            (
                "occurrence_transcript_sha256",
                s(&reference.occurrence_transcript_sha256),
            ),
            ("outcome", s("pass")),
        ])
    );

    for slice in &catalog.slices {
        println!(
            "{}",
            event(&[
                ("event", s("appendix_slice_checked")),
                ("ordinal", n(slice.ordinal)),
                ("row_id", s(&slice.id)),
                ("bead_id", s(&slice.bead_id)),
                ("title", s(&slice.title)),
                ("start_line", n(slice.start_line)),
                ("end_line", n(slice.end_line)),
                ("line_count", n(slice.line_count)),
                ("byte_count", n(slice.byte_count)),
                ("sha256", s(&slice.sha256)),
                ("predecessor", s(&slice.predecessor)),
                ("successor", s(&slice.successor)),
                (
                    "expected_projection_classes",
                    arr(slice.expected_projection_classes.clone()),
                ),
                ("definition_status", s(&slice.definition_status)),
                (
                    "top_level_candidate_count",
                    n(slice.top_level_candidate_count),
                ),
                (
                    "top_level_candidate_ids_sha256",
                    s(&slice.top_level_candidate_ids_sha256),
                ),
                ("field_candidate_count", n(slice.field_candidate_count)),
                (
                    "field_candidate_ids_sha256",
                    s(&slice.field_candidate_ids_sha256),
                ),
                ("union_candidate_count", n(slice.union_candidate_count)),
                (
                    "union_candidate_ids_sha256",
                    s(&slice.union_candidate_ids_sha256),
                ),
                ("arm_candidate_count", n(slice.arm_candidate_count)),
                (
                    "arm_candidate_ids_sha256",
                    s(&slice.arm_candidate_ids_sha256),
                ),
                ("ambiguity_count", n(slice.ambiguity_count)),
                (
                    "ambiguity_ids_sha256",
                    s(&slice.ambiguity_ids_sha256),
                ),
                ("outcome", s("pass")),
            ])
        );
    }

    for (registry, file) in appendix_a::PROJECTION_FILES {
        let rows = catalog
            .projection_rows
            .iter()
            .filter(|row| row.projection == registry)
            .count();
        let registry_epoch = catalog
            .projection_epochs
            .get(registry)
            .copied()
            .unwrap_or_default();
        let violations = projection_violations
            .iter()
            .filter(|violation| violation.row_id == file)
            .count();
        println!(
            "{}",
            event(&[
                ("event", s("appendix_projection_checked")),
                ("registry", s(registry)),
                ("file", s(file)),
                ("rows", n(rows as i64)),
                ("registry_epoch", n(registry_epoch)),
                ("violations", n(violations as i64)),
                ("outcome", s(if violations == 0 { "pass" } else { "fail" }),),
            ])
        );
    }
    println!(
        "{}",
        event(&[
            ("event", s("appendix_closure_checked")),
            ("reservations", n(catalog.reservations.len() as i64)),
            (
                "existing_reservations",
                n(catalog
                    .reservations
                    .iter()
                    .filter(|row| row.disposition == "existing")
                    .count() as i64),
            ),
            (
                "reserved_reservations",
                n(catalog
                    .reservations
                    .iter()
                    .filter(|row| row.disposition == "reserved")
                    .count() as i64),
            ),
            (
                "source_dispositions",
                n(catalog.source_symbol_dispositions.len() as i64),
            ),
            (
                "top_level_candidates",
                n(catalog.top_level_candidates.len() as i64),
            ),
            ("targets", n(catalog.targets.len() as i64)),
            (
                "semantic_bindings",
                n(catalog.semantic_bindings.len() as i64),
            ),
            ("evidence_rows", n(catalog.evidence.len() as i64)),
            (
                "reference_only_symbols",
                n(catalog
                    .source_symbol_dispositions
                    .iter()
                    .filter(|row| row.disposition == "reference-only")
                    .count() as i64),
            ),
            (
                "appendix_structural_symbols",
                n(catalog
                    .source_symbol_dispositions
                    .iter()
                    .filter(|row| row.disposition == "appendix-structural-definition")
                    .count() as i64),
            ),
            (
                "outside_structural_symbols",
                n(catalog
                    .source_symbol_dispositions
                    .iter()
                    .filter(|row| row.disposition == "outside-structural-definition")
                    .count() as i64),
            ),
            (
                "source_location_pairs",
                n(catalog
                    .source_symbol_dispositions
                    .iter()
                    .filter(|row| row.slice_id != "g0")
                    .map(|row| row.source_locations.len())
                    .sum::<usize>() as i64),
            ),
            (
                "g0_projection_dispositions",
                n(catalog
                    .source_symbol_dispositions
                    .iter()
                    .filter(|row| row.slice_id == "g0")
                    .count() as i64),
            ),
            ("outcome", s("pass")),
        ])
    );
}

/// Verify Appendix A without mutating its generated consumer registries.
fn run_appendix(root: &Path) -> Result<usize, String> {
    let catalog = match appendix_a::load_and_verify(root) {
        Ok(catalog) => catalog,
        Err(violations) => return finish_appendix_load_failure("appendix_completed", &violations),
    };
    let violations = appendix_a::appendix_a_catalog_projection_diff(root, &catalog);
    emit_appendix_catalog(&catalog, &violations);
    emit_appendix_violations(&violations);
    let structural = appendix_has_structural_error(&violations);
    println!(
        "{}",
        event(&[
            ("event", s("appendix_completed")),
            ("slices", n(catalog.slices.len() as i64)),
            ("projection_rows", n(catalog.projection_rows.len() as i64),),
            (
                "projection_files",
                n(appendix_a::PROJECTION_FILES.len() as i64),
            ),
            ("reservations", n(catalog.reservations.len() as i64)),
            (
                "source_dispositions",
                n(catalog.source_symbol_dispositions.len() as i64),
            ),
            (
                "top_level_candidates",
                n(catalog.top_level_candidates.len() as i64),
            ),
            ("targets", n(catalog.targets.len() as i64)),
            (
                "semantic_bindings",
                n(catalog.semantic_bindings.len() as i64),
            ),
            ("evidence_rows", n(catalog.evidence.len() as i64)),
            (
                "reference_only_symbols",
                n(catalog
                    .source_symbol_dispositions
                    .iter()
                    .filter(|row| row.disposition == "reference-only")
                    .count() as i64),
            ),
            ("violations", n(violations.len() as i64)),
            (
                "outcome",
                s(if structural {
                    "error"
                } else if violations.is_empty() {
                    "pass"
                } else {
                    "fail"
                }),
            ),
        ])
    );
    if structural {
        Err("Appendix A projection load failed; see redacted violation events".to_string())
    } else {
        Ok(violations.len())
    }
}

/// Render Appendix A consumer registries in memory, then byte-verify them.
fn run_appendix_generate(root: &Path) -> Result<usize, String> {
    let catalog = match appendix_a::load_and_verify(root) {
        Ok(catalog) => catalog,
        Err(violations) => {
            return finish_appendix_load_failure("appendix_generation_completed", &violations);
        }
    };
    let violations = appendix_a::appendix_a_catalog_projection_diff(root, &catalog);
    let generated = appendix_a::generated_projections(&catalog);
    for ((file, contents), (registry, _expected_file)) in
        generated.into_iter().zip(appendix_a::PROJECTION_FILES)
    {
        let rows = catalog
            .projection_rows
            .iter()
            .filter(|row| row.projection == registry)
            .count();
        let file_violations = violations
            .iter()
            .filter(|violation| violation.row_id == file)
            .count();
        println!(
            "{}",
            event(&[
                ("event", s("appendix_projection_generated")),
                ("registry", s(registry)),
                ("file", s(&file)),
                ("rows", n(rows as i64)),
                ("byte_count", n(contents.len() as i64)),
                (
                    "sha256",
                    s(registry_check::hash::sha256_hex(contents.as_bytes())),
                ),
                ("violations", n(file_violations as i64)),
                (
                    "outcome",
                    s(if file_violations == 0 { "pass" } else { "fail" }),
                ),
            ])
        );
    }
    emit_appendix_violations(&violations);
    let structural = appendix_has_structural_error(&violations);
    println!(
        "{}",
        event(&[
            ("event", s("appendix_generation_completed")),
            (
                "projection_files",
                n(appendix_a::PROJECTION_FILES.len() as i64),
            ),
            ("violations", n(violations.len() as i64)),
            (
                "outcome",
                s(if structural {
                    "error"
                } else if violations.is_empty() {
                    "pass"
                } else {
                    "fail"
                }),
            ),
        ])
    );
    if structural {
        Err(
            "Appendix A generated projection load failed; see redacted violation events"
                .to_string(),
        )
    } else {
        Ok(violations.len())
    }
}

fn identity_violation_diff(
    violation: &Violation,
    assignment_pins: &[identity::AssignmentPin],
) -> String {
    let pin = assignment_pins
        .iter()
        .find(|pin| pin.registry == violation.registry);
    match (violation.code.as_str(), pin) {
        ("registry_epoch_mismatch", Some(pin)) => format!(
            "expected_epoch={} actual_epoch={}",
            pin.expected_epoch, pin.actual_epoch
        ),
        ("registry_assignment_drift", Some(pin)) => format!(
            "expected_pin={} actual_pin={}",
            pin.expected_pin, pin.actual_pin
        ),
        _ => violation.msg.clone(),
    }
}

/// Validate the identity constitution (the five class registries plus
/// durable_fields.toml); emit complete deterministic row, assignment,
/// construction-DAG, and digest-recipe evidence; return the violation count.
fn run_identity(root: &Path) -> Result<usize, String> {
    let ir = identity::load_identity(&root.join("registries")).map_err(|e| e.to_string())?;
    let violations = identity::validate_identity(&ir);
    let assignment_pins = identity::assignment_pins(&ir);
    let registry_rows: [(&str, i64, i64); 6] = [
        (
            "logical_object_kinds",
            ir.logical.len() as i64,
            ir.logical_epoch,
        ),
        (
            "physical_record_kinds",
            ir.physical.len() as i64,
            ir.physical_epoch,
        ),
        (
            "bootstrap_frames",
            ir.bootstrap.len() as i64,
            ir.bootstrap_epoch,
        ),
        (
            "prebootstrap_artifact_kinds",
            ir.prebootstrap.len() as i64,
            ir.prebootstrap_epoch,
        ),
        ("wire_types", ir.wire.len() as i64, ir.wire_epoch),
        ("durable_fields", ir.fields.len() as i64, ir.fields_epoch),
    ];
    for (name, rows, epoch) in registry_rows {
        let count = violations.iter().filter(|v| v.registry == name).count();
        println!(
            "{}",
            event(&[
                ("event", s("registry_generated")),
                ("registry", s(name)),
                ("rows", n(rows)),
                ("registry_epoch", n(epoch)),
                ("violations", n(count as i64)),
                ("outcome", s(if count == 0 { "pass" } else { "fail" })),
            ])
        );
    }

    for pin in &assignment_pins {
        let ok = pin.actual_epoch == pin.expected_epoch && pin.actual_pin == pin.expected_pin;
        let mut fields = vec![
            ("event", s("assignment_pin_checked")),
            ("registry", s(pin.registry)),
            ("expected_registry_epoch", n(pin.expected_epoch)),
            ("actual_registry_epoch", n(pin.actual_epoch)),
            ("expected_assignment_pin", s(pin.expected_pin)),
            ("actual_assignment_pin", s(&pin.actual_pin)),
        ];
        if !ok {
            fields.push((
                "diff",
                s(format!(
                    "expected_epoch={} actual_epoch={} expected_pin={} actual_pin={}",
                    pin.expected_epoch, pin.actual_epoch, pin.expected_pin, pin.actual_pin
                )),
            ));
        }
        fields.push(("outcome", s(if ok { "pass" } else { "fail" })));
        println!("{}", event(&fields));
    }

    for kind in &ir.logical {
        let row_violations = identity_row_faults(&violations, &kind.name);
        println!(
            "{}",
            event(&[
                ("event", s("row_checked")),
                ("registry", s("logical_object_kinds")),
                ("row_kind", s("logical_object_kind")),
                ("identity_class", s("logical")),
                ("object_kind", s(format!("{:#06x}", kind.object_kind))),
                ("row_id", s(&kind.name)),
                ("status", s(&kind.status)),
                ("construction_order", n(kind.construction_order)),
                ("role_predicate", s(&kind.role_predicate)),
                ("max_size_bytes", n(kind.max_size_bytes)),
                ("golden_corpus", s(&kind.golden_corpus)),
                ("violations", n(row_violations as i64)),
                (
                    "outcome",
                    s(if row_violations == 0 { "pass" } else { "fail" }),
                ),
            ])
        );
    }

    for kind in &ir.physical {
        let row_violations = identity_row_faults(&violations, &kind.name);
        println!(
            "{}",
            event(&[
                ("event", s("row_checked")),
                ("registry", s("physical_record_kinds")),
                ("row_kind", s("physical_record_kind")),
                ("identity_class", s("physical")),
                ("record_kind", s(format!("{:#06x}", kind.record_kind))),
                ("row_id", s(&kind.name)),
                ("identity_law", s(&kind.identity_law)),
                ("status", s(&kind.status)),
                ("transcript", s(&kind.transcript)),
                ("owning_identity", s(&kind.owning_identity)),
                ("max_size_bytes", n(kind.max_size_bytes)),
                ("violations", n(row_violations as i64)),
                (
                    "outcome",
                    s(if row_violations == 0 { "pass" } else { "fail" }),
                ),
            ])
        );
    }

    for frame in &ir.bootstrap {
        let row_violations = identity_row_faults(&violations, &frame.name);
        println!(
            "{}",
            event(&[
                ("event", s("row_checked")),
                ("registry", s("bootstrap_frames")),
                ("row_kind", s("bootstrap_frame")),
                ("identity_class", s("bootstrap")),
                ("frame_kind", s(format!("{:#06x}", frame.frame_kind))),
                ("row_id", s(&frame.name)),
                ("status", s(&frame.status)),
                ("byte_size", n(frame.byte_size)),
                ("location", s(&frame.location)),
                ("update_protocol", s(&frame.update_protocol)),
                ("tear_validation", s(&frame.tear_validation)),
                ("opener_fields", s(&frame.opener_fields)),
                ("compatibility_gate", s(&frame.compatibility_gate)),
                ("recovery_vectors", s(&frame.recovery_vectors)),
                ("violations", n(row_violations as i64)),
                (
                    "outcome",
                    s(if row_violations == 0 { "pass" } else { "fail" }),
                ),
            ])
        );
    }

    for kind in &ir.prebootstrap {
        let row_violations = identity_row_faults(&violations, &kind.name);
        println!(
            "{}",
            event(&[
                ("event", s("row_checked")),
                ("registry", s("prebootstrap_artifact_kinds")),
                ("row_kind", s("prebootstrap_artifact_kind")),
                ("identity_class", s("prebootstrap")),
                ("artifact_kind", s(format!("{:#06x}", kind.artifact_kind)),),
                ("row_id", s(&kind.name)),
                ("status", s(&kind.status)),
                ("target_claim_domain", s(&kind.target_claim_domain)),
                ("allowed_containers", s(&kind.allowed_containers)),
                ("import_target", s(&kind.import_target)),
                ("max_size_bytes", n(kind.max_size_bytes)),
                ("violations", n(row_violations as i64)),
                (
                    "outcome",
                    s(if row_violations == 0 { "pass" } else { "fail" }),
                ),
            ])
        );
    }

    for wire_type in &ir.wire {
        let row_violations = identity_row_faults(&violations, &wire_type.name);
        let mut fields = vec![
            ("event", s("row_checked")),
            ("registry", s("wire_types")),
            ("row_kind", s("wire_type")),
            ("identity_class", s("wire")),
            (
                "wire_type_id",
                s(format!("{:#06x}", wire_type.wire_type_id)),
            ),
            ("row_id", s(&wire_type.name)),
            ("kind", s(&wire_type.kind)),
            ("status", s(&wire_type.status)),
        ];
        if let Some(containing_union) = &wire_type.containing_union {
            fields.push(("containing_union", s(containing_union)));
        }
        if let Some(wire_tag) = wire_type.wire_tag {
            fields.push(("wire_tag", s(format!("{wire_tag:#06x}"))));
        }
        fields.extend([
            ("encoding_context", s(&wire_type.encoding_context)),
            (
                "allowed_containing_schemas",
                arr(wire_type.allowed_containing_schemas.clone()),
            ),
            ("max_size_bytes", n(wire_type.max_size_bytes)),
            ("violations", n(row_violations as i64)),
            (
                "outcome",
                s(if row_violations == 0 { "pass" } else { "fail" }),
            ),
        ]);
        println!("{}", event(&fields));
    }

    for field in &ir.fields {
        let row_id = format!("{}#{}", field.containing_schema, field.stable_name);
        let row_violations = identity_row_faults(&violations, &row_id);
        let mut fields = vec![
            ("event", s("row_checked")),
            ("registry", s("durable_fields")),
            ("row_kind", s("durable_field")),
            ("row_id", s(&row_id)),
            ("containing_schema", s(&field.containing_schema)),
            ("field_tag", n(field.field_tag)),
            ("stable_name", s(&field.stable_name)),
            ("exact_wire_type", s(&field.exact_wire_type)),
            ("cardinality", s(&field.cardinality)),
            ("identity_class", s(&field.identity_class)),
            ("reference_semantics", s(&field.reference_semantics)),
        ];
        if let Some(target_schema_id) = &field.target_schema_id {
            fields.push(("target_schema_id", s(target_schema_id)));
        }
        fields.extend([
            ("construction_order", n(field.construction_order)),
            ("role_predicate", s(&field.role_predicate)),
            ("retention_and_cut_rule", s(&field.retention_and_cut_rule)),
            ("version_status", s(&field.version_status)),
            ("max_size_bytes", n(field.max_size_bytes)),
        ]);
        if let Some(digest_class) = &field.digest_class {
            fields.push(("digest_class", s(digest_class)));
        }
        fields.extend([
            ("violations", n(row_violations as i64)),
            (
                "outcome",
                s(if row_violations == 0 { "pass" } else { "fail" }),
            ),
        ]);
        println!("{}", event(&fields));
    }

    for union in &ir.unions {
        let row_violations = identity_row_faults(&violations, &union.union_name);
        let anchor = ir.fields.iter().find(|field| {
            field.containing_schema == union.containing_schema
                && field.field_tag == union.field_tag
                && field.exact_wire_type == union.union_name
        });
        let anchor_row_id = anchor
            .map(|field| format!("{}#{}", field.containing_schema, field.stable_name))
            .unwrap_or_else(|| {
                format!("{}#field-tag-{}", union.containing_schema, union.field_tag)
            });
        let mut fields = vec![
            ("event", s("row_checked")),
            ("registry", s("durable_fields")),
            ("row_kind", s("reference_union")),
            ("row_id", s(&union.union_name)),
            ("union_name", s(&union.union_name)),
            ("containing_schema", s(&union.containing_schema)),
            ("field_tag", n(union.field_tag)),
            ("role", s(&union.role)),
            ("arm_count", n(union.arms.len() as i64)),
            ("anchor_present", b(anchor.is_some())),
            ("anchor_row_id", s(&anchor_row_id)),
        ];
        if let Some(anchor) = anchor {
            fields.extend([
                ("anchor_exact_wire_type", s(&anchor.exact_wire_type)),
                ("anchor_identity_class", s(&anchor.identity_class)),
                ("anchor_reference_semantics", s(&anchor.reference_semantics)),
                ("anchor_role_predicate", s(&anchor.role_predicate)),
                (
                    "anchor_retention_and_cut_rule",
                    s(&anchor.retention_and_cut_rule),
                ),
                ("anchor_version_status", s(&anchor.version_status)),
            ]);
        }
        fields.extend([
            ("violations", n(row_violations as i64)),
            (
                "outcome",
                s(if row_violations == 0 { "pass" } else { "fail" }),
            ),
        ]);
        println!("{}", event(&fields));

        for arm in &union.arms {
            let arm_row_id = format!("{}#{}", union.union_name, arm.stable_name);
            let arm_violations = identity_row_faults(&violations, &arm_row_id);
            let target = ir
                .logical
                .iter()
                .find(|kind| kind.name == arm.target_schema_id);
            let mut fields = vec![
                ("event", s("row_checked")),
                ("registry", s("durable_fields")),
                ("row_kind", s("reference_union_arm")),
                ("row_id", s(&arm_row_id)),
                ("union_name", s(&arm.union_name)),
                ("containing_schema", s(&arm.containing_schema)),
                ("field_tag", n(arm.field_tag)),
                ("arm_tag", n(arm.arm_tag)),
                ("stable_name", s(&arm.stable_name)),
                ("target_schema_id", s(&arm.target_schema_id)),
                ("role", s(&arm.role)),
                ("identity_class", s(&arm.identity_class)),
                ("reference_semantics", s(&arm.reference_semantics)),
                ("role_predicate", s(&arm.role_predicate)),
                ("retention_and_cut_rule", s(&arm.retention_and_cut_rule)),
                ("version_status", s(&arm.version_status)),
                ("max_size_bytes", n(arm.max_size_bytes)),
                ("anchor_row_id", s(&anchor_row_id)),
                ("target_present", b(target.is_some())),
            ];
            if let Some(target) = target {
                fields.extend([
                    ("target_status", s(&target.status)),
                    ("target_role_predicate", s(&target.role_predicate)),
                    ("target_construction_order", n(target.construction_order)),
                ]);
            }
            fields.extend([
                ("violations", n(arm_violations as i64)),
                (
                    "outcome",
                    s(if arm_violations == 0 { "pass" } else { "fail" }),
                ),
            ]);
            println!("{}", event(&fields));
        }
    }

    let dag_faults = violations
        .iter()
        .filter(|v| v.code.starts_with("dag_"))
        .count();
    println!(
        "{}",
        event(&[
            ("event", s("dag_checked")),
            ("registry", s("durable_fields")),
            (
                "retaining_field_rows",
                n(ir.fields
                    .iter()
                    .filter(|field| matches!(
                        field.reference_semantics.as_str(),
                        "strong" | "conditional"
                    ))
                    .count() as i64),
            ),
            ("reference_unions", n(ir.unions.len() as i64)),
            (
                "reference_union_arms",
                n(ir.unions
                    .iter()
                    .map(|union| union.arms.len())
                    .sum::<usize>() as i64),
            ),
            ("faults", n(dag_faults as i64)),
            ("outcome", s(if dag_faults == 0 { "pass" } else { "fail" })),
        ])
    );
    for field in ir
        .fields
        .iter()
        .filter(|field| field.digest_class.is_some())
    {
        let row_id = format!("{}#{}", field.containing_schema, field.stable_name);
        let row_violations = violations.iter().filter(|v| v.row_id == row_id).count();
        let digest_class = field.digest_class.as_deref().unwrap_or_default();
        let mut fields = vec![
            ("event", s("digest_verified")),
            ("registry", s("durable_fields")),
            ("row_id", s(&row_id)),
            ("recipe_id", s(&row_id)),
            ("digest_class", s(digest_class)),
            (
                "transcript_recipe",
                s(field.transcript_recipe.as_deref().unwrap_or_default()),
            ),
            (
                "recipe_pin",
                s(field.recipe_pin.as_deref().unwrap_or_default()),
            ),
        ];
        if matches!(digest_class, "body") {
            if let Some(domain) = &field.bd_domain_separator {
                fields.push(("bd_domain_separator", s(domain)));
            }
            if let Some(schema_major) = field.bd_schema_major {
                fields.push(("bd_schema_major", n(schema_major)));
            }
            if let Some(included) = &field.bd_included_field_tags {
                fields.push(("bd_included_field_tags", numeric_array(included)));
            }
            if let Some(excluded) = &field.bd_excluded_field_tags {
                fields.push(("bd_excluded_field_tags", numeric_array(excluded)));
            }
            if let (Some(domain), Some(schema_major), Some(included), Some(excluded)) = (
                &field.bd_domain_separator,
                field.bd_schema_major,
                &field.bd_included_field_tags,
                &field.bd_excluded_field_tags,
            ) {
                let transcript = identity::bodydigest_transcript(
                    &field.containing_schema,
                    domain,
                    schema_major,
                    included,
                    excluded,
                );
                fields.extend([
                    ("bodydigest_transcript", s(&transcript)),
                    (
                        "recomputed_recipe_pin",
                        s(identity::bodydigest_pin(&transcript)),
                    ),
                ]);
            }
        }
        fields.extend([
            ("violations", n(row_violations as i64)),
            (
                "outcome",
                s(if row_violations == 0 { "pass" } else { "fail" }),
            ),
        ]);
        println!("{}", event(&fields));
    }
    for v in &violations {
        let diff = identity_violation_diff(v, &assignment_pins);
        println!(
            "{}",
            event(&[
                ("event", s("violation")),
                ("code", s(&v.code)),
                ("registry", s(&v.registry)),
                ("row_id", s(&v.row_id)),
                ("msg", s(&v.msg)),
                ("diff", s(diff)),
            ])
        );
        eprintln!(
            "violation[{}] {}::{}: {}",
            v.code, v.registry, v.row_id, v.msg
        );
    }
    Ok(violations.len())
}

fn load(root: &Path) -> Result<Registries, String> {
    model::load_registries(&root.join("registries")).map_err(|e| e.to_string())
}

/// Emit registry_validated / clause_checked events; return violation count.
fn run_validate(r: &Registries, root: &Path) -> usize {
    let violations = validate::validate_all(r, root);
    let by_registry = |name: &str| -> Vec<&Violation> {
        violations.iter().filter(|v| v.registry == name).collect()
    };
    let row_counts: [(&str, i64); 6] = [
        (
            "constitution",
            (r.constitution.claim_classes.len()
                + r.constitution.constraints.len()
                + r.constitution.bets.len()) as i64,
        ),
        ("invariants", r.invariants.invariants.len() as i64),
        ("evidence", r.evidence.rows.len() as i64),
        ("slo", r.slo.rows.len() as i64),
        ("proof_lanes", r.proof_lanes.len() as i64),
        ("checker_index", r.checker_index.len() as i64),
    ];
    for (name, rows) in row_counts {
        let vs = by_registry(name);
        println!(
            "{}",
            event(&[
                ("event", s("registry_validated")),
                ("registry", s(name)),
                ("rows", n(rows)),
                ("violations", n(vs.len() as i64)),
                ("outcome", s(if vs.is_empty() { "pass" } else { "fail" })),
            ])
        );
    }
    for inv in &r.invariants.invariants {
        for clause in &inv.clauses {
            let clause_violations: Vec<&Violation> = violations
                .iter()
                .filter(|v| v.row_id == clause.key)
                .collect();
            println!(
                "{}",
                event(&[
                    ("event", s("clause_checked")),
                    ("registry", s("invariants")),
                    ("row_id", s(&clause.key)),
                    ("claim_class", s(&clause.claim_class)),
                    ("checker_symbol", s(&clause.checker_entrypoint)),
                    ("negative_test_symbol", s(&clause.negative_test_entrypoint)),
                    (
                        "outcome",
                        s(if clause_violations.is_empty() {
                            "pass"
                        } else {
                            "fail"
                        }),
                    ),
                ])
            );
        }
    }
    for v in &violations {
        println!(
            "{}",
            event(&[
                ("event", s("violation")),
                ("code", s(&v.code)),
                ("registry", s(&v.registry)),
                ("row_id", s(&v.row_id)),
                ("msg", s(&v.msg)),
            ])
        );
        eprintln!(
            "violation[{}] {}::{}: {}",
            v.code, v.registry, v.row_id, v.msg
        );
    }
    violations.len()
}

fn run_hash(r: &Registries) -> usize {
    let actual_ids: Vec<String> = r
        .invariants
        .invariants
        .iter()
        .map(|i| i.id.clone())
        .collect();
    let expected_ids = expected_invariant_ids();
    let recomputed = id_table_hash(&actual_ids);
    let pinned = &r.invariants.twenty_id_hash;
    let ok = recomputed == *pinned && actual_ids == expected_ids;
    let mut fields = vec![
        ("event", s("hash_checked")),
        ("registry", s("invariants")),
        ("pinned", s(pinned)),
        ("recomputed", s(&recomputed)),
        ("outcome", s(if ok { "pass" } else { "fail" })),
    ];
    // On any mismatch, log the exact row-level diff.
    let missing: Vec<String> = expected_ids
        .iter()
        .filter(|id| !actual_ids.contains(id))
        .cloned()
        .collect();
    let extra: Vec<String> = actual_ids
        .iter()
        .filter(|id| !expected_ids.contains(id))
        .cloned()
        .collect();
    if !ok {
        fields.push(("expected_ids", arr(expected_ids.clone())));
        fields.push(("actual_ids", arr(actual_ids.clone())));
        fields.push(("missing", arr(missing)));
        fields.push(("extra", arr(extra)));
    }
    println!("{}", event(&fields));
    usize::from(!ok)
}

fn run_lint(r: &Registries, root: &Path) -> Result<usize, String> {
    let config =
        lint::load_config(&root.join("registries/claims_lint.toml")).map_err(|e| e.to_string())?;
    let registered = lint::registered_markers(r);
    let hits = lint::run(root, &config, &registered).map_err(|e| e.to_string())?;
    for hit in &hits {
        println!(
            "{}",
            event(&[
                ("event", s("lint_hit")),
                ("file", s(&hit.file)),
                ("line", n(hit.line as i64)),
                ("marker", s(&hit.marker)),
                ("text", s(&hit.text)),
            ])
        );
        eprintln!(
            "{}:{}: unregistered claim marker {} in: {}",
            hit.file, hit.line, hit.marker, hit.text
        );
    }
    println!(
        "{}",
        event(&[
            ("event", s("lint_completed")),
            ("files_scanned", n(config.scan.len() as i64)),
            ("hits", n(hits.len() as i64)),
            ("outcome", s(if hits.is_empty() { "pass" } else { "fail" })),
        ])
    );
    Ok(hits.len())
}

fn run_closure(r: &Registries, manifest_path: &Path) -> Result<usize, String> {
    let manifest = model::load_manifest(manifest_path).map_err(|e| e.to_string())?;
    let report = closure::compute(r, &manifest);
    println!(
        "{}",
        event(&[
            ("event", s("closure_computed")),
            ("manifest", s(&report.manifest)),
            ("reachable", n(report.reachable.len() as i64)),
            ("live", n(report.live.len() as i64)),
            ("absent", n(report.absent.len() as i64)),
            ("absent_clauses", arr(report.absent.iter().cloned())),
            ("outcome", s(if report.ok() { "pass" } else { "fail" })),
        ])
    );
    for (capability, clauses) in &report.absent_capabilities {
        println!(
            "{}",
            event(&[
                ("event", s("capability_absent")),
                ("capability", s(capability)),
                ("clauses", arr(clauses.iter().cloned())),
                (
                    "reason",
                    s("reachable clause is not live; the capability is absent")
                ),
            ])
        );
        eprintln!("capability {capability:?} absent: non-live reachable clauses {clauses:?}");
    }
    Ok(report.absent.len())
}

fn run() -> Result<usize, String> {
    let args = parse_args()?;
    match args.command.as_str() {
        "appendix" => return run_appendix(&args.root),
        "appendix-generate" => return run_appendix_generate(&args.root),
        _ => {}
    }
    let r = load(&args.root)?;
    match args.command.as_str() {
        "validate" => Ok(run_validate(&r, &args.root)),
        "hash" => Ok(run_hash(&r)),
        "identity" => run_identity(&args.root),
        "lint" => run_lint(&r, &args.root),
        "closure" => {
            let manifest = args.manifest.ok_or("closure requires --manifest <path>")?;
            run_closure(&r, &manifest)
        }
        "all" => {
            let mut failures = run_validate(&r, &args.root);
            failures += run_hash(&r);
            failures += run_identity(&args.root)?;
            failures += run_appendix(&args.root)?;
            failures += run_lint(&r, &args.root)?;
            let manifest = args
                .manifest
                .unwrap_or_else(|| args.root.join("registries/sample_capability_manifest.toml"));
            failures += run_closure(&r, &manifest)?;
            println!(
                "{}",
                event(&[
                    ("event", s("run_completed")),
                    ("failures", n(failures as i64)),
                    ("outcome", s(if failures == 0 { "pass" } else { "fail" })),
                ])
            );
            Ok(failures)
        }
        other => Err(format!("unknown command {other:?}\n{}", usage())),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(0) => ExitCode::SUCCESS,
        Ok(_) => ExitCode::from(1),
        Err(msg) => {
            eprintln!("registry-check: {msg}");
            println!(
                "{}",
                event(&[
                    ("event", s("run_error")),
                    ("msg", s(&msg)),
                    ("outcome", s("error")),
                ])
            );
            ExitCode::from(2)
        }
    }
}

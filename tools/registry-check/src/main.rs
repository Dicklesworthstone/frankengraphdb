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
//!   registry-check appendix-regenerate --root <repo-root>
//!   registry-check all      --root <repo-root> [--manifest <path>]

use registry_check::appendix_a;
use registry_check::closure;
use registry_check::hash::id_table_hash;
use registry_check::identity;
use registry_check::jsonl::{JsonValue, arr, b, event, n, s};
use registry_check::lint;
use registry_check::model::{self, Registries};
use registry_check::validate::{self, Violation, expected_invariant_ids};
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
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
        "<validate|lint|closure|hash|identity|appendix|appendix-generate|appendix-regenerate|all> ",
        "--root <repo-root> [--manifest <path>]\n",
        "  appendix           verify the Appendix A catalog, source, and checked-in projections\n",
        "  appendix-generate  render in memory and byte-verify the six Appendix A projections\n",
        "  appendix-regenerate  verify Appendix A inputs, then write only its six projections"
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
            | "target_manifest"
            | "repository_bindings"
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
    let outcome = if structural { "error" } else { "fail" };
    match completion_event {
        "appendix_generation_completed" => println!(
            "{}",
            event(&[
                ("event", s(completion_event)),
                ("projection_files", n(0)),
                ("violations", n(violations.len() as i64)),
                ("outcome", s(outcome)),
            ])
        ),
        "appendix_regeneration_completed" => println!(
            "{}",
            event(&[
                ("event", s(completion_event)),
                ("projection_files", n(0)),
                ("changed_files", n(0)),
                ("unchanged_files", n(0)),
                ("published_files", n(0)),
                ("violations", n(violations.len() as i64)),
                ("outcome", s(outcome)),
            ])
        ),
        _ => println!(
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
                ("reference_only_symbols", n(0)),
                ("violations", n(violations.len() as i64)),
                ("outcome", s(outcome)),
            ])
        ),
    }
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

    let targets = &catalog.target_manifest;
    println!(
        "{}",
        event(&[
            ("event", s("appendix_target_manifest")),
            ("target_count", n(targets.target_count)),
            (
                "projection_fallback_count",
                n(targets.projection_fallback_count),
            ),
            (
                "target_source_assignment_sha256",
                s(&targets.target_source_assignment_sha256),
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
                ("ambiguity_ids_sha256", s(&slice.ambiguity_ids_sha256),),
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

#[cfg(unix)]
fn metadata_has_one_link(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    metadata.nlink() == 1
}

#[cfg(windows)]
fn metadata_has_one_link(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    metadata.number_of_links() == Some(1)
}

#[cfg(not(any(unix, windows)))]
fn metadata_has_one_link(_metadata: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn metadata_identifies_same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(windows)]
fn metadata_identifies_same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    matches!(
        (
            left.volume_serial_number(),
            left.file_index(),
            right.volume_serial_number(),
            right.file_index(),
        ),
        (Some(left_volume), Some(left_index), Some(right_volume), Some(right_index))
            if left_volume == right_volume && left_index == right_index
    )
}

#[cfg(not(any(unix, windows)))]
fn metadata_identifies_same_file(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    false
}

fn canonical_appendix_registries(canonical_root: &Path) -> Result<PathBuf, String> {
    let registries = canonical_root.join("registries");
    let metadata = fs::symlink_metadata(&registries)
        .map_err(|_| "cannot inspect the Appendix A registry directory".to_string())?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err("the Appendix A registry directory is not a real directory".to_string());
    }
    let canonical_registries = fs::canonicalize(&registries)
        .map_err(|_| "cannot canonicalize the Appendix A registry directory".to_string())?;
    if canonical_registries != registries
        || canonical_registries.parent() != Some(canonical_root)
        || canonical_registries
            .file_name()
            .and_then(|name| name.to_str())
            != Some("registries")
    {
        return Err(
            "the Appendix A registry directory is not a direct canonical child of the repository root"
                .to_string(),
        );
    }
    Ok(canonical_registries)
}

fn lock_appendix_catalog(root: &Path) -> Result<(PathBuf, PathBuf, File), String> {
    let canonical_root = fs::canonicalize(root)
        .map_err(|_| "cannot canonicalize the Appendix A repository root".to_string())?;
    if !canonical_root.is_dir() {
        return Err("the Appendix A repository root is not a directory".to_string());
    }
    let canonical_registries = canonical_appendix_registries(&canonical_root)?;
    let catalog_path = canonical_root.join(appendix_a::CATALOG_PATH);
    if catalog_path.parent() != Some(canonical_registries.as_path()) {
        return Err("the canonical Appendix A catalog path is outside registries".to_string());
    }
    let catalog_metadata = fs::symlink_metadata(&catalog_path)
        .map_err(|_| "cannot inspect the canonical Appendix A catalog".to_string())?;
    if !catalog_metadata.file_type().is_file() || catalog_metadata.file_type().is_symlink() {
        return Err("the canonical Appendix A catalog is not a regular file".to_string());
    }
    let canonical_catalog = fs::canonicalize(&catalog_path)
        .map_err(|_| "cannot canonicalize the Appendix A catalog".to_string())?;
    if canonical_catalog != catalog_path {
        return Err("the canonical Appendix A catalog escapes its fixed path".to_string());
    }
    let catalog_lock = OpenOptions::new()
        .read(true)
        .open(&canonical_catalog)
        .map_err(|_| "cannot open the canonical Appendix A catalog lock".to_string())?;
    catalog_lock
        .lock()
        .map_err(|_| "cannot lock the canonical Appendix A catalog".to_string())?;
    Ok((canonical_root, canonical_registries, catalog_lock))
}

fn validate_projection_destination(
    canonical_registries: &Path,
    destination: &Path,
) -> Result<(), String> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(
                    "an Appendix A projection destination is not a regular file".to_string()
                );
            }
            if !metadata_has_one_link(&metadata) {
                return Err("an Appendix A projection destination is not singly linked".to_string());
            }
            let canonical_destination = fs::canonicalize(destination).map_err(|_| {
                "cannot canonicalize an Appendix A projection destination".to_string()
            })?;
            if canonical_destination.parent() != Some(canonical_registries)
                || canonical_destination.file_name() != destination.file_name()
            {
                return Err(
                    "an Appendix A projection destination escapes the registry directory"
                        .to_string(),
                );
            }
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(_) => {
            return Err("cannot inspect an Appendix A projection destination".to_string());
        }
    }
    Ok(())
}

fn appendix_projection_destinations(canonical_registries: &Path) -> Result<Vec<PathBuf>, String> {
    let mut destinations = Vec::with_capacity(appendix_a::PROJECTION_FILES.len());
    for (_, file) in appendix_a::PROJECTION_FILES {
        let mut components = Path::new(file).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err("an Appendix A projection destination is not a safe file name".to_string());
        }
        let destination = canonical_registries.join(file);
        validate_projection_destination(canonical_registries, &destination)?;
        destinations.push(destination);
    }
    Ok(destinations)
}

struct PreparedAppendixProjection {
    path: PathBuf,
    file: File,
}

struct AppendixProjectionWrite {
    registry: &'static str,
    file_name: String,
    contents: String,
    destination: PathBuf,
    rows: usize,
    changed: bool,
    prepared: Option<PreparedAppendixProjection>,
}

fn prepare_appendix_projection(
    canonical_registries: &Path,
    file_name: &str,
    contents: &[u8],
) -> Result<PreparedAppendixProjection, String> {
    let digest = registry_check::hash::sha256_hex(contents);
    for attempt in 0..1_024_u16 {
        let prepared_name =
            format!(".appendix-regenerate-{file_name}-{digest}-{attempt:04}.prepared");
        let prepared_path = canonical_registries.join(prepared_name);
        let mut prepared_file = match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&prepared_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(_) => return Err("cannot create an Appendix A prepared projection".to_string()),
        };
        prepared_file
            .write_all(contents)
            .map_err(|_| "cannot write an Appendix A prepared projection".to_string())?;
        prepared_file
            .sync_all()
            .map_err(|_| "cannot sync an Appendix A prepared projection".to_string())?;
        return Ok(PreparedAppendixProjection {
            path: prepared_path,
            file: prepared_file,
        });
    }
    Err("cannot allocate a collision-free Appendix A prepared projection".to_string())
}

fn revalidate_prepared_projection(
    canonical_registries: &Path,
    prepared: &mut PreparedAppendixProjection,
    expected: &[u8],
) -> Result<(), String> {
    let path_metadata = fs::symlink_metadata(&prepared.path)
        .map_err(|_| "cannot inspect an Appendix A prepared projection".to_string())?;
    let file_metadata = prepared
        .file
        .metadata()
        .map_err(|_| "cannot inspect an open Appendix A prepared projection".to_string())?;
    if !path_metadata.file_type().is_file()
        || path_metadata.file_type().is_symlink()
        || !file_metadata.file_type().is_file()
        || !metadata_has_one_link(&path_metadata)
        || !metadata_has_one_link(&file_metadata)
        || !metadata_identifies_same_file(&path_metadata, &file_metadata)
    {
        return Err("an Appendix A prepared projection changed identity".to_string());
    }
    let canonical_prepared = fs::canonicalize(&prepared.path)
        .map_err(|_| "cannot canonicalize an Appendix A prepared projection".to_string())?;
    if canonical_prepared.parent() != Some(canonical_registries)
        || canonical_prepared.file_name() != prepared.path.file_name()
    {
        return Err("an Appendix A prepared projection escapes registries".to_string());
    }
    prepared
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|_| "cannot seek an Appendix A prepared projection".to_string())?;
    let mut persisted = Vec::new();
    prepared
        .file
        .read_to_end(&mut persisted)
        .map_err(|_| "cannot read an Appendix A prepared projection".to_string())?;
    if persisted != expected {
        return Err("an Appendix A prepared projection failed byte verification".to_string());
    }
    Ok(())
}

#[derive(Default)]
struct AppendixRegenerationProgress {
    projection_files: usize,
    changed_files: usize,
    unchanged_files: usize,
    published_files: usize,
    violations: usize,
}

fn emit_appendix_regeneration_completed(progress: &AppendixRegenerationProgress, outcome: &str) {
    println!(
        "{}",
        event(&[
            ("event", s("appendix_regeneration_completed")),
            ("projection_files", n(progress.projection_files as i64)),
            ("changed_files", n(progress.changed_files as i64)),
            ("unchanged_files", n(progress.unchanged_files as i64)),
            ("published_files", n(progress.published_files as i64)),
            ("violations", n(progress.violations as i64)),
            ("outcome", s(outcome)),
        ])
    );
}

/// Regenerate the six Appendix A consumer registries after all canonical
/// catalog, source, and repository bindings have passed. This is intentionally
/// distinct from `appendix-generate`, whose read-only contract is permanent.
fn run_appendix_regenerate(root: &Path) -> Result<usize, String> {
    let mut progress = AppendixRegenerationProgress::default();
    let result = run_appendix_regenerate_inner(root, &mut progress);
    let outcome = match &result {
        Ok(0) => "pass",
        Ok(_) => "fail",
        Err(_) => "error",
    };
    emit_appendix_regeneration_completed(&progress, outcome);
    result
}

fn run_appendix_regenerate_inner(
    root: &Path,
    progress: &mut AppendixRegenerationProgress,
) -> Result<usize, String> {
    let (canonical_root, canonical_registries, _catalog_lock) = lock_appendix_catalog(root)?;
    let catalog = match appendix_a::load_and_verify(&canonical_root) {
        Ok(catalog) => catalog,
        Err(violations) => {
            emit_appendix_violations(&violations);
            progress.violations = violations.len();
            if appendix_has_structural_error(&violations) {
                return Err(
                    "Appendix A structural load failed; see redacted violation events".to_string(),
                );
            }
            return Ok(violations.len());
        }
    };
    let generated = appendix_a::generated_projections(&catalog);
    progress.projection_files = generated.len();
    if generated.len() != appendix_a::PROJECTION_FILES.len() {
        return Err("Appendix A did not render exactly six projections".to_string());
    }
    let destinations = appendix_projection_destinations(&canonical_registries)?;

    let mut writes = Vec::with_capacity(appendix_a::PROJECTION_FILES.len());
    for (((generated_file, contents), (registry, expected_file)), destination) in generated
        .into_iter()
        .zip(appendix_a::PROJECTION_FILES)
        .zip(destinations)
    {
        if generated_file != expected_file {
            return Err("Appendix A rendered an unexpected projection destination".to_string());
        }
        let existing = match fs::read(&destination) {
            Ok(existing) => Some(existing),
            Err(error) if error.kind() == ErrorKind::NotFound => None,
            Err(_) => return Err("cannot read an Appendix A projection destination".to_string()),
        };
        let changed = existing
            .as_deref()
            .is_none_or(|bytes| bytes != contents.as_bytes());
        let rows = catalog
            .projection_rows
            .iter()
            .filter(|row| row.projection == registry)
            .count();
        writes.push(AppendixProjectionWrite {
            registry,
            file_name: generated_file,
            contents,
            destination,
            rows,
            changed,
            prepared: None,
        });
    }

    let changed_files = writes.iter().filter(|write| write.changed).count();
    progress.changed_files = changed_files;
    progress.unchanged_files = writes.len() - changed_files;
    for write in &mut writes {
        if write.changed {
            write.prepared = Some(prepare_appendix_projection(
                &canonical_registries,
                &write.file_name,
                write.contents.as_bytes(),
            )?);
        }
    }

    let revalidated_registries = canonical_appendix_registries(&canonical_root)?;
    if revalidated_registries != canonical_registries {
        return Err("the Appendix A registry directory changed identity".to_string());
    }
    for write in &mut writes {
        validate_projection_destination(&canonical_registries, &write.destination)?;
        if let Some(prepared) = &mut write.prepared {
            revalidate_prepared_projection(
                &canonical_registries,
                prepared,
                write.contents.as_bytes(),
            )?;
        }
    }

    for write in &mut writes {
        let Some(prepared) = &mut write.prepared else {
            continue;
        };
        let revalidated_registries = canonical_appendix_registries(&canonical_root)?;
        if revalidated_registries != canonical_registries {
            return Err("the Appendix A registry directory changed identity".to_string());
        }
        validate_projection_destination(&canonical_registries, &write.destination)?;
        revalidate_prepared_projection(&canonical_registries, prepared, write.contents.as_bytes())?;
        fs::rename(&prepared.path, &write.destination)
            .map_err(|_| "cannot publish an Appendix A prepared projection".to_string())?;
        progress.published_files += 1;
        let persisted = fs::read(&write.destination)
            .map_err(|_| "cannot verify a regenerated Appendix A projection".to_string())?;
        if persisted != write.contents.as_bytes() {
            return Err("a regenerated Appendix A projection failed byte verification".to_string());
        }
    }
    File::open(&canonical_registries)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| "cannot sync the Appendix A registry directory".to_string())?;

    let violations = appendix_a::appendix_a_catalog_projection_diff(&canonical_root, &catalog);
    progress.violations = violations.len();
    if !violations.is_empty() {
        emit_appendix_violations(&violations);
        return Ok(violations.len());
    }

    for write in &writes {
        println!(
            "{}",
            event(&[
                ("event", s("appendix_projection_regenerated")),
                ("registry", s(write.registry)),
                ("file", s(&write.file_name)),
                ("rows", n(write.rows as i64)),
                ("byte_count", n(write.contents.len() as i64)),
                (
                    "sha256",
                    s(registry_check::hash::sha256_hex(write.contents.as_bytes())),
                ),
                ("changed", b(write.changed)),
                ("outcome", s("pass")),
            ])
        );
    }
    Ok(0)
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
    let durable_rows = ir.fields.len()
        + ir.ordinary_unions.len()
        + ir.ordinary_unions
            .iter()
            .map(|union| union.arms.len())
            .sum::<usize>()
        + ir.unions.len()
        + ir.unions
            .iter()
            .map(|union| union.arms.len())
            .sum::<usize>();
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
        ("durable_fields", durable_rows as i64, ir.fields_epoch),
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
        "appendix-regenerate" => return run_appendix_regenerate(&args.root),
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

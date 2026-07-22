//! Identity-constitution validation (bead fgdb-g0-identity-registries-hrx).
//!
//! Loads and validates the five disjoint identity-class registries plus the
//! `durable_fields.toml` cross-index (plan §5.1):
//!
//!   logical_object_kinds.toml        keyed-ObjectId logical schemas
//!   physical_record_kinds.toml       non-ObjectId identity laws
//!   bootstrap_frames.toml            fixed-location mutable frames
//!   prebootstrap_artifact_kinds.toml restore artifacts predating K_oid
//!   wire_types.toml                  embedded canonical types / closed tags
//!   durable_fields.toml              the sole per-field cross-index +
//!                                    generated reference unions
//!
//! Violation codes (stable, asserted by negative fixtures):
//!   code_invalid            code is 0x0000/0xffff or outside u16
//!   code_duplicate          code/tag reuse (retired codes are never reassigned)
//!   experimental_in_production  0xc000..=0xfffe row in a shipped registry
//!   range_status_mismatch   status/code-range coherence violation
//!   disjointness_dual_class one schema name in two identity classes
//!   field_unresolved_schema containing_schema resolves nowhere
//!   field_unresolved_wire_type  exact_wire_type resolves nowhere
//!   bare_strong_ref         polymorphic strong ref without a generated union
//!   ref_target_not_logical  strong/conditional target outside class 1
//!   ref_target_unresolved   named target resolves nowhere
//!   frame_strong_ref        bootstrap frame with a retaining reference
//!   union_field_mismatch    union not anchored to its declaring field row
//!   union_arm_duplicate_tag duplicate arm tag in one union
//!   union_arm_unresolved    arm target is not a live logical row
//!   dag_self_edge / dag_cycle / dag_future_result   construction-DAG faults
//!   digest_missing_class    digest-typed field without a declared class
//!   digest_missing_recipe   transcript digest without a recipe
//!   bodydigest_two_fields   two BodyDigest rows in one schema
//!   bodydigest_unknown_exclusion  include/exclude names an unregistered tag
//!   bodydigest_self_included      the digest's own tag is not excluded
//!   bodydigest_pin_mismatch       recipe drift against the FNV pin
//!   unregistered_field      encodability check: field not in the table
//!   bad_field               enum/shape violation

use crate::hash::fnv1a64;
use crate::model::LoadError;
use crate::toml::{
    self, ReadError, Table, get_int, get_opt_str, get_str, get_str_array, get_table,
    get_table_array,
};
use crate::validate::Violation;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// Builtin scalar wire types (documented in durable_fields.toml).
/// `digest256` REQUIRES a declared digest_class; `id256`/`oid256` are raw
/// 256-bit identities, not digests-of-something.
pub const BUILTIN_WIRE_TYPES: [&str; 10] = [
    "u16",
    "u32",
    "u64",
    "i64",
    "bool",
    "bytes",
    "string",
    "id256",
    "digest256",
    "oid256",
];

#[derive(Debug, Clone, PartialEq)]
pub struct LogicalKind {
    pub object_kind: i64,
    pub name: String,
    pub status: String,
    pub construction_order: i64,
    pub role_predicate: String,
    pub max_size_bytes: i64,
    pub golden_corpus: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PhysicalKind {
    pub record_kind: i64,
    pub name: String,
    pub identity_law: String,
    pub status: String,
    pub transcript: String,
    pub owning_identity: String,
    pub max_size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BootstrapFrame {
    pub frame_kind: i64,
    pub name: String,
    pub status: String,
    pub byte_size: i64,
    pub location: String,
    pub update_protocol: String,
    pub tear_validation: String,
    pub opener_fields: String,
    pub compatibility_gate: String,
    pub recovery_vectors: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrebootstrapKind {
    pub artifact_kind: i64,
    pub name: String,
    pub status: String,
    pub target_claim_domain: String,
    pub allowed_containers: String,
    pub import_target: String,
    pub max_size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WireType {
    pub wire_type_id: i64,
    pub name: String,
    pub kind: String,
    pub status: String,
    pub containing_union: Option<String>,
    pub wire_tag: Option<i64>,
    pub encoding_context: String,
    pub allowed_containing_schemas: Vec<String>,
    pub max_size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldRow {
    pub containing_schema: String,
    pub field_tag: i64,
    pub stable_name: String,
    pub exact_wire_type: String,
    pub cardinality: String,
    pub identity_class: String,
    pub reference_semantics: String,
    pub target_schema_id: Option<String>,
    pub construction_order: i64,
    pub role_predicate: String,
    pub retention_and_cut_rule: String,
    pub version_status: String,
    pub max_size_bytes: i64,
    pub digest_class: Option<String>,
    pub transcript_recipe: Option<String>,
    pub bd_domain_separator: Option<String>,
    pub bd_schema_major: Option<i64>,
    pub bd_included_field_tags: Option<Vec<i64>>,
    pub bd_excluded_field_tags: Option<Vec<i64>>,
    pub recipe_pin: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceUnion {
    pub union_name: String,
    pub containing_schema: String,
    pub field_tag: i64,
    pub role: String,
    /// (arm_tag, target_schema)
    pub arms: Vec<(i64, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IdentityRegistries {
    pub logical: Vec<LogicalKind>,
    pub logical_epoch: i64,
    pub physical: Vec<PhysicalKind>,
    pub physical_epoch: i64,
    pub bootstrap: Vec<BootstrapFrame>,
    pub bootstrap_epoch: i64,
    pub prebootstrap: Vec<PrebootstrapKind>,
    pub prebootstrap_epoch: i64,
    pub wire: Vec<WireType>,
    pub wire_epoch: i64,
    pub fields: Vec<FieldRow>,
    pub fields_epoch: i64,
    pub unions: Vec<ReferenceUnion>,
}

fn get_int_array(table: &Table, key: &str, ctx: &str) -> Result<Option<Vec<i64>>, ReadError> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Array(items)) => {
            let mut out = Vec::new();
            for (i, item) in items.iter().enumerate() {
                match item {
                    toml::Value::Int(v) => out.push(*v),
                    _ => {
                        return Err(ReadError {
                            path: format!("{ctx}.{key}[{i}]"),
                            msg: "expected integer".into(),
                        });
                    }
                }
            }
            Ok(Some(out))
        }
        Some(_) => Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "expected array of integers".into(),
        }),
    }
}

fn get_opt_int(table: &Table, key: &str, ctx: &str) -> Result<Option<i64>, ReadError> {
    match table.get(key) {
        None => Ok(None),
        Some(toml::Value::Int(v)) => Ok(Some(*v)),
        Some(_) => Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "expected integer".into(),
        }),
    }
}

fn registry_header(root: &Table, expected: &str, file: &str) -> Result<i64, ReadError> {
    let registry = get_table(root, "registry", file)?;
    let name = get_str(registry, "name", &format!("{file}.registry"))?;
    if name != expected {
        return Err(ReadError {
            path: format!("{file}.registry.name"),
            msg: format!("expected {expected:?}, found {name:?}"),
        });
    }
    get_int(registry, "registry_epoch", &format!("{file}.registry"))
}

fn load_table(dir: &Path, file: &str) -> Result<Table, LoadError> {
    let path = dir.join(file);
    let text = std::fs::read_to_string(&path).map_err(|e| LoadError {
        file: path.display().to_string(),
        msg: format!("cannot read: {e}"),
    })?;
    toml::parse(&text).map_err(|e| LoadError {
        file: path.display().to_string(),
        msg: e.to_string(),
    })
}

fn wrap(dir: &Path, file: &str, e: ReadError) -> LoadError {
    LoadError {
        file: dir.join(file).display().to_string(),
        msg: e.to_string(),
    }
}

pub fn logical_from(root: &Table) -> Result<(i64, Vec<LogicalKind>), ReadError> {
    let epoch = registry_header(root, "logical_object_kinds", "logical_object_kinds.toml")?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "kind", "logical_object_kinds.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("logical_object_kinds.toml.kind[{i}]");
        rows.push(LogicalKind {
            object_kind: get_int(t, "object_kind", &ctx)?,
            name: get_str(t, "name", &ctx)?,
            status: get_str(t, "status", &ctx)?,
            construction_order: get_int(t, "construction_order", &ctx)?,
            role_predicate: get_str(t, "role_predicate", &ctx)?,
            max_size_bytes: get_int(t, "max_size_bytes", &ctx)?,
            golden_corpus: get_str(t, "golden_corpus", &ctx)?,
        });
    }
    Ok((epoch, rows))
}

pub fn physical_from(root: &Table) -> Result<(i64, Vec<PhysicalKind>), ReadError> {
    let epoch = registry_header(root, "physical_record_kinds", "physical_record_kinds.toml")?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "kind", "physical_record_kinds.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("physical_record_kinds.toml.kind[{i}]");
        rows.push(PhysicalKind {
            record_kind: get_int(t, "record_kind", &ctx)?,
            name: get_str(t, "name", &ctx)?,
            identity_law: get_str(t, "identity_law", &ctx)?,
            status: get_str(t, "status", &ctx)?,
            transcript: get_str(t, "transcript", &ctx)?,
            owning_identity: get_str(t, "owning_identity", &ctx)?,
            max_size_bytes: get_int(t, "max_size_bytes", &ctx)?,
        });
    }
    Ok((epoch, rows))
}

pub fn bootstrap_from(root: &Table) -> Result<(i64, Vec<BootstrapFrame>), ReadError> {
    let epoch = registry_header(root, "bootstrap_frames", "bootstrap_frames.toml")?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "frame", "bootstrap_frames.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("bootstrap_frames.toml.frame[{i}]");
        rows.push(BootstrapFrame {
            frame_kind: get_int(t, "frame_kind", &ctx)?,
            name: get_str(t, "name", &ctx)?,
            status: get_str(t, "status", &ctx)?,
            byte_size: get_int(t, "byte_size", &ctx)?,
            location: get_str(t, "location", &ctx)?,
            update_protocol: get_str(t, "update_protocol", &ctx)?,
            tear_validation: get_str(t, "tear_validation", &ctx)?,
            opener_fields: get_str(t, "opener_fields", &ctx)?,
            compatibility_gate: get_str(t, "compatibility_gate", &ctx)?,
            recovery_vectors: get_str(t, "recovery_vectors", &ctx)?,
        });
    }
    Ok((epoch, rows))
}

pub fn prebootstrap_from(root: &Table) -> Result<(i64, Vec<PrebootstrapKind>), ReadError> {
    let epoch = registry_header(
        root,
        "prebootstrap_artifact_kinds",
        "prebootstrap_artifact_kinds.toml",
    )?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "kind", "prebootstrap_artifact_kinds.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("prebootstrap_artifact_kinds.toml.kind[{i}]");
        rows.push(PrebootstrapKind {
            artifact_kind: get_int(t, "artifact_kind", &ctx)?,
            name: get_str(t, "name", &ctx)?,
            status: get_str(t, "status", &ctx)?,
            target_claim_domain: get_str(t, "target_claim_domain", &ctx)?,
            allowed_containers: get_str(t, "allowed_containers", &ctx)?,
            import_target: get_str(t, "import_target", &ctx)?,
            max_size_bytes: get_int(t, "max_size_bytes", &ctx)?,
        });
    }
    Ok((epoch, rows))
}

pub fn wire_from(root: &Table) -> Result<(i64, Vec<WireType>), ReadError> {
    let epoch = registry_header(root, "wire_types", "wire_types.toml")?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "type", "wire_types.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("wire_types.toml.type[{i}]");
        rows.push(WireType {
            wire_type_id: get_int(t, "wire_type_id", &ctx)?,
            name: get_str(t, "name", &ctx)?,
            kind: get_str(t, "kind", &ctx)?,
            status: get_str(t, "status", &ctx)?,
            containing_union: get_opt_str(t, "containing_union", &ctx)?,
            wire_tag: get_opt_int(t, "wire_tag", &ctx)?,
            encoding_context: get_str(t, "encoding_context", &ctx)?,
            allowed_containing_schemas: get_str_array(t, "allowed_containing_schemas", &ctx)?,
            max_size_bytes: get_int(t, "max_size_bytes", &ctx)?,
        });
    }
    Ok((epoch, rows))
}

pub fn fields_from(root: &Table) -> Result<(i64, Vec<FieldRow>, Vec<ReferenceUnion>), ReadError> {
    let epoch = registry_header(root, "durable_fields", "durable_fields.toml")?;
    let mut fields = Vec::new();
    for (i, t) in get_table_array(root, "field", "durable_fields.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("durable_fields.toml.field[{i}]");
        fields.push(FieldRow {
            containing_schema: get_str(t, "containing_schema", &ctx)?,
            field_tag: get_int(t, "field_tag", &ctx)?,
            stable_name: get_str(t, "stable_name", &ctx)?,
            exact_wire_type: get_str(t, "exact_wire_type", &ctx)?,
            cardinality: get_str(t, "cardinality", &ctx)?,
            identity_class: get_str(t, "identity_class", &ctx)?,
            reference_semantics: get_str(t, "reference_semantics", &ctx)?,
            target_schema_id: get_opt_str(t, "target_schema_id", &ctx)?,
            construction_order: get_int(t, "construction_order", &ctx)?,
            role_predicate: get_str(t, "role_predicate", &ctx)?,
            retention_and_cut_rule: get_str(t, "retention_and_cut_rule", &ctx)?,
            version_status: get_str(t, "version_status", &ctx)?,
            max_size_bytes: get_int(t, "max_size_bytes", &ctx)?,
            digest_class: get_opt_str(t, "digest_class", &ctx)?,
            transcript_recipe: get_opt_str(t, "transcript_recipe", &ctx)?,
            bd_domain_separator: get_opt_str(t, "bd_domain_separator", &ctx)?,
            bd_schema_major: get_opt_int(t, "bd_schema_major", &ctx)?,
            bd_included_field_tags: get_int_array(t, "bd_included_field_tags", &ctx)?,
            bd_excluded_field_tags: get_int_array(t, "bd_excluded_field_tags", &ctx)?,
            recipe_pin: get_opt_str(t, "recipe_pin", &ctx)?,
        });
    }
    let mut unions = Vec::new();
    for (i, t) in get_table_array(root, "reference_union", "durable_fields.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("durable_fields.toml.reference_union[{i}]");
        let mut arms = Vec::new();
        for (j, arm) in get_str_array(t, "arms", &ctx)?.iter().enumerate() {
            let (tag, target) = arm.split_once(':').ok_or_else(|| ReadError {
                path: format!("{ctx}.arms[{j}]"),
                msg: format!("expected \"tag:Target\", found {arm:?}"),
            })?;
            let tag: i64 = tag.parse().map_err(|_| ReadError {
                path: format!("{ctx}.arms[{j}]"),
                msg: format!("invalid arm tag in {arm:?}"),
            })?;
            arms.push((tag, target.to_string()));
        }
        unions.push(ReferenceUnion {
            union_name: get_str(t, "union_name", &ctx)?,
            containing_schema: get_str(t, "containing_schema", &ctx)?,
            field_tag: get_int(t, "field_tag", &ctx)?,
            role: get_str(t, "role", &ctx)?,
            arms,
        });
    }
    Ok((epoch, fields, unions))
}

/// Load all six identity artifacts from a `registries/` directory.
pub fn load_identity(dir: &Path) -> Result<IdentityRegistries, LoadError> {
    let (logical_epoch, logical) = logical_from(&load_table(dir, "logical_object_kinds.toml")?)
        .map_err(|e| wrap(dir, "logical_object_kinds.toml", e))?;
    let (physical_epoch, physical) = physical_from(&load_table(dir, "physical_record_kinds.toml")?)
        .map_err(|e| wrap(dir, "physical_record_kinds.toml", e))?;
    let (bootstrap_epoch, bootstrap) = bootstrap_from(&load_table(dir, "bootstrap_frames.toml")?)
        .map_err(|e| wrap(dir, "bootstrap_frames.toml", e))?;
    let (prebootstrap_epoch, prebootstrap) =
        prebootstrap_from(&load_table(dir, "prebootstrap_artifact_kinds.toml")?)
            .map_err(|e| wrap(dir, "prebootstrap_artifact_kinds.toml", e))?;
    let (wire_epoch, wire) = wire_from(&load_table(dir, "wire_types.toml")?)
        .map_err(|e| wrap(dir, "wire_types.toml", e))?;
    let (fields_epoch, fields, unions) = fields_from(&load_table(dir, "durable_fields.toml")?)
        .map_err(|e| wrap(dir, "durable_fields.toml", e))?;
    Ok(IdentityRegistries {
        logical,
        logical_epoch,
        physical,
        physical_epoch,
        bootstrap,
        bootstrap_epoch,
        prebootstrap,
        prebootstrap_epoch,
        wire,
        wire_epoch,
        fields,
        fields_epoch,
        unions,
    })
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn v(code: &str, registry: &str, row_id: &str, msg: impl Into<String>) -> Violation {
    Violation {
        code: code.into(),
        registry: registry.into(),
        row_id: row_id.into(),
        msg: msg.into(),
    }
}

/// The shared code-space law for every class registry.
fn check_code_space(
    registry: &str,
    rows: &[(i64, String, String)], // (code, name, status)
    out: &mut Vec<Violation>,
) {
    let mut seen_codes: BTreeMap<i64, &str> = BTreeMap::new();
    let mut seen_names: BTreeSet<&str> = BTreeSet::new();
    for (code, name, status) in rows {
        if *code <= 0 || *code >= 0xffff {
            out.push(v(
                "code_invalid",
                registry,
                name,
                format!(
                    "code {code:#06x} outside the valid space (0x0000/0xffff permanently invalid)"
                ),
            ));
        }
        if let Some(prior) = seen_codes.insert(*code, name) {
            out.push(v(
                "code_duplicate",
                registry,
                name,
                format!(
                    "code {code:#06x} already assigned to {prior:?}; a released code is never reassigned"
                ),
            ));
        }
        if !seen_names.insert(name.as_str()) {
            out.push(v("bad_field", registry, name, "duplicate schema name"));
        }
        if !matches!(
            status.as_str(),
            "active" | "reserved" | "retired" | "experimental"
        ) {
            out.push(v(
                "bad_field",
                registry,
                name,
                format!("status {status:?} not in {{active, reserved, retired, experimental}}"),
            ));
        }
        let experimental_range = (0xc000..=0xfffe).contains(code);
        if experimental_range && status != "experimental" {
            out.push(v(
                "range_status_mismatch",
                registry,
                name,
                format!(
                    "code {code:#06x} is in the test/experimental range but status is {status:?}"
                ),
            ));
        }
        if !experimental_range && status == "experimental" {
            out.push(v(
                "range_status_mismatch",
                registry,
                name,
                format!("status experimental requires a 0xc000..=0xfffe code, found {code:#06x}"),
            ));
        }
        if status == "experimental" {
            // Shipped registries are production surfaces: production readers
            // reject experimental codes, so a shipped experimental row fails.
            out.push(v(
                "experimental_in_production",
                registry,
                name,
                "experimental rows are rejected by production readers and may not ship in the registry",
            ));
        }
    }
}

/// Canonical BodyDigest recipe transcript (drift pin input; NOT the BLAKE3
/// identity law — that is implemented by w1-generated-parsers).
pub fn bodydigest_transcript(
    schema: &str,
    domain: &str,
    major: i64,
    included: &[i64],
    excluded: &[i64],
) -> String {
    let join = |tags: &[i64]| {
        let mut sorted: Vec<i64> = tags.to_vec();
        sorted.sort_unstable();
        sorted
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        "bodydigest|{schema}|{domain}|major:{major}|included:{}|excluded:{}",
        join(included),
        join(excluded)
    )
}

pub fn bodydigest_pin(transcript: &str) -> String {
    format!("fnv1a64:{:016x}", fnv1a64(transcript.as_bytes()))
}

/// Encodability check: every field a producer wants to encode must have a
/// registered row for its containing schema ("a field absent from the table
/// is unencodable"). Returns one violation per unregistered field.
pub fn check_encodable(
    r: &IdentityRegistries,
    schema: &str,
    field_names: &[&str],
) -> Vec<Violation> {
    let registered: BTreeSet<&str> = r
        .fields
        .iter()
        .filter(|f| f.containing_schema == schema)
        .map(|f| f.stable_name.as_str())
        .collect();
    field_names
        .iter()
        .filter(|name| !registered.contains(**name))
        .map(|name| {
            v(
                "unregistered_field",
                "durable_fields",
                schema,
                format!("field {name:?} has no durable_fields.toml row and is unencodable"),
            )
        })
        .collect()
}

pub fn validate_identity(r: &IdentityRegistries) -> Vec<Violation> {
    let mut out = Vec::new();

    // --- per-registry code-space law ---------------------------------------
    check_code_space(
        "logical_object_kinds",
        &r.logical
            .iter()
            .map(|k| (k.object_kind, k.name.clone(), k.status.clone()))
            .collect::<Vec<_>>(),
        &mut out,
    );
    check_code_space(
        "physical_record_kinds",
        &r.physical
            .iter()
            .map(|k| (k.record_kind, k.name.clone(), k.status.clone()))
            .collect::<Vec<_>>(),
        &mut out,
    );
    check_code_space(
        "bootstrap_frames",
        &r.bootstrap
            .iter()
            .map(|k| (k.frame_kind, k.name.clone(), k.status.clone()))
            .collect::<Vec<_>>(),
        &mut out,
    );
    check_code_space(
        "prebootstrap_artifact_kinds",
        &r.prebootstrap
            .iter()
            .map(|k| (k.artifact_kind, k.name.clone(), k.status.clone()))
            .collect::<Vec<_>>(),
        &mut out,
    );
    check_code_space(
        "wire_types",
        &r.wire
            .iter()
            .map(|k| (k.wire_type_id, k.name.clone(), k.status.clone()))
            .collect::<Vec<_>>(),
        &mut out,
    );
    for (epoch, reg) in [
        (r.logical_epoch, "logical_object_kinds"),
        (r.physical_epoch, "physical_record_kinds"),
        (r.bootstrap_epoch, "bootstrap_frames"),
        (r.prebootstrap_epoch, "prebootstrap_artifact_kinds"),
        (r.wire_epoch, "wire_types"),
        (r.fields_epoch, "durable_fields"),
    ] {
        if epoch < 1 {
            out.push(v(
                "bad_field",
                reg,
                "registry",
                "registry_epoch must be >= 1",
            ));
        }
    }

    // --- physical identity laws --------------------------------------------
    for k in &r.physical {
        if !matches!(
            k.identity_law.as_str(),
            "ciphertext_id"
                | "encoding_id"
                | "placement_id"
                | "symbol_record"
                | "locator_entry"
                | "pack"
        ) {
            out.push(v(
                "bad_field",
                "physical_record_kinds",
                &k.name,
                format!("unknown identity_law {:?}", k.identity_law),
            ));
        }
    }

    // --- wire-type shape ----------------------------------------------------
    let wire_names: BTreeSet<&str> = r.wire.iter().map(|w| w.name.as_str()).collect();
    for w in &r.wire {
        if !matches!(
            w.kind.as_str(),
            "record" | "union" | "union_variant" | "reference_wrapper" | "discriminant" | "framing"
        ) {
            out.push(v(
                "bad_field",
                "wire_types",
                &w.name,
                format!("unknown kind {:?}", w.kind),
            ));
        }
        match (w.kind.as_str(), &w.containing_union, w.wire_tag) {
            ("union_variant", Some(union), Some(tag)) => {
                if !wire_names.contains(union.as_str()) {
                    out.push(v(
                        "bad_field",
                        "wire_types",
                        &w.name,
                        format!("containing_union {union:?} is not a registered wire type"),
                    ));
                }
                if tag <= 0 || tag >= 0xffff {
                    out.push(v(
                        "code_invalid",
                        "wire_types",
                        &w.name,
                        format!("wire_tag {tag:#06x} outside the valid space"),
                    ));
                }
            }
            ("union_variant", _, _) => out.push(v(
                "bad_field",
                "wire_types",
                &w.name,
                "union_variant requires containing_union and wire_tag",
            )),
            (_, Some(_), _) | (_, _, Some(_)) => out.push(v(
                "bad_field",
                "wire_types",
                &w.name,
                "containing_union/wire_tag are only legal on union_variant rows",
            )),
            _ => {}
        }
    }
    // Variant tags unique within a union.
    let mut variant_tags: BTreeMap<(&str, i64), &str> = BTreeMap::new();
    for w in &r.wire {
        if let (Some(union), Some(tag)) = (&w.containing_union, w.wire_tag)
            && let Some(prior) = variant_tags.insert((union.as_str(), tag), &w.name)
        {
            out.push(v(
                "code_duplicate",
                "wire_types",
                &w.name,
                format!("wire_tag {tag:#06x} in union {union:?} already assigned to {prior:?}"),
            ));
        }
    }

    // --- disjointness across the five classes ------------------------------
    let mut class_of: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for k in &r.logical {
        class_of.entry(k.name.as_str()).or_default().push("logical");
    }
    for k in &r.physical {
        class_of
            .entry(k.name.as_str())
            .or_default()
            .push("physical");
    }
    for k in &r.bootstrap {
        class_of
            .entry(k.name.as_str())
            .or_default()
            .push("bootstrap");
    }
    for k in &r.prebootstrap {
        class_of
            .entry(k.name.as_str())
            .or_default()
            .push("prebootstrap");
    }
    for k in &r.wire {
        class_of.entry(k.name.as_str()).or_default().push("wire");
    }
    for (name, classes) in &class_of {
        if classes.len() > 1 {
            out.push(v(
                "disjointness_dual_class",
                "identity",
                name,
                format!("schema inhabits {classes:?}; no schema may inhabit more than one identity class"),
            ));
        }
    }

    // --- field rows ---------------------------------------------------------
    let logical_by_name: BTreeMap<&str, &LogicalKind> =
        r.logical.iter().map(|k| (k.name.as_str(), k)).collect();
    let bootstrap_names: BTreeSet<&str> = r.bootstrap.iter().map(|k| k.name.as_str()).collect();
    let physical_names: BTreeSet<&str> = r.physical.iter().map(|k| k.name.as_str()).collect();
    let prebootstrap_names: BTreeSet<&str> =
        r.prebootstrap.iter().map(|k| k.name.as_str()).collect();
    let union_by_name: BTreeMap<&str, &ReferenceUnion> = r
        .unions
        .iter()
        .map(|u| (u.union_name.as_str(), u))
        .collect();

    let mut field_tags: BTreeMap<(&str, i64), &str> = BTreeMap::new();
    let mut body_rows_per_schema: BTreeMap<&str, Vec<&FieldRow>> = BTreeMap::new();
    let tags_per_schema: BTreeMap<&str, BTreeSet<i64>> = {
        let mut m: BTreeMap<&str, BTreeSet<i64>> = BTreeMap::new();
        for f in &r.fields {
            m.entry(f.containing_schema.as_str())
                .or_default()
                .insert(f.field_tag);
        }
        m
    };

    for f in &r.fields {
        let row_id = format!("{}#{}", f.containing_schema, f.stable_name);
        // Containing schema must resolve in one identity class.
        let containing_logical = logical_by_name.get(f.containing_schema.as_str());
        let resolves = containing_logical.is_some()
            || bootstrap_names.contains(f.containing_schema.as_str())
            || physical_names.contains(f.containing_schema.as_str())
            || prebootstrap_names.contains(f.containing_schema.as_str());
        if !resolves {
            out.push(v(
                "field_unresolved_schema",
                "durable_fields",
                &row_id,
                format!(
                    "containing_schema {:?} resolves in no identity class",
                    f.containing_schema
                ),
            ));
        }
        // Tag uniqueness + validity.
        if f.field_tag <= 0 || f.field_tag >= 0xffff {
            out.push(v(
                "code_invalid",
                "durable_fields",
                &row_id,
                format!("field_tag {:#06x} outside the valid space", f.field_tag),
            ));
        }
        if let Some(prior) =
            field_tags.insert((f.containing_schema.as_str(), f.field_tag), &f.stable_name)
        {
            out.push(v(
                "code_duplicate",
                "durable_fields",
                &row_id,
                format!("field_tag {} already assigned to {prior:?}", f.field_tag),
            ));
        }
        // Enum shapes.
        if !matches!(f.cardinality.as_str(), "one" | "optional" | "many") {
            out.push(v("bad_field", "durable_fields", &row_id, "bad cardinality"));
        }
        if !matches!(
            f.identity_class.as_str(),
            "scalar" | "inline" | "logical" | "physical" | "bootstrap_local"
        ) {
            out.push(v(
                "bad_field",
                "durable_fields",
                &row_id,
                "bad identity_class",
            ));
        }
        if !matches!(
            f.reference_semantics.as_str(),
            "none" | "strong" | "conditional" | "weak_digest" | "locator"
        ) {
            out.push(v(
                "bad_field",
                "durable_fields",
                &row_id,
                "bad reference_semantics",
            ));
        }
        if !matches!(
            f.version_status.as_str(),
            "active" | "reserved" | "retired" | "experimental"
        ) {
            out.push(v(
                "bad_field",
                "durable_fields",
                &row_id,
                "bad version_status",
            ));
        }
        // Wire-type resolution: builtin -> wire_types -> reference_union.
        let is_builtin = BUILTIN_WIRE_TYPES.contains(&f.exact_wire_type.as_str());
        let is_wire = wire_names.contains(f.exact_wire_type.as_str());
        let is_union = union_by_name.contains_key(f.exact_wire_type.as_str());
        if !is_builtin && !is_wire && !is_union {
            out.push(v(
                "field_unresolved_wire_type",
                "durable_fields",
                &row_id,
                format!("exact_wire_type {:?} resolves nowhere", f.exact_wire_type),
            ));
        }
        // Construction-order consistency with the containing logical kind.
        if let Some(kind) = containing_logical
            && f.construction_order != kind.construction_order
        {
            out.push(v(
                "bad_field",
                "durable_fields",
                &row_id,
                format!(
                    "construction_order {} != containing kind's {}",
                    f.construction_order, kind.construction_order
                ),
            ));
        }
        // Reference discipline.
        let is_retaining = matches!(f.reference_semantics.as_str(), "strong" | "conditional");
        if is_retaining {
            if bootstrap_names.contains(f.containing_schema.as_str()) {
                out.push(v(
                    "frame_strong_ref",
                    "durable_fields",
                    &row_id,
                    "bootstrap frames are not graph nodes and may not carry retaining references",
                ));
            }
            if f.identity_class != "logical" {
                out.push(v(
                    "bad_field",
                    "durable_fields",
                    &row_id,
                    "strong/conditional references must have identity_class = \"logical\"",
                ));
            }
            match &f.target_schema_id {
                Some(target) => {
                    if physical_names.contains(target.as_str())
                        || bootstrap_names.contains(target.as_str())
                        || prebootstrap_names.contains(target.as_str())
                    {
                        out.push(v(
                            "ref_target_not_logical",
                            "durable_fields",
                            &row_id,
                            format!(
                                "strong/conditional target {target:?} is not a logical object (physical realizations, frames, and prebootstrap artifacts are never StrongRef targets)"
                            ),
                        ));
                    } else if !logical_by_name.contains_key(target.as_str()) {
                        out.push(v(
                            "ref_target_unresolved",
                            "durable_fields",
                            &row_id,
                            format!("target {target:?} resolves nowhere"),
                        ));
                    }
                }
                None => {
                    // Polymorphic: must be a generated union anchored to this row.
                    match union_by_name.get(f.exact_wire_type.as_str()) {
                        Some(u)
                            if u.containing_schema == f.containing_schema
                                && u.field_tag == f.field_tag => {}
                        _ => out.push(v(
                            "bare_strong_ref",
                            "durable_fields",
                            &row_id,
                            "polymorphic strong/conditional field without its generated reference union (bare StrongRef<A|B> is invalid in normative bytes)",
                        )),
                    }
                }
            }
        } else if let Some(target) = &f.target_schema_id {
            // weak_digest/locator targets: must at least resolve somewhere
            // (weak digests of logical objects; locators may name logical
            // or physical realizations).
            let known = logical_by_name.contains_key(target.as_str())
                || physical_names.contains(target.as_str());
            if !known {
                out.push(v(
                    "ref_target_unresolved",
                    "durable_fields",
                    &row_id,
                    format!("nonretaining target {target:?} resolves nowhere"),
                ));
            }
        }
        // Digest discipline: digest-typed fields declare exactly one class;
        // never by naming convention.
        let digest_typed = f.exact_wire_type == "digest256" || f.exact_wire_type == "WeakDigest";
        match &f.digest_class {
            None if digest_typed => out.push(v(
                "digest_missing_class",
                "durable_fields",
                &row_id,
                "digest-typed field without a declared digest_class (target|transcript|weak_identity|body)",
            )),
            None => {}
            Some(class) => {
                match class.as_str() {
                    "target" | "weak_identity" => {}
                    "transcript" => {
                        if f.transcript_recipe.as_deref().is_none_or(|t| t.trim().is_empty()) {
                            out.push(v(
                                "digest_missing_recipe",
                                "durable_fields",
                                &row_id,
                                "transcript digest without a registered recipe",
                            ));
                        }
                    }
                    "body" => {
                        body_rows_per_schema
                            .entry(f.containing_schema.as_str())
                            .or_default()
                            .push(f);
                    }
                    other => out.push(v(
                        "bad_field",
                        "durable_fields",
                        &row_id,
                        format!("unknown digest_class {other:?}"),
                    )),
                }
            }
        }
    }

    // --- BodyDigest recipes -------------------------------------------------
    for (schema, rows) in &body_rows_per_schema {
        if rows.len() > 1 {
            out.push(v(
                "bodydigest_two_fields",
                "durable_fields",
                schema,
                format!(
                    "{} BodyDigest fields in one schema; exactly one is legal",
                    rows.len()
                ),
            ));
        }
        for f in rows {
            let row_id = format!("{}#{}", f.containing_schema, f.stable_name);
            let (Some(domain), Some(major), Some(included), Some(excluded), Some(pin)) = (
                &f.bd_domain_separator,
                f.bd_schema_major,
                &f.bd_included_field_tags,
                &f.bd_excluded_field_tags,
                &f.recipe_pin,
            ) else {
                out.push(v(
                    "bad_field",
                    "durable_fields",
                    &row_id,
                    "BodyDigest row requires bd_domain_separator, bd_schema_major, bd_included_field_tags, bd_excluded_field_tags, recipe_pin",
                ));
                continue;
            };
            let known_tags = tags_per_schema.get(schema).cloned().unwrap_or_default();
            for tag in included.iter().chain(excluded.iter()) {
                if !known_tags.contains(tag) {
                    out.push(v(
                        "bodydigest_unknown_exclusion",
                        "durable_fields",
                        &row_id,
                        format!("recipe names unregistered field tag {tag} of {schema}"),
                    ));
                }
            }
            // The digest's own field must be excluded and never included:
            // computing over bytes that include the digest itself is a G0
            // error (self-including computation).
            if included.contains(&f.field_tag) || !excluded.contains(&f.field_tag) {
                out.push(v(
                    "bodydigest_self_included",
                    "durable_fields",
                    &row_id,
                    "the BodyDigest field's own tag must be excluded from its recipe",
                ));
            }
            let transcript = bodydigest_transcript(schema, domain, major, included, excluded);
            let recomputed = bodydigest_pin(&transcript);
            if recomputed != *pin {
                out.push(v(
                    "bodydigest_pin_mismatch",
                    "durable_fields",
                    &row_id,
                    format!(
                        "recipe drift: pinned {pin:?} != recomputed {recomputed:?} over transcript {transcript:?}"
                    ),
                ));
            }
        }
    }

    // --- reference unions ---------------------------------------------------
    let mut union_names_seen = BTreeSet::new();
    for u in &r.unions {
        if !union_names_seen.insert(u.union_name.as_str()) {
            out.push(v(
                "bad_field",
                "durable_fields",
                &u.union_name,
                "duplicate reference_union name",
            ));
        }
        // Anchor: the declaring field row must exist and use this union.
        let anchored = r.fields.iter().any(|f| {
            f.containing_schema == u.containing_schema
                && f.field_tag == u.field_tag
                && f.exact_wire_type == u.union_name
        });
        if !anchored {
            out.push(v(
                "union_field_mismatch",
                "durable_fields",
                &u.union_name,
                format!(
                    "no field row ({}, tag {}) declares exact_wire_type {:?}",
                    u.containing_schema, u.field_tag, u.union_name
                ),
            ));
        }
        let mut arm_tags = BTreeSet::new();
        for (tag, target) in &u.arms {
            if !arm_tags.insert(*tag) {
                out.push(v(
                    "union_arm_duplicate_tag",
                    "durable_fields",
                    &u.union_name,
                    format!("duplicate arm tag {tag}"),
                ));
            }
            match logical_by_name.get(target.as_str()) {
                None => out.push(v(
                    "union_arm_unresolved",
                    "durable_fields",
                    &u.union_name,
                    format!("arm {tag} target {target:?} is not a registered logical object"),
                )),
                Some(target_kind) => {
                    if let Some(containing) = logical_by_name.get(u.containing_schema.as_str())
                        && target_kind.construction_order > containing.construction_order
                    {
                        out.push(v(
                            "dag_future_result",
                            "durable_fields",
                            &u.union_name,
                            format!(
                                "arm target {target:?} (order {}) is constructed after containing {:?} (order {}): a future result is never referenceable",
                                target_kind.construction_order,
                                u.containing_schema,
                                containing.construction_order
                            ),
                        ));
                    }
                }
            }
        }
    }

    // --- construction DAG over logical kinds --------------------------------
    let mut edges: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for f in &r.fields {
        if !matches!(f.reference_semantics.as_str(), "strong" | "conditional") {
            continue;
        }
        let Some(containing) = logical_by_name.get(f.containing_schema.as_str()) else {
            continue;
        };
        let mut targets: Vec<&str> = Vec::new();
        if let Some(t) = &f.target_schema_id {
            targets.push(t.as_str());
        } else if let Some(u) = union_by_name.get(f.exact_wire_type.as_str()) {
            targets.extend(u.arms.iter().map(|(_, t)| t.as_str()));
        }
        for target in targets {
            let Some(target_kind) = logical_by_name.get(target) else {
                continue;
            };
            let row_id = format!("{}#{}", f.containing_schema, f.stable_name);
            if target == f.containing_schema {
                out.push(v(
                    "dag_self_edge",
                    "durable_fields",
                    &row_id,
                    "a schema may not strongly reference itself",
                ));
                continue;
            }
            if target_kind.construction_order > containing.construction_order {
                out.push(v(
                    "dag_future_result",
                    "durable_fields",
                    &row_id,
                    format!(
                        "target {target:?} (order {}) is constructed after {:?} (order {}): every strong value must already be known",
                        target_kind.construction_order,
                        f.containing_schema,
                        containing.construction_order
                    ),
                ));
            }
            edges
                .entry(containing.name.as_str())
                .or_default()
                .insert(target_kind.name.as_str());
        }
    }
    if let Some(cycle) = find_cycle_str(&edges) {
        out.push(v(
            "dag_cycle",
            "durable_fields",
            cycle.first().copied().unwrap_or(""),
            format!("construction-DAG cycle: {cycle:?}"),
        ));
    }

    out
}

/// Iterative three-color DFS over string-keyed edges.
fn find_cycle_str<'a>(edges: &BTreeMap<&'a str, BTreeSet<&'a str>>) -> Option<Vec<&'a str>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Gray,
        Black,
    }
    let mut color: BTreeMap<&str, Color> = BTreeMap::new();
    for (from, targets) in edges {
        color.entry(from).or_insert(Color::White);
        for t in targets {
            color.entry(t).or_insert(Color::White);
        }
    }
    let nodes: Vec<&str> = color.keys().copied().collect();
    for start in nodes {
        if color.get(start) != Some(&Color::White) {
            continue;
        }
        let mut stack: Vec<(&str, Vec<&str>, usize)> = Vec::new();
        let children: Vec<&str> = edges
            .get(start)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        stack.push((start, children, 0));
        color.insert(start, Color::Gray);
        while let Some((node, children, idx)) = stack.last().cloned() {
            if idx < children.len() {
                if let Some(frame) = stack.last_mut() {
                    frame.2 += 1;
                }
                let child = children[idx];
                match color.get(child) {
                    Some(Color::Gray) => {
                        let mut cycle: Vec<&str> = stack.iter().map(|(n, _, _)| *n).collect();
                        if let Some(pos) = cycle.iter().position(|n| *n == child) {
                            cycle.drain(..pos);
                        }
                        cycle.push(child);
                        return Some(cycle);
                    }
                    Some(Color::White) => {
                        color.insert(child, Color::Gray);
                        let grand: Vec<&str> = edges
                            .get(child)
                            .map(|s| s.iter().copied().collect())
                            .unwrap_or_default();
                        stack.push((child, grand, 0));
                    }
                    _ => {}
                }
            } else {
                color.insert(node, Color::Black);
                stack.pop();
            }
        }
    }
    None
}

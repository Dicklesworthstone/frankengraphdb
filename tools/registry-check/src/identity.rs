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
//!                                    ordinary and generated reference unions
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
//!   ordinary_union_duplicate_path  two ordinary unions claim one schema path
//!   ordinary_union_name_collision ordinary/reference union name collision
//!   ordinary_union_unresolved_schema containing schema has no unique identity class
//!   ordinary_union_arm_duplicate_tag duplicate ordinary-union arm tag
//!   ordinary_union_arm_metadata_mismatch arm does not match its union owner
//!   ordinary_union_arm_lifecycle_mismatch arm outlives its ordinary union
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
pub const BUILTIN_WIRE_TYPES: [&str; 11] = [
    "u8",
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
    pub arms: Vec<ReferenceUnionArm>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceUnionArm {
    pub union_name: String,
    pub containing_schema: String,
    pub field_tag: i64,
    pub arm_tag: i64,
    pub stable_name: String,
    pub target_schema_id: String,
    pub role: String,
    pub identity_class: String,
    pub reference_semantics: String,
    pub role_predicate: String,
    pub retention_and_cut_rule: String,
    pub version_status: String,
    pub max_size_bytes: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrdinaryUnion {
    pub union_name: String,
    pub containing_schema: String,
    pub union_path: String,
    pub field_tag: Option<i64>,
    pub tag_wire_type: String,
    pub encoding_context: String,
    pub role_predicate: String,
    pub version_status: String,
    pub max_size_bytes: i64,
    pub arms: Vec<OrdinaryUnionArm>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrdinaryUnionArm {
    pub union_name: String,
    pub containing_schema: String,
    pub union_path: String,
    pub arm_tag: i64,
    pub source_arm_name: String,
    pub stable_name: String,
    pub payload_kind: String,
    pub payload_sha256: Option<String>,
    pub role_predicate: String,
    pub version_status: String,
    pub max_size_bytes: i64,
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
    pub ordinary_unions: Vec<OrdinaryUnion>,
}

pub type DurableFieldsRows = (i64, Vec<FieldRow>, Vec<OrdinaryUnion>, Vec<ReferenceUnion>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssignmentPin {
    pub registry: &'static str,
    pub expected_epoch: i64,
    pub actual_epoch: i64,
    pub expected_pin: &'static str,
    pub actual_pin: String,
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

/// Require that a table contains no keys outside its versioned schema.
///
/// `Table` is a `BTreeMap`, so when several unknown keys are present the
/// lexicographically first one is reported.  This keeps the error path stable
/// across runs while naming the exact rejected key.
fn exact_keys(table: &Table, allowed: &[&str], ctx: &str) -> Result<(), ReadError> {
    if let Some(key) = table.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(ReadError {
            path: format!("{ctx}.{key}"),
            msg: "unknown key in closed schema".into(),
        });
    }
    Ok(())
}

fn registry_header(
    root: &Table,
    expected: &str,
    file: &str,
    row_keys: &[&str],
) -> Result<i64, ReadError> {
    let mut allowed_root_keys = Vec::with_capacity(2 + row_keys.len());
    allowed_root_keys.extend_from_slice(&["schema_version", "registry"]);
    allowed_root_keys.extend_from_slice(row_keys);
    exact_keys(root, &allowed_root_keys, file)?;

    let schema_version = get_int(root, "schema_version", file)?;
    if schema_version != 1 {
        return Err(ReadError {
            path: format!("{file}.schema_version"),
            msg: format!("expected schema version 1, found {schema_version}"),
        });
    }

    let registry = get_table(root, "registry", file)?;
    let registry_ctx = format!("{file}.registry");
    exact_keys(registry, &["name", "registry_epoch"], &registry_ctx)?;
    let name = get_str(registry, "name", &registry_ctx)?;
    if name != expected {
        return Err(ReadError {
            path: format!("{file}.registry.name"),
            msg: format!("expected {expected:?}, found {name:?}"),
        });
    }
    get_int(registry, "registry_epoch", &registry_ctx)
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
    let epoch = registry_header(
        root,
        "logical_object_kinds",
        "logical_object_kinds.toml",
        &["kind"],
    )?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "kind", "logical_object_kinds.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("logical_object_kinds.toml.kind[{i}]");
        exact_keys(
            t,
            &[
                "object_kind",
                "name",
                "status",
                "construction_order",
                "role_predicate",
                "max_size_bytes",
                "golden_corpus",
            ],
            &ctx,
        )?;
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
    let epoch = registry_header(
        root,
        "physical_record_kinds",
        "physical_record_kinds.toml",
        &["kind"],
    )?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "kind", "physical_record_kinds.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("physical_record_kinds.toml.kind[{i}]");
        exact_keys(
            t,
            &[
                "record_kind",
                "name",
                "identity_law",
                "status",
                "transcript",
                "owning_identity",
                "max_size_bytes",
            ],
            &ctx,
        )?;
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
    let epoch = registry_header(
        root,
        "bootstrap_frames",
        "bootstrap_frames.toml",
        &["frame"],
    )?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "frame", "bootstrap_frames.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("bootstrap_frames.toml.frame[{i}]");
        exact_keys(
            t,
            &[
                "frame_kind",
                "name",
                "status",
                "byte_size",
                "location",
                "update_protocol",
                "tear_validation",
                "opener_fields",
                "compatibility_gate",
                "recovery_vectors",
            ],
            &ctx,
        )?;
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
        &["kind"],
    )?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "kind", "prebootstrap_artifact_kinds.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("prebootstrap_artifact_kinds.toml.kind[{i}]");
        exact_keys(
            t,
            &[
                "artifact_kind",
                "name",
                "status",
                "target_claim_domain",
                "allowed_containers",
                "import_target",
                "max_size_bytes",
            ],
            &ctx,
        )?;
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
    let epoch = registry_header(root, "wire_types", "wire_types.toml", &["type"])?;
    let mut rows = Vec::new();
    for (i, t) in get_table_array(root, "type", "wire_types.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("wire_types.toml.type[{i}]");
        exact_keys(
            t,
            &[
                "wire_type_id",
                "name",
                "kind",
                "status",
                "containing_union",
                "wire_tag",
                "encoding_context",
                "allowed_containing_schemas",
                "max_size_bytes",
            ],
            &ctx,
        )?;
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

pub fn fields_from(root: &Table) -> Result<DurableFieldsRows, ReadError> {
    let epoch = registry_header(
        root,
        "durable_fields",
        "durable_fields.toml",
        &[
            "field",
            "union",
            "union_arm",
            "reference_union",
            "reference_union_arm",
        ],
    )?;
    let mut fields = Vec::new();
    for (i, t) in get_table_array(root, "field", "durable_fields.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("durable_fields.toml.field[{i}]");
        exact_keys(
            t,
            &[
                "containing_schema",
                "field_tag",
                "stable_name",
                "exact_wire_type",
                "cardinality",
                "identity_class",
                "reference_semantics",
                "target_schema_id",
                "construction_order",
                "role_predicate",
                "retention_and_cut_rule",
                "version_status",
                "max_size_bytes",
                "digest_class",
                "transcript_recipe",
                "bd_domain_separator",
                "bd_schema_major",
                "bd_included_field_tags",
                "bd_excluded_field_tags",
                "recipe_pin",
            ],
            &ctx,
        )?;
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
    let mut reference_unions = Vec::new();
    for (i, t) in get_table_array(root, "reference_union", "durable_fields.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("durable_fields.toml.reference_union[{i}]");
        exact_keys(
            t,
            &["union_name", "containing_schema", "field_tag", "role"],
            &ctx,
        )?;
        reference_unions.push(ReferenceUnion {
            union_name: get_str(t, "union_name", &ctx)?,
            containing_schema: get_str(t, "containing_schema", &ctx)?,
            field_tag: get_int(t, "field_tag", &ctx)?,
            role: get_str(t, "role", &ctx)?,
            arms: Vec::new(),
        });
    }

    let mut reference_union_index = BTreeMap::new();
    for (index, union) in reference_unions.iter().enumerate() {
        reference_union_index.insert(union.union_name.clone(), index);
    }
    for (i, t) in get_table_array(root, "reference_union_arm", "durable_fields.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("durable_fields.toml.reference_union_arm[{i}]");
        exact_keys(
            t,
            &[
                "union_name",
                "containing_schema",
                "field_tag",
                "arm_tag",
                "stable_name",
                "target_schema_id",
                "role",
                "identity_class",
                "reference_semantics",
                "role_predicate",
                "retention_and_cut_rule",
                "version_status",
                "max_size_bytes",
            ],
            &ctx,
        )?;
        let arm = ReferenceUnionArm {
            union_name: get_str(t, "union_name", &ctx)?,
            containing_schema: get_str(t, "containing_schema", &ctx)?,
            field_tag: get_int(t, "field_tag", &ctx)?,
            arm_tag: get_int(t, "arm_tag", &ctx)?,
            stable_name: get_str(t, "stable_name", &ctx)?,
            target_schema_id: get_str(t, "target_schema_id", &ctx)?,
            role: get_str(t, "role", &ctx)?,
            identity_class: get_str(t, "identity_class", &ctx)?,
            reference_semantics: get_str(t, "reference_semantics", &ctx)?,
            role_predicate: get_str(t, "role_predicate", &ctx)?,
            retention_and_cut_rule: get_str(t, "retention_and_cut_rule", &ctx)?,
            version_status: get_str(t, "version_status", &ctx)?,
            max_size_bytes: get_int(t, "max_size_bytes", &ctx)?,
        };
        let Some(index) = reference_union_index.get(&arm.union_name).copied() else {
            return Err(ReadError {
                path: format!("{ctx}.union_name"),
                msg: format!(
                    "reference-union arm names undeclared union {:?}",
                    arm.union_name
                ),
            });
        };
        reference_unions[index].arms.push(arm);
    }

    let mut ordinary_unions = Vec::new();
    for (i, t) in get_table_array(root, "union", "durable_fields.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("durable_fields.toml.union[{i}]");
        exact_keys(
            t,
            &[
                "union_name",
                "containing_schema",
                "union_path",
                "field_tag",
                "tag_wire_type",
                "encoding_context",
                "role_predicate",
                "version_status",
                "max_size_bytes",
            ],
            &ctx,
        )?;
        ordinary_unions.push(OrdinaryUnion {
            union_name: get_str(t, "union_name", &ctx)?,
            containing_schema: get_str(t, "containing_schema", &ctx)?,
            union_path: get_str(t, "union_path", &ctx)?,
            field_tag: get_opt_int(t, "field_tag", &ctx)?,
            tag_wire_type: get_str(t, "tag_wire_type", &ctx)?,
            encoding_context: get_str(t, "encoding_context", &ctx)?,
            role_predicate: get_str(t, "role_predicate", &ctx)?,
            version_status: get_str(t, "version_status", &ctx)?,
            max_size_bytes: get_int(t, "max_size_bytes", &ctx)?,
            arms: Vec::new(),
        });
    }

    let mut ordinary_union_index = BTreeMap::new();
    for (index, union) in ordinary_unions.iter().enumerate() {
        ordinary_union_index.insert(union.union_name.clone(), index);
    }
    for (i, t) in get_table_array(root, "union_arm", "durable_fields.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("durable_fields.toml.union_arm[{i}]");
        exact_keys(
            t,
            &[
                "union_name",
                "containing_schema",
                "union_path",
                "arm_tag",
                "source_arm_name",
                "stable_name",
                "payload_kind",
                "payload_sha256",
                "role_predicate",
                "version_status",
                "max_size_bytes",
            ],
            &ctx,
        )?;
        let arm = OrdinaryUnionArm {
            union_name: get_str(t, "union_name", &ctx)?,
            containing_schema: get_str(t, "containing_schema", &ctx)?,
            union_path: get_str(t, "union_path", &ctx)?,
            arm_tag: get_int(t, "arm_tag", &ctx)?,
            source_arm_name: get_str(t, "source_arm_name", &ctx)?,
            stable_name: get_str(t, "stable_name", &ctx)?,
            payload_kind: get_str(t, "payload_kind", &ctx)?,
            payload_sha256: get_opt_str(t, "payload_sha256", &ctx)?,
            role_predicate: get_str(t, "role_predicate", &ctx)?,
            version_status: get_str(t, "version_status", &ctx)?,
            max_size_bytes: get_int(t, "max_size_bytes", &ctx)?,
        };
        let Some(index) = ordinary_union_index.get(&arm.union_name).copied() else {
            return Err(ReadError {
                path: format!("{ctx}.union_name"),
                msg: format!(
                    "ordinary-union arm names undeclared union {:?}",
                    arm.union_name
                ),
            });
        };
        ordinary_unions[index].arms.push(arm);
    }

    Ok((epoch, fields, ordinary_unions, reference_unions))
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
    let (fields_epoch, fields, ordinary_unions, unions) =
        fields_from(&load_table(dir, "durable_fields.toml")?)
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
        ordinary_unions,
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

fn rows_pin(mut rows: Vec<String>) -> String {
    rows.sort();
    let transcript = rows.join("\n");
    format!("fnv1a64:{:016x}", fnv1a64(transcript.as_bytes()))
}

fn predicate_allows_role(predicate: &str, role: &str) -> bool {
    predicate == "true"
        || predicate
            .split("||")
            .map(str::trim)
            .any(|term| term == format!("role-{role}"))
}

fn is_lowercase_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

fn check_ordinary_union_version_status(status: &str, row_id: &str, out: &mut Vec<Violation>) {
    match status {
        "active" | "reserved" | "retired" => {}
        "experimental" => out.push(v(
            "experimental_in_production",
            "durable_fields",
            row_id,
            "experimental ordinary-union rows may not ship in the production registry",
        )),
        _ => out.push(v(
            "bad_field",
            "durable_fields",
            row_id,
            format!("version_status {status:?} is not one of active|reserved|retired"),
        )),
    }
}

/// Independent, review-updated pins for the released identity assignments.
///
/// Registry rows are the canonical descriptions; these constants are compact
/// historical witnesses, not a second allowlist.  Adding or retiring a row
/// requires an epoch bump and an intentional pin update.  Deleting a released
/// row, reassigning its code/tag, or silently changing a union arm therefore
/// fails even when the resulting current snapshot is internally consistent.
pub fn assignment_pins(r: &IdentityRegistries) -> Vec<AssignmentPin> {
    const LOGICAL: &str = "fnv1a64:1455c06e68eeebb1";
    const PHYSICAL: &str = "fnv1a64:6eb820a69bc263b2";
    const BOOTSTRAP: &str = "fnv1a64:c756ad93d4fcbcf7";
    const PREBOOTSTRAP: &str = "fnv1a64:d2a221d86d3adc80";
    const WIRE: &str = "fnv1a64:3225fb5d17c23088";
    const FIELDS: &str = "fnv1a64:d63c00f968930c2e";

    let logical = rows_pin(
        r.logical
            .iter()
            .map(|row| format!("kind|{:04x}|{}|{}", row.object_kind, row.name, row.status))
            .collect(),
    );
    let physical = rows_pin(
        r.physical
            .iter()
            .map(|row| format!("kind|{:04x}|{}|{}", row.record_kind, row.name, row.status))
            .collect(),
    );
    let bootstrap = rows_pin(
        r.bootstrap
            .iter()
            .map(|row| format!("frame|{:04x}|{}|{}", row.frame_kind, row.name, row.status))
            .collect(),
    );
    let prebootstrap = rows_pin(
        r.prebootstrap
            .iter()
            .map(|row| format!("kind|{:04x}|{}|{}", row.artifact_kind, row.name, row.status))
            .collect(),
    );
    let wire = rows_pin(
        r.wire
            .iter()
            .map(|row| {
                format!(
                    "type|{:04x}|{}|{}|{}|{}|{}",
                    row.wire_type_id,
                    row.name,
                    row.kind,
                    row.status,
                    row.containing_union.as_deref().unwrap_or("-"),
                    row.wire_tag
                        .map(|tag| format!("{tag:04x}"))
                        .unwrap_or_else(|| "-".into())
                )
            })
            .collect(),
    );
    let mut field_rows: Vec<String> = r
        .fields
        .iter()
        .map(|row| {
            format!(
                "field|{}|{:04x}|{}|{}|{}|{}|{}|{}|{}",
                row.containing_schema,
                row.field_tag,
                row.stable_name,
                row.exact_wire_type,
                row.cardinality,
                row.identity_class,
                row.reference_semantics,
                row.target_schema_id.as_deref().unwrap_or("-"),
                row.version_status
            )
        })
        .collect();
    for union in &r.unions {
        field_rows.push(format!(
            "union|{}|{}|{:04x}|{}",
            union.union_name, union.containing_schema, union.field_tag, union.role
        ));
        field_rows.extend(union.arms.iter().map(|arm| {
            format!(
                "arm|{}|{}|{:04x}|{:04x}|{}|{}|{}|{}|{}|{}",
                arm.union_name,
                arm.containing_schema,
                arm.field_tag,
                arm.arm_tag,
                arm.stable_name,
                arm.target_schema_id,
                arm.role,
                arm.identity_class,
                arm.reference_semantics,
                arm.version_status
            )
        }));
    }
    for union in &r.ordinary_unions {
        field_rows.push(format!(
            "ordinary-union|{}|{}|{}|{}|{}|{}|{}|{}|{}",
            union.union_name,
            union.containing_schema,
            union.union_path,
            union
                .field_tag
                .map(|tag| format!("{tag:04x}"))
                .unwrap_or_else(|| "-".into()),
            union.tag_wire_type,
            union.encoding_context,
            union.role_predicate,
            union.version_status,
            union.max_size_bytes
        ));
        field_rows.extend(union.arms.iter().map(|arm| {
            format!(
                "ordinary-arm|{}|{}|{}|{:04x}|{}|{}|{}|{}|{}|{}|{}",
                arm.union_name,
                arm.containing_schema,
                arm.union_path,
                arm.arm_tag,
                arm.source_arm_name,
                arm.stable_name,
                arm.payload_kind,
                arm.payload_sha256.as_deref().unwrap_or("-"),
                arm.role_predicate,
                arm.version_status,
                arm.max_size_bytes
            )
        }));
    }
    let fields = rows_pin(field_rows);

    vec![
        AssignmentPin {
            registry: "logical_object_kinds",
            expected_epoch: 1,
            actual_epoch: r.logical_epoch,
            expected_pin: LOGICAL,
            actual_pin: logical,
        },
        AssignmentPin {
            registry: "physical_record_kinds",
            expected_epoch: 1,
            actual_epoch: r.physical_epoch,
            expected_pin: PHYSICAL,
            actual_pin: physical,
        },
        AssignmentPin {
            registry: "bootstrap_frames",
            expected_epoch: 2,
            actual_epoch: r.bootstrap_epoch,
            expected_pin: BOOTSTRAP,
            actual_pin: bootstrap,
        },
        AssignmentPin {
            registry: "prebootstrap_artifact_kinds",
            expected_epoch: 1,
            actual_epoch: r.prebootstrap_epoch,
            expected_pin: PREBOOTSTRAP,
            actual_pin: prebootstrap,
        },
        AssignmentPin {
            registry: "wire_types",
            expected_epoch: 2,
            actual_epoch: r.wire_epoch,
            expected_pin: WIRE,
            actual_pin: wire,
        },
        AssignmentPin {
            registry: "durable_fields",
            expected_epoch: 3,
            actual_epoch: r.fields_epoch,
            expected_pin: FIELDS,
            actual_pin: fields,
        },
    ]
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
    for pin in assignment_pins(r) {
        if pin.actual_epoch != pin.expected_epoch {
            out.push(v(
                "registry_epoch_mismatch",
                pin.registry,
                "registry",
                format!(
                    "released assignment epoch is {}, found {}; an epoch changes only with an intentional row add/retire and pin update",
                    pin.expected_epoch, pin.actual_epoch
                ),
            ));
        }
        if pin.actual_pin != pin.expected_pin {
            out.push(v(
                "registry_assignment_drift",
                pin.registry,
                "registry",
                format!(
                    "released assignment pin {:?} != recomputed {:?}; released codes, tags, names, lifecycle states, and union arms are append-only",
                    pin.expected_pin, pin.actual_pin
                ),
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
        if k.transcript.trim().is_empty()
            || k.owning_identity.trim().is_empty()
            || k.max_size_bytes <= 0
        {
            out.push(v(
                "bad_field",
                "physical_record_kinds",
                &k.name,
                "identity transcript, owning identity, and positive resource bound are required",
            ));
        }
    }
    for k in &r.logical {
        if k.role_predicate.trim().is_empty()
            || k.golden_corpus.trim().is_empty()
            || k.max_size_bytes <= 0
        {
            out.push(v(
                "bad_field",
                "logical_object_kinds",
                &k.name,
                "role predicate, reserved corpus path, and positive resource bound are required",
            ));
        }
    }
    for frame in &r.bootstrap {
        if frame.byte_size <= 0
            || frame.location.trim().is_empty()
            || frame.update_protocol.trim().is_empty()
            || frame.tear_validation.trim().is_empty()
            || frame.opener_fields.trim().is_empty()
            || frame.compatibility_gate.trim().is_empty()
            || frame.recovery_vectors.trim().is_empty()
        {
            out.push(v(
                "bad_field",
                "bootstrap_frames",
                &frame.name,
                "fixed size, location, update/tear/open/compatibility contracts, and recovery vectors are required",
            ));
        }
    }
    for artifact in &r.prebootstrap {
        if artifact.target_claim_domain.trim().is_empty()
            || artifact.allowed_containers.trim().is_empty()
            || artifact.import_target.trim().is_empty()
            || artifact.max_size_bytes <= 0
        {
            out.push(v(
                "bad_field",
                "prebootstrap_artifact_kinds",
                &artifact.name,
                "claim domain, legal container closure, import target, and positive resource bound are required",
            ));
        }
    }

    // --- wire-type shape ----------------------------------------------------
    let wire_names: BTreeSet<&str> = r.wire.iter().map(|w| w.name.as_str()).collect();
    let wire_by_name: BTreeMap<&str, &WireType> =
        r.wire.iter().map(|w| (w.name.as_str(), w)).collect();
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
        if w.encoding_context.trim().is_empty()
            || w.allowed_containing_schemas.is_empty()
            || w.max_size_bytes <= 0
        {
            out.push(v(
                "bad_field",
                "wire_types",
                &w.name,
                "encoding context, containing-schema closure, and positive resource bound are required",
            ));
        }
        match (w.kind.as_str(), &w.containing_union, w.wire_tag) {
            ("union_variant", Some(union), Some(tag)) => {
                match wire_by_name.get(union.as_str()) {
                    None => out.push(v(
                        "bad_field",
                        "wire_types",
                        &w.name,
                        format!("containing_union {union:?} is not a registered wire type"),
                    )),
                    Some(parent) if !matches!(parent.kind.as_str(), "union" | "discriminant") => out.push(v(
                        "bad_field",
                        "wire_types",
                        &w.name,
                        format!(
                            "containing_union {union:?} is neither kind=union nor kind=discriminant"
                        ),
                    )),
                    Some(parent)
                        if matches!(parent.status.as_str(), "retired" | "experimental")
                            && w.status != parent.status =>
                    {
                        out.push(v(
                            "bad_field",
                            "wire_types",
                            &w.name,
                            format!(
                                "variant lifecycle {:?} is incompatible with containing union lifecycle {:?}",
                                w.status, parent.status
                            ),
                        ));
                    }
                    Some(_) => {}
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
    let ordinary_union_names: BTreeSet<&str> = r
        .ordinary_unions
        .iter()
        .map(|u| u.union_name.as_str())
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
            "none" | "strong" | "conditional" | "weak_digest" | "locator" | "external_root"
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
        if f.version_status == "experimental" {
            out.push(v(
                "experimental_in_production",
                "durable_fields",
                &row_id,
                "experimental field rows may not ship in the production registry",
            ));
        }
        if f.max_size_bytes <= 0 {
            out.push(v(
                "bad_field",
                "durable_fields",
                &row_id,
                "max_size_bytes must be positive",
            ));
        }
        if f.role_predicate.trim().is_empty() || f.retention_and_cut_rule.trim().is_empty() {
            out.push(v(
                "bad_field",
                "durable_fields",
                &row_id,
                "role_predicate and retention_and_cut_rule must be nonblank",
            ));
        }
        // Wire-type resolution: builtin -> wire_types -> ordinary union ->
        // generated reference union.
        let is_builtin = BUILTIN_WIRE_TYPES.contains(&f.exact_wire_type.as_str());
        let is_wire = wire_names.contains(f.exact_wire_type.as_str());
        let is_ordinary_union = ordinary_union_names.contains(f.exact_wire_type.as_str());
        let is_union = union_by_name.contains_key(f.exact_wire_type.as_str());
        // A bootstrap frame may appear inline as a field's exact type
        // (RootSlot.bootstrap: RootBootstrap at a pinned offset, §5.1) —
        // frames are schemas in the bootstrap identity class, not wire types.
        let is_inline_frame = bootstrap_names.contains(f.exact_wire_type.as_str());
        if !is_builtin && !is_wire && !is_ordinary_union && !is_union && !is_inline_frame {
            out.push(v(
                "field_unresolved_wire_type",
                "durable_fields",
                &row_id,
                format!("exact_wire_type {:?} resolves nowhere", f.exact_wire_type),
            ));
        }
        if let Some(wire_type) = wire_by_name.get(f.exact_wire_type.as_str())
            && !wire_type
                .allowed_containing_schemas
                .iter()
                .any(|schema| schema == "*" || schema == &f.containing_schema)
        {
            out.push(v(
                "wire_context_mismatch",
                "durable_fields",
                &row_id,
                format!(
                    "wire type {:?} is not permitted in containing schema {:?}",
                    f.exact_wire_type, f.containing_schema
                ),
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
        // Reference discipline. `external_root` is the distinct traversal
        // class for the bootstrap-slot root identity (Appendix A ~1435):
        // followed as strong by GC from OUTSIDE the object graph, legal only
        // inside a bootstrap frame — an in-graph object must use an ordinary
        // strong/conditional edge instead.
        let is_retaining = matches!(
            f.reference_semantics.as_str(),
            "strong" | "conditional" | "external_root"
        );
        if is_retaining {
            let in_frame = bootstrap_names.contains(f.containing_schema.as_str());
            if f.reference_semantics == "external_root" {
                if !in_frame {
                    out.push(v(
                        "external_root_outside_frame",
                        "durable_fields",
                        &row_id,
                        "external_root references are legal only inside bootstrap frames; in-graph objects use strong/conditional edges",
                    ));
                }
            } else if in_frame {
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
        let digest_typed = matches!(f.exact_wire_type.as_str(), "digest256" | "WeakDigest");
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
                    "target" | "weak_identity" => {
                        if !digest_typed {
                            out.push(v(
                                "bad_field",
                                "durable_fields",
                                &row_id,
                                "target/weak-identity digest classes require digest256 or WeakDigest wire types",
                            ));
                        }
                        if f.transcript_recipe.is_some()
                            || f.bd_domain_separator.is_some()
                            || f.bd_schema_major.is_some()
                            || f.bd_included_field_tags.is_some()
                            || f.bd_excluded_field_tags.is_some()
                            || f.recipe_pin.is_some()
                        {
                            out.push(v(
                                "bad_field",
                                "durable_fields",
                                &row_id,
                                "target/weak-identity digests may not carry transcript or BodyDigest recipe metadata",
                            ));
                        }
                    }
                    "transcript" => {
                        if !digest_typed && f.exact_wire_type != "u64" {
                            out.push(v(
                                "bad_field",
                                "durable_fields",
                                &row_id,
                                "transcript digest/checksum class requires digest256, WeakDigest, or an explicit u64 checksum wire type",
                            ));
                        }
                        if f.transcript_recipe.as_deref().is_none_or(|t| t.trim().is_empty()) {
                            out.push(v(
                                "digest_missing_recipe",
                                "durable_fields",
                                &row_id,
                                "transcript digest without a registered recipe",
                            ));
                        }
                        if f.bd_domain_separator.is_some()
                            || f.bd_schema_major.is_some()
                            || f.bd_included_field_tags.is_some()
                            || f.bd_excluded_field_tags.is_some()
                            || f.recipe_pin.is_some()
                        {
                            out.push(v(
                                "bad_field",
                                "durable_fields",
                                &row_id,
                                "transcript digest may not carry BodyDigest recipe metadata",
                            ));
                        }
                    }
                    "body" => {
                        if f.exact_wire_type != "digest256" || f.transcript_recipe.is_some() {
                            out.push(v(
                                "bad_field",
                                "durable_fields",
                                &row_id,
                                "BodyDigest must use digest256 and its generated BodyDigest metadata, not a transcript_recipe",
                            ));
                        }
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
            let included_set: BTreeSet<i64> = included.iter().copied().collect();
            let excluded_set: BTreeSet<i64> = excluded.iter().copied().collect();
            if included_set.len() != included.len()
                || excluded_set.len() != excluded.len()
                || !included_set.is_disjoint(&excluded_set)
                || excluded_set != BTreeSet::from([f.field_tag])
                || (!included_set.is_empty()
                    && included_set
                        .union(&excluded_set)
                        .copied()
                        .collect::<BTreeSet<_>>()
                        != known_tags)
            {
                out.push(v(
                    "bodydigest_incomplete_partition",
                    "durable_fields",
                    &row_id,
                    "BodyDigest include/exclude tags must be unique and disjoint; exclusions contain exactly the BodyDigest field, and an explicit include list must complete the schema partition",
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

    // --- ordinary closed tagged unions -------------------------------------
    let reference_union_names: BTreeSet<&str> =
        r.unions.iter().map(|u| u.union_name.as_str()).collect();
    let mut ordinary_union_names_seen = BTreeSet::new();
    let mut ordinary_union_paths: BTreeMap<(&str, &str), &str> = BTreeMap::new();
    for u in &r.ordinary_unions {
        let row_id = u.union_name.as_str();
        if u.union_name.trim().is_empty()
            || u.containing_schema.trim().is_empty()
            || u.union_path.trim().is_empty()
        {
            out.push(v(
                "bad_field",
                "durable_fields",
                row_id,
                "ordinary union name, containing schema, and union path must be nonblank",
            ));
        }
        let containing_schema_classes =
            usize::from(logical_by_name.contains_key(u.containing_schema.as_str()))
                + usize::from(physical_names.contains(u.containing_schema.as_str()))
                + usize::from(bootstrap_names.contains(u.containing_schema.as_str()))
                + usize::from(prebootstrap_names.contains(u.containing_schema.as_str()))
                + usize::from(wire_names.contains(u.containing_schema.as_str()));
        if containing_schema_classes != 1 {
            out.push(v(
                "ordinary_union_unresolved_schema",
                "durable_fields",
                row_id,
                format!(
                    "containing_schema {:?} resolves in {containing_schema_classes} identity classes; exactly one is required",
                    u.containing_schema
                ),
            ));
        }
        if !ordinary_union_names_seen.insert(u.union_name.as_str()) {
            out.push(v(
                "ordinary_union_name_collision",
                "durable_fields",
                row_id,
                "duplicate ordinary-union name",
            ));
        }
        let collides_with_reference = reference_union_names.contains(u.union_name.as_str());
        let collides_with_wire = BUILTIN_WIRE_TYPES.contains(&u.union_name.as_str())
            || wire_names.contains(u.union_name.as_str());
        if collides_with_reference {
            out.push(v(
                "ordinary_union_name_collision",
                "durable_fields",
                row_id,
                "ordinary-union name collides with a generated reference-union name",
            ));
        }
        if collides_with_wire {
            out.push(v(
                "ordinary_union_name_collision",
                "durable_fields",
                row_id,
                "ordinary-union name collides with a builtin or registered wire type",
            ));
        }
        if let Some(prior) = ordinary_union_paths.insert(
            (u.containing_schema.as_str(), u.union_path.as_str()),
            u.union_name.as_str(),
        ) {
            out.push(v(
                "ordinary_union_duplicate_path",
                "durable_fields",
                row_id,
                format!(
                    "ordinary-union path {:?} in containing schema {:?} is already assigned to {prior:?}",
                    u.union_path, u.containing_schema
                ),
            ));
        }
        if !matches!(u.tag_wire_type.as_str(), "u8" | "u16") {
            out.push(v(
                "bad_field",
                "durable_fields",
                row_id,
                format!("tag_wire_type {:?} is not one of u8|u16", u.tag_wire_type),
            ));
        }
        if u.encoding_context != "closed-tagged" {
            out.push(v(
                "bad_field",
                "durable_fields",
                row_id,
                format!(
                    "encoding_context {:?} must be the nonblank closed-tagged encoding",
                    u.encoding_context
                ),
            ));
        }
        check_ordinary_union_version_status(&u.version_status, row_id, &mut out);
        if u.role_predicate.trim().is_empty() || u.max_size_bytes <= 0 {
            out.push(v(
                "bad_field",
                "durable_fields",
                row_id,
                "ordinary union requires a nonblank role predicate and positive resource bound",
            ));
        }
        if u.arms.is_empty() {
            out.push(v(
                "ordinary_union_arm_missing",
                "durable_fields",
                row_id,
                "closed ordinary union has no registered arms",
            ));
        }

        let anchor_fields: Vec<_> = if collides_with_reference || collides_with_wire {
            Vec::new()
        } else {
            r.fields
                .iter()
                .filter(|field| field.exact_wire_type == u.union_name)
                .collect()
        };
        if let Some(field_tag) = u.field_tag {
            if field_tag <= 0 || field_tag >= 0xffff {
                out.push(v(
                    "code_invalid",
                    "durable_fields",
                    row_id,
                    format!("ordinary-union field_tag {field_tag:#06x} outside the valid space"),
                ));
            }
            match anchor_fields.iter().copied().find(|field| {
                field.containing_schema == u.containing_schema && field.field_tag == field_tag
            }) {
                Some(field) if anchor_fields.len() == 1 => {
                    if field.identity_class != "inline"
                        || field.reference_semantics != "none"
                        || field.target_schema_id.is_some()
                        || field.max_size_bytes < u.max_size_bytes
                        || field.version_status != u.version_status
                    {
                        out.push(v(
                            "ordinary_union_field_mismatch",
                            "durable_fields",
                            row_id,
                            "an embedded ordinary-union anchor must be inline, non-reference, target-free, large enough for the complete union encoding, and lifecycle-identical to its union",
                        ));
                    }
                }
                Some(_) => out.push(v(
                    "ordinary_union_field_mismatch",
                    "durable_fields",
                    row_id,
                    "an embedded ordinary union must have exactly one field anchor",
                )),
                None => out.push(v(
                    "ordinary_union_field_mismatch",
                    "durable_fields",
                    row_id,
                    format!(
                        "no field row ({}, tag {}) anchors ordinary union {:?}",
                        u.containing_schema, field_tag, u.union_name
                    ),
                )),
            }
        } else if !anchor_fields.is_empty() {
            out.push(v(
                "ordinary_union_field_mismatch",
                "durable_fields",
                row_id,
                "a top-level ordinary union without field_tag must not be used as an embedded field wire type",
            ));
        }

        let maximum_arm_tag = match u.tag_wire_type.as_str() {
            "u8" => Some(i64::from(u8::MAX)),
            // The upper quarter of the u16 space is reserved for experimental
            // assignments and cannot occur in a shipped production registry.
            "u16" => Some(0xbfff),
            _ => None,
        };
        let mut arm_tags = BTreeSet::new();
        let mut arm_names = BTreeSet::new();
        let mut source_arm_names = BTreeSet::new();
        for arm in &u.arms {
            let arm_row_id = format!("{}#{}", u.union_name, arm.stable_name);
            if arm.union_name != u.union_name
                || arm.containing_schema != u.containing_schema
                || arm.union_path != u.union_path
            {
                out.push(v(
                    "ordinary_union_arm_metadata_mismatch",
                    "durable_fields",
                    &arm_row_id,
                    "arm union name, containing schema, and union path must exactly match its ordinary union",
                ));
            }
            if arm.source_arm_name.trim().is_empty() || arm.stable_name.trim().is_empty() {
                out.push(v(
                    "bad_field",
                    "durable_fields",
                    &arm_row_id,
                    "ordinary-union source arm name and stable name must be nonblank",
                ));
            }
            if arm.role_predicate.trim().is_empty() || arm.max_size_bytes <= 0 {
                out.push(v(
                    "bad_field",
                    "durable_fields",
                    &arm_row_id,
                    "ordinary-union arm requires a nonblank role predicate and positive resource bound",
                ));
            }
            if arm.max_size_bytes > u.max_size_bytes {
                out.push(v(
                    "ordinary_union_arm_bound_exceeds_union",
                    "durable_fields",
                    &arm_row_id,
                    format!(
                        "arm max_size_bytes {} exceeds union max_size_bytes {}",
                        arm.max_size_bytes, u.max_size_bytes
                    ),
                ));
            }
            check_ordinary_union_version_status(&arm.version_status, &arm_row_id, &mut out);
            let lifecycle_is_coherent = match u.version_status.as_str() {
                "active" => matches!(
                    arm.version_status.as_str(),
                    "active" | "reserved" | "retired"
                ),
                "reserved" => arm.version_status == "reserved",
                "retired" => arm.version_status == "retired",
                _ => true,
            };
            if !lifecycle_is_coherent {
                out.push(v(
                    "ordinary_union_arm_lifecycle_mismatch",
                    "durable_fields",
                    &arm_row_id,
                    format!(
                        "arm lifecycle {:?} is incompatible with ordinary-union lifecycle {:?}",
                        arm.version_status, u.version_status
                    ),
                ));
            }
            if let Some(maximum_arm_tag) = maximum_arm_tag
                && (arm.arm_tag <= 0 || arm.arm_tag > maximum_arm_tag)
            {
                out.push(v(
                    "code_invalid",
                    "durable_fields",
                    &arm_row_id,
                    format!(
                        "ordinary-union arm tag {:#06x} is outside the positive production range for {}",
                        arm.arm_tag, u.tag_wire_type
                    ),
                ));
            }
            if !arm_tags.insert(arm.arm_tag) {
                out.push(v(
                    "ordinary_union_arm_duplicate_tag",
                    "durable_fields",
                    &arm_row_id,
                    format!("duplicate arm tag {}", arm.arm_tag),
                ));
            }
            if !arm_names.insert(arm.stable_name.as_str()) {
                out.push(v(
                    "ordinary_union_arm_duplicate_name",
                    "durable_fields",
                    &arm_row_id,
                    format!("duplicate stable arm name {:?}", arm.stable_name),
                ));
            }
            if !source_arm_names.insert(arm.source_arm_name.as_str()) {
                out.push(v(
                    "ordinary_union_arm_duplicate_source_name",
                    "durable_fields",
                    &arm_row_id,
                    format!("duplicate source arm token {:?}", arm.source_arm_name),
                ));
            }
            match (arm.payload_kind.as_str(), arm.payload_sha256.as_deref()) {
                ("unit", None) => {}
                ("inline-record", Some(payload_sha256))
                    if is_lowercase_sha256(payload_sha256) => {}
                ("unit", Some(_)) => out.push(v(
                    "ordinary_union_arm_payload_mismatch",
                    "durable_fields",
                    &arm_row_id,
                    "payload_kind=unit must not declare payload_sha256",
                )),
                ("inline-record", None) => out.push(v(
                    "ordinary_union_arm_payload_mismatch",
                    "durable_fields",
                    &arm_row_id,
                    "payload_kind=inline-record requires payload_sha256",
                )),
                ("inline-record", Some(_)) => out.push(v(
                    "ordinary_union_arm_payload_mismatch",
                    "durable_fields",
                    &arm_row_id,
                    "inline-record payload_sha256 must be exactly 64 lowercase hexadecimal characters",
                )),
                (payload_kind, _) => out.push(v(
                    "bad_field",
                    "durable_fields",
                    &arm_row_id,
                    format!(
                        "payload_kind {payload_kind:?} is not one of unit|inline-record"
                    ),
                )),
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
        let anchor = r.fields.iter().find(|f| {
            f.containing_schema == u.containing_schema
                && f.field_tag == u.field_tag
                && f.exact_wire_type == u.union_name
        });
        if anchor.is_none() {
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
        if !matches!(u.role.as_str(), "local" | "meta" | "shard") {
            out.push(v(
                "union_role_invalid",
                "durable_fields",
                &u.union_name,
                format!("role {:?} is not one of local|meta|shard", u.role),
            ));
        }
        if u.arms.is_empty() {
            out.push(v(
                "union_arm_missing",
                "durable_fields",
                &u.union_name,
                "closed reference union has no registered arms",
            ));
        }
        if let Some(containing) = logical_by_name.get(u.containing_schema.as_str())
            && !predicate_allows_role(&containing.role_predicate, &u.role)
        {
            out.push(v(
                "union_role_mismatch",
                "durable_fields",
                &u.union_name,
                format!(
                    "union role {:?} is excluded by containing schema predicate {:?}",
                    u.role, containing.role_predicate
                ),
            ));
        }
        if let Some(field) = anchor {
            if !matches!(field.reference_semantics.as_str(), "strong" | "conditional")
                || field.target_schema_id.is_some()
                || field.identity_class != "logical"
            {
                out.push(v(
                    "union_field_mismatch",
                    "durable_fields",
                    &u.union_name,
                    "union anchor must be a polymorphic logical strong/conditional reference",
                ));
            }
            if !predicate_allows_role(&field.role_predicate, &u.role) {
                out.push(v(
                    "union_role_mismatch",
                    "durable_fields",
                    &u.union_name,
                    format!(
                        "union role {:?} is excluded by anchor predicate {:?}",
                        u.role, field.role_predicate
                    ),
                ));
            }
        }
        let mut arm_tags = BTreeSet::new();
        let mut arm_targets = BTreeSet::new();
        for arm in &u.arms {
            let row_id = format!("{}#{}", u.union_name, arm.stable_name);
            if arm.union_name != u.union_name
                || arm.containing_schema != u.containing_schema
                || arm.field_tag != u.field_tag
                || arm.role != u.role
            {
                out.push(v(
                    "union_arm_metadata_mismatch",
                    "durable_fields",
                    &row_id,
                    "arm union/anchor/role metadata does not exactly match its reference_union",
                ));
            }
            if arm.stable_name != arm.target_schema_id {
                out.push(v(
                    "union_arm_metadata_mismatch",
                    "durable_fields",
                    &row_id,
                    "arm stable_name must equal its canonical target_schema_id",
                ));
            }
            if arm.identity_class != "logical"
                || !matches!(arm.reference_semantics.as_str(), "strong" | "conditional")
            {
                out.push(v(
                    "union_arm_identity_mismatch",
                    "durable_fields",
                    &row_id,
                    "reference-union arms must be retaining logical references",
                ));
            }
            if let Some(field) = anchor
                && (arm.reference_semantics != field.reference_semantics
                    || arm.version_status != field.version_status)
            {
                out.push(v(
                    "union_arm_lifecycle_mismatch",
                    "durable_fields",
                    &row_id,
                    "arm reference semantics and lifecycle must match the anchored field",
                ));
            }
            if !predicate_allows_role(&arm.role_predicate, &u.role)
                || arm.retention_and_cut_rule.trim().is_empty()
                || arm.max_size_bytes <= 0
            {
                out.push(v(
                    "union_arm_policy_mismatch",
                    "durable_fields",
                    &row_id,
                    "arm role predicate, retention rule, and resource bound must authorize its union role",
                ));
            }
            if arm.arm_tag <= 0 || arm.arm_tag >= 0xc000 {
                out.push(v(
                    "code_invalid",
                    "durable_fields",
                    &row_id,
                    format!(
                        "reference-union arm tag {:#06x} is not a production tag",
                        arm.arm_tag
                    ),
                ));
            }
            if !arm_tags.insert(arm.arm_tag) {
                out.push(v(
                    "union_arm_duplicate_tag",
                    "durable_fields",
                    &row_id,
                    format!("duplicate arm tag {}", arm.arm_tag),
                ));
            }
            if !arm_targets.insert(arm.target_schema_id.as_str()) {
                out.push(v(
                    "union_arm_duplicate_target",
                    "durable_fields",
                    &row_id,
                    format!("duplicate target {:?}", arm.target_schema_id),
                ));
            }
            match logical_by_name.get(arm.target_schema_id.as_str()) {
                None => out.push(v(
                    "union_arm_unresolved",
                    "durable_fields",
                    &row_id,
                    format!(
                        "arm {} target {:?} is not a registered logical object",
                        arm.arm_tag, arm.target_schema_id
                    ),
                )),
                Some(target_kind) => {
                    if matches!(target_kind.status.as_str(), "retired" | "experimental") {
                        out.push(v(
                            "union_arm_lifecycle_mismatch",
                            "durable_fields",
                            &row_id,
                            format!(
                                "arm target {:?} has non-referenceable lifecycle {:?}",
                                arm.target_schema_id, target_kind.status
                            ),
                        ));
                    }
                    if !predicate_allows_role(&target_kind.role_predicate, &u.role) {
                        out.push(v(
                            "union_role_mismatch",
                            "durable_fields",
                            &row_id,
                            format!(
                                "union role {:?} is excluded by target {:?} predicate {:?}",
                                u.role, arm.target_schema_id, target_kind.role_predicate
                            ),
                        ));
                    }
                    if let Some(containing) = logical_by_name.get(u.containing_schema.as_str())
                        && target_kind.construction_order > containing.construction_order
                    {
                        out.push(v(
                            "dag_future_result",
                            "durable_fields",
                            &u.union_name,
                            format!(
                                "arm target {:?} (order {}) is constructed after containing {:?} (order {}): a future result is never referenceable",
                                arm.target_schema_id,
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
        if !matches!(
            f.reference_semantics.as_str(),
            "strong" | "conditional" | "weak_digest"
        ) {
            continue;
        }
        let Some(containing) = logical_by_name.get(f.containing_schema.as_str()) else {
            continue;
        };
        let mut targets: Vec<&str> = Vec::new();
        if let Some(t) = &f.target_schema_id {
            targets.push(t.as_str());
        } else if let Some(u) = union_by_name.get(f.exact_wire_type.as_str()) {
            targets.extend(u.arms.iter().map(|arm| arm.target_schema_id.as_str()));
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

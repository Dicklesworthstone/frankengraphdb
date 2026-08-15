//! Fail-closed validator for the §5.1 durable state-slot constitution.
//!
//! `durable_state_slots.toml` owns the complete cross-plane semantics.  The
//! four per-plane `*_state_fields.toml` files are intentionally small identity
//! projections: duplicating type/reference/lifecycle recipes there would make
//! two authorities that could disagree.  Validation therefore joins three
//! independently authored surfaces:
//!
//! * every command-contract slot reference has exactly one cross-plane row;
//! * every cross-plane row's writer set equals the command-contract writers;
//! * every non-Bootstrap row has exactly one field in its plane projection.

use crate::command_contracts::{CONTRACT_ROLES, ContractRegistry, SLOT_PLANES};
use crate::toml::{get_int, get_str, get_str_array, get_table_array, parse};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub const SLOT_REGISTRY_PATH: &str = "registries/durable_state_slots.toml";
pub const RESERVED_SENTINEL: &str = "ReservedPendingActivation";

pub const SLOT_STATUSES: [&str; 3] = ["reserved", "active", "retired"];

pub const KNOWN_SLOT_KEYS: [&str; 18] = [
    "plane",
    "role",
    "slot_tag",
    "backing_registry",
    "stable_name",
    "exact_wire_type",
    "reference_semantics",
    "target_schema_id",
    "activation_predicate",
    "initial_value_recipe",
    "transition_writer_contract_ids",
    "checkpoint_or_snapshot_class",
    "floor_class",
    "backup_class",
    "restore_reconciliation_class",
    "gc_class",
    "role_transition_class",
    "status",
];

pub const KNOWN_BACKING_TOP_LEVEL_KEYS: [&str; 4] =
    ["schema_version", "registry_epoch", "plane", "field"];
pub const KNOWN_BACKING_FIELD_KEYS: [&str; 3] = ["role", "slot_tag", "status"];

pub const PLANE_BACKING_REGISTRIES: [(&str, &str); 5] = [
    ("SemanticPayload", "state_payload_fields.toml"),
    ("Protocol", "protocol_state_fields.toml"),
    ("PreparedOwnership", "prepared_state_fields.toml"),
    ("Consensus", "consensus_state_fields.toml"),
    ("Bootstrap", "bootstrap_frames.toml"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub plane: String,
    pub role: String,
    pub slot_tag: String,
    pub backing_registry: String,
    pub stable_name: String,
    pub exact_wire_type: String,
    pub reference_semantics: String,
    pub target_schema_id: String,
    pub activation_predicate: String,
    pub initial_value_recipe: String,
    pub transition_writer_contract_ids: Vec<String>,
    pub checkpoint_or_snapshot_class: String,
    pub floor_class: String,
    pub backup_class: String,
    pub restore_reconciliation_class: String,
    pub gc_class: String,
    pub role_transition_class: String,
    pub status: String,
}

impl Slot {
    pub fn slot_ref(&self) -> String {
        format!("{}|{}|{}", self.plane, self.role, self.slot_tag)
    }

    fn sentinel_fields(&self) -> [(&'static str, &str); 11] {
        [
            ("exact_wire_type", &self.exact_wire_type),
            ("reference_semantics", &self.reference_semantics),
            ("target_schema_id", &self.target_schema_id),
            ("activation_predicate", &self.activation_predicate),
            ("initial_value_recipe", &self.initial_value_recipe),
            (
                "checkpoint_or_snapshot_class",
                &self.checkpoint_or_snapshot_class,
            ),
            ("floor_class", &self.floor_class),
            ("backup_class", &self.backup_class),
            (
                "restore_reconciliation_class",
                &self.restore_reconciliation_class,
            ),
            ("gc_class", &self.gc_class),
            ("role_transition_class", &self.role_transition_class),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotRegistry {
    pub registry_epoch: i64,
    pub slots: Vec<Slot>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BackingField {
    pub role: String,
    pub slot_tag: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackingRegistry {
    pub schema_version: i64,
    pub registry_epoch: i64,
    pub plane: String,
    pub fields: Vec<BackingField>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    pub path: String,
    pub message: String,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub code: String,
    pub subject: String,
    pub message: String,
}

impl Violation {
    fn new(
        code: impl Into<String>,
        subject: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            subject: subject.into(),
            message: message.into(),
        }
    }
}

pub fn parse_slot_registry(text: &str) -> Result<SlotRegistry, String> {
    let table = parse(text).map_err(|error| error.to_string())?;
    for key in table.keys() {
        if key != "registry_epoch" && key != "slot" {
            return Err(format!(
                "durable_state_slots.toml: unknown top-level key {key:?}"
            ));
        }
    }
    let registry_epoch =
        get_int(&table, "registry_epoch", "durable_state_slots.toml").map_err(|e| e.to_string())?;
    let rows =
        get_table_array(&table, "slot", "durable_state_slots.toml").map_err(|e| e.to_string())?;
    let mut slots = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let ctx = format!("durable_state_slots.toml [[slot]] #{}", index + 1);
        for key in row.keys() {
            if !KNOWN_SLOT_KEYS.contains(&key.as_str()) {
                return Err(format!("{ctx}: unknown key {key:?}"));
            }
        }
        slots.push(Slot {
            plane: get_str(row, "plane", &ctx).map_err(|e| e.to_string())?,
            role: get_str(row, "role", &ctx).map_err(|e| e.to_string())?,
            slot_tag: get_str(row, "slot_tag", &ctx).map_err(|e| e.to_string())?,
            backing_registry: get_str(row, "backing_registry", &ctx).map_err(|e| e.to_string())?,
            stable_name: get_str(row, "stable_name", &ctx).map_err(|e| e.to_string())?,
            exact_wire_type: get_str(row, "exact_wire_type", &ctx).map_err(|e| e.to_string())?,
            reference_semantics: get_str(row, "reference_semantics", &ctx)
                .map_err(|e| e.to_string())?,
            target_schema_id: get_str(row, "target_schema_id", &ctx).map_err(|e| e.to_string())?,
            activation_predicate: get_str(row, "activation_predicate", &ctx)
                .map_err(|e| e.to_string())?,
            initial_value_recipe: get_str(row, "initial_value_recipe", &ctx)
                .map_err(|e| e.to_string())?,
            transition_writer_contract_ids: get_str_array(
                row,
                "transition_writer_contract_ids",
                &ctx,
            )
            .map_err(|e| e.to_string())?,
            checkpoint_or_snapshot_class: get_str(row, "checkpoint_or_snapshot_class", &ctx)
                .map_err(|e| e.to_string())?,
            floor_class: get_str(row, "floor_class", &ctx).map_err(|e| e.to_string())?,
            backup_class: get_str(row, "backup_class", &ctx).map_err(|e| e.to_string())?,
            restore_reconciliation_class: get_str(row, "restore_reconciliation_class", &ctx)
                .map_err(|e| e.to_string())?,
            gc_class: get_str(row, "gc_class", &ctx).map_err(|e| e.to_string())?,
            role_transition_class: get_str(row, "role_transition_class", &ctx)
                .map_err(|e| e.to_string())?,
            status: get_str(row, "status", &ctx).map_err(|e| e.to_string())?,
        });
    }
    Ok(SlotRegistry {
        registry_epoch,
        slots,
    })
}

pub fn parse_backing_registry(text: &str, file_name: &str) -> Result<BackingRegistry, String> {
    let table = parse(text).map_err(|error| error.to_string())?;
    for key in table.keys() {
        if !KNOWN_BACKING_TOP_LEVEL_KEYS.contains(&key.as_str()) {
            return Err(format!("{file_name}: unknown top-level key {key:?}"));
        }
    }
    let schema_version = get_int(&table, "schema_version", file_name).map_err(|e| e.to_string())?;
    let registry_epoch = get_int(&table, "registry_epoch", file_name).map_err(|e| e.to_string())?;
    let plane = get_str(&table, "plane", file_name).map_err(|e| e.to_string())?;
    let rows = get_table_array(&table, "field", file_name).map_err(|e| e.to_string())?;
    let mut fields = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let ctx = format!("{file_name} [[field]] #{}", index + 1);
        for key in row.keys() {
            if !KNOWN_BACKING_FIELD_KEYS.contains(&key.as_str()) {
                return Err(format!("{ctx}: unknown key {key:?}"));
            }
        }
        fields.push(BackingField {
            role: get_str(row, "role", &ctx).map_err(|e| e.to_string())?,
            slot_tag: get_str(row, "slot_tag", &ctx).map_err(|e| e.to_string())?,
            status: get_str(row, "status", &ctx).map_err(|e| e.to_string())?,
        });
    }
    Ok(BackingRegistry {
        schema_version,
        registry_epoch,
        plane,
        fields,
    })
}

fn read_parse<T>(
    path: &Path,
    parse_fn: impl FnOnce(&str) -> Result<T, String>,
) -> Result<T, LoadError> {
    let text = fs::read_to_string(path).map_err(|error| LoadError {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    parse_fn(&text).map_err(|message| LoadError {
        path: path.display().to_string(),
        message,
    })
}

pub fn load_slots(path: &Path) -> Result<SlotRegistry, LoadError> {
    read_parse(path, parse_slot_registry)
}

pub fn load_backing(path: &Path) -> Result<BackingRegistry, LoadError> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("backing registry");
    read_parse(path, |text| parse_backing_registry(text, file_name))
}

pub fn slot_registry_path(root: &Path) -> PathBuf {
    root.join(SLOT_REGISTRY_PATH)
}

pub fn load_from_repo(
    root: &Path,
) -> Result<(SlotRegistry, BTreeMap<String, BackingRegistry>), LoadError> {
    let slots = load_slots(&slot_registry_path(root))?;
    let mut backings = BTreeMap::new();
    for (plane, file_name) in PLANE_BACKING_REGISTRIES {
        let path = root.join("registries").join(file_name);
        if plane == "Bootstrap" {
            fs::metadata(&path).map_err(|error| LoadError {
                path: path.display().to_string(),
                message: error.to_string(),
            })?;
        } else {
            backings.insert(file_name.to_string(), load_backing(&path)?);
        }
    }
    Ok((slots, backings))
}

fn expected_backing(plane: &str) -> Option<&'static str> {
    PLANE_BACKING_REGISTRIES
        .iter()
        .find_map(|(candidate, file)| (*candidate == plane).then_some(*file))
}

fn parse_slot_ref(value: &str) -> Option<(&str, &str, &str)> {
    let mut parts = value.split('|');
    let result = (parts.next()?, parts.next()?, parts.next()?);
    parts.next().is_none().then_some(result)
}

pub fn validate(
    slots: &SlotRegistry,
    backings: &BTreeMap<String, BackingRegistry>,
    contracts: &ContractRegistry,
) -> Vec<Violation> {
    let mut out = Vec::new();
    if slots.registry_epoch < 1 {
        out.push(Violation::new(
            "slot_registry_epoch_invalid",
            SLOT_REGISTRY_PATH,
            "registry_epoch must be a positive integer",
        ));
    }

    let mut declared_refs = BTreeSet::new();
    let mut expected_writers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for contract in &contracts.contracts {
        for value in contract
            .consumed_state_slots
            .iter()
            .chain(contract.written_state_slots.iter())
        {
            if parse_slot_ref(value).is_some() {
                declared_refs.insert(value.clone());
            }
        }
        for value in &contract.written_state_slots {
            if parse_slot_ref(value).is_some() {
                expected_writers
                    .entry(value.clone())
                    .or_default()
                    .insert(contract.command_contract_id.clone());
            }
        }
    }

    let mut row_refs = BTreeSet::new();
    let mut projected: BTreeMap<String, BTreeSet<BackingField>> = BTreeMap::new();
    for slot in &slots.slots {
        let slot_ref = slot.slot_ref();
        if !row_refs.insert(slot_ref.clone()) {
            out.push(Violation::new(
                "slot_ref_duplicate",
                &slot_ref,
                "plane|role|slot_tag is declared more than once",
            ));
        }
        if !SLOT_PLANES.contains(&slot.plane.as_str()) {
            out.push(Violation::new(
                "slot_plane_invalid",
                &slot_ref,
                format!("plane {:?} is outside the closed vocabulary", slot.plane),
            ));
        }
        if !CONTRACT_ROLES.contains(&slot.role.as_str()) {
            out.push(Violation::new(
                "slot_role_invalid",
                &slot_ref,
                format!("role {:?} is outside Local|Meta|Shard", slot.role),
            ));
        }
        if slot.slot_tag.is_empty() {
            out.push(Violation::new(
                "slot_tag_empty",
                &slot_ref,
                "slot_tag must not be empty",
            ));
        }
        if slot.status == "reserved" && slot.stable_name != slot.slot_tag {
            out.push(Violation::new(
                "slot_stable_name_mismatch",
                &slot_ref,
                format!(
                    "reserved slot stable_name {:?} must equal slot_tag {:?}",
                    slot.stable_name, slot.slot_tag
                ),
            ));
        }
        match expected_backing(&slot.plane) {
            Some(expected) if slot.backing_registry != expected => out.push(Violation::new(
                "slot_backing_registry_mismatch",
                &slot_ref,
                format!(
                    "plane {} must use {expected}, not {:?}",
                    slot.plane, slot.backing_registry
                ),
            )),
            None => {}
            Some(_) => {}
        }
        if !SLOT_STATUSES.contains(&slot.status.as_str()) {
            out.push(Violation::new(
                "slot_status_invalid",
                &slot_ref,
                format!(
                    "status {:?} is outside reserved|active|retired",
                    slot.status
                ),
            ));
        }
        if slot.status == "active" {
            for (key, value) in slot.sentinel_fields() {
                if value == RESERVED_SENTINEL {
                    out.push(Violation::new(
                        "slot_active_with_reserved_sentinel",
                        &slot_ref,
                        format!("active row retains {RESERVED_SENTINEL} in {key}"),
                    ));
                }
            }
            if slot.transition_writer_contract_ids.is_empty() {
                out.push(Violation::new(
                    "slot_active_without_writer",
                    &slot_ref,
                    "every active slot must name at least one legal transition writer",
                ));
            }
        }

        let writers: BTreeSet<String> = slot
            .transition_writer_contract_ids
            .iter()
            .cloned()
            .collect();
        let canonical_writers: Vec<String> = writers.iter().cloned().collect();
        if canonical_writers != slot.transition_writer_contract_ids {
            out.push(Violation::new(
                "slot_writer_set_noncanonical",
                &slot_ref,
                "transition_writer_contract_ids must be sorted and duplicate-free",
            ));
        }
        let derived = expected_writers.get(&slot_ref).cloned().unwrap_or_default();
        if writers != derived {
            out.push(Violation::new(
                "slot_writer_set_mismatch",
                &slot_ref,
                format!(
                    "declared writers {:?} do not equal command-contract writers {:?}",
                    writers, derived
                ),
            ));
        }

        if slot.plane != "Bootstrap" {
            projected
                .entry(slot.backing_registry.clone())
                .or_default()
                .insert(BackingField {
                    role: slot.role.clone(),
                    slot_tag: slot.slot_tag.clone(),
                    status: slot.status.clone(),
                });
        }
    }

    for missing in declared_refs.difference(&row_refs) {
        out.push(Violation::new(
            "contract_slot_ref_missing_row",
            missing,
            "command contract references a durable slot with no cross-plane row",
        ));
    }
    for extra in row_refs.difference(&declared_refs) {
        out.push(Violation::new(
            "slot_row_without_contract_ref",
            extra,
            "cross-plane slot row is not consumed or written by any command contract",
        ));
    }

    for (plane, file_name) in PLANE_BACKING_REGISTRIES {
        if plane == "Bootstrap" {
            continue;
        }
        let Some(backing) = backings.get(file_name) else {
            out.push(Violation::new(
                "slot_backing_registry_missing",
                file_name,
                "required per-plane backing registry was not loaded",
            ));
            continue;
        };
        if backing.schema_version != 1 {
            out.push(Violation::new(
                "slot_backing_schema_version_invalid",
                file_name,
                format!("schema_version must be 1, got {}", backing.schema_version),
            ));
        }
        if backing.registry_epoch < 1 {
            out.push(Violation::new(
                "slot_backing_registry_epoch_invalid",
                file_name,
                "registry_epoch must be a positive integer",
            ));
        }
        if backing.plane != plane {
            out.push(Violation::new(
                "slot_backing_plane_mismatch",
                file_name,
                format!("file must declare plane {plane:?}, got {:?}", backing.plane),
            ));
        }
        let mut actual = BTreeSet::new();
        for field in &backing.fields {
            let subject = format!("{}|{}|{}", backing.plane, field.role, field.slot_tag);
            if !CONTRACT_ROLES.contains(&field.role.as_str()) {
                out.push(Violation::new(
                    "slot_backing_role_invalid",
                    &subject,
                    format!("role {:?} is outside Local|Meta|Shard", field.role),
                ));
            }
            if field.slot_tag.is_empty() {
                out.push(Violation::new(
                    "slot_backing_tag_empty",
                    &subject,
                    "slot_tag must not be empty",
                ));
            }
            if !SLOT_STATUSES.contains(&field.status.as_str()) {
                out.push(Violation::new(
                    "slot_backing_status_invalid",
                    &subject,
                    format!(
                        "status {:?} is outside reserved|active|retired",
                        field.status
                    ),
                ));
            }
            if !actual.insert(field.clone()) {
                out.push(Violation::new(
                    "slot_backing_field_duplicate",
                    &subject,
                    "role|slot_tag|status is declared more than once",
                ));
            }
        }
        let expected = projected.get(file_name).cloned().unwrap_or_default();
        for missing in expected.difference(&actual) {
            out.push(Violation::new(
                "slot_backing_field_missing",
                format!("{plane}|{}|{}", missing.role, missing.slot_tag),
                format!("{file_name} lacks the exact projected field/status row"),
            ));
        }
        for extra in actual.difference(&expected) {
            out.push(Violation::new(
                "slot_backing_field_extra",
                format!("{plane}|{}|{}", extra.role, extra.slot_tag),
                format!(
                    "{file_name} contains a field/status row absent from the cross-plane registry"
                ),
            ));
        }
    }

    out
}

pub fn validate_repo(
    root: &Path,
    contracts: &ContractRegistry,
) -> Result<Vec<Violation>, LoadError> {
    let (slots, backings) = load_from_repo(root)?;
    Ok(validate(&slots, &backings, contracts))
}

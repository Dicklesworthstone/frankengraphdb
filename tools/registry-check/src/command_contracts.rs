//! The role-valid command-arm contract registry
//! (`registries/command_contracts.toml`) and its validator.
//!
//! Bead: fgdb-5uw2.
//!
//! WHAT THIS FILE IS FOR. Plan §5.1 (line 294) makes this registry the sole
//! normative source for ordered-transition command contracts — "hand-written
//! lists in this document are generated readability projections and are never
//! a second source of truth" — and line 296 binds it into a G0-enforced
//! two-way total bijection over LIVE rows and INHABITABLE command-union arms.
//! At creation the registry carries zero rows because zero arms are
//! inhabitable and zero handlers exist; both quantification domains are empty
//! and the bijection holds vacuously. The registry file's header records why,
//! and the fgdb-5uw2 Phase B derivation owns the full family expansion that
//! must precede any tag-minting row.
//!
//! WHY UNKNOWN KEYS ARE REJECTED. Same law as `laws.rs`: a row carrying a
//! field this reader does not understand has not been understood, and the
//! reader fails closed before reading anything else.
//!
//! WHY TAG BOUNDS ARE CHECKED HERE. Plan line 290: each registry has a 16-bit
//! code space with `0x0000` and `0xffff` permanently invalid, and a released
//! code is never reassigned. A row that lands outside the space is a durable
//! defect the moment it is released, so the validator refuses it on entry.

use crate::toml::{get_int, get_opt_str, get_str, get_str_array, get_table_array, parse};
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub const REGISTRY_PATH: &str = "registries/command_contracts.toml";

/// Every key a `[[contract]]` row may carry — the exact §5.1 schema (plan line
/// 294). A key outside this set is a load error, not a shrug.
pub const KNOWN_CONTRACT_KEYS: [&str; 28] = [
    "command_contract_id",
    "role",
    "outer_command_union",
    "outer_wire_tag",
    "input_schema_id",
    "input_wire_tag",
    "inner_wire_tag",
    "body_schema_id",
    "result_schema_id",
    "applied_record_schema_id",
    "handler_symbol",
    "transition_class",
    "sequence_effects",
    "expected_state_schema_id",
    "authority_arm",
    "authority_evidence_target_schema_id",
    "terminal_audit_freeze_arm",
    "terminal_audit_gate_arm",
    "payload_availability_rule",
    "publication_mode",
    "construction_dag_recipe_id",
    "consumed_state_slots",
    "written_state_slots",
    "checkpoint_floor_classes",
    "backup_restore_gc_classes",
    "posture_feature_predicate",
    "format_epoch_range",
    "status",
];

/// The closed role vocabulary (plan line 294: `role:Local|Meta|Shard`).
pub const CONTRACT_ROLES: [&str; 3] = ["Local", "Meta", "Shard"];

/// The closed transition-class vocabulary (plan line 294).
pub const TRANSITION_CLASSES: [&str; 3] = ["Semantic", "Maintenance", "Scaffolding"];

/// The closed lifecycle vocabulary. `live` is what the plan-line-296 bijection
/// quantifies over; a row is `reserved` until its union arm, canonical input,
/// body/result/applied-record triple, and Rust apply handler all exist.
pub const CONTRACT_STATUSES: [&str; 3] = ["reserved", "live", "retired"];

/// The closed `DurableStateSlotRef` plane vocabulary (plan line 294).
pub const SLOT_PLANES: [&str; 5] = [
    "SemanticPayload",
    "Protocol",
    "PreparedOwnership",
    "Consensus",
    "Bootstrap",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contract {
    pub command_contract_id: String,
    pub role: String,
    pub outer_command_union: String,
    pub outer_wire_tag: i64,
    pub input_schema_id: String,
    pub input_wire_tag: i64,
    pub inner_wire_tag: Option<i64>,
    pub body_schema_id: String,
    pub result_schema_id: String,
    pub applied_record_schema_id: String,
    pub handler_symbol: String,
    pub transition_class: String,
    pub sequence_effects: String,
    pub expected_state_schema_id: String,
    pub authority_arm: String,
    pub authority_evidence_target_schema_id: Option<String>,
    pub terminal_audit_freeze_arm: String,
    pub terminal_audit_gate_arm: String,
    pub payload_availability_rule: String,
    pub publication_mode: String,
    pub construction_dag_recipe_id: String,
    pub consumed_state_slots: Vec<String>,
    pub written_state_slots: Vec<String>,
    pub checkpoint_floor_classes: Vec<String>,
    pub backup_restore_gc_classes: Vec<String>,
    pub posture_feature_predicate: String,
    pub format_epoch_range: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContractRegistry {
    pub registry_epoch: i64,
    pub contracts: Vec<Contract>,
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
        Violation {
            code: code.into(),
            subject: subject.into(),
            message: message.into(),
        }
    }
}

fn parse_contracts(text: &str) -> Result<ContractRegistry, String> {
    let table = parse(text).map_err(|error| error.to_string())?;
    let registry_epoch =
        get_int(&table, "registry_epoch", "command_contracts.toml").map_err(|e| e.to_string())?;
    let rows = get_table_array(&table, "contract", "command_contracts.toml")
        .map_err(|error| error.to_string())?;
    let mut contracts = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let ctx = format!("command_contracts.toml [[contract]] #{}", index + 1);
        // Fail closed on an unknown key BEFORE reading anything.
        for key in row.keys() {
            if !KNOWN_CONTRACT_KEYS.contains(&key.as_str()) {
                return Err(format!("{ctx}: unknown key {key:?}"));
            }
        }
        let inner_wire_tag = if row.contains_key("inner_wire_tag") {
            Some(get_int(row, "inner_wire_tag", &ctx).map_err(|e| e.to_string())?)
        } else {
            None
        };
        contracts.push(Contract {
            command_contract_id: get_str(row, "command_contract_id", &ctx)
                .map_err(|e| e.to_string())?,
            role: get_str(row, "role", &ctx).map_err(|e| e.to_string())?,
            outer_command_union: get_str(row, "outer_command_union", &ctx)
                .map_err(|e| e.to_string())?,
            outer_wire_tag: get_int(row, "outer_wire_tag", &ctx).map_err(|e| e.to_string())?,
            input_schema_id: get_str(row, "input_schema_id", &ctx).map_err(|e| e.to_string())?,
            input_wire_tag: get_int(row, "input_wire_tag", &ctx).map_err(|e| e.to_string())?,
            inner_wire_tag,
            body_schema_id: get_str(row, "body_schema_id", &ctx).map_err(|e| e.to_string())?,
            result_schema_id: get_str(row, "result_schema_id", &ctx).map_err(|e| e.to_string())?,
            applied_record_schema_id: get_str(row, "applied_record_schema_id", &ctx)
                .map_err(|e| e.to_string())?,
            handler_symbol: get_str(row, "handler_symbol", &ctx).map_err(|e| e.to_string())?,
            transition_class: get_str(row, "transition_class", &ctx).map_err(|e| e.to_string())?,
            sequence_effects: get_str(row, "sequence_effects", &ctx).map_err(|e| e.to_string())?,
            expected_state_schema_id: get_str(row, "expected_state_schema_id", &ctx)
                .map_err(|e| e.to_string())?,
            authority_arm: get_str(row, "authority_arm", &ctx).map_err(|e| e.to_string())?,
            authority_evidence_target_schema_id: get_opt_str(
                row,
                "authority_evidence_target_schema_id",
                &ctx,
            )
            .map_err(|e| e.to_string())?,
            terminal_audit_freeze_arm: get_str(row, "terminal_audit_freeze_arm", &ctx)
                .map_err(|e| e.to_string())?,
            terminal_audit_gate_arm: get_str(row, "terminal_audit_gate_arm", &ctx)
                .map_err(|e| e.to_string())?,
            payload_availability_rule: get_str(row, "payload_availability_rule", &ctx)
                .map_err(|e| e.to_string())?,
            publication_mode: get_str(row, "publication_mode", &ctx).map_err(|e| e.to_string())?,
            construction_dag_recipe_id: get_str(row, "construction_dag_recipe_id", &ctx)
                .map_err(|e| e.to_string())?,
            consumed_state_slots: get_str_array(row, "consumed_state_slots", &ctx)
                .map_err(|e| e.to_string())?,
            written_state_slots: get_str_array(row, "written_state_slots", &ctx)
                .map_err(|e| e.to_string())?,
            checkpoint_floor_classes: get_str_array(row, "checkpoint_floor_classes", &ctx)
                .map_err(|e| e.to_string())?,
            backup_restore_gc_classes: get_str_array(row, "backup_restore_gc_classes", &ctx)
                .map_err(|e| e.to_string())?,
            posture_feature_predicate: get_str(row, "posture_feature_predicate", &ctx)
                .map_err(|e| e.to_string())?,
            format_epoch_range: get_str(row, "format_epoch_range", &ctx)
                .map_err(|e| e.to_string())?,
            status: get_str(row, "status", &ctx).map_err(|e| e.to_string())?,
        });
    }
    Ok(ContractRegistry {
        registry_epoch,
        contracts,
    })
}

pub fn load_contracts(path: &Path) -> Result<ContractRegistry, LoadError> {
    let text = fs::read_to_string(path).map_err(|error| LoadError {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    parse_contracts(&text).map_err(|message| LoadError {
        path: path.display().to_string(),
        message,
    })
}

pub fn load_from_repo(root: &Path) -> Result<ContractRegistry, LoadError> {
    load_contracts(&root.join(REGISTRY_PATH))
}

pub fn registry_path(root: &Path) -> PathBuf {
    root.join(REGISTRY_PATH)
}

/// `0x0000` and `0xffff` are permanently invalid in every registry code space
/// (plan line 290); everything else in u16 range is assignable.
fn wire_tag_in_space(tag: i64) -> bool {
    tag > 0x0000 && tag < 0xffff
}

/// A `DurableStateSlotRef` is written `plane|role|slot_tag` with the closed
/// plane vocabulary and a role from the contract-role vocabulary.
fn slot_ref_is_wellformed(slot: &str) -> bool {
    let mut parts = slot.split('|');
    let (Some(plane), Some(role), Some(slot_tag), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    SLOT_PLANES.contains(&plane) && CONTRACT_ROLES.contains(&role) && !slot_tag.is_empty()
}

pub fn validate_contracts(registry: &ContractRegistry) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut seen_ids: BTreeSet<&str> = BTreeSet::new();
    let mut seen_arm_slots: BTreeSet<(&str, &str, i64, Option<i64>)> = BTreeSet::new();
    // Per (role, union, outer_tag): (armless row seen, armed row seen). One
    // outer tag is either one armless command or one armed family, never both
    // (plan line 294: a family "never hides an open subcommand").
    let mut outer_modes: std::collections::BTreeMap<(&str, &str, i64), (bool, bool)> =
        std::collections::BTreeMap::new();

    if registry.registry_epoch < 1 {
        out.push(Violation::new(
            "contract_registry_epoch_invalid",
            REGISTRY_PATH,
            "registry_epoch must be a positive integer",
        ));
    }

    // NOTE: an empty contract list is NOT a violation. Plan line 296's
    // bijection quantifies over live rows and inhabitable arms; at registry
    // creation both domains are measured empty, and the registry exists so G0
    // has the artifact to enforce against as rows land (fgdb-5uw2 Phase B).

    for contract in &registry.contracts {
        let id = contract.command_contract_id.as_str();
        if id.trim().is_empty() {
            out.push(Violation::new(
                "contract_id_empty",
                REGISTRY_PATH,
                "a contract row with no command_contract_id cannot be referenced",
            ));
        } else if !seen_ids.insert(id) {
            out.push(Violation::new(
                "contract_id_duplicate",
                id,
                format!("command_contract_id {id:?} is declared more than once"),
            ));
        }
        if !CONTRACT_ROLES.contains(&contract.role.as_str()) {
            out.push(Violation::new(
                "contract_role_invalid",
                id,
                format!("role {:?} is not one of Local|Meta|Shard", contract.role),
            ));
        }
        if !TRANSITION_CLASSES.contains(&contract.transition_class.as_str()) {
            out.push(Violation::new(
                "contract_transition_class_invalid",
                id,
                format!(
                    "transition_class {:?} is not one of Semantic|Maintenance|Scaffolding",
                    contract.transition_class
                ),
            ));
        }
        if !CONTRACT_STATUSES.contains(&contract.status.as_str()) {
            out.push(Violation::new(
                "contract_status_invalid",
                id,
                format!(
                    "status {:?} is not one of reserved|live|retired",
                    contract.status
                ),
            ));
        }
        for (label, tag) in [
            ("outer_wire_tag", Some(contract.outer_wire_tag)),
            ("input_wire_tag", Some(contract.input_wire_tag)),
            ("inner_wire_tag", contract.inner_wire_tag),
        ] {
            if let Some(tag) = tag
                && !wire_tag_in_space(tag)
            {
                out.push(Violation::new(
                    "contract_wire_tag_out_of_space",
                    id,
                    format!(
                        "{label} {tag:#06x} is outside the assignable space; 0x0000 and 0xffff are permanently invalid"
                    ),
                ));
            }
        }
        // Rows are per concrete inner tag (plan line 294): an armed member's
        // rows share the member's outer tag and differ by inner_wire_tag, so
        // the duplicate key includes the inner tag. Sharing an outer tag with
        // the SAME inner tag (or with none on both) encodes a second command
        // under one tag.
        if !seen_arm_slots.insert((
            contract.role.as_str(),
            contract.outer_command_union.as_str(),
            contract.outer_wire_tag,
            contract.inner_wire_tag,
        )) {
            out.push(Violation::new(
                "contract_arm_slot_duplicate",
                id,
                format!(
                    "(role, outer_command_union, outer_wire_tag, inner_wire_tag) = ({:?}, {:?}, {:#06x}, {:?}) is claimed by more than one row; duplicate tags encode a second command",
                    contract.role,
                    contract.outer_command_union,
                    contract.outer_wire_tag,
                    contract.inner_wire_tag
                ),
            ));
        }
        let modes = outer_modes
            .entry((
                contract.role.as_str(),
                contract.outer_command_union.as_str(),
                contract.outer_wire_tag,
            ))
            .or_insert((false, false));
        if contract.inner_wire_tag.is_some() {
            modes.1 = true;
        } else {
            modes.0 = true;
        }
        if modes.0 && modes.1 {
            out.push(Violation::new(
                "contract_arm_slot_duplicate",
                id,
                format!(
                    "outer tag {:#06x} carries both an armless row and an armed family; one outer tag is one command or one family, never both",
                    contract.outer_wire_tag
                ),
            ));
        }
        for slot in contract
            .consumed_state_slots
            .iter()
            .chain(contract.written_state_slots.iter())
        {
            if !slot_ref_is_wellformed(slot) {
                out.push(Violation::new(
                    "contract_state_slot_malformed",
                    id,
                    format!(
                        "state slot {slot:?} is not plane|role|slot_tag with a registered plane and role"
                    ),
                ));
            }
        }
        if contract.status == "live" {
            // The plan-line-296 bijection quantifies over live rows: a live row
            // must resolve to one union arm, one input, one triple, one handler.
            // Those resolutions need enumerators that do not exist yet, so a
            // live row is refused outright until they do — an honest UNRUN is
            // not available inside a validator, and green-over-unchecked is the
            // exact failure this registry exists to prevent.
            out.push(Violation::new(
                "contract_live_row_unverifiable",
                id,
                "a live row requires the arm/input/triple/handler bijection checks, which are not yet buildable; land the row as reserved (fgdb-5uw2 Phase B)",
            ));
        }
    }

    out
}

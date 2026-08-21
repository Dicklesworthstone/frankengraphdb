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
pub const LIVE_HANDLER_SOURCE_PATH: &str = "crates/fgdb/src/lib.rs";
const LIVE_HANDLER_INVENTORY_NAME: &str = "LIVE_LOCAL_SEMANTIC_HANDLER_INVENTORY";

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveHandlerDeclaration {
    contract_id: String,
    handler_symbol: String,
    union_arm: String,
}

fn live_handler_declarations(source: &str) -> Result<Vec<LiveHandlerDeclaration>, String> {
    let start = source
        .find(LIVE_HANDLER_INVENTORY_NAME)
        .ok_or_else(|| format!("missing {LIVE_HANDLER_INVENTORY_NAME}"))?;
    let tail = &source[start..];
    let end = tail
        .find("];" )
        .ok_or_else(|| format!("unterminated {LIVE_HANDLER_INVENTORY_NAME}"))?;
    let quoted: Vec<String> = tail[..end]
        .split('"')
        .enumerate()
        .filter_map(|(index, part)| (index % 2 == 1).then(|| part.to_owned()))
        .collect();
    if !quoted.len().is_multiple_of(3) {
        return Err(format!(
            "{LIVE_HANDLER_INVENTORY_NAME} must contain contract-id, handler-symbol, union-arm triples"
        ));
    }
    Ok(quoted
        .chunks_exact(3)
        .map(|fields| LiveHandlerDeclaration {
            contract_id: fields[0].clone(),
            handler_symbol: fields[1].clone(),
            union_arm: fields[2].clone(),
        })
        .collect())
}

fn source_declares_type(source: &str, type_name: &str) -> bool {
    ["struct", "enum"]
        .iter()
        .any(|kind| source.contains(&format!("pub {kind} {type_name}")))
}

/// Enforce the live-row/inhabitable-handler bijection against the Rust source.
///
/// The source inventory is next to the exhaustive command dispatcher.  This
/// makes both directions checkable: deleting a handler leaves a live row with
/// no declaration, while adding a handler declaration without a live row is
/// independently red.
pub fn validate_live_handler_source(
    registry: &ContractRegistry,
    source: &str,
) -> Vec<Violation> {
    let mut out = Vec::new();
    let declarations = match live_handler_declarations(source) {
        Ok(declarations) => declarations,
        Err(message) => {
            out.push(Violation::new(
                "contract_handler_inventory_invalid",
                LIVE_HANDLER_SOURCE_PATH,
                message,
            ));
            return out;
        }
    };
    let mut seen = BTreeSet::new();
    for declaration in &declarations {
        if !seen.insert(declaration.contract_id.as_str()) {
            out.push(Violation::new(
                "contract_handler_duplicate",
                &declaration.contract_id,
                "one contract id has more than one live handler declaration",
            ));
        }
        let Some(row) = registry.contracts.iter().find(|row| {
            row.status == "live" && row.command_contract_id == declaration.contract_id
        }) else {
            out.push(Violation::new(
                "contract_handler_row_missing",
                &declaration.contract_id,
                "a declared live handler has no matching live command-contract row",
            ));
            continue;
        };
        if row.handler_symbol != declaration.handler_symbol {
            out.push(Violation::new(
                "contract_handler_symbol_mismatch",
                &declaration.contract_id,
                format!(
                    "registry handler {:?} differs from source declaration {:?}",
                    row.handler_symbol, declaration.handler_symbol
                ),
            ));
        }
    }

    for row in registry.contracts.iter().filter(|row| row.status == "live") {
        let Some(declaration) = declarations
            .iter()
            .find(|declaration| declaration.contract_id == row.command_contract_id)
        else {
            out.push(Violation::new(
                "contract_live_handler_missing",
                &row.command_contract_id,
                "live command-contract row has no source handler declaration",
            ));
            continue;
        };
        let handler_name = declaration
            .handler_symbol
            .rsplit("::")
            .next()
            .unwrap_or_default();
        if !source.contains(&format!("fn {handler_name}(")) {
            out.push(Violation::new(
                "contract_live_handler_missing",
                &row.command_contract_id,
                format!("declared handler function {handler_name:?} is absent"),
            ));
        }
        if !source.contains(&format!("pub enum {}", row.outer_command_union))
            || !source.contains(&format!("{}(", declaration.union_arm))
            || !source.contains(&format!(
                "{}::{}",
                row.outer_command_union, declaration.union_arm
            ))
        {
            out.push(Violation::new(
                "contract_live_arm_missing",
                &row.command_contract_id,
                "live row does not resolve to an inhabitable union arm and exhaustive apply match",
            ));
        }
        for type_name in [
            row.body_schema_id.as_str(),
            row.result_schema_id.as_str(),
            row.applied_record_schema_id.as_str(),
        ] {
            if !source_declares_type(source, type_name) {
                out.push(Violation::new(
                    "contract_live_type_missing",
                    &row.command_contract_id,
                    format!("live body/result/applied-record type {type_name:?} is absent"),
                ));
            }
        }
    }
    out
}

pub fn validate_live_handlers_from_repo(
    root: &Path,
    registry: &ContractRegistry,
) -> Vec<Violation> {
    match fs::read_to_string(root.join(LIVE_HANDLER_SOURCE_PATH)) {
        Ok(source) => validate_live_handler_source(registry, &source),
        Err(error) => vec![Violation::new(
            "contract_handler_source_unreadable",
            LIVE_HANDLER_SOURCE_PATH,
            error.to_string(),
        )],
    }
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
    }

    out
}

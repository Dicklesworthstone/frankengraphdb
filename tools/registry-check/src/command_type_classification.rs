//! The input-looking type classification registry
//! (`registries/command_type_classification.toml`) and its validator.
//!
//! Bead: fgdb-5uw2.
//!
//! WHAT THIS FILE IS FOR. Plan §5.1 (line 294) pairs this registry with
//! `command_contracts.toml`: every independently addressable normative
//! input-looking type — including every type whose stable name ends in `Spec`
//! — carries exactly one of six classes, and "a suffix conveys no semantics".
//! Plan line 296 adds the coupling laws this validator holds: every
//! `RegisteredCommandInput` row names its contract, a type is never classified
//! as its own future result, and classification is total over the addressable
//! input-looking population (totality is enforced by the census-side test
//! layer as the population enumerator lands; the validator holds the row-local
//! laws).
//!
//! WHY `source_location` IS REQUIRED. Same law as `laws.rs`: the anchor is
//! what makes a classification falsifiable — a reader opens the cited plan
//! line and checks that the class is forced there, not chosen.

use crate::command_contracts::ContractRegistry;
use crate::toml::{get_int, get_opt_str, get_str, get_table_array, parse};
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub const REGISTRY_PATH: &str = "registries/command_type_classification.toml";

/// Every key a `[[classification]]` row may carry. A key outside this set is a
/// load error, not a shrug.
pub const KNOWN_CLASSIFICATION_KEYS: [&str; 6] = [
    "type_name",
    "class",
    "command_contract_id",
    "source_location",
    "statement",
    "status",
];

/// The closed class vocabulary — verbatim from plan §5.1 line 294.
pub const CLASSIFICATION_CLASSES: [&str; 6] = [
    "RegisteredCommandInput",
    "PreOrderArtifact",
    "CertificateInput",
    "ResultOnly",
    "ProjectionInput",
    "RuntimeOnly",
];

/// The closed status vocabulary. Only `registered` exists today; a future
/// state must widen this list in the same commit that first uses it.
pub const CLASSIFICATION_STATUSES: [&str; 1] = ["registered"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub type_name: String,
    pub class: String,
    pub command_contract_id: Option<String>,
    pub source_location: String,
    pub statement: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationRegistry {
    pub registry_epoch: i64,
    pub classifications: Vec<Classification>,
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

fn parse_classifications(text: &str) -> Result<ClassificationRegistry, String> {
    let table = parse(text).map_err(|error| error.to_string())?;
    let registry_epoch = get_int(&table, "registry_epoch", "command_type_classification.toml")
        .map_err(|e| e.to_string())?;
    let rows = get_table_array(&table, "classification", "command_type_classification.toml")
        .map_err(|error| error.to_string())?;
    let mut classifications = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let ctx = format!(
            "command_type_classification.toml [[classification]] #{}",
            index + 1
        );
        // Fail closed on an unknown key BEFORE reading anything.
        for key in row.keys() {
            if !KNOWN_CLASSIFICATION_KEYS.contains(&key.as_str()) {
                return Err(format!("{ctx}: unknown key {key:?}"));
            }
        }
        classifications.push(Classification {
            type_name: get_str(row, "type_name", &ctx).map_err(|e| e.to_string())?,
            class: get_str(row, "class", &ctx).map_err(|e| e.to_string())?,
            command_contract_id: get_opt_str(row, "command_contract_id", &ctx)
                .map_err(|e| e.to_string())?,
            source_location: get_str(row, "source_location", &ctx).map_err(|e| e.to_string())?,
            statement: get_str(row, "statement", &ctx).map_err(|e| e.to_string())?,
            status: get_str(row, "status", &ctx).map_err(|e| e.to_string())?,
        });
    }
    Ok(ClassificationRegistry {
        registry_epoch,
        classifications,
    })
}

pub fn load_classifications(path: &Path) -> Result<ClassificationRegistry, LoadError> {
    let text = fs::read_to_string(path).map_err(|error| LoadError {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    parse_classifications(&text).map_err(|message| LoadError {
        path: path.display().to_string(),
        message,
    })
}

pub fn load_from_repo(root: &Path) -> Result<ClassificationRegistry, LoadError> {
    load_classifications(&root.join(REGISTRY_PATH))
}

pub fn registry_path(root: &Path) -> PathBuf {
    root.join(REGISTRY_PATH)
}

/// `aNN:LINE` — the same anchor form `laws.rs` requires; a classification that
/// cannot be opened and checked is a choice wearing a citation.
fn is_source_anchor(value: &str) -> bool {
    let Some((slice, line)) = value.split_once(':') else {
        return false;
    };
    let mut chars = slice.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && chars.clone().count() == 2
        && chars.all(|c| c.is_ascii_digit())
        && !line.is_empty()
        && line.chars().all(|c| c.is_ascii_digit())
}

pub fn validate_classifications(
    registry: &ClassificationRegistry,
    contracts: &ContractRegistry,
) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut seen_types: BTreeSet<&str> = BTreeSet::new();
    let contract_ids: BTreeSet<&str> = contracts
        .contracts
        .iter()
        .map(|c| c.command_contract_id.as_str())
        .collect();

    if registry.registry_epoch < 1 {
        out.push(Violation::new(
            "classification_registry_epoch_invalid",
            REGISTRY_PATH,
            "registry_epoch must be a positive integer",
        ));
    }

    if registry.classifications.is_empty() {
        out.push(Violation::new(
            "classification_registry_empty",
            REGISTRY_PATH,
            "the classification registry declares no rows; the source-forced seed population is nonempty, so an empty registry means the file was gutted",
        ));
    }

    for row in &registry.classifications {
        let subject = row.type_name.as_str();
        if subject.trim().is_empty() {
            out.push(Violation::new(
                "classification_type_empty",
                REGISTRY_PATH,
                "a classification row with no type_name classifies nothing",
            ));
        } else if !seen_types.insert(subject) {
            out.push(Violation::new(
                "classification_type_duplicate",
                subject,
                format!(
                    "type_name {subject:?} is classified more than once; §5.1 assigns exactly one class"
                ),
            ));
        }
        if !CLASSIFICATION_CLASSES.contains(&row.class.as_str()) {
            out.push(Violation::new(
                "classification_class_invalid",
                subject,
                format!("class {:?} is not one of the six §5.1 classes", row.class),
            ));
        }
        if !CLASSIFICATION_STATUSES.contains(&row.status.as_str()) {
            out.push(Violation::new(
                "classification_status_invalid",
                subject,
                format!("status {:?} is not in the closed vocabulary", row.status),
            ));
        }
        match (row.class.as_str(), row.command_contract_id.as_deref()) {
            ("RegisteredCommandInput", None) => out.push(Violation::new(
                "classification_contract_id_missing",
                subject,
                "class RegisteredCommandInput requires command_contract_id (plan line 294: RegisteredCommandInput{command_contract_id})",
            )),
            ("RegisteredCommandInput", Some(contract_id)) => {
                // Family resolution (round-12 T4): an armed member has one
                // contract row per inner arm ("{root}:{arm}" ids, the C-a
                // grammar), so its classification binds the member-root id.
                // A root resolves iff some row carries exactly that id or an
                // id in its ":"-delimited family; the ":" boundary keeps a
                // bare prefix (e.g. "cc:local:local-attempt") from resolving
                // through an unrelated longer id.
                let family_prefix = format!("{contract_id}:");
                let resolves = contract_ids.contains(contract_id)
                    || contract_ids
                        .iter()
                        .any(|id| id.starts_with(family_prefix.as_str()));
                if !resolves {
                    out.push(Violation::new(
                        "classification_contract_unresolved",
                        subject,
                        format!(
                            "command_contract_id {contract_id:?} resolves to no command_contracts.toml row, neither exactly nor as the member root of a {{root}}:{{arm}} family"
                        ),
                    ));
                }
            }
            (_, Some(_)) => out.push(Violation::new(
                "classification_contract_id_forbidden",
                subject,
                format!(
                    "class {:?} must not carry command_contract_id; only RegisteredCommandInput binds a contract",
                    row.class
                ),
            )),
            (_, None) => {}
        }
        if !is_source_anchor(&row.source_location) {
            out.push(Violation::new(
                "classification_source_anchor_malformed",
                subject,
                format!(
                    "source_location {:?} is not an aNN:LINE plan anchor; without it the class is a choice, not a derivation",
                    row.source_location
                ),
            ));
        }
        if row.statement.trim().is_empty() {
            out.push(Violation::new(
                "classification_statement_empty",
                subject,
                "the forcing quote is the evidence; a row without it cannot be checked against its anchor",
            ));
        }
    }

    out
}

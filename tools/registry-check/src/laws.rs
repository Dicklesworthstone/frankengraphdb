//! The named-law registry (`registries/laws.toml`) and its validator.
//!
//! Bead: fgdb-law-citation-sweep-uzzh.
//!
//! WHAT THIS FILE IS FOR. Catalog prose cites named laws — "per the
//! flattened-rendering law", "under the Appendix A u64 floor law". Measured
//! 2026-07-27, there were 93 such citations across 10 distinct names and not
//! one name was declared anywhere in the repository. A citation whose referent
//! nothing declares cannot be distinguished from an invented one, which is how
//! a rule that exists nowhere passed every gate. This registry declares the
//! referents so that a later guard can require citations to resolve to IDs
//! rather than to prose.
//!
//! WHY `source_location` IS REQUIRED ON A REGISTERED ROW. It is the field that
//! makes a law falsifiable: a reader opens the cited plan line and checks. The
//! adjudication that produced the seed rows found the rule held for 10 of 10
//! names — every cited law carrying an anchor resolved, every cited law without
//! one did not — so the anchor is not bookkeeping, it is the evidence.
//!
//! WHY UNKNOWN KEYS ARE REJECTED. A field added to some rows and silently
//! dropped on others is the same failure mode this registry exists to end, one
//! level up. The reader fails closed on a key it does not know.

use crate::toml::{get_opt_str, get_str, get_table_array, parse};
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub const REGISTRY_PATH: &str = "registries/laws.toml";

/// Every key a `[[law]]` row may carry. A key outside this set is a violation,
/// not a shrug.
pub const KNOWN_LAW_KEYS: [&str; 7] = [
    "id",
    "name",
    "source_location",
    "statement",
    "enforcement",
    "status",
    "note",
];

/// The closed status vocabulary. `registered` licenses a citation; the other
/// two do not, and a future citation guard keys on exactly this distinction.
pub const LAW_STATUSES: [&str; 3] = ["registered", "unadjudicated", "fabrication-candidate"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Law {
    pub id: String,
    pub name: String,
    pub source_location: String,
    pub statement: String,
    pub enforcement: String,
    pub status: String,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LawRegistry {
    pub laws: Vec<Law>,
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

fn parse_laws(text: &str) -> Result<LawRegistry, String> {
    let table = parse(text).map_err(|error| error.to_string())?;
    let rows = get_table_array(&table, "law", "laws.toml").map_err(|error| error.to_string())?;
    let mut laws = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let ctx = format!("laws.toml [[law]] #{}", index + 1);
        // Fail closed on an unknown key BEFORE reading anything: a row carrying
        // a field this reader does not understand has not been understood.
        for key in row.keys() {
            if !KNOWN_LAW_KEYS.contains(&key.as_str()) {
                return Err(format!("{ctx}: unknown key {key:?}"));
            }
        }
        laws.push(Law {
            id: get_str(row, "id", &ctx).map_err(|e| e.to_string())?,
            name: get_str(row, "name", &ctx).map_err(|e| e.to_string())?,
            source_location: get_opt_str(row, "source_location", &ctx)
                .map_err(|e| e.to_string())?
                .unwrap_or_default(),
            statement: get_opt_str(row, "statement", &ctx)
                .map_err(|e| e.to_string())?
                .unwrap_or_default(),
            enforcement: get_opt_str(row, "enforcement", &ctx)
                .map_err(|e| e.to_string())?
                .unwrap_or_default(),
            status: get_str(row, "status", &ctx).map_err(|e| e.to_string())?,
            note: get_opt_str(row, "note", &ctx)
                .map_err(|e| e.to_string())?
                .unwrap_or_default(),
        });
    }
    Ok(LawRegistry { laws })
}

pub fn load_laws(path: &Path) -> Result<LawRegistry, LoadError> {
    let text = fs::read_to_string(path).map_err(|error| LoadError {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    parse_laws(&text).map_err(|message| LoadError {
        path: path.display().to_string(),
        message,
    })
}

pub fn load_from_repo(root: &Path) -> Result<LawRegistry, LoadError> {
    load_laws(&root.join(REGISTRY_PATH))
}

pub fn registry_path(root: &Path) -> PathBuf {
    root.join(REGISTRY_PATH)
}

/// `aNN:LINE` — the anchor form every resolvable citation in the catalog uses.
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

fn is_law_id(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("FG-LAW-") else {
        return false;
    };
    rest.len() == 2 && rest.chars().all(|c| c.is_ascii_digit())
}

pub fn validate_laws(registry: &LawRegistry) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut seen_ids: BTreeSet<&str> = BTreeSet::new();
    let mut seen_names: BTreeSet<&str> = BTreeSet::new();

    if registry.laws.is_empty() {
        out.push(Violation::new(
            "law_registry_empty",
            REGISTRY_PATH,
            "the law registry declares no laws; an empty registry resolves every citation to nothing",
        ));
    }

    for law in &registry.laws {
        if !is_law_id(&law.id) {
            out.push(Violation::new(
                "law_id_malformed",
                &law.id,
                format!("law id {:?} is not of the form FG-LAW-NN", law.id),
            ));
        }
        if !seen_ids.insert(law.id.as_str()) {
            out.push(Violation::new(
                "law_id_duplicate",
                &law.id,
                format!("law id {:?} is declared more than once", law.id),
            ));
        }
        if law.name.trim().is_empty() {
            out.push(Violation::new(
                "law_name_empty",
                &law.id,
                "a law with no name cannot be cited",
            ));
        } else if !seen_names.insert(law.name.as_str()) {
            out.push(Violation::new(
                "law_name_duplicate",
                &law.id,
                format!(
                    "law name {:?} is declared more than once; a citation would resolve ambiguously",
                    law.name
                ),
            ));
        }
        if !LAW_STATUSES.contains(&law.status.as_str()) {
            out.push(Violation::new(
                "law_status_unknown",
                &law.id,
                format!(
                    "status {:?} is outside the closed vocabulary {:?}",
                    law.status, LAW_STATUSES
                ),
            ));
        }
        if law.status == "registered" {
            if law.statement.trim().is_empty() {
                out.push(Violation::new(
                    "law_statement_missing",
                    &law.id,
                    "a registered law must state what the rule says",
                ));
            }
            if law.enforcement.trim().is_empty() {
                out.push(Violation::new(
                    "law_enforcement_missing",
                    &law.id,
                    "a registered law must name the mechanism that enforces it",
                ));
            }
            if !is_source_anchor(&law.source_location) {
                out.push(Violation::new(
                    "law_source_anchor_missing",
                    &law.id,
                    format!(
                        "registered law has source_location {:?}, which is not an aNN:LINE anchor; the anchor is what makes the law falsifiable",
                        law.source_location
                    ),
                ));
            }
        } else if law.note.trim().is_empty() {
            // An unregistered row records an open question, so it must carry
            // the reasoning. Silence here is how the question gets lost.
            out.push(Violation::new(
                "law_adjudication_note_missing",
                &law.id,
                format!(
                    "law {:?} has status {:?} but no note; an unregistered row must record why",
                    law.name, law.status
                ),
            ));
        }
    }
    out
}

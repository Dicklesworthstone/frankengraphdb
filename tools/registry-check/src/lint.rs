//! claims-lint: scan merged prose artifacts for unregistered load-bearing
//! claim markers (bead acceptance: "fails with file/line").
//!
//! The marker shape is `FG-<NAMESPACE>-<NN>`: `FG-`, 2–5 uppercase ASCII
//! letters, `-`, exactly two ASCII digits not followed by a third. Wildcard
//! namespace references (`FG-INV-*`) never match. The scan set and the
//! excluded historical artifacts (each with a recorded reason) come from
//! `registries/claims_lint.toml`.

use crate::toml::{self, ReadError, get_str, get_str_array, get_table, get_table_array};
use std::collections::BTreeSet;
use std::path::Path;

/// The one marker pattern this lint supports. The config must declare
/// exactly this pattern; anything else is a config error (the matcher is
/// hand-rolled — std-only — so an unreviewed pattern change must fail loud,
/// not silently mismatch).
pub const SUPPORTED_MARKER_PATTERN: &str = "FG-[A-Z]{2,5}-[0-9]{2}";

#[derive(Debug, Clone, PartialEq)]
pub struct LintConfig {
    pub scan: Vec<String>,
    pub excludes: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LintHit {
    pub file: String,
    pub line: usize,
    pub marker: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LintError {
    pub msg: String,
}

impl std::fmt::Display for LintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}

impl std::error::Error for LintError {}

impl From<ReadError> for LintError {
    fn from(e: ReadError) -> Self {
        LintError { msg: e.to_string() }
    }
}

pub fn load_config(path: &Path) -> Result<LintConfig, LintError> {
    let text = std::fs::read_to_string(path).map_err(|e| LintError {
        msg: format!("{}: cannot read: {e}", path.display()),
    })?;
    let root = toml::parse(&text).map_err(|e| LintError {
        msg: format!("{}: {e}", path.display()),
    })?;
    let lint = get_table(&root, "lint", "claims_lint.toml")?;
    let pattern = get_str(lint, "marker_pattern", "claims_lint.toml.lint")?;
    if pattern != SUPPORTED_MARKER_PATTERN {
        return Err(LintError {
            msg: format!(
                "claims_lint.toml declares marker_pattern {pattern:?} but this checker implements exactly {SUPPORTED_MARKER_PATTERN:?}; change both together"
            ),
        });
    }
    let scan = get_str_array(lint, "scan", "claims_lint.toml.lint")?;
    let mut excludes = Vec::new();
    for (i, t) in get_table_array(&root, "exclude", "claims_lint.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("claims_lint.toml.exclude[{i}]");
        let path = get_str(t, "path", &ctx)?;
        let reason = get_str(t, "reason", &ctx)?;
        if reason.trim().is_empty() {
            return Err(LintError {
                msg: format!("{ctx}: an exclusion without a reason is a schema error"),
            });
        }
        excludes.push((path, reason));
    }
    Ok(LintConfig { scan, excludes })
}

/// Extract every claim marker in a line: `FG-` + 2..=5 uppercase + `-` + two
/// digits, with a non-alphanumeric boundary before and no third digit after.
pub fn markers_in_line(line: &str) -> Vec<String> {
    let bytes = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= bytes.len() {
        if bytes[i] == b'F'
            && bytes.get(i + 1) == Some(&b'G')
            && bytes.get(i + 2) == Some(&b'-')
            && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
        {
            let ns_start = i + 3;
            let mut j = ns_start;
            while j < bytes.len() && bytes[j].is_ascii_uppercase() && j - ns_start < 5 {
                j += 1;
            }
            let ns_len = j - ns_start;
            let is_marker = (2..=5).contains(&ns_len)
                && bytes.get(j) == Some(&b'-')
                && bytes.get(j + 1).is_some_and(u8::is_ascii_digit)
                && bytes.get(j + 2).is_some_and(u8::is_ascii_digit)
                && !bytes.get(j + 3).is_some_and(u8::is_ascii_digit);
            if is_marker {
                // The span is ASCII by construction.
                if let Ok(m) = std::str::from_utf8(&bytes[i..j + 3]) {
                    out.push(m.to_string());
                }
                i = j + 3;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Scan the configured prose files; return hits for unregistered markers.
pub fn run(
    root: &Path,
    config: &LintConfig,
    registered: &BTreeSet<String>,
) -> Result<Vec<LintHit>, LintError> {
    let mut hits = Vec::new();
    for file in &config.scan {
        let path = root.join(file);
        let text = std::fs::read_to_string(&path).map_err(|e| LintError {
            msg: format!("{}: cannot read: {e}", path.display()),
        })?;
        for (lineno, line) in text.lines().enumerate() {
            for marker in markers_in_line(line) {
                if !registered.contains(&marker) {
                    hits.push(LintHit {
                        file: file.clone(),
                        line: lineno + 1,
                        marker,
                        text: line.trim().to_string(),
                    });
                }
            }
        }
    }
    Ok(hits)
}

/// The registered marker universe: every claim/constraint ID across the
/// registries (top-level invariant IDs, clause keys are not markers).
pub fn registered_markers(r: &crate::model::Registries) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for inv in &r.invariants.invariants {
        set.insert(inv.id.clone());
    }
    for row in &r.evidence.rows {
        set.insert(row.id.clone());
    }
    for row in &r.slo.rows {
        set.insert(row.id.clone());
    }
    for c in &r.constitution.constraints {
        set.insert(c.id.clone());
    }
    set
}

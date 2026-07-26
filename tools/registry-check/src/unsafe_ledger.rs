//! unsafe_ledger — the unsafe-boundary ledger and its enforcement surface
//! (bead `fgdb-w1-unsafe-ledger-icp`; plan §1 constraint 2, §8.7, §18.1).
//!
//! The ledger is not documentation. AGENTS.md constraint 2 makes memory safety
//! *structural*: the workspace forbids `unsafe_code`, `forbid` cannot be
//! lowered, and therefore raw-pointer work may exist only inside separately
//! named `fgdb-unsafe-*` boundary crates whose roots `deny` instead, with
//! narrowly scoped `#[allow(unsafe_code)]` sites. The claim "this database is
//! memory-safe Rust" is worth exactly as much as the mechanism that enumerates
//! that surface — so an unledgered site is a build failure, never a review
//! judgement.
//!
//! # Why this checker fails instead of skipping
//!
//! Today the workspace has zero islands and zero unsafe sites. A checker
//! written the obvious way would report "0 sites, 0 orphans, pass" — and would
//! report exactly the same thing if its scanner were broken, if the ledger file
//! had been deleted, or if it could not read a single source file. That is the
//! looks-exactly-like-a-pass family, and this session produced six bugs with
//! that signature. Three structural answers:
//!
//! 1. **The ledger is a positive claim, not an absence.** `[[island]]` rows
//!    declare the intended roster with a `status`. A `present` island whose
//!    directory is missing fails; a `planned` island whose directory has
//!    appeared fails. Silence is never the same as agreement.
//! 2. **The scanner self-tests before it is trusted.** [`SCANNER_FIXTURE`] has
//!    a known site count; if scanning it does not reproduce that count exactly,
//!    the run fails with `site_scanner_self_test_failed` and every "zero sites"
//!    conclusion in the same run is treated as unlicensed. This is the control
//!    that makes an empty result mean something.
//! 3. **Unreadable is a failure, not a skip.** A manifest or source file that
//!    cannot be read fails the run; it never silently drops out of the scan.
//!
//! Every crate escapes the workspace `forbid` the moment its manifest omits
//! `[lints] workspace = true`, which is invisible at the crate root — so that
//! inheritance is checked per member rather than assumed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::toml::{self, Table, get_str, get_table_array};

/// Repo-relative location of the ledger.
pub const LEDGER_PATH: &str = "registries/unsafe_boundary_ledger.toml";

/// A source fixture with a known number of `#[allow(...)]` sites, ASSEMBLED AT
/// RUNTIME rather than written as literal source text.
///
/// This indirection is not stylistic. The checker scans every workspace member,
/// including the crate it lives in, so a fixture written literally would be
/// indistinguishable from two real unledgered sites in this very file — which
/// is exactly what happened on the first run: the checker failed its own
/// source with `unsafe_allow_outside_island` and `site_unledgered`. Building
/// the attribute text from a `char` keeps the token sequence off every source
/// line here, and `fixture_is_not_visible_in_this_source` pins that property so
/// it cannot regress.
///
/// The fixture is deliberately awkward — a multi-argument attribute, one
/// occurrence inside a comment, and one inside a string literal — because a
/// scanner that cannot tell those apart will miscount real islands.
pub fn scanner_fixture() -> String {
    let hash = '#';
    let allow = format!("{hash}[allow(unsafe_code)]");
    let allow_multi = format!("{hash}[allow(unsafe_code, clippy::undocumented_unsafe_blocks)]");
    format!(
        "{allow}\nunsafe fn one() {{}}\n\n\
         // {allow}  <- a comment, must NOT count\n\
         const NOT_A_SITE: &str = \"{allow}\";\n\n\
         {allow_multi}\nunsafe fn two() {{}}\n"
    )
}

/// The exact number of real sites in [`scanner_fixture`].
pub const SCANNER_FIXTURE_SITES: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub code: String,
    pub subject: String,
    pub source_anchor: String,
    pub message: String,
}

impl Violation {
    fn new(
        code: impl Into<String>,
        subject: impl Into<String>,
        source_anchor: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            subject: subject.into(),
            source_anchor: source_anchor.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug)]
pub struct LoadError {
    pub path: String,
    pub msg: String,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.msg)
    }
}

/// A declared boundary crate. The roster is the ledger's positive claim about
/// which crates are permitted to relax `forbid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Island {
    pub name: String,
    pub charter: String,
    /// `present` — the crate exists and must relax to `deny`.
    /// `planned`  — the crate must NOT exist yet.
    pub status: String,
}

/// One `#[allow(unsafe_code)]` site, with the evidence that justifies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LedgerSite {
    pub row_id: String,
    pub island: String,
    pub path: String,
    pub symbol: String,
    pub stated_invariant: String,
    pub evidence: String,
    pub fallback: String,
    pub no_claim_boundary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsafeLedger {
    pub schema_version: i64,
    pub islands: Vec<Island>,
    pub sites: Vec<LedgerSite>,
}

/// A site as found in the tree, independent of what the ledger claims.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedSite {
    pub path: String,
    pub line: usize,
    pub symbol: String,
}

pub fn load_ledger(path: &Path) -> Result<UnsafeLedger, LoadError> {
    let text = fs::read_to_string(path).map_err(|error| LoadError {
        path: path.display().to_string(),
        msg: error.to_string(),
    })?;
    let table = toml::parse(&text).map_err(|error| LoadError {
        path: path.display().to_string(),
        msg: error.to_string(),
    })?;
    let read = |e: crate::toml::ReadError| LoadError {
        path: path.display().to_string(),
        msg: e.to_string(),
    };
    let schema_version = crate::toml::get_int(&table, "schema_version", "ledger").map_err(read)?;
    let mut islands = Vec::new();
    for (i, row) in get_table_array(&table, "island", "ledger")
        .map_err(read)?
        .into_iter()
        .enumerate()
    {
        let ctx = format!("island[{i}]");
        islands.push(Island {
            name: get_str(row, "name", &ctx).map_err(read)?,
            charter: get_str(row, "charter", &ctx).map_err(read)?,
            status: get_str(row, "status", &ctx).map_err(read)?,
        });
    }
    let mut sites = Vec::new();
    let site_rows = match table.get("site") {
        None => Vec::new(),
        Some(_) => get_table_array(&table, "site", "ledger").map_err(read)?,
    };
    for (i, row) in site_rows.into_iter().enumerate() {
        let ctx = format!("site[{i}]");
        sites.push(LedgerSite {
            row_id: get_str(row, "row_id", &ctx).map_err(read)?,
            island: get_str(row, "island", &ctx).map_err(read)?,
            path: get_str(row, "path", &ctx).map_err(read)?,
            symbol: get_str(row, "symbol", &ctx).map_err(read)?,
            stated_invariant: get_str(row, "stated_invariant", &ctx).map_err(read)?,
            evidence: get_str(row, "evidence", &ctx).map_err(read)?,
            fallback: get_str(row, "fallback", &ctx).map_err(read)?,
            no_claim_boundary: get_str(row, "no_claim_boundary", &ctx).map_err(read)?,
        });
    }
    Ok(UnsafeLedger {
        schema_version,
        islands,
        sites,
    })
}

/// Find every real `#[allow(unsafe_code)]` site in one source text.
///
/// Deliberately conservative about what counts: a line whose first
/// non-whitespace is `//` is a comment, and an attribute that is not at the
/// start of the trimmed line is inside a string or a nested expression. Both
/// are excluded, and [`SCANNER_FIXTURE`] pins that behaviour.
pub fn scan_sites(path: &str, text: &str) -> Vec<ScannedSite> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if line.starts_with("//") || !line.starts_with("#[") {
            continue;
        }
        if !attribute_names_unsafe_code(line) {
            continue;
        }
        // The symbol is the next line that is not another attribute or blank;
        // it is what a reviewer reads to know what the allow actually covers.
        let symbol = lines[index + 1..]
            .iter()
            .map(|l| l.trim())
            .find(|l| !l.is_empty() && !l.starts_with("#[") && !l.starts_with("//"))
            .unwrap_or("")
            .to_owned();
        out.push(ScannedSite {
            path: path.to_owned(),
            line: index + 1,
            symbol,
        });
    }
    out
}

fn attribute_names_unsafe_code(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("#[") else {
        return false;
    };
    let Some(end) = rest.find(']') else {
        return false;
    };
    let body = &rest[..end];
    let Some(args) = body.strip_prefix("allow(").and_then(|b| b.strip_suffix(')')) else {
        return false;
    };
    args.split(',').any(|a| a.trim() == "unsafe_code")
}

/// Walk a directory collecting `.rs` files. An unreadable directory is an
/// error rather than an empty result: a scan that cannot see the tree must not
/// report that the tree is clean.
fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let looks_like_source = path.extension().is_some_and(|e| e == "rs");
        if file_type.is_dir() {
            // A DIRECTORY named `*.rs` is not a source file and must never be
            // silently walked past: it reads as a source file to every human
            // and to `ls`, so skipping it lets a crate be reported clean while
            // something unscannable sits in plain sight. The mutation suite
            // caught this checker doing exactly that.
            if looks_like_source {
                return Err(format!(
                    "{}: a directory occupies a path that names a source file; it cannot be                      scanned, so the crate cannot be reported clean",
                    path.display()
                ));
            }
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            collect_rs(&path, out)?;
        } else if looks_like_source {
            out.push(path);
        }
    }
    Ok(())
}

/// The machine-readable report the bead's acceptance criteria require.
#[derive(Debug, Default)]
pub struct Report {
    pub crates_scanned: usize,
    pub forbid_verdicts: BTreeMap<String, bool>,
    pub scanned_sites: Vec<ScannedSite>,
    pub orphan_rows: Vec<String>,
    pub scanner_self_test_sites: usize,
}

/// Verify the whole unsafe boundary. Returns the report alongside violations so
/// a caller can log what was actually examined, not merely the verdict.
pub fn check_workspace(root: &Path) -> (Report, Vec<Violation>) {
    let mut v = Vec::new();
    let mut report = Report::default();

    // --- control first: the scanner must prove itself before any result of
    // --- its is trusted, because every other check reads its output.
    let self_test = scan_sites("<fixture>", &scanner_fixture());
    report.scanner_self_test_sites = self_test.len();
    let scanner_trustworthy = self_test.len() == SCANNER_FIXTURE_SITES;
    if !scanner_trustworthy {
        v.push(Violation::new(
            "site_scanner_self_test_failed",
            "unsafe_ledger",
            SCANNER_FIXTURE_SITES.to_string(),
            format!(
                "the site scanner found {} sites in its own fixture, expected {}: every \
                 zero-site result this run would be unlicensed, so the run fails rather \
                 than reporting a clean boundary",
                self_test.len(),
                SCANNER_FIXTURE_SITES
            ),
        ));
    }

    // --- the workspace default itself
    let ws_path = root.join("Cargo.toml");
    let ws_text = match fs::read_to_string(&ws_path) {
        Ok(t) => t,
        Err(e) => {
            v.push(Violation::new(
                "workspace_manifest_unreadable",
                "Cargo.toml",
                ws_path.display().to_string(),
                format!("cannot read the workspace manifest, so no boundary claim can be made: {e}"),
            ));
            return (report, v);
        }
    };
    if !ws_text.contains("unsafe_code = \"forbid\"") {
        v.push(Violation::new(
            "workspace_forbid_absent",
            "Cargo.toml",
            "workspace.lints.rust",
            "the workspace default must be unsafe_code = \"forbid\"; forbid cannot be \
             lowered, which is the whole reason islands are separate crates",
        ));
    }

    // --- the ledger must exist. Absent ledger is the sharpest vacuity case:
    // --- a checker that passes when it cannot find its own ledger is worse
    // --- than no checker, because it launders an unaudited tree as audited.
    let ledger_path = root.join(LEDGER_PATH);
    let ledger = match load_ledger(&ledger_path) {
        Ok(l) => l,
        Err(e) => {
            v.push(Violation::new(
                "ledger_absent_or_unreadable",
                LEDGER_PATH,
                e.path.clone(),
                format!(
                    "the unsafe-boundary ledger could not be loaded ({}); the run fails \
                     rather than reporting an empty unsafe surface",
                    e.msg
                ),
            ));
            return (report, v);
        }
    };
    if ledger.schema_version != 1 {
        v.push(Violation::new(
            "ledger_schema_version_unknown",
            LEDGER_PATH,
            ledger.schema_version.to_string(),
            "unknown ledger schema_version",
        ));
    }

    let island_names: BTreeSet<&str> = ledger.islands.iter().map(|i| i.name.as_str()).collect();

    // --- the roster is a claim, and both directions of it are checked
    for island in &ledger.islands {
        let dir = root.join("crates").join(&island.name);
        let exists = dir.is_dir();
        match island.status.as_str() {
            "present" if !exists => v.push(Violation::new(
                "island_declared_present_but_absent",
                &island.name,
                dir.display().to_string(),
                "the ledger claims this boundary crate exists; it does not",
            )),
            "planned" if exists => v.push(Violation::new(
                "island_declared_planned_but_present",
                &island.name,
                dir.display().to_string(),
                "the boundary crate has appeared while the ledger still calls it planned: \
                 an island must be admitted to the ledger before it lands, never after",
            )),
            "present" | "planned" => {}
            other => v.push(Violation::new(
                "island_status_unknown",
                &island.name,
                other.to_string(),
                "island status must be \"present\" or \"planned\"",
            )),
        }
        if island.charter.trim().is_empty() {
            v.push(Violation::new(
                "island_charter_empty",
                &island.name,
                "charter",
                "an island without a stated charter cannot bound what may live in it",
            ));
        }
    }

    // --- every ledger row must carry real evidence, not a placeholder
    let mut seen_rows: BTreeSet<&str> = BTreeSet::new();
    for site in &ledger.sites {
        if !seen_rows.insert(site.row_id.as_str()) {
            v.push(Violation::new(
                "ledger_row_id_duplicated",
                &site.row_id,
                LEDGER_PATH,
                "row_id must be unique",
            ));
        }
        if !island_names.contains(site.island.as_str()) {
            v.push(Violation::new(
                "ledger_row_island_unknown",
                &site.row_id,
                &site.island,
                "a site names an island that the roster does not declare",
            ));
        }
        for (field, value) in [
            ("stated_invariant", &site.stated_invariant),
            ("evidence", &site.evidence),
            ("fallback", &site.fallback),
            ("no_claim_boundary", &site.no_claim_boundary),
        ] {
            if value.trim().is_empty() {
                v.push(Violation::new(
                    "ledger_row_field_vacuous",
                    &site.row_id,
                    field,
                    format!(
                        "{field} is empty: an unsafe site with no {field} is an unaudited \
                         site wearing a ledger row"
                    ),
                ));
            }
        }
    }

    // --- scan every workspace member
    let members = workspace_members(&ws_text);
    report.crates_scanned = members.len();
    for member in &members {
        let name = member
            .rsplit('/')
            .next()
            .unwrap_or(member.as_str())
            .to_owned();
        let is_island = island_names.contains(name.as_str());
        let manifest = root.join(member).join("Cargo.toml");
        match fs::read_to_string(&manifest) {
            Err(e) => {
                v.push(Violation::new(
                    "member_manifest_unreadable",
                    &name,
                    manifest.display().to_string(),
                    format!("cannot read member manifest, so its boundary is unknown: {e}"),
                ));
                continue;
            }
            Ok(text) => {
                // A member that omits `[lints] workspace = true` silently keeps
                // its own (absent) lint table and escapes the workspace forbid.
                // Nothing at the crate root reveals this.
                let inherits = text.contains("[lints]") && text.contains("workspace = true");
                report.forbid_verdicts.insert(name.clone(), inherits);
                if !inherits && !is_island {
                    v.push(Violation::new(
                        "member_does_not_inherit_forbid",
                        &name,
                        manifest.display().to_string(),
                        "an ordinary crate must carry `[lints] workspace = true`; without it \
                         the workspace unsafe_code = \"forbid\" does not apply and the crate \
                         may use unsafe while every gate stays green",
                    ));
                }
            }
        }

        let src = root.join(member).join("src");
        if !src.is_dir() {
            continue;
        }
        let mut files = Vec::new();
        if let Err(e) = collect_rs(&src, &mut files) {
            v.push(Violation::new(
                "source_tree_unreadable",
                &name,
                src.display().to_string(),
                format!("cannot walk the crate source, so it cannot be reported clean: {e}"),
            ));
            continue;
        }
        for file in files {
            let rel = file
                .strip_prefix(root)
                .unwrap_or(&file)
                .display()
                .to_string();
            let text = match fs::read_to_string(&file) {
                Ok(t) => t,
                Err(e) => {
                    v.push(Violation::new(
                        "source_unreadable",
                        &name,
                        rel.clone(),
                        format!("cannot read source file, so it cannot be reported clean: {e}"),
                    ));
                    continue;
                }
            };
            for site in scan_sites(&rel, &text) {
                if !is_island {
                    v.push(Violation::new(
                        "unsafe_allow_outside_island",
                        &name,
                        format!("{}:{}", site.path, site.line),
                        "only a named fgdb-unsafe-* boundary crate may allow unsafe_code",
                    ));
                }
                report.scanned_sites.push(site);
            }
        }
    }

    // --- bijection: sites <-> rows, both directions
    let mut matched: BTreeSet<String> = BTreeSet::new();
    for site in &report.scanned_sites {
        let row = ledger
            .sites
            .iter()
            .find(|r| r.path == site.path && r.symbol == site.symbol);
        match row {
            Some(r) => {
                matched.insert(r.row_id.clone());
            }
            None => v.push(Violation::new(
                "site_unledgered",
                format!("{}:{}", site.path, site.line),
                site.symbol.clone(),
                "an allow(unsafe_code) site with no ledger row: CI rejects an unledgered site",
            )),
        }
    }
    for r in &ledger.sites {
        if !matched.contains(&r.row_id) {
            report.orphan_rows.push(r.row_id.clone());
            v.push(Violation::new(
                "ledger_row_orphaned",
                &r.row_id,
                format!("{}#{}", r.path, r.symbol),
                "a ledger row with no matching site: the ledger must not rot into a \
                 description of unsafe code that no longer exists",
            ));
        }
    }

    (report, v)
}

fn workspace_members(manifest: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_members = false;
    for raw in manifest.lines() {
        let line = raw.trim();
        if line.starts_with("members") {
            in_members = true;
            continue;
        }
        if in_members {
            if line.starts_with(']') {
                break;
            }
            if let Some(start) = line.find('"')
                && let Some(end) = line[start + 1..].find('"')
            {
                out.push(line[start + 1..start + 1 + end].to_owned());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_reproduces_its_own_fixture_exactly() {
        // If this ever drifts, every "zero sites" result elsewhere is unlicensed.
        let sites = scan_sites("<fixture>", &scanner_fixture());
        assert_eq!(sites.len(), SCANNER_FIXTURE_SITES, "fixture site count");
        assert_eq!(sites[0].symbol, "unsafe fn one() {}");
        assert_eq!(sites[1].symbol, "unsafe fn two() {}");
    }

    #[test]
    fn scanner_rejects_comments_and_string_literals() {
        // The two decoys in the fixture are the whole reason the count is 2
        // and not 4; a naive `contains` scanner passes the count check by
        // accident only when the decoys are absent.
        assert!(!attribute_names_unsafe_code("// #[allow(unsafe_code)]"));
        assert!(attribute_names_unsafe_code("#[allow(unsafe_code)]"));
        assert!(attribute_names_unsafe_code(
            "#[allow(unsafe_code, clippy::x)]"
        ));
        assert!(!attribute_names_unsafe_code("#[allow(dead_code)]"));
        assert!(!attribute_names_unsafe_code("#[deny(unsafe_code)]"));
    }

    /// The fixture must never be visible as real sites in this file. If this
    /// fails, the checker will fail its own crate -- and worse, a reader would
    /// see two "unsafe sites" in a std-only tooling crate that has none.
    #[test]
    fn fixture_is_not_visible_in_this_source() {
        let own_source = include_str!("unsafe_ledger.rs");
        assert_eq!(
            scan_sites("unsafe_ledger.rs", own_source),
            Vec::new(),
            "the fixture leaked into this file as literal source text"
        );
    }

    #[test]
    fn workspace_members_parses_the_real_shape() {
        let manifest = "[workspace]\nresolver = \"3\"\nmembers = [\n    \"crates/a\",\n    \"tools/b\",\n]\n\n[workspace.package]\n";
        assert_eq!(workspace_members(manifest), vec!["crates/a", "tools/b"]);
    }
}

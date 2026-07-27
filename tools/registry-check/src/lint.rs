//! claims-lint: the prose⇄registry claim check, in BOTH directions
//! (bead acceptance: "fails with file/line").
//!
//! Direction 1 — **every marker that is written must resolve.** The marker
//! shape is `FG-<NAMESPACE>-<NN>`: `FG-`, 2–5 uppercase ASCII letters, `-`,
//! exactly two ASCII digits not followed by a third. Wildcard namespace
//! references (`FG-INV-*`) never match. Every marker in a scanned artifact
//! must name a registered row.
//!
//! Direction 2 — **every load-bearing claim must carry a marker.** Direction 1
//! alone is vacuous on a claim that cites nothing: a throughput budget written
//! with no `FG-SLO-nn` beside it is not an unresolved marker, it is not a
//! marker at all, and the marker scan cannot see it. So the config declares
//! the regions where a bare number *is* the claim — today, README's §Performance
//! gate table — and every row of such a region must cite a resolvable marker.
//! Rows that do not yet are enumerated in the config with an owner bead; that
//! ledger is itself checked in both directions, so the gap is exact and every
//! move in it (a row marked, a row added) changes a checked number.
//!
//! Why direction 2 is scoped to declared regions rather than "any number in
//! any prose": the same paragraph that states "≥ 40M edges/s sustained" also
//! states "32-core/64-thread, 256 GB RAM, PCIe-4 NVMe at 7 GB/s" — a
//! description of the reference machine, not a claim about this system. No
//! numeric pattern separates those two, so a corpus-wide number scan would
//! need an exemption list larger than the set of real claims, and an exemption
//! list is exactly the instrument that goes quietly dead. A declared region is
//! reviewable, and the plan's own law already puts the claim unit at the gate
//! row ("every gate names its operation class in the operation-cost registry").
//!
//! Closure — the scan set is an allowlist and the exclude set is a denylist,
//! and neither is self-validating. Three laws keep them honest: every prose
//! file ANYWHERE under a declared closure root — the walk is recursive — must
//! be named by one of them; every `presence = "required"` exclusion must still
//! name a file that exists; and every `presence = "required"` closure prune
//! must still name a directory that exists. A rule that no longer matches
//! anything is deleted, not carried, whether it widens or narrows.
//!
//! The walk became recursive for fgdb-claims-lint-scan-set-not-total-nldg.
//! Reading each root one level deep made the law total over the corpus as it
//! stood and blind everywhere else: MEASURED 2026-07-26, all 11 tracked `.md`
//! sat in `.` or `docs/`, while the repository had 50 tracked directories below
//! depth 1 in which a `.md` drew no hit at all. A law that holds only because
//! of where files happen to sit today is a coincidence, not a law.

use crate::toml::{
    self, ReadError, get_opt_str, get_str, get_str_array, get_table, get_table_array,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

/// The one marker pattern this lint supports. The config must declare
/// exactly this pattern; anything else is a config error (the matcher is
/// hand-rolled — std-only — so an unreviewed pattern change must fail loud,
/// not silently mismatch).
pub const SUPPORTED_MARKER_PATTERN: &str = "FG-[A-Z]{2,5}-[0-9]{2}";

/// Whether an exclusion's path must still exist on disk. `Required` is the
/// default and the only value that keeps an exclusion honest; `Optional` is
/// for a path that is legitimately absent from a clean checkout (a gitignored
/// working document), and must carry its own reason for being optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Required,
    Optional,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Exclude {
    pub path: String,
    pub reason: String,
    pub presence: Presence,
}

/// A directory the closure walk does not descend into.
///
/// The walk is recursive, so without this a build-output tree would drag every
/// vendored `.md` of every dependency into the closure obligation. A prune is a
/// narrowing of the law and is held to the same discipline as an exclusion: it
/// carries a reason, and a `presence = "required"` prune whose directory does
/// not exist is a dead rule that must be deleted rather than carried.
#[derive(Debug, Clone, PartialEq)]
pub struct Prune {
    pub dir: String,
    pub reason: String,
    pub presence: Presence,
}

/// A declared region where a bare number is a load-bearing claim: a markdown
/// table under `heading` in `file`, every row of which must cite a resolvable
/// claim marker. `unmarked_rows` is the ledger of rows that do not yet, keyed
/// by the row's first cell.
#[derive(Debug, Clone, PartialEq)]
pub struct GateTable {
    pub file: String,
    pub heading: String,
    pub owner_bead: String,
    pub unmarked_rows: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LintConfig {
    pub scan: Vec<String>,
    pub excludes: Vec<Exclude>,
    pub closure_dirs: Vec<String>,
    pub closure_prunes: Vec<Prune>,
    pub gate_tables: Vec<GateTable>,
}

/// What a hit accuses. Every variant is a distinct law; the code string is
/// stable and is what a negative test asserts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum HitKind {
    /// Direction 1: a marker is written that no registry row defines.
    UnregisteredMarker,
    /// Direction 2: a gate row states a budget and cites no marker.
    UnmarkedGateRow,
    /// The unmarked-row ledger names a row that is gone, or now carries a
    /// marker — either way the entry is stale and must be deleted.
    DeadGateExemption,
    /// A prose artifact in a closure directory is neither scanned nor excluded.
    UnclaimedProse,
    /// A `presence = "required"` exclusion names a path that does not exist.
    DeadExclude,
    /// A `presence = "required"` closure prune names a directory that does not
    /// exist: the walk is narrowed by a rule that no longer narrows anything.
    DeadPrune,
}

impl HitKind {
    pub fn code(self) -> &'static str {
        match self {
            HitKind::UnregisteredMarker => "unregistered_marker",
            HitKind::UnmarkedGateRow => "unmarked_gate_row",
            HitKind::DeadGateExemption => "dead_gate_exemption",
            HitKind::UnclaimedProse => "unclaimed_prose",
            HitKind::DeadExclude => "dead_exclude",
            HitKind::DeadPrune => "dead_closure_prune",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LintHit {
    pub kind: HitKind,
    pub file: String,
    /// 1-based source line, or 0 for a hit against the config itself.
    pub line: usize,
    /// The marker, gate-row key, or path the hit is about.
    pub subject: String,
    pub text: String,
}

/// What the lint actually examined. Reported on every run so a green result is
/// never silent about its own scope: a check that examines nothing passes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LintCensus {
    pub files_scanned: usize,
    pub markers_seen: usize,
    pub gate_rows_read: usize,
    pub gate_rows_marked: usize,
    pub gate_rows_unmarked: usize,
    pub prose_files_seen: usize,
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
    if scan.is_empty() {
        return Err(LintError {
            msg: "claims_lint.toml.lint.scan is empty: a lint that scans nothing passes".into(),
        });
    }
    let mut seen_scan = BTreeSet::new();
    for file in &scan {
        if !seen_scan.insert(file.clone()) {
            return Err(LintError {
                msg: format!(
                    "claims_lint.toml.lint.scan lists {file:?} twice, which would double every count it contributes to the census"
                ),
            });
        }
    }
    let closure_dirs = get_str_array(lint, "closure_dirs", "claims_lint.toml.lint")?;
    if closure_dirs.is_empty() {
        return Err(LintError {
            msg: "claims_lint.toml.lint.closure_dirs is empty: nothing would hold the scan set and the exclusion set to account".into(),
        });
    }

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
        let presence = match get_opt_str(t, "presence", &ctx)?.as_deref() {
            None | Some("required") => Presence::Required,
            Some("optional") => Presence::Optional,
            Some(other) => {
                return Err(LintError {
                    msg: format!(
                        "{ctx}: presence {other:?} is not one of \"required\" | \"optional\""
                    ),
                });
            }
        };
        if scan.contains(&path) {
            return Err(LintError {
                msg: format!("{ctx}: {path:?} is both scanned and excluded"),
            });
        }
        if excludes.iter().any(|e: &Exclude| e.path == path) {
            return Err(LintError {
                msg: format!("{ctx}: {path:?} is excluded twice"),
            });
        }
        excludes.push(Exclude {
            path,
            reason,
            presence,
        });
    }

    let mut closure_prunes = Vec::new();
    for (i, t) in get_table_array(&root, "closure_prune", "claims_lint.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("claims_lint.toml.closure_prune[{i}]");
        let dir = get_str(t, "dir", &ctx)?;
        let reason = get_str(t, "reason", &ctx)?;
        if reason.trim().is_empty() {
            return Err(LintError {
                msg: format!("{ctx}: a closure prune without a reason is a schema error"),
            });
        }
        if dir.trim().is_empty() || dir.contains("..") {
            return Err(LintError {
                msg: format!("{ctx}: dir {dir:?} must be a non-empty relative directory"),
            });
        }
        let presence = match get_opt_str(t, "presence", &ctx)?.as_deref() {
            None | Some("required") => Presence::Required,
            Some("optional") => Presence::Optional,
            Some(other) => {
                return Err(LintError {
                    msg: format!(
                        "{ctx}: presence {other:?} is not one of \"required\" | \"optional\""
                    ),
                });
            }
        };
        if closure_dirs.contains(&dir) {
            return Err(LintError {
                msg: format!("{ctx}: {dir:?} is both a closure root and pruned from the walk"),
            });
        }
        if closure_prunes.iter().any(|p: &Prune| p.dir == dir) {
            return Err(LintError {
                msg: format!("{ctx}: {dir:?} is pruned twice"),
            });
        }
        closure_prunes.push(Prune {
            dir,
            reason,
            presence,
        });
    }

    let mut gate_tables = Vec::new();
    for (i, t) in get_table_array(&root, "gate_table", "claims_lint.toml")?
        .iter()
        .enumerate()
    {
        let ctx = format!("claims_lint.toml.gate_table[{i}]");
        let file = get_str(t, "file", &ctx)?;
        if !scan.contains(&file) {
            return Err(LintError {
                msg: format!(
                    "{ctx}: {file:?} declares a gate table but is not in lint.scan, so the markers its rows cite would never be resolved"
                ),
            });
        }
        let heading = get_str(t, "heading", &ctx)?;
        let owner_bead = get_str(t, "owner_bead", &ctx)?;
        let unmarked_rows = get_str_array(t, "unmarked_rows", &ctx)?;
        let mut seen = BTreeSet::new();
        for row in &unmarked_rows {
            if !seen.insert(row.clone()) {
                return Err(LintError {
                    msg: format!("{ctx}.unmarked_rows: {row:?} is listed twice"),
                });
            }
        }
        gate_tables.push(GateTable {
            file,
            heading,
            owner_bead,
            unmarked_rows,
        });
    }
    if gate_tables.is_empty() {
        return Err(LintError {
            msg: "claims_lint.toml declares no [[gate_table]]: the second direction of this lint would examine nothing".into(),
        });
    }

    Ok(LintConfig {
        scan,
        excludes,
        closure_dirs,
        closure_prunes,
        gate_tables,
    })
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

/// Split one markdown table row into its trimmed cells. THE reader for that
/// shape in this crate — `topology::parse_layer_table` calls it too, so a
/// disagreement about what a cell is cannot exist between two table readers.
pub fn table_cells(line: &str) -> Vec<&str> {
    line.trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(str::trim)
        .collect()
}

fn is_separator_row(cells: &[&str]) -> bool {
    !cells.is_empty()
        && cells.iter().all(|c| {
            let c = c.trim_start_matches(':').trim_end_matches(':');
            !c.is_empty() && c.bytes().all(|b| b == b'-')
        })
}

/// One row of a declared gate table.
#[derive(Debug, Clone, PartialEq)]
pub struct GateRow {
    /// 1-based line in the source file.
    pub line: usize,
    /// First cell — the row's key in the unmarked-row ledger.
    pub key: String,
    pub text: String,
    pub markers: Vec<String>,
}

/// Read the markdown table that follows `heading` in `text`.
///
/// Every failure here is a structural break, not a hit: a heading that moved,
/// a table that vanished, a separator that is not a separator all mean the
/// second direction of this lint is now aimed at nothing, and a lint aimed at
/// nothing must fail loudly rather than pass quietly.
pub fn read_gate_rows(text: &str, file: &str, heading: &str) -> Result<Vec<GateRow>, LintError> {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.trim() == heading)
        .ok_or_else(|| LintError {
            msg: format!("{file}: no line is exactly {heading:?} — the declared gate table region does not exist"),
        })?;

    let mut i = start + 1;
    while i < lines.len() {
        let t = lines[i].trim();
        if t.starts_with('|') {
            break;
        }
        if t.starts_with('#') {
            return Err(LintError {
                msg: format!(
                    "{file}: heading {heading:?} is followed by heading {t:?} with no table between them"
                ),
            });
        }
        i += 1;
    }
    if i >= lines.len() {
        return Err(LintError {
            msg: format!("{file}: heading {heading:?} is not followed by a table"),
        });
    }

    let header = table_cells(lines[i]);
    let width = header.len();
    i += 1;
    if i >= lines.len() || !is_separator_row(&table_cells(lines[i])) {
        return Err(LintError {
            msg: format!(
                "{file}:{}: the table under {heading:?} has no header separator row",
                i + 1
            ),
        });
    }
    i += 1;

    let mut rows = Vec::new();
    while i < lines.len() && lines[i].trim().starts_with('|') {
        let cells = table_cells(lines[i]);
        if cells.len() != width {
            return Err(LintError {
                msg: format!(
                    "{file}:{}: gate row has {} cells, header has {width}",
                    i + 1,
                    cells.len()
                ),
            });
        }
        rows.push(GateRow {
            line: i + 1,
            key: cells[0].to_string(),
            text: lines[i].trim().to_string(),
            markers: markers_in_line(lines[i]),
        });
        i += 1;
    }
    if rows.is_empty() {
        return Err(LintError {
            msg: format!(
                "{file}: the table under {heading:?} has no rows — this lint would examine nothing"
            ),
        });
    }
    Ok(rows)
}

/// Scan the configured prose; return every hit and what was examined.
///
/// One entry point, both directions, one census. A second lint path is how a
/// direction goes quietly missing.
pub fn run(
    root: &Path,
    config: &LintConfig,
    registered: &BTreeSet<String>,
) -> Result<(Vec<LintHit>, LintCensus), LintError> {
    let mut hits = Vec::new();
    let mut census = LintCensus::default();

    // ---- direction 1: every marker written must resolve ----
    let mut texts: BTreeMap<&str, String> = BTreeMap::new();
    for file in &config.scan {
        let path = root.join(file);
        let text = std::fs::read_to_string(&path).map_err(|e| LintError {
            msg: format!("{}: cannot read: {e}", path.display()),
        })?;
        census.files_scanned += 1;
        for (lineno, line) in text.lines().enumerate() {
            for marker in markers_in_line(line) {
                census.markers_seen += 1;
                if !registered.contains(&marker) {
                    hits.push(LintHit {
                        kind: HitKind::UnregisteredMarker,
                        file: file.clone(),
                        line: lineno + 1,
                        subject: marker,
                        text: line.trim().to_string(),
                    });
                }
            }
        }
        texts.insert(file.as_str(), text);
    }

    // ---- direction 2: every load-bearing claim must carry a marker ----
    for gt in &config.gate_tables {
        let text = texts.get(gt.file.as_str()).ok_or_else(|| LintError {
            msg: format!("{}: declared as a gate-table file but not scanned", gt.file),
        })?;
        let rows = read_gate_rows(text, &gt.file, &gt.heading)?;
        let ledger: BTreeSet<&str> = gt.unmarked_rows.iter().map(String::as_str).collect();
        let mut by_key: BTreeMap<&str, &GateRow> = BTreeMap::new();
        for row in &rows {
            census.gate_rows_read += 1;
            if row.markers.is_empty() {
                census.gate_rows_unmarked += 1;
                if !ledger.contains(row.key.as_str()) {
                    hits.push(LintHit {
                        kind: HitKind::UnmarkedGateRow,
                        file: gt.file.clone(),
                        line: row.line,
                        subject: row.key.clone(),
                        text: format!(
                            "gate row states a budget and cites no claim marker; register it, or list it in claims_lint.toml gate_table.unmarked_rows under {}",
                            gt.owner_bead
                        ),
                    });
                }
            } else {
                census.gate_rows_marked += 1;
            }
            by_key.insert(row.key.as_str(), row);
        }
        // The converse of the same ledger: an entry that no longer names an
        // unmarked row is stale. Without this, marking a row leaves its
        // exemption behind and the next unmarked row inherits a free pass.
        for key in &gt.unmarked_rows {
            match by_key.get(key.as_str()) {
                None => hits.push(LintHit {
                    kind: HitKind::DeadGateExemption,
                    file: gt.file.clone(),
                    line: 0,
                    subject: key.clone(),
                    text: format!(
                        "claims_lint.toml lists this row as unmarked, but the table under {:?} has no such row",
                        gt.heading
                    ),
                }),
                Some(row) if !row.markers.is_empty() => hits.push(LintHit {
                    kind: HitKind::DeadGateExemption,
                    file: gt.file.clone(),
                    line: row.line,
                    subject: key.clone(),
                    text: format!(
                        "this row now cites {}; delete its claims_lint.toml unmarked_rows entry",
                        row.markers.join(", ")
                    ),
                }),
                Some(_) => {}
            }
        }
    }

    // ---- closure: the allowlist and the denylist must account for the corpus ----
    let claimed: BTreeSet<&str> = config
        .scan
        .iter()
        .map(String::as_str)
        .chain(config.excludes.iter().map(|e| e.path.as_str()))
        .collect();
    // The walk is RECURSIVE. It used to read each closure directory one level
    // deep, which made the law total over the directories that happened to hold
    // prose and blind everywhere else. MEASURED 2026-07-26 at HEAD b77982e: all
    // 11 tracked `.md` sit in `.` or `docs/`, so the one-level law was total
    // over the corpus as it stood — but the repository has 50 tracked
    // directories below depth 1, and a `.md` written into any of them was in
    // neither list and drew no hit. A file with the exact text the lint exists
    // to catch, written to `crates/fgdb-bigint/README.md`, left
    // `registry-check all` at `failures: 0, outcome: pass`, exit 0.
    let pruned: BTreeSet<&str> = config
        .closure_prunes
        .iter()
        .map(|p| p.dir.as_str())
        .collect();
    // Deduplicated across roots: with a recursive walk, `docs` is reachable
    // from `.`, and a file must be one closure obligation, not two.
    let mut found = BTreeSet::new();
    for dir in &config.closure_dirs {
        let abs = if dir == "." {
            root.to_path_buf()
        } else {
            root.join(dir)
        };
        let mut found_here = BTreeSet::new();
        let mut stack = vec![(
            abs.clone(),
            if dir == "." {
                String::new()
            } else {
                format!("{dir}/")
            },
        )];
        while let Some((cur, prefix)) = stack.pop() {
            let entries = std::fs::read_dir(&cur).map_err(|e| LintError {
                msg: format!("{}: closure directory cannot be read: {e}", cur.display()),
            })?;
            for entry in entries {
                let entry = entry.map_err(|e| LintError {
                    msg: format!("{}: {e}", cur.display()),
                })?;
                let name = entry.file_name().to_string_lossy().into_owned();
                // Hidden entries are not deliverables (this also drops the
                // `._*.md` AppleDouble forks that sit beside the plan documents).
                if name.starts_with('.') {
                    continue;
                }
                let rel = format!("{prefix}{name}");
                let path = entry.path();
                // file_type() describes the ENTRY, not its target, so a
                // symlinked directory is neither descended into nor counted.
                // A recursive walk that followed links could loop forever, and
                // a linked-in document is not this repository's deliverable.
                // There are no symlinks in the tree today (measured: 0 outside
                // .git) — this is here so that stays true by construction.
                let ft = entry.file_type().map_err(|e| LintError {
                    msg: format!("{}: {e}", path.display()),
                })?;
                if ft.is_dir() {
                    if pruned.contains(rel.as_str()) {
                        continue;
                    }
                    stack.push((path, format!("{rel}/")));
                    continue;
                }
                if !name.ends_with(".md") || !ft.is_file() {
                    continue;
                }
                found_here.insert(rel);
            }
        }
        if found_here.is_empty() {
            return Err(LintError {
                msg: format!(
                    "{}: closure directory holds no prose — the closure law would be vacuous here",
                    abs.display()
                ),
            });
        }
        found.extend(found_here);
    }
    for rel in found {
        census.prose_files_seen += 1;
        if !claimed.contains(rel.as_str()) {
            hits.push(LintHit {
                kind: HitKind::UnclaimedProse,
                file: rel.clone(),
                line: 0,
                subject: rel,
                text: "prose artifact is neither in claims_lint.toml lint.scan nor excluded with a reason".into(),
            });
        }
    }

    // ---- the denylist itself: an exclusion must still exclude something ----
    for ex in &config.excludes {
        if ex.presence == Presence::Required && !root.join(&ex.path).exists() {
            hits.push(LintHit {
                kind: HitKind::DeadExclude,
                file: "registries/claims_lint.toml".into(),
                line: 0,
                subject: ex.path.clone(),
                text: "exclusion names a path that does not exist; delete it rather than carrying a rule that matches nothing".into(),
            });
        }
    }

    // ---- the prune list's own liveness, on the same terms as the denylist ----
    for p in &config.closure_prunes {
        if p.presence == Presence::Required && !root.join(&p.dir).is_dir() {
            hits.push(LintHit {
                kind: HitKind::DeadPrune,
                file: "registries/claims_lint.toml".into(),
                line: 0,
                subject: p.dir.clone(),
                text: "closure prune names a directory that does not exist; delete it rather than carrying a rule that narrows nothing".into(),
            });
        }
    }

    hits.sort_by(|a, b| {
        (a.kind, &a.file, a.line, &a.subject).cmp(&(b.kind, &b.file, b.line, &b.subject))
    });
    Ok((hits, census))
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

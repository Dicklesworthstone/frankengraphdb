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
//! The workspace began with zero islands and zero unsafe sites; it now carries
//! three islands with seven ledgered sites, and every crate outside them still
//! scans to zero. A checker written the obvious way would report "0 sites, 0
//! orphans, pass" — and would report exactly the same thing if its scanner were
//! broken, if the ledger file had been deleted, or if it could not read a
//! single source file. That is the
//! looks-exactly-like-a-pass family, and this session produced six bugs with
//! that signature. Five structural answers:
//!
//! 1. **The ledger is a positive claim, not an absence.** `[[island]]` rows
//!    declare the intended roster with a `status`. A `present` island whose
//!    directory is missing fails; a `planned` island whose directory has
//!    appeared fails. Silence is never the same as agreement.
//! 2. **The scanner self-tests before it is trusted.** [`scanner_fixture`] has
//!    a known site count; if scanning it does not reproduce that count exactly,
//!    the run fails with `site_scanner_self_test_failed` and every "zero sites"
//!    conclusion in the same run is treated as unlicensed. This is the control
//!    that makes an empty result mean something.
//! 3. **Unreadable is a failure, not a skip.** A manifest or source file that
//!    cannot be read fails the run; it never silently drops out of the scan.
//! 4. **The scanner matches structure, not spelling.** An attribute relaxes
//!    `unsafe_code` if a level weaker than `deny` names it *anywhere* inside the
//!    attribute — nested in `cfg_attr`, spread across lines, spelled `expect` or
//!    `warn`, or written as an inner `#!` attribute over a whole module. The
//!    first version of this scanner required the attribute body to begin
//!    literally with `allow(`, so a `cfg_attr`-wrapped allow was never counted,
//!    never matched against the ledger, and never reported. That was invisible
//!    only because ordinary crates inherit `forbid`, which no `allow` can lower
//!    — and it became live the day the first island landed, since an island root
//!    uses `deny`, which every one of those forms *can* lower. Three islands
//!    have since landed, so this is not a hypothetical. See
//!    [`LEVELS_BELOW_DENY`].
//! 5. **Candidacy is structural too, not just the body.** An attribute is found
//!    by masking the whole file once and looking for one at ANY column — never
//!    by testing the trimmed prefix of a line. That prefix test was (4) with the
//!    fix applied one layer too shallow, and it was wrong in both directions at
//!    once: `impl T { #[allow(unsafe_code)] unsafe fn f() {} }` was not a
//!    candidate at all, while a block-commented attribute — text the compiler
//!    deletes — was counted as a real site, as was one inside a multi-line
//!    string. The same test decided the crate-ROOT policy, where the fail-open
//!    direction is sharper still: a commented-out `#![forbid(unsafe_code)]` read
//!    as a live forbid. See [`scan_sites`] and [`root_unsafe_code_levels`].
//!
//! Every crate escapes the workspace `forbid` the moment its manifest omits
//! `[lints] workspace = true`, which is invisible at the crate root — so that
//! inheritance is checked per member rather than assumed. That check reads the
//! manifest BY SECTION, via [`crate::topology::scan_manifest`], because the
//! substring form it replaced was the same class of bug as (4) one layer down:
//! `text.contains("[lints]") && text.contains("workspace = true")` is satisfied
//! by a commented-out lint table, and by any crate carrying its own `[lints]`
//! beside an idiomatic `dep = { workspace = true }`. Both were run against the
//! real tree: this checker called the crate clean and exited 0 while
//! `topology-check`, which had always parsed by section, failed the same tree
//! with `lints_not_inherited`. Two checkers reading one fact must not be able to
//! disagree, so there is now one reader.
//!
//! The same rule had to be applied twice more, one level up, to the WORKSPACE
//! manifest. Two facts were being read out of it as raw text:
//!
//! * the workspace lint default, via `ws_text.contains("unsafe_code =
//!   \"forbid\"")`. That was already vacuous on this repository: `Cargo.toml`
//!   opens with a prose comment reading ``Workspace-level `unsafe_code =
//!   "forbid"` ``, so the substring was present regardless of what the lint
//!   table said, and deleting both live lint lines left the check passing.
//!   Every claim this project makes about memory safety being structural
//!   rested on it. It was wrong in the other direction too, rejecting
//!   `unsafe_code='forbid'` and accepting the level under
//!   `[workspace.lints.clippy]` or a package-level `[lints.rust]`.
//! * the member roster, via a line scan taking the first double-quoted span on
//!   each line between `members` and a line beginning `]`. A roster in TOML
//!   literal quotes, or written on one line, or with two entries sharing a
//!   line, came back short or EMPTY -- and an empty roster meant every
//!   "0 sites, 0 orphans" conclusion below was quantified over nothing while
//!   the run exited 0.
//!
//! Both are now read as data from a single [`crate::toml::parse`] of the
//! manifest, the roster through the same resolver `appendix_a` uses, and an
//! empty roster is itself a violation. That last part is doctrine #2 applied
//! where it was missing: the scanner self-test licenses a zero-SITE result,
//! but nothing licensed a zero-CRATE one.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::toml::{self, get_str, get_str_array, get_table, get_table_array};

/// Repo-relative location of the ledger.
pub const LEDGER_PATH: &str = "registries/unsafe_boundary_ledger.toml";

/// Per-tool, per-site dynamic-verification posture.
pub const VERIFICATION_LANES_PATH: &str = "registries/unsafe_verification_lanes.toml";

/// Closed verification-tool universe for the unsafe boundary.
pub const VERIFICATION_TOOLS: [&str; 3] = ["miri", "asan", "tsan"];

/// Every current lane executes on the native x86-64 Linux posture that owns
/// the SIMD and inline-assembly sites. A free-form target field would be
/// decorative: the runner executes this target, so the manifest must say this
/// target or fail.
const LANE_TARGET: &str = "x86_64-unknown-linux-gnu";

/// The one registered gate that consumes checked cells.
const LANE_RUNNER: &str = "scripts/w1_unsafe_tool_lanes.sh";

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
/// The fixture is deliberately awkward, and every awkward case is a form that
/// has to be counted or has to be ignored for a real reason:
///
/// * counted — the plain attribute; a multi-argument attribute; an allow nested
///   inside `cfg_attr` on one line and again spread across four lines; an
///   `expect`, which suppresses `deny` exactly as `allow` does; and an inner
///   `#!` attribute, which relaxes a whole module at once.
/// * ignored — an occurrence in a comment; one inside a string literal; a
///   `dead_code` allow; a bare `deny`; a `cfg_attr`-wrapped **forbid**, which
///   has the shape of the evasion but tightens rather than relaxes; and a `doc`
///   attribute whose *string* names an allow.
///
/// The last two are the ones that keep the matcher honest. A scanner that
/// answered "does `unsafe_code` appear inside a `cfg_attr`" would count the
/// forbid; one that answered "does the text contain `allow(unsafe_code)`" would
/// count the doc string. Both are wrong in the direction that puts a bogus row
/// in the ledger, and the ledger's whole value is that its rows mean something.
pub fn scanner_fixture() -> String {
    let hash = '#';
    let allow = format!("{hash}[allow(unsafe_code)]");
    let allow_multi = format!("{hash}[allow(unsafe_code, clippy::undocumented_unsafe_blocks)]");
    let cfg_allow = format!("{hash}[cfg_attr(target_arch = \"x86_64\", allow(unsafe_code))]");
    let cfg_allow_multiline = format!(
        "{hash}[cfg_attr(\n    all(target_arch = \"aarch64\", target_feature = \"neon\"),\n    allow(unsafe_code)\n)]"
    );
    let expect_attr = format!("{hash}[expect(unsafe_code)]");
    let dead = format!("{hash}[allow(dead_code)]");
    let deny = format!("{hash}[deny(unsafe_code)]");
    let cfg_forbid = format!("{hash}[cfg_attr(feature = \"paranoid\", forbid(unsafe_code))]");
    let doc_decoy = format!("{hash}[doc = \"write allow(unsafe_code) at the site, never here\"]");
    let inner_allow = format!("{hash}![allow(unsafe_code)]");
    format!(
        "{allow}\nunsafe fn one() {{}}\n\n\
         // {allow}  <- a comment, must NOT count\n\
         const NOT_A_SITE: &str = \"{allow}\";\n\n\
         {allow_multi}\nunsafe fn two() {{}}\n\n\
         {cfg_allow}\nunsafe fn three() {{}}\n\n\
         {cfg_allow_multiline}\nunsafe fn four() {{}}\n\n\
         {expect_attr}\nunsafe fn five() {{}}\n\n\
         {dead}\n{deny}\n{cfg_forbid}\n{doc_decoy}\nfn not_a_site() {{}}\n\n\
         mod inner {{\n    {inner_allow}\n    unsafe fn six() {{}}\n}}\n"
    )
}

/// The exact number of real sites in [`scanner_fixture`].
pub const SCANNER_FIXTURE_SITES: usize = 6;

/// The symbol recorded for an inner (`#!`) attribute.
///
/// An inner attribute has no following item to name: it relaxes everything up
/// to the end of the enclosing module. Reporting the next line as the symbol
/// would understate that scope in the ledger row a reviewer then reads, which
/// is evidence inflation written into the enforcement surface itself.
pub const MODULE_SCOPE_SYMBOL: &str = "<module scope>";

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
    /// Exactly one structured no-claim boundary for every tool in
    /// [`VERIFICATION_TOOLS`], encoded as `tool|disposition|rationale`.
    pub tool_no_claim_boundaries: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsafeLedger {
    pub schema_version: i64,
    pub islands: Vec<Island>,
    pub sites: Vec<LedgerSite>,
}

/// One dynamic-verification lane. `checked` means at least one cell has a live
/// workload; `declared` means every cell remains either a candidate or an
/// explicit technical exclusion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationLane {
    pub tool: String,
    pub status: String,
    pub target: String,
    pub required_components: Vec<String>,
    pub runner: String,
    pub no_claim_boundary: String,
}

/// One site x tool cell in the complete verification matrix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerificationCell {
    pub site_row_id: String,
    pub tool: String,
    pub disposition: String,
    pub rationale: String,
    pub workload: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsafeVerificationLanes {
    pub schema_version: i64,
    pub lanes: Vec<VerificationLane>,
    pub cells: Vec<VerificationCell>,
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
            tool_no_claim_boundaries: get_str_array(row, "tool_no_claim_boundaries", &ctx)
                .map_err(read)?,
        });
    }
    Ok(UnsafeLedger {
        schema_version,
        islands,
        sites,
    })
}

pub fn load_verification_lanes(path: &Path) -> Result<UnsafeVerificationLanes, LoadError> {
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
    let version =
        crate::toml::get_int(&table, "schema_version", "verification_lanes").map_err(read)?;
    let mut lanes = Vec::new();
    for (index, row) in get_table_array(&table, "lane", "verification_lanes")
        .map_err(read)?
        .into_iter()
        .enumerate()
    {
        let context = format!("lane[{index}]");
        lanes.push(VerificationLane {
            tool: get_str(row, "tool", &context).map_err(read)?,
            status: get_str(row, "status", &context).map_err(read)?,
            target: get_str(row, "target", &context).map_err(read)?,
            required_components: get_str_array(row, "required_components", &context)
                .map_err(read)?,
            runner: get_str(row, "runner", &context).map_err(read)?,
            no_claim_boundary: get_str(row, "no_claim_boundary", &context).map_err(read)?,
        });
    }
    let mut cells = Vec::new();
    for (index, row) in get_table_array(&table, "cell", "verification_lanes")
        .map_err(read)?
        .into_iter()
        .enumerate()
    {
        let context = format!("cell[{index}]");
        cells.push(VerificationCell {
            site_row_id: get_str(row, "site_row_id", &context).map_err(read)?,
            tool: get_str(row, "tool", &context).map_err(read)?,
            disposition: get_str(row, "disposition", &context).map_err(read)?,
            rationale: get_str(row, "rationale", &context).map_err(read)?,
            workload: get_str(row, "workload", &context).map_err(read)?,
        });
    }
    Ok(UnsafeVerificationLanes {
        schema_version: version,
        lanes,
        cells,
    })
}

/// The lint levels that leave `unsafe` COMPILING under a `deny(unsafe_code)`
/// island root. `deny` and `forbid` are the only two that do not, which is why
/// they are the only two missing here.
///
/// `expect` and `warn` belong on this list for exactly the reason `allow` does:
/// a level weaker than `deny` *is* a relaxation of `deny`, whatever it is
/// called. An `expect` compiles the item silently; a `warn` compiles it with a
/// diagnostic that only fails a lane running `-D warnings`. Either way the
/// unsafe surface grew without a ledger row, which is the single outcome this
/// file exists to make impossible.
const LEVELS_BELOW_DENY: [&str; 3] = ["allow", "expect", "warn"];

/// How far an attribute may run before the scanner stops following it. A real
/// attribute is a line or two; the cap only bounds the damage from a malformed
/// one, and an attribute that never closes inside it is still checked over
/// everything read, so the failure direction is a spurious site rather than a
/// missed one.
const MAX_ATTRIBUTE_LINES: usize = 64;

/// Find every site in one source text that relaxes `unsafe_code`.
///
/// The file is masked ONCE — comments and string, raw-string and character
/// literals blanked, every other byte left where it was — and attributes are
/// then found in that masked text **at any column**. Both halves of that were
/// wrong in the scanner this replaced, which decided candidacy from the trimmed
/// prefix of a line and so could only ever see an attribute that began one:
///
/// * **Fail-open.** `impl T { #[allow(unsafe_code)] unsafe fn f() {} }` scanned
///   to zero sites. Sharing a line is valid Rust in item, statement and
///   expression position, and rustfmt does not normalise it inside a macro
///   body. Inside an island — whose root is `deny`, which an inner `allow` CAN
///   lower — that placement compiles unsafe code with no ledger row, no
///   `site_unledgered`, and no `unsafe_allow_outside_island`. It is the
///   `cfg_attr` bypass one layer out: the attribute BODY was already read
///   structurally, but only on the lines the scan deigned to consider.
/// * **Fail-closed but harmful.** Text the compiler cannot see was counted
///   anyway. A block-commented attribute, and one inside a multi-line string or
///   raw string, all begin their line and all produced a site. The masker
///   already understood every one of those constructs; it was simply run per
///   candidate, starting AT the candidate, so it could never know that the line
///   it was handed was already inside a `/*` opened above it. Inventing a site
///   is the direction that puts a bogus row in the ledger, and the ledger's
///   whole value is that its rows mean something.
///
/// Everything downstream of candidacy is unchanged: the attribute is followed
/// to its matching `]`, and it counts if any of [`LEVELS_BELOW_DENY`] names
/// `unsafe_code` at any depth inside it.
pub fn scan_sites(path: &str, text: &str) -> Vec<ScannedSite> {
    let raw: Vec<&str> = text.lines().collect();
    let masked = mask_source(text);
    let mut out = Vec::new();
    for attribute in find_attributes(&masked) {
        if !body_relaxes_unsafe_code(&attribute.body) {
            continue;
        }
        // The symbol is what a reviewer reads to know what the site covers: for
        // an outer attribute the item it precedes, for an inner one the module
        // it sits inside, which is broader and must not be reported as narrower.
        let symbol = if attribute.inner {
            MODULE_SCOPE_SYMBOL.to_owned()
        } else {
            symbol_after(&masked, &raw, attribute.after)
        };
        out.push(ScannedSite {
            path: path.to_owned(),
            line: attribute.line,
            symbol,
        });
    }
    out
}

/// One attribute, located structurally in the masked text rather than by the
/// shape of the line it happens to sit on.
struct Attribute {
    /// 1-based line the `#` sits on.
    line: usize,
    /// `true` for an inner (`#!`) attribute, which covers a whole module.
    inner: bool,
    /// The text between the outermost brackets, masked.
    body: String,
    /// Byte offset just past the closing `]`.
    after: usize,
}

/// A source text with its comments and literals blanked out, plus the offset at
/// which each line begins.
///
/// The mask is byte-exact: a blanked character is replaced by as many spaces as
/// it occupied, so an offset in the masked text names the same column of the
/// same source line, and a raw line cut at one can never be split inside a
/// character.
pub struct Masked {
    text: String,
    line_starts: Vec<usize>,
}

impl Masked {
    /// The masked text.
    ///
    /// It has the same line count as the source and every line has the same
    /// byte length, so `masked.text().lines().zip(source.lines())` pairs each
    /// line with its own mask and an offset found in one names the same column
    /// of the other. Read structure from the mask and values from the source:
    /// that is what lets a caller keep a string literal's CONTENT (blanked in
    /// the mask) while still refusing to read a comment.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The 1-based line containing `offset`.
    fn line_of(&self, offset: usize) -> usize {
        self.line_starts.partition_point(|start| *start <= offset)
    }
}

/// Mask a whole source file in one pass, so comment and literal state carries
/// across lines. Running the masker per candidate — which is what this scanner
/// used to do — cannot see that a line is already inside a `/*` opened three
/// lines above it, which is why commented-out attributes counted as real sites.
///
/// This is the ONE reader for "which bytes of this Rust source are live code".
/// It is public because `validate`'s active-arm scanner needs the same fact
/// about `refs.rs` that this module needs about every crate source, and the
/// separate comment-handling it had instead counted a commented-out match arm
/// as a live one. Two readers of one fact is how a fixed reader stays fixed
/// while its twin rots.
pub fn mask_source(text: &str) -> Masked {
    let mut out = String::with_capacity(text.len());
    let mut line_starts = Vec::new();
    let mut state = MaskState::Code;
    for (index, line) in text.lines().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        line_starts.push(out.len());
        mask_line(line, &mut state, &mut out);
    }
    Masked {
        text: out,
        line_starts,
    }
}

/// Every attribute in the masked text, in source order.
///
/// The search advances one byte at a time rather than jumping past each
/// attribute it finds: a malformed attribute that never closes must not be able
/// to hide the rest of the file behind it.
fn find_attributes(masked: &Masked) -> Vec<Attribute> {
    let bytes = masked.text.as_bytes();
    let mut out = Vec::new();
    for at in 0..bytes.len() {
        let Some((inner, open)) = attribute_open(bytes, at) else {
            continue;
        };
        let close = matching_bracket(bytes, open);
        out.push(Attribute {
            line: masked.line_of(at),
            inner,
            body: masked.text[open + 1..close].to_owned(),
            after: (close + 1).min(bytes.len()),
        });
    }
    out
}

/// Does an attribute open at `at`? Yields whether it is inner, and the offset
/// of its opening bracket.
///
/// This runs over MASKED text, where the `#` of a raw string (`r#"…"#`) is the
/// only other `#` Rust admits and is never followed by `[`.
fn attribute_open(bytes: &[u8], at: usize) -> Option<(bool, usize)> {
    if bytes.get(at) != Some(&b'#') {
        return None;
    }
    match bytes.get(at + 1) {
        Some(b'[') => Some((false, at + 1)),
        Some(b'!') if bytes.get(at + 2) == Some(&b'[') => Some((true, at + 2)),
        _ => None,
    }
}

/// The offset of the `]` closing the bracket at `open`.
///
/// Brackets inside comments and literals are already blanked, so only real ones
/// are counted — without that a `]` in a string truncates the body early (a
/// missed site). An attribute that has not closed within [`MAX_ATTRIBUTE_LINES`]
/// stops there and is still checked over everything read, so the failure
/// direction for a malformed attribute is a spurious site, never a missed one.
fn matching_bracket(bytes: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    let mut lines = 0usize;
    for (index, byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'\n' => {
                lines += 1;
                if lines >= MAX_ATTRIBUTE_LINES {
                    return index;
                }
            }
            b'[' => depth += 1,
            b']' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    return index;
                }
            }
            _ => {}
        }
    }
    bytes.len()
}

/// The item an outer attribute applies to: the first code following it, with
/// any further attributes stepped over as whole spans, so that a multi-line one
/// cannot leave its own arguments standing in for the item.
///
/// The symbol is cut from the RAW line at the item's own column rather than
/// taken as the whole trimmed line, so an attribute sharing a line with its
/// item names the item and not everything printed to the left of it. Blank
/// lines and comments need no special case here: the masker has already turned
/// them into whitespace.
fn symbol_after(masked: &Masked, raw: &[&str], after: usize) -> String {
    let bytes = masked.text.as_bytes();
    let mut at = after;
    loop {
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        match attribute_open(bytes, at) {
            Some((_, open)) => at = matching_bracket(bytes, open) + 1,
            None => break,
        }
    }
    if at >= bytes.len() {
        return String::new();
    }
    let line = masked.line_of(at);
    let column = at - masked.line_starts[line - 1];
    raw.get(line - 1)
        .and_then(|text| text.get(column..))
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Does this attribute body set `unsafe_code` to a level below `deny`?
fn body_relaxes_unsafe_code(body: &str) -> bool {
    LEVELS_BELOW_DENY
        .iter()
        .any(|level| body_sets_unsafe_code(body, level))
}

/// Does this attribute body set `unsafe_code` to `level`, at any nesting depth?
///
/// `level` is matched as a whole identifier immediately followed by its
/// parenthesised argument list, and `unsafe_code` as a whole identifier inside
/// that list, so `dead_code` never reads as `unsafe_code`, `disallow` never as
/// `allow`, and a `cfg_attr` wrapper is transparent.
fn body_sets_unsafe_code(body: &str, level: &str) -> bool {
    let bytes = body.as_bytes();
    for at in standalone_idents(body, level) {
        let mut open = at + level.len();
        while bytes.get(open).is_some_and(u8::is_ascii_whitespace) {
            open += 1;
        }
        if bytes.get(open) != Some(&b'(') {
            continue;
        }
        let close = matching_paren(bytes, open);
        if !standalone_idents(&body[open + 1..close], "unsafe_code").is_empty() {
            return true;
        }
    }
    false
}

/// Every lint level an INNER attribute at a crate root sets for `unsafe_code`.
///
/// This is the one reader for "what does this crate root declare about
/// `unsafe_code`". It exists because `topology.rs` was answering that question
/// with whole-line string equality against `#![forbid(unsafe_code)]`, which read
/// as "declares nothing" for every ordinary respelling — a trailing comment, a
/// lint grouped with a sibling, or inner spacing — while this module was already
/// parsing attributes structurally two functions away. Two readers of one fact
/// is how a fixed reader stays fixed while its twin rots.
///
/// Only inner (`#!`) attributes are considered: an outer `#[forbid(...)]` binds
/// one item, not the crate, and reporting it as a crate-root policy would
/// overstate what the root actually guarantees.
///
/// It reads the SAME masked, any-column attribute stream [`scan_sites`] does,
/// because the line-anchored candidacy test was wrong here in the direction that
/// matters most: `/*\n#![forbid(unsafe_code)]\n*/` — a root policy that has been
/// commented out — read as a live `forbid`, so `topology`'s `root_forbids_unsafe`
/// said the crate was forbidding unsafe while the compiler had been told nothing
/// at all.
pub fn root_unsafe_code_levels(text: &str) -> BTreeSet<String> {
    const ALL_LEVELS: [&str; 5] = ["allow", "expect", "warn", "deny", "forbid"];
    let masked = mask_source(text);
    let mut out = BTreeSet::new();
    for attribute in find_attributes(&masked).iter().filter(|a| a.inner) {
        for level in ALL_LEVELS {
            if body_sets_unsafe_code(&attribute.body, level) {
                out.insert(level.to_owned());
            }
        }
    }
    out
}

/// Byte offsets at which `needle` appears as a whole identifier, so
/// `dead_code` never reads as `unsafe_code` and `disallow` never as `allow`.
fn standalone_idents(hay: &str, needle: &str) -> Vec<usize> {
    let bytes = hay.as_bytes();
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(found) = hay[from..].find(needle) {
        let at = from + found;
        from = at + needle.len();
        let before = at == 0 || !ident(bytes[at - 1]);
        let after = bytes.get(from).is_none_or(|b| !ident(*b));
        if before && after {
            out.push(at);
        }
    }
    out
}

/// The offset of the `)` closing `open`, or the end of the slice when the
/// attribute is malformed — checking too much beats stopping early.
fn matching_paren(bytes: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'(' => depth += 1,
            b')' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    return index;
                }
            }
            _ => {}
        }
    }
    bytes.len()
}

/// Where the masker is inside a construct that spans lines.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MaskState {
    Code,
    Block(usize),
    Str,
    RawStr(usize),
}

/// Copy one line into `out` with the CONTENT of comments, strings, and
/// character literals replaced by spaces. Delimiters survive so the result
/// still reads as code, and every other byte keeps its position, so an offset
/// found in the masked text names the same place in the source.
fn mask_line(text: &str, state: &mut MaskState, out: &mut String) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match *state {
            MaskState::Block(depth) => {
                if c == '*' && next == Some('/') {
                    blank(out, &chars[i..i + 2]);
                    i += 2;
                    *state = if depth <= 1 {
                        MaskState::Code
                    } else {
                        MaskState::Block(depth - 1)
                    };
                } else if c == '/' && next == Some('*') {
                    blank(out, &chars[i..i + 2]);
                    i += 2;
                    *state = MaskState::Block(depth + 1);
                } else {
                    blank(out, &chars[i..i + 1]);
                    i += 1;
                }
            }
            MaskState::Str => {
                if c == '\\' {
                    blank(out, &chars[i..chars.len().min(i + 2)]);
                    i += 2;
                } else if c == '"' {
                    out.push('"');
                    i += 1;
                    *state = MaskState::Code;
                } else {
                    blank(out, &chars[i..i + 1]);
                    i += 1;
                }
            }
            MaskState::RawStr(hashes) => {
                let closes = c == '"'
                    && i + 1 + hashes <= chars.len()
                    && chars[i + 1..i + 1 + hashes].iter().all(|c| *c == '#');
                if closes {
                    out.push('"');
                    blank(out, &chars[i + 1..i + 1 + hashes]);
                    i += 1 + hashes;
                    *state = MaskState::Code;
                } else {
                    blank(out, &chars[i..i + 1]);
                    i += 1;
                }
            }
            MaskState::Code => {
                if c == '/' && next == Some('/') {
                    blank(out, &chars[i..]);
                    i = chars.len();
                } else if c == '/' && next == Some('*') {
                    blank(out, &chars[i..i + 2]);
                    i += 2;
                    *state = MaskState::Block(1);
                } else if let Some((consumed, hashes)) = raw_string_open(&chars, i) {
                    for prefix in &chars[i..i + consumed] {
                        out.push(*prefix);
                    }
                    i += consumed;
                    *state = MaskState::RawStr(hashes);
                } else if c == '"' {
                    out.push('"');
                    i += 1;
                    *state = MaskState::Str;
                } else if c == '\'' {
                    // `'a` is a lifetime and must stay code; `'x'` is a literal
                    // and must not, or a `'"'` opens a string that swallows the
                    // rest of the attribute.
                    match char_literal_len(&chars, i) {
                        Some(len) => {
                            out.push('\'');
                            blank(out, &chars[i + 1..i + len - 1]);
                            out.push('\'');
                            i += len;
                        }
                        None => {
                            out.push('\'');
                            i += 1;
                        }
                    }
                } else {
                    out.push(c);
                    i += 1;
                }
            }
        }
    }
}

/// Replace `masked` with one space per BYTE it occupied.
///
/// Byte-exactness is load-bearing, not tidiness: [`symbol_after`] cuts a raw
/// source line at an offset found in the masked text, and a mask that collapsed
/// a multi-byte character to a single space would move every offset after it —
/// far enough to slice a line inside a character, which panics.
fn blank(out: &mut String, masked: &[char]) {
    for c in masked {
        for _ in 0..c.len_utf8() {
            out.push(' ');
        }
    }
}

/// If a raw string opens at `i`, the chars consumed through its opening quote
/// and its hash count. `r`/`br` only start one when they are not the tail of a
/// longer identifier.
fn raw_string_open(chars: &[char], i: usize) -> Option<(usize, usize)> {
    if chars[i] != 'r' && chars[i] != 'b' {
        return None;
    }
    if i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_') {
        return None;
    }
    let mut j = i;
    if chars[j] == 'b' {
        j += 1;
    }
    if chars.get(j) != Some(&'r') {
        return None;
    }
    j += 1;
    let mut hashes = 0;
    while chars.get(j + hashes) == Some(&'#') {
        hashes += 1;
    }
    if chars.get(j + hashes) != Some(&'"') {
        return None;
    }
    Some((j + hashes + 1 - i, hashes))
}

/// The length of a character literal starting at `i`, or `None` for a lifetime.
fn char_literal_len(chars: &[char], i: usize) -> Option<usize> {
    if chars.get(i + 1) == Some(&'\\') {
        // The char after the backslash is the escape selector and is never the
        // terminator, which is the whole difficulty of `'\''`.
        let limit = chars.len().min(i + 14);
        return (i + 3..limit)
            .find(|j| chars[*j] == '\'')
            .map(|end| end - i + 1);
    }
    if chars.get(i + 2) == Some(&'\'') {
        return Some(3);
    }
    None
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

fn parse_tool_boundary(value: &str) -> Option<(&str, &str, &str)> {
    let mut parts = value.splitn(3, '|');
    let tool = parts.next()?.trim();
    let disposition = parts.next()?.trim();
    let rationale = parts.next()?.trim();
    if tool.is_empty() || disposition.is_empty() || rationale.is_empty() {
        None
    } else {
        Some((tool, disposition, rationale))
    }
}

fn safe_repo_relative(path: &str) -> bool {
    !path.trim().is_empty()
        && !Path::new(path).is_absolute()
        && !Path::new(path).components().any(|component| {
            matches!(
                component,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        })
}

fn required_lane_components(tool: &str) -> BTreeSet<&'static str> {
    match tool {
        "miri" => ["miri", "rust-src"].into_iter().collect(),
        "asan" | "tsan" => ["rust-src", "llvm-tools-preview"].into_iter().collect(),
        _ => BTreeSet::new(),
    }
}

fn pinned_toolchain_components(root: &Path, out: &mut Vec<Violation>) -> BTreeSet<String> {
    let path = root.join("rust-toolchain.toml");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            out.push(Violation::new(
                "unsafe_lane_toolchain_unreadable",
                "rust-toolchain.toml",
                path.display().to_string(),
                format!(
                    "cannot read the pinned toolchain, so lane component availability is \
                     unknown: {error}"
                ),
            ));
            return BTreeSet::new();
        }
    };
    let table = match toml::parse(&text) {
        Ok(table) => table,
        Err(error) => {
            out.push(Violation::new(
                "unsafe_lane_toolchain_unreadable",
                "rust-toolchain.toml",
                path.display().to_string(),
                format!(
                    "cannot parse the pinned toolchain, so lane component availability is \
                     unknown: {error}"
                ),
            ));
            return BTreeSet::new();
        }
    };
    let toolchain = match get_table(&table, "toolchain", "rust-toolchain.toml") {
        Ok(toolchain) => toolchain,
        Err(error) => {
            out.push(Violation::new(
                "unsafe_lane_toolchain_unreadable",
                "rust-toolchain.toml",
                "toolchain",
                error.to_string(),
            ));
            return BTreeSet::new();
        }
    };
    match get_str_array(toolchain, "components", "rust-toolchain.toml.toolchain") {
        Ok(components) => components.into_iter().collect(),
        Err(error) => {
            out.push(Violation::new(
                "unsafe_lane_toolchain_unreadable",
                "rust-toolchain.toml",
                "toolchain.components",
                error.to_string(),
            ));
            BTreeSet::new()
        }
    }
}

fn live_script_artifacts(
    root: &Path,
    requested: &BTreeSet<&str>,
    out: &mut Vec<Violation>,
) -> BTreeSet<String> {
    let path = root.join("registries/checker_index.toml");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) => {
            out.push(Violation::new(
                "unsafe_lane_checker_index_unreadable",
                "checker_index.toml",
                path.display().to_string(),
                format!("cannot read the checker index that must own each lane runner: {error}"),
            ));
            return BTreeSet::new();
        }
    };
    let table = match toml::parse(&text) {
        Ok(table) => table,
        Err(error) => {
            out.push(Violation::new(
                "unsafe_lane_checker_index_unreadable",
                "checker_index.toml",
                path.display().to_string(),
                format!("cannot parse the checker index that must own each lane runner: {error}"),
            ));
            return BTreeSet::new();
        }
    };
    let checkers = match crate::model::checker_index_from(&table) {
        Ok(checkers) => checkers,
        Err(error) => {
            out.push(Violation::new(
                "unsafe_lane_checker_index_unreadable",
                "checker_index.toml",
                path.display().to_string(),
                error.to_string(),
            ));
            return BTreeSet::new();
        }
    };

    let self_test = crate::liveness::self_test();
    if !self_test.licensed() {
        out.push(Violation::new(
            "unsafe_lane_liveness_self_test_failed",
            "checker_index.toml",
            "<self-test>",
            format!(
                "the checker-liveness reader got {} of {} known answers wrong ({}); no \
                 lane runner may be reported live",
                self_test.failures.len(),
                self_test.cases,
                self_test.failures.join(", ")
            ),
        ));
        return BTreeSet::new();
    }

    let prover = crate::liveness::Prover::new(root);
    let mut live = BTreeSet::new();
    let mut rejected: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for checker in checkers {
        if !matches!(
            (checker.kind.as_str(), checker.status.as_str()),
            ("script", "live")
        ) || !requested.contains(checker.artifact.as_str())
        {
            continue;
        }
        let defects = prover.assess(&checker);
        if defects.is_empty() {
            live.insert(checker.artifact);
        } else {
            rejected.entry(checker.artifact).or_default().extend(
                defects
                    .into_iter()
                    .map(|defect| format!("{}: {}", defect.kind.code(), defect.detail)),
            );
        }
    }
    for (artifact, reasons) in rejected {
        if !live.contains(&artifact) {
            out.push(Violation::new(
                "unsafe_lane_runner_not_live",
                &artifact,
                "checker_index.toml",
                format!(
                    "the registered lane runner failed the authoritative liveness proof: {}",
                    reasons.join("; ")
                ),
            ));
        }
    }
    live
}

fn verify_verification_lanes(
    root: &Path,
    ledger: &UnsafeLedger,
    lanes: &UnsafeVerificationLanes,
    report: &mut Report,
    out: &mut Vec<Violation>,
) {
    report.verification_lanes = lanes.lanes.len();
    report.verification_cells = lanes.cells.len();
    report.checked_cells = lanes
        .cells
        .iter()
        .filter(|cell| matches!(cell.disposition.as_str(), "checked"))
        .count();
    report.candidate_cells = lanes
        .cells
        .iter()
        .filter(|cell| matches!(cell.disposition.as_str(), "candidate"))
        .count();
    report.excluded_cells = lanes
        .cells
        .iter()
        .filter(|cell| matches!(cell.disposition.as_str(), "excluded"))
        .count();

    match lanes.schema_version {
        1 => {}
        other => out.push(Violation::new(
            "unsafe_lane_schema_version_unknown",
            VERIFICATION_LANES_PATH,
            other.to_string(),
            "unknown unsafe verification-lane schema_version",
        )),
    }

    let required_tools: BTreeSet<&str> = VERIFICATION_TOOLS.into_iter().collect();
    let pinned_components = pinned_toolchain_components(root, out);
    let requested_runners: BTreeSet<&str> = lanes
        .lanes
        .iter()
        .map(|lane| lane.runner.as_str())
        .collect();
    let live_scripts = live_script_artifacts(root, &requested_runners, out);
    let mut lane_by_tool = BTreeMap::new();
    for lane in &lanes.lanes {
        if !required_tools.contains(lane.tool.as_str()) {
            out.push(Violation::new(
                "unsafe_lane_tool_unknown",
                &lane.tool,
                VERIFICATION_LANES_PATH,
                "tool must be exactly miri|asan|tsan",
            ));
        }
        if lane_by_tool.insert(lane.tool.as_str(), lane).is_some() {
            out.push(Violation::new(
                "unsafe_lane_tool_duplicate",
                &lane.tool,
                VERIFICATION_LANES_PATH,
                "each verification tool must own exactly one lane",
            ));
        }
        if !matches!(lane.status.as_str(), "checked" | "declared") {
            out.push(Violation::new(
                "unsafe_lane_status_unknown",
                &lane.tool,
                &lane.status,
                "lane status must be checked|declared",
            ));
        }
        match lane.target.as_str() {
            LANE_TARGET => {}
            other => out.push(Violation::new(
                "unsafe_lane_target_mismatch",
                &lane.tool,
                other,
                format!(
                    "the unsafe runner executes {LANE_TARGET}; the lane target may not \
                     drift from the workload it describes"
                ),
            )),
        }
        match lane.runner.as_str() {
            LANE_RUNNER => {}
            other => out.push(Violation::new(
                "unsafe_lane_runner_mismatch",
                &lane.tool,
                other,
                format!("every unsafe lane must name the consuming gate {LANE_RUNNER}"),
            )),
        }
        for (field, value) in [
            ("target", lane.target.as_str()),
            ("runner", lane.runner.as_str()),
            ("no_claim_boundary", lane.no_claim_boundary.as_str()),
        ] {
            if value.trim().is_empty() {
                out.push(Violation::new(
                    "unsafe_lane_field_vacuous",
                    &lane.tool,
                    field,
                    format!("{field} may not be empty"),
                ));
            }
        }
        if lane.required_components.is_empty() {
            out.push(Violation::new(
                "unsafe_lane_field_vacuous",
                &lane.tool,
                "required_components",
                "a lane must declare the complete pinned component set it requires",
            ));
        }
        let declared_components: BTreeSet<&str> = lane
            .required_components
            .iter()
            .map(String::as_str)
            .collect();
        let required_components = required_lane_components(&lane.tool);
        if !required_components.is_empty()
            && (!declared_components.iter().eq(required_components.iter())
                || !declared_components
                    .len()
                    .eq(&lane.required_components.len()))
        {
            out.push(Violation::new(
                "unsafe_lane_component_contract_mismatch",
                &lane.tool,
                "required_components",
                format!(
                    "declared components {:?} must exactly equal {:?}, without duplicates",
                    lane.required_components, required_components
                ),
            ));
        }
        for component in &lane.required_components {
            if !pinned_components.contains(component) {
                out.push(Violation::new(
                    "unsafe_lane_component_unpinned",
                    &lane.tool,
                    component,
                    "a lane component must be declared in rust-toolchain.toml",
                ));
            }
        }
        if !safe_repo_relative(&lane.runner) {
            out.push(Violation::new(
                "unsafe_lane_runner_path_unsafe",
                &lane.tool,
                &lane.runner,
                "lane runner must be a safe repository-relative path",
            ));
        } else {
            if !root.join(&lane.runner).is_file() {
                out.push(Violation::new(
                    "unsafe_lane_runner_missing",
                    &lane.tool,
                    &lane.runner,
                    "lane runner does not exist",
                ));
            }
            if !live_scripts.contains(&lane.runner) {
                out.push(Violation::new(
                    "unsafe_lane_runner_unregistered",
                    &lane.tool,
                    &lane.runner,
                    "lane runner must resolve as a live script in checker_index.toml",
                ));
            }
        }
    }
    for tool in required_tools.iter().copied() {
        if !lane_by_tool.contains_key(tool) {
            out.push(Violation::new(
                "unsafe_lane_tool_missing",
                tool,
                VERIFICATION_LANES_PATH,
                "the closed tool universe requires exactly one lane per tool",
            ));
        }
    }

    let site_ids: BTreeSet<&str> = ledger
        .sites
        .iter()
        .map(|site| site.row_id.as_str())
        .collect();
    let mut cell_by_key: BTreeMap<(&str, &str), &VerificationCell> = BTreeMap::new();
    let mut dispositions_by_tool: BTreeMap<&str, (usize, usize, usize)> = BTreeMap::new();
    for cell in &lanes.cells {
        let key = (cell.site_row_id.as_str(), cell.tool.as_str());
        if cell_by_key.insert(key, cell).is_some() {
            out.push(Violation::new(
                "unsafe_lane_cell_duplicate",
                &cell.site_row_id,
                &cell.tool,
                "each site x tool pair must have exactly one manifest cell",
            ));
        }
        if !site_ids.contains(cell.site_row_id.as_str()) {
            out.push(Violation::new(
                "unsafe_lane_cell_orphaned",
                &cell.site_row_id,
                &cell.tool,
                "manifest cell names no ledger site",
            ));
        }
        if !lane_by_tool.contains_key(cell.tool.as_str()) {
            out.push(Violation::new(
                "unsafe_lane_cell_tool_unresolved",
                &cell.site_row_id,
                &cell.tool,
                "manifest cell names no verification lane",
            ));
        }
        if !matches!(
            cell.disposition.as_str(),
            "checked" | "candidate" | "excluded"
        ) {
            out.push(Violation::new(
                "unsafe_lane_cell_disposition_unknown",
                &cell.site_row_id,
                &cell.disposition,
                "cell disposition must be checked|candidate|excluded",
            ));
        }
        if cell.rationale.trim().is_empty() {
            out.push(Violation::new(
                "unsafe_lane_cell_rationale_vacuous",
                &cell.site_row_id,
                &cell.tool,
                "every cell must state why its disposition is honest",
            ));
        }
        if matches!(cell.disposition.as_str(), "checked") && cell.workload.trim().is_empty() {
            out.push(Violation::new(
                "unsafe_lane_checked_cell_without_workload",
                &cell.site_row_id,
                &cell.tool,
                "a checked cell must name the exact workload the runner executes",
            ));
        }
        if !matches!(cell.disposition.as_str(), "checked") && !cell.workload.trim().is_empty() {
            out.push(Violation::new(
                "unsafe_lane_unchecked_cell_with_workload",
                &cell.site_row_id,
                &cell.tool,
                "candidate and excluded cells may not imply an unexecuted workload",
            ));
        }
        let counts = dispositions_by_tool
            .entry(cell.tool.as_str())
            .or_insert((0, 0, 0));
        match cell.disposition.as_str() {
            "checked" => counts.0 += 1,
            "candidate" => counts.1 += 1,
            "excluded" => counts.2 += 1,
            _ => {}
        }
    }

    for lane in &lanes.lanes {
        let (checked, candidate, _) = dispositions_by_tool
            .get(lane.tool.as_str())
            .copied()
            .unwrap_or_default();
        if !ledger.sites.is_empty()
            && matches!(lane.status.as_str(), "checked")
            && matches!(checked, 0)
        {
            out.push(Violation::new(
                "unsafe_lane_checked_without_cell",
                &lane.tool,
                "status",
                "a checked lane must own at least one checked cell",
            ));
        }
        if matches!(lane.status.as_str(), "declared") && !matches!(checked, 0) {
            out.push(Violation::new(
                "unsafe_lane_declared_with_checked_cell",
                &lane.tool,
                "status",
                "a lane with checked cells must itself be checked",
            ));
        }
        if !ledger.sites.is_empty()
            && matches!(lane.status.as_str(), "declared")
            && matches!(candidate, 0)
        {
            out.push(Violation::new(
                "unsafe_lane_declared_without_candidate",
                &lane.tool,
                "status",
                "a declared lane must identify at least one executable candidate",
            ));
        }
    }

    for site in &ledger.sites {
        let prose = site.no_claim_boundary.to_ascii_lowercase();
        let mut boundary_by_tool = BTreeMap::new();
        for encoded in &site.tool_no_claim_boundaries {
            let Some((tool, disposition, rationale)) = parse_tool_boundary(encoded) else {
                out.push(Violation::new(
                    "unsafe_ledger_tool_boundary_malformed",
                    &site.row_id,
                    encoded,
                    "tool boundary must be tool|disposition|non-empty rationale",
                ));
                continue;
            };
            if !required_tools.contains(tool) {
                out.push(Violation::new(
                    "unsafe_ledger_tool_boundary_unknown",
                    &site.row_id,
                    tool,
                    "tool boundary must name exactly miri|asan|tsan",
                ));
            }
            if !matches!(disposition, "checked" | "candidate" | "excluded") {
                out.push(Violation::new(
                    "unsafe_ledger_tool_boundary_disposition_unknown",
                    &site.row_id,
                    disposition,
                    "tool boundary disposition must be checked|candidate|excluded",
                ));
            }
            if boundary_by_tool
                .insert(tool, (disposition, rationale))
                .is_some()
            {
                out.push(Violation::new(
                    "unsafe_ledger_tool_boundary_duplicate",
                    &site.row_id,
                    tool,
                    "each ledger site must state exactly one boundary per tool",
                ));
            }
        }
        for tool in required_tools.iter().copied() {
            if !prose.contains(tool) {
                out.push(Violation::new(
                    "unsafe_ledger_tool_boundary_not_named_in_prose",
                    &site.row_id,
                    tool,
                    "the human no_claim_boundary must name every structured tool boundary",
                ));
            }
            let Some((disposition, rationale)) = boundary_by_tool.get(tool).copied() else {
                out.push(Violation::new(
                    "unsafe_ledger_tool_boundary_missing",
                    &site.row_id,
                    tool,
                    "each ledger site must state one structured boundary for every tool",
                ));
                continue;
            };
            let Some(cell) = cell_by_key.get(&(site.row_id.as_str(), tool)).copied() else {
                out.push(Violation::new(
                    "unsafe_lane_cell_missing",
                    &site.row_id,
                    tool,
                    "ledger boundary has no matching manifest cell",
                ));
                continue;
            };
            if !cell.disposition.as_str().eq(disposition) || !cell.rationale.as_str().eq(rationale)
            {
                out.push(Violation::new(
                    "unsafe_lane_cell_mismatch",
                    &site.row_id,
                    tool,
                    "manifest disposition and rationale must byte-match the ledger boundary",
                ));
            }
        }
    }
}

/// The machine-readable report the bead's acceptance criteria require.
#[derive(Debug, Default)]
pub struct Report {
    pub crates_scanned: usize,
    pub forbid_verdicts: BTreeMap<String, bool>,
    pub scanned_sites: Vec<ScannedSite>,
    pub orphan_rows: Vec<String>,
    pub scanner_self_test_sites: usize,
    /// Island crates whose safe-facing API was read.
    pub islands_api_scanned: usize,
    /// Source files read for the safe-facing API check.
    pub island_api_files: usize,
    /// Public items the API reader classified across those files. Reported
    /// because "0 violations" over 0 items is not a pass.
    pub island_public_items: usize,
    /// Findings the API reader reproduced in its own fixture.
    pub safe_facing_self_test_findings: usize,
    /// Dynamic unsafe-verification lane rows read from the live manifest.
    pub verification_lanes: usize,
    /// Complete site x tool matrix cells read from the live manifest.
    pub verification_cells: usize,
    pub checked_cells: usize,
    pub candidate_cells: usize,
    pub excluded_cells: usize,
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

    // --- the second control, for the second reader. The API reader concludes
    // --- "this island exports nothing unsafe", and that sentence is worthless
    // --- from a parser that cannot find anything. It reproduces its own
    // --- fixture — both halves of it: the violations it must catch AND the
    // --- near misses it must not, since a reader that rejected every asterisk
    // --- would pass a count-only check.
    let api_self_test = public_api(safe_facing_fixture());
    report.safe_facing_self_test_findings = api_self_test.findings.len();
    let api_trustworthy = api_self_test.findings.len() == SAFE_FACING_FIXTURE_FINDINGS
        && api_self_test.pub_tokens == SAFE_FACING_FIXTURE_PUB_TOKENS
        && api_self_test.pub_tokens_claimed == api_self_test.pub_tokens
        && api_self_test.parse_failures.is_empty();
    if !api_trustworthy {
        v.push(Violation::new(
            "safe_facing_self_test_failed",
            "unsafe_ledger",
            SAFE_FACING_FIXTURE_FINDINGS.to_string(),
            format!(
                "the safe-facing API reader found {} findings ({} expected) and claimed {} of \
                 {} pub tokens ({} parse failures) in its own fixture: every \"this island \
                 exports nothing unsafe\" below would be unlicensed, so the run fails rather \
                 than reporting a clean safe-facing boundary",
                api_self_test.findings.len(),
                SAFE_FACING_FIXTURE_FINDINGS,
                api_self_test.pub_tokens_claimed,
                api_self_test.pub_tokens,
                api_self_test.parse_failures.len(),
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
                format!(
                    "cannot read the workspace manifest, so no boundary claim can be made: {e}"
                ),
            ));
            return (report, v);
        }
    };
    // The manifest is parsed ONCE, structurally, and both facts this checker
    // needs from it -- the workspace lint level and the member roster -- are
    // read off that one parse. An unparseable workspace manifest is a failure,
    // never a skip: it is the root of every claim made below it.
    let ws_table = match toml::parse(&ws_text) {
        Ok(t) => t,
        Err(e) => {
            v.push(Violation::new(
                "workspace_manifest_unparseable",
                "Cargo.toml",
                ws_path.display().to_string(),
                format!(
                    "cannot parse the workspace manifest, so neither the lint default nor \
                     the member roster is known and no boundary claim can be made: {e}"
                ),
            ));
            return (report, v);
        }
    };
    let ws_section = match ws_table.get("workspace") {
        Some(crate::toml::Value::Table(t)) => t,
        _ => {
            v.push(Violation::new(
                "workspace_section_absent",
                "Cargo.toml",
                "workspace",
                "the manifest declares no [workspace] section, so there is no workspace \
                 lint default and no member roster to scan",
            ));
            return (report, v);
        }
    };

    match workspace_unsafe_lint_level(ws_section) {
        Some(level) if level == "forbid" => {}
        other => v.push(Violation::new(
            "workspace_forbid_absent",
            "Cargo.toml",
            "workspace.lints.rust",
            format!(
                "the workspace default must be unsafe_code = \"forbid\"; found {}. forbid \
                 cannot be lowered, which is the whole reason islands are separate crates",
                match &other {
                    Some(level) => format!("{level:?}"),
                    None => "no [workspace.lints.rust] unsafe_code entry".to_owned(),
                }
            ),
        )),
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

    let verification_lanes_path = root.join(VERIFICATION_LANES_PATH);
    let verification_lanes = match load_verification_lanes(&verification_lanes_path) {
        Ok(lanes) => lanes,
        Err(error) => {
            v.push(Violation::new(
                "unsafe_verification_lanes_absent_or_unreadable",
                VERIFICATION_LANES_PATH,
                error.path.clone(),
                format!(
                    "the per-tool unsafe verification manifest could not be loaded ({}); \
                     the run fails rather than inferring omitted site x tool cells",
                    error.msg
                ),
            ));
            return (report, v);
        }
    };

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

    verify_verification_lanes(root, &ledger, &verification_lanes, &mut report, &mut v);

    // --- scan every workspace member
    //
    // The roster is resolved by the SAME reader the appendix-A checker already
    // uses (`crate::appendix_a::workspace_member_paths`), so globs and
    // `[workspace] exclude` resolve identically in both places. The line scan
    // this replaced took the first double-quoted span on each line between
    // `members` and a line starting with `]`, which meant a members array in
    // TOML literal quotes, or one written on a single line, or two entries
    // sharing a physical line, silently yielded a SHORT OR EMPTY roster -- and
    // an empty roster is a clean pass with nothing examined.
    let members = match resolve_members(root, ws_section) {
        Ok(m) => m,
        Err(e) => {
            v.push(Violation::new(
                "workspace_members_unresolvable",
                "Cargo.toml",
                "workspace.members",
                format!(
                    "cannot resolve the workspace member roster, so no crate can be \
                     reported clean: {e}"
                ),
            ));
            return (report, v);
        }
    };
    report.crates_scanned = members.len();
    // The non-vacuity control for the roster, and the reason it is not merely
    // defensive: every "0 sites, 0 orphans" conclusion below is quantified over
    // this list. `scanner_fixture` proves the site scanner found something it
    // was supposed to find; nothing proved the roster was non-empty, so a
    // manifest this reader could not understand reported a clean boundary over
    // zero crates. An empty roster is now a failure in its own right.
    if members.is_empty() {
        v.push(Violation::new(
            "workspace_has_no_members",
            "Cargo.toml",
            "workspace.members",
            "the workspace resolved to zero members: every unsafe-surface conclusion in \
             this run would be quantified over an empty set, so the run fails rather \
             than reporting a clean boundary over nothing",
        ));
    }
    for member in &members {
        let name = member
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
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
                //
                // This must be a SECTION-AWARE read of the manifest, not a
                // substring test, and the difference is not pedantic. The first
                // version asked whether the text contained `[lints]` and
                // `workspace = true` anywhere at all, which two ordinary
                // manifests satisfy without inheriting anything: one whose lint
                // table is a comment (`# TODO: restore [lints] workspace =
                // true`), and one that carries its own `[lints]` table plus any
                // idiomatic `dep = { workspace = true }` dependency. Both were
                // run: `unsafe-ledger-check` reported the crate as inheriting
                // `forbid` and exited 0 while `topology-check`, which has always
                // parsed the manifest by section, failed the same tree with
                // `lints_not_inherited`. Two checkers reading one fact and
                // disagreeing means one of them is wrong; this was the wrong
                // one. Reusing [`crate::topology::scan_manifest`] is what stops
                // them from drifting apart again.
                let inherits =
                    match crate::topology::scan_manifest(&member.to_string_lossy(), &text) {
                        Ok(scanned) => scanned.lints_workspace,
                        Err(e) => {
                            // Fail closed. An unparseable manifest is not evidence
                            // of inheritance, and guessing `true` here would be the
                            // looks-exactly-like-a-pass failure this file exists to
                            // prevent.
                            v.push(Violation::new(
                                "member_manifest_unparseable",
                                &name,
                                manifest.display().to_string(),
                                format!(
                                    "cannot parse the member manifest, so its lint inheritance is \
                                 unknown and no boundary claim can be made: {e}"
                                ),
                            ));
                            report.forbid_verdicts.insert(name.clone(), false);
                            continue;
                        }
                    };
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
            // For an ordinary crate this is nothing: there is no source, so
            // there is no unsafe. For an ISLAND it is the vacuity case — the
            // crate is on the roster, so something claims it needs unsafe, and
            // a boundary whose source cannot be found must not be reported
            // clean.
            if is_island {
                v.push(Violation::new(
                    "island_api_unscannable",
                    &name,
                    src.display().to_string(),
                    "a boundary crate with no src/ directory: its safe-facing API cannot be \
                     read, so no claim can be made about what it exports",
                ));
            }
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
        if is_island {
            if files.is_empty() {
                v.push(Violation::new(
                    "island_api_unscannable",
                    &name,
                    src.display().to_string(),
                    "a boundary crate whose src/ holds no Rust source: its safe-facing API \
                     cannot be read, so no claim can be made about what it exports",
                ));
            } else {
                report.islands_api_scanned += 1;
            }
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

            if rel == REGION_VEC_BOUNDARY_PATH {
                for code in region_vec_contract_violations(&text) {
                    let message = match code {
                        "allocator_impl_contract_changed" => {
                            "the sole unsafe trait impl must be exactly \
                             `core::alloc::Allocator for PrivateRegionAllocator<'_>`; adding, \
                             removing, or broadening an unsafe impl moves the boundary"
                        }
                        "allocator_impl_method_set_changed" => {
                            "the pinned allocator override set must remain exactly safe \
                             `allocate` plus unsafe `deallocate`, matching the pinned nightly"
                        }
                        "allocator_adapter_declaration_changed" => {
                            "the Allocator self type must remain the one private, lifetime-bound \
                             `PrivateRegionAllocator<'region>` adapter"
                        }
                        "allocator_pointer_provenance_changed" => {
                            "allocator pointers must come directly from `Vec::as_mut_ptr` plus the \
                             checked slot offset; forming a backing-slice or one-byte reference \
                             invalidates live RegionVec provenance"
                        }
                        "region_vec_allocation_context_missing" => {
                            "every allocation-capable RegionVec method must carry `&QueryCx` in \
                             its public signature"
                        }
                        "region_vec_public_method_set_changed" => {
                            "the RegionVec public method set changed without updating the \
                             allocation-context audit; new escape hatches fail closed"
                        }
                        "region_vec_trait_set_changed" => {
                            "the RegionVec trait surface changed; allocating Clone, Extend, \
                             FromIterator, Deref, or any unreviewed trait path is forbidden"
                        }
                        _ => "unknown RegionVec boundary-contract finding",
                    };
                    v.push(Violation::new(
                        code,
                        "fgdb-unsafe-arena",
                        rel.clone(),
                        message,
                    ));
                }
            }

            // --- the safe-facing API of an island (bead fgdb-n7mb)
            //
            // The site scan above answers "where is unsafe WRITTEN". This
            // answers "what can a safe crate REACH", and nothing else in this
            // file does: a `pub unsafe fn` or a raw pointer in a public
            // signature adds no allow site, so the ledger stays complete, the
            // bijection stays satisfied, and the island stops being one.
            if !is_island {
                continue;
            }
            report.island_api_files += 1;
            let api = public_api(&text);
            report.island_public_items += api.public_items;
            for finding in &api.findings {
                let anchor = format!("{rel}:{}", finding.line);
                if finding.unsafe_fn {
                    v.push(Violation::new(
                        "island_public_unsafe_fn",
                        &name,
                        anchor.clone(),
                        format!(
                            "`{}` is a publicly reachable unsafe fn. An island exists so that \
                             safe crates consume it through safe APIs; exporting an unsafe fn \
                             moves the boundary outward without adding a ledger row, and every \
                             other gate here stays green",
                            finding.name
                        ),
                    ));
                }
                if finding.unsafe_impl {
                    v.push(Violation::new(
                        "island_public_unsafe_impl",
                        &name,
                        anchor.clone(),
                        format!(
                            "`{}` is an unsafe foreign-trait impl outside the one pinned private \
                             allocator adapter. A marker impl can export a proof obligation even \
                             when it adds no unsafe fn for the site scanner to count",
                            finding.name
                        ),
                    ));
                }
                if finding.raw_pointer {
                    v.push(Violation::new(
                        "island_public_raw_pointer",
                        &name,
                        anchor.clone(),
                        format!(
                            "the public {} `{}` carries a raw pointer in its exported type. A \
                             raw pointer in the safe-facing API hands the crate's unsafe \
                             obligations to callers who never signed for them",
                            finding.kind, finding.name
                        ),
                    ));
                }
                if finding.boundary_type {
                    v.push(Violation::new(
                        "island_public_allocator_boundary_type",
                        &name,
                        anchor,
                        format!(
                            "the public {} `{}` carries sealed allocator vocabulary (`NonNull`, \
                             `Allocator`, `PrivateRegionAllocator`, or `Vec<T, A>`). The typed \
                             arena boundary must remain the safe RegionVec facade",
                            finding.kind, finding.name
                        ),
                    ));
                }
            }
            // The zero licence, per file. Every `pub` in live Rust is a
            // visibility; one this reader did not consume is a region it walked
            // past, and "no findings" over source nobody parsed is exactly the
            // shape of pass this suite exists to refuse.
            if api.pub_tokens_claimed != api.pub_tokens || !api.parse_failures.is_empty() {
                v.push(Violation::new(
                    "island_public_api_unparsed",
                    &name,
                    rel.clone(),
                    format!(
                        "the safe-facing API reader claimed {} of {} pub tokens and failed to \
                         parse {} public item(s) (first at line {}): its verdict on this file \
                         is quantified over source it did not understand, so the run fails \
                         rather than reporting the island clean",
                        api.pub_tokens_claimed,
                        api.pub_tokens,
                        api.parse_failures.len(),
                        api.parse_failures.first().copied().unwrap_or(0),
                    ),
                ));
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

/// The `unsafe_code` lint level the workspace declares, read as DATA out of
/// `[workspace.lints.rust]`.
///
/// This replaced `ws_text.contains("unsafe_code = \"forbid\"")`, which was
/// vacuous on this very repository: `Cargo.toml` opens with a prose comment
/// reading ``Workspace-level `unsafe_code = "forbid"` ``, so the substring was
/// present no matter what the lint table said, and deleting both live lint
/// lines left the check passing. It was also wrong in the opposite direction on
/// every ordinary respelling -- `unsafe_code='forbid'`, or no spaces around the
/// `=` -- and it accepted the level under `[workspace.lints.clippy]`, or under a
/// package-level `[lints.rust]` that no member inherits.
///
/// Reading the parsed table answers the question actually being asked: what
/// level does the WORKSPACE set for the `rust::unsafe_code` lint? A level Cargo
/// writes as an inline table (`{ level = "forbid", priority = -1 }`) is outside
/// the registry TOML subset, so the document fails to parse and the caller
/// reports `workspace_manifest_unparseable` -- loud and closed, never silent.
fn workspace_unsafe_lint_level(workspace: &crate::toml::Table) -> Option<String> {
    let crate::toml::Value::Table(lints) = workspace.get("lints")? else {
        return None;
    };
    let crate::toml::Value::Table(rust) = lints.get("rust")? else {
        return None;
    };
    match rust.get("unsafe_code")? {
        crate::toml::Value::Str(level) => Some(level.clone()),
        _ => None,
    }
}

/// The workspace member roster, resolved through the one reader the appendix-A
/// checker already uses so the two cannot drift apart.
fn resolve_members(root: &Path, workspace: &crate::toml::Table) -> Result<Vec<PathBuf>, String> {
    let members = crate::toml::get_str_array(workspace, "members", "Cargo.toml.workspace")
        .map_err(|e| e.to_string())?;
    let excludes = crate::appendix_a::workspace_exact_excludes(workspace)?;
    crate::appendix_a::workspace_member_paths(root, &members, &excludes)
}

// ===========================================================================
// The safe-facing API of an island (bead `fgdb-n7mb`)
// ===========================================================================
//
// The ledger below enumerates `allow(unsafe_code)` SITES. That is a complete
// account of where unsafe code is WRITTEN and no account at all of where it is
// REACHABLE FROM. An island exists so that safe crates consume it through safe
// APIs; a `pub unsafe fn`, or a `*const`/`*mut` in a public signature, moves the
// boundary outward without adding a single site, so every check above it stays
// green while the property the islands exist for is gone. Until this reader
// landed the rule lived in three crate-root doc comments and was checked by
// nobody.
//
// It is a structural reader, and the suite header in `tests/metamorphic.rs`
// says why in the general case: four of one session's seven "looks exactly like
// a pass" bugs were a substring, prefix, or whole-line-equality test standing in
// for structural parsing, inside a checker whose job is to be unfoolable. The
// specific traps here are ordinary Rust, not exotica:
//
// * a doc comment or a string literal naming `*mut u8` — read through
//   [`mask_source`], the ONE reader for which bytes of a source are live code,
//   so a line-wise matcher's false positives cannot happen here;
// * a signature wrapped across lines, which no line-wise matcher can see whole;
// * a raw pointer in a fn BODY, which is ledgered unsafe code and NOT an
//   exported signature, so the region checked stops at the opening brace;
// * `pub struct S { pub slots: Slots<u32, *mut u8> }`, where the comma inside
//   the angle brackets splits the field for any reader that finds field
//   boundaries by scanning for commas at paren depth — and the half carrying the
//   raw pointer then has no `pub` in front of it, which is a SILENT ACCEPT.
//
// Two over-approximations are deliberate, and both fail loudly rather than
// quietly:
//
// * Module privacy is not modelled: a `pub` item is treated as safe-facing
//   wherever it sits. Modelling it without also modelling `pub use` re-export
//   (`mod internal; pub use internal::Thing;` — the common shape) would be
//   fail-open, and fail-open is the direction this file exists to close.
// * A trait impl's items are treated as public, because a foreign trait's
//   `unsafe fn` is reachable through the island's type even though the impl
//   cannot write `pub`.
//
// NO-CLAIM BOUNDARY. This says nothing about an address deliberately erased to
// `usize`, nothing about items generated by a macro expansion, and nothing
// about whether a public item is *sound* — only about whether the safe-facing
// surface is spelled safely. It DOES reject the pointer/allocator vocabulary
// relevant to the sealed RegionVec boundary: `NonNull`, `Allocator`,
// `PrivateRegionAllocator`, and allocator-parameterized `Vec<T, A>`.
// `macro_rules!` bodies are not parsed at all, and the unclaimed-`pub` control
// below turns that into a violation rather than a silent gap.

/// One public item that violates the safe-facing rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicApiFinding {
    /// `fn`, `field`, `type`, `const`, `static`, `use` or `impl`.
    pub kind: &'static str,
    /// The item's or field's name, as written.
    pub name: String,
    /// 1-based line of the item's defining keyword.
    pub line: usize,
    /// A publicly reachable `unsafe fn`.
    pub unsafe_fn: bool,
    /// An unsafe foreign-trait impl other than the single pinned private
    /// allocator adapter.
    pub unsafe_impl: bool,
    /// `*const` or `*mut` inside the item's exported type region.
    pub raw_pointer: bool,
    /// Sealed pointer/allocator vocabulary inside the exported type region.
    pub boundary_type: bool,
}

#[derive(Debug, Clone, Copy)]
struct PublicApiHazards {
    unsafe_fn: bool,
    unsafe_impl: bool,
    raw_pointer: bool,
    boundary_type: bool,
}

/// What the API reader concluded about one source file.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PublicApi {
    /// The violations, in source order.
    pub findings: Vec<PublicApiFinding>,
    /// Public items the parser classified, violating or not. Reported so a
    /// reader can tell an island with a small API from one the parser missed.
    pub public_items: usize,
    /// Every `pub` keyword token in live code.
    ///
    /// This is NOT a second reader of "what items does this file export" — it
    /// answers a strictly weaker question, and it exists only to license a zero.
    /// Both counts come from the same mask and the same token stream, so they
    /// cannot disagree about which bytes are code; they can only disagree about
    /// whether the parser understood them.
    pub pub_tokens: usize,
    /// The `pub` tokens the parser consumed AS a visibility. Every `pub` in live
    /// Rust is a visibility, so a gap here is a region the parser walked past —
    /// which is exactly how a structural reader reports "clean" over source it
    /// never understood.
    pub pub_tokens_claimed: usize,
    /// 1-based lines where a `pub` was read and the item then failed to parse.
    pub parse_failures: Vec<usize>,
}

/// A fixture with a known set of safe-facing violations, and — just as load
/// bearing — a known set of near misses that must NOT count.
///
/// Every counted case is a form that has to be caught; every ignored case is a
/// form a plausible wrong reader catches anyway. The ignored half is what keeps
/// the reader from being "reject anything containing an asterisk".
///
/// It is a string literal, so [`mask_source`] blanks it: this fixture cannot be
/// mistaken for a real declaration in this file the way the first site scanner's
/// fixture was.
pub fn safe_facing_fixture() -> &'static str {
    SAFE_FACING_FIXTURE
}

/// The exact number of findings in [`safe_facing_fixture`].
pub const SAFE_FACING_FIXTURE_FINDINGS: usize = 18;

/// The exact number of live `pub` tokens in [`safe_facing_fixture`]. Pinning it
/// keeps the claim control itself honest: a parser that claimed nothing and a
/// fixture that contained nothing look identical without this.
pub const SAFE_FACING_FIXTURE_PUB_TOKENS: usize = 25;

const SAFE_FACING_FIXTURE: &str = r#"
//! A fixture island source. The *mut u8 in this module doc must not count.

use core::ptr;

/// COUNTED: a public unsafe fn.
pub unsafe fn public_unsafe() {}

/// COUNTED: a raw pointer parameter. This doc line names *mut u8 as well.
pub fn takes_raw(p: *mut u8) -> usize {
    p as usize
}

/// COUNTED: a raw pointer return, on a signature wrapped across lines.
pub fn gives_raw(
    len: usize,
) -> *const u8 {
    let _ = len;
    ptr::null()
}

/// COUNTED: a comma inside angle brackets, ahead of the raw pointer.
pub fn nested(map: Slots<u32, *mut u8>) -> usize {
    map.len()
}

/// COUNTED twice: a public named field, and one whose type carries an angle
/// bracketed comma before the raw pointer.
pub struct Named {
    pub block: *mut u8,
    pub slots: Slots<u32, *mut u8>,
    len: usize,
}

/// COUNTED: a public tuple field.
pub struct Tuple(pub *const u8, usize);

/// COUNTED: a public type alias.
pub type Alias = *mut u8;

/// COUNTED twice: two enum variant payloads, which are public with the enum.
pub enum Carrier {
    Empty,
    Raw(*mut u8),
    Named { at: *const u8 },
}

/// COUNTED twice: a trait method that is unsafe, and one taking a raw pointer.
/// Neither says pub; both are as public as the trait.
pub trait Boundary {
    unsafe fn arm();
    fn hand(p: *mut u8);
}

/// COUNTED: a public static.
pub static ORIGIN: *const u8 = ptr::null();

/// COUNTED: pointer-like boundary vocabulary without a raw-pointer spelling.
pub fn gives_non_null() -> NonNull<u8> {
    todo!()
}

/// COUNTED: the allocator trait in a public bound.
pub fn takes_allocator<A: Allocator>(allocator: A) {
    let _ = allocator;
}

/// COUNTED: an allocator-parameterized standard vector.
pub type AllocatedVec = Vec<u8, LocalAllocator>;

/// COUNTED: re-exporting the private adapter makes it reachable by name.
pub use hidden::PrivateRegionAllocator;

/// COUNTED: an unsafe trait impl moves a foreign proof obligation onto the
/// island's type even when the marker trait has no methods.
unsafe impl Marker for HiddenAdapter {}

/// IGNORED: restricted visibility is not the safe-facing API.
pub(crate) fn restricted(p: *mut u8) -> usize {
    p as usize
}

/// IGNORED: no visibility at all.
fn private_raw(p: *mut u8) -> usize {
    p as usize
}

/// IGNORED: unsafe, but not public.
unsafe fn private_unsafe() {}

/// IGNORED: a private struct's public field is not reachable.
struct Hidden {
    pub block: *mut u8,
}

/// IGNORED: a private trait's unsafe method.
trait Interior {
    unsafe fn arm();
}

/// IGNORED: the raw pointer is in the BODY. That is ledgered unsafe code, not
/// an exported signature, and confusing the two would make every island fail.
pub fn body_only(v: &mut [u8]) -> usize {
    let base: *mut u8 = v.as_mut_ptr();
    base as usize
}

/// IGNORED: a raw pointer spelled inside a string literal.
pub fn spells_it() -> &'static str {
    "*mut u8"
}

/// IGNORED: the DECLARED type is `usize`. The initialiser names a raw pointer
/// in a turbofish, which is live code and not blanked by the mask — so a reader
/// that scanned the whole item instead of the type region would flag it.
pub const NOWHERE: usize = core::mem::size_of::<*mut u8>();

/// IGNORED: multiplication is not a raw pointer.
pub fn arithmetic(lanes: [u8; WIDTH * 2]) -> usize {
    lanes.len()
}

/* IGNORED: a block-commented declaration the compiler never sees.
pub unsafe fn commented() {}
pub fn commented_raw(p: *mut u8) {}
*/

/// IGNORED: an item inside a function body is unreachable from anywhere.
pub fn encloses() {
    pub fn inner(p: *mut u8) -> usize {
        p as usize
    }
    let _ = inner;
}
"#;

/// Read the safe-facing public API of one Rust source.
pub fn public_api(text: &str) -> PublicApi {
    let masked = mask_source(text);
    let toks = tokenize(masked.text());
    let mut parser = ApiParser {
        masked: &masked,
        out: PublicApi {
            pub_tokens: toks.iter().filter(|t| t.ident && t.text == "pub").count(),
            ..PublicApi::default()
        },
        toks,
    };
    let end = parser.toks.len();
    parser.items(0, end, Scope::file());
    parser.out
}

/// One token of masked source. Comments and literal CONTENT are already blanked,
/// so the only tokens here are live code.
#[derive(Debug, Clone, Copy)]
struct Token<'a> {
    text: &'a str,
    ident: bool,
    start: usize,
}

/// Split masked source into identifiers and punctuation.
///
/// `->`, `=>` and `::` are single tokens on purpose: the `>` of an arrow must
/// not close a generic argument list, which is what [`ApiParser::skip_type`]
/// balances to find a field's end.
fn tokenize(masked: &str) -> Vec<Token<'_>> {
    let chars: Vec<(usize, char)> = masked.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let (at, c) = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        let end_of = |j: usize| chars.get(j).map_or(masked.len(), |(o, _)| *o);
        if c == '_' || c.is_alphabetic() {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].1 == '_' || chars[j].1.is_alphanumeric()) {
                j += 1;
            }
            out.push(Token {
                text: &masked[at..end_of(j)],
                ident: true,
                start: at,
            });
            i = j;
            continue;
        }
        if c.is_numeric() {
            let mut j = i + 1;
            while j < chars.len() && (chars[j].1 == '_' || chars[j].1.is_alphanumeric()) {
                j += 1;
            }
            out.push(Token {
                text: &masked[at..end_of(j)],
                ident: false,
                start: at,
            });
            i = j;
            continue;
        }
        let two = &masked[at..end_of(i + 2)];
        if matches!(two, "->" | "=>" | "::") {
            out.push(Token {
                text: two,
                ident: false,
                start: at,
            });
            i += 2;
            continue;
        }
        out.push(Token {
            text: &masked[at..end_of(i + 1)],
            ident: false,
            start: at,
        });
        i += 1;
    }
    out
}

/// The source file carrying the one pinned allocator implementation and the
/// safe RegionVec surface governed with it.
pub const REGION_VEC_BOUNDARY_PATH: &str = "crates/fgdb-unsafe-arena/src/region.rs";

const PINNED_ALLOCATOR_HEADER: [&str; 14] = [
    "unsafe",
    "impl",
    "core",
    "::",
    "alloc",
    "::",
    "Allocator",
    "for",
    "PrivateRegionAllocator",
    "<",
    "'",
    "_",
    ">",
    "{",
];

const PINNED_REGION_VEC_METHODS: [&str; 26] = [
    "as_mut_slice",
    "as_slice",
    "capacity",
    "clear",
    "get",
    "get_mut",
    "is_empty",
    "iter",
    "iter_mut",
    "len",
    "new_in",
    "pop",
    "remove",
    "replace",
    "swap_remove",
    "truncate",
    "try_clone",
    "try_extend",
    "try_extend_from_slice",
    "try_insert",
    "try_push",
    "try_reserve",
    "try_reserve_exact",
    "try_resize",
    "try_resize_with",
    "with_capacity_in",
];

const QUERY_GATED_REGION_VEC_METHODS: [&str; 10] = [
    "try_clone",
    "try_extend",
    "try_extend_from_slice",
    "try_insert",
    "try_push",
    "try_reserve",
    "try_reserve_exact",
    "try_resize",
    "try_resize_with",
    "with_capacity_in",
];

const ALLOWED_REGION_VEC_TRAITS: [&str; 6] = ["AsMut", "AsRef", "Debug", "Drop", "Eq", "PartialEq"];

fn lexeme_sequence_at(tokens: &[Token<'_>], at: usize, expected: &[&str]) -> bool {
    tokens
        .get(at..at.saturating_add(expected.len()))
        .is_some_and(|actual| {
            actual
                .iter()
                .map(|lexeme| lexeme.text)
                .eq(expected.iter().copied())
        })
}

fn lexeme_sequence_count(tokens: &[Token<'_>], expected: &[&str]) -> usize {
    (0..tokens.len())
        .filter(|&index| lexeme_sequence_at(tokens, index, expected))
        .count()
}

fn matching_delimiter(tokens: &[Token<'_>], open: usize, end: usize) -> usize {
    let (opener, closer) = match tokens.get(open).map(|lexeme| lexeme.text) {
        Some("(") => ("(", ")"),
        Some("[") => ("[", "]"),
        Some("{") => ("{", "}"),
        _ => return open,
    };
    let mut depth = 0_usize;
    for (index, lexeme) in tokens.iter().enumerate().take(end).skip(open) {
        if lexeme.text == opener {
            depth += 1;
        } else if lexeme.text == closer {
            depth = depth.saturating_sub(1);
            if depth == 0 {
                return index;
            }
        }
    }
    end
}

fn matching_angle(tokens: &[Token<'_>], open: usize, end: usize) -> usize {
    let mut depth = 0_usize;
    for (index, lexeme) in tokens.iter().enumerate().take(end).skip(open) {
        match lexeme.text {
            "<" => depth += 1,
            ">" => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return index;
                }
            }
            _ => {}
        }
    }
    end
}

fn impl_body_open(tokens: &[Token<'_>], start: usize) -> Option<usize> {
    let mut paren = 0_usize;
    let mut bracket = 0_usize;
    for (index, lexeme) in tokens.iter().enumerate().skip(start) {
        match lexeme.text {
            "(" => paren += 1,
            ")" => paren = paren.saturating_sub(1),
            "[" => bracket += 1,
            "]" => bracket = bracket.saturating_sub(1),
            "{" if paren == 0 && bracket == 0 => return Some(index),
            ";" if paren == 0 && bracket == 0 => return None,
            _ => {}
        }
    }
    None
}

fn top_level_methods<'a>(
    tokens: &'a [Token<'a>],
    open: usize,
    close: usize,
    public_only: bool,
) -> Vec<(&'a str, bool, usize, usize)> {
    let mut out = Vec::new();
    let mut index = open + 1;
    while index < close {
        if tokens[index].text == "#"
            && tokens
                .get(index + 1)
                .is_some_and(|lexeme| lexeme.text == "[")
        {
            index = matching_delimiter(tokens, index + 1, close).saturating_add(1);
            continue;
        }
        let start = index;
        let public = tokens[index].ident && tokens[index].text == "pub";
        if public {
            index += 1;
            if tokens.get(index).is_some_and(|lexeme| lexeme.text == "(") {
                index = matching_delimiter(tokens, index, close).saturating_add(1);
            }
        }
        let unsafe_method = tokens
            .get(index)
            .is_some_and(|lexeme| lexeme.text == "unsafe");
        if unsafe_method {
            index += 1;
        }
        while tokens.get(index).is_some_and(|lexeme| {
            lexeme.ident && matches!(lexeme.text, "async" | "const" | "default")
        }) {
            index += 1;
        }
        if tokens.get(index).is_none_or(|lexeme| lexeme.text != "fn") {
            index = start + 1;
            continue;
        }
        let Some(name) = tokens.get(index + 1).filter(|lexeme| lexeme.ident) else {
            index = start + 1;
            continue;
        };
        let signature_start = start;
        let mut body_open = index + 2;
        let mut paren = 0_usize;
        let mut bracket = 0_usize;
        while body_open < close {
            match tokens[body_open].text {
                "(" => paren += 1,
                ")" => paren = paren.saturating_sub(1),
                "[" => bracket += 1,
                "]" => bracket = bracket.saturating_sub(1),
                "{" if paren == 0 && bracket == 0 => break,
                ";" if paren == 0 && bracket == 0 => break,
                _ => {}
            }
            body_open += 1;
        }
        if !public_only || public {
            out.push((name.text, unsafe_method, signature_start, body_open));
        }
        if tokens
            .get(body_open)
            .is_some_and(|lexeme| lexeme.text == "{")
        {
            index = matching_delimiter(tokens, body_open, close).saturating_add(1);
        } else {
            index = body_open.saturating_add(1);
        }
    }
    out
}

/// Mechanically pin the sole private Allocator impl and the context-bearing
/// RegionVec surface. These codes are mapped into ordinary checker violations
/// by [`check_workspace`] and are exposed so mutation tests can exercise the
/// contract without creating a second parser.
pub fn region_vec_contract_violations(text: &str) -> Vec<&'static str> {
    let masked = mask_source(text);
    let tokens = tokenize(masked.text());
    let mut findings = Vec::new();

    let unsafe_impls: Vec<usize> = (0..tokens.len())
        .filter(|&index| {
            tokens[index].ident
                && tokens[index].text == "unsafe"
                && tokens
                    .get(index + 1)
                    .is_some_and(|lexeme| lexeme.text == "impl")
        })
        .collect();
    let pinned: Vec<usize> = unsafe_impls
        .iter()
        .copied()
        .filter(|&index| lexeme_sequence_at(&tokens, index, &PINNED_ALLOCATOR_HEADER))
        .collect();
    if unsafe_impls.len() != 1 || pinned.len() != 1 {
        findings.push("allocator_impl_contract_changed");
    }
    if let Some(&start) = pinned.first() {
        let open = start + PINNED_ALLOCATOR_HEADER.len() - 1;
        let close = matching_delimiter(&tokens, open, tokens.len());
        let allocator_methods = top_level_methods(&tokens, open, close, false);
        let methods: Vec<(&str, bool)> = allocator_methods
            .iter()
            .map(|(name, unsafe_method, _, _)| (*name, *unsafe_method))
            .collect();
        if methods.as_slice() != [("allocate", false), ("deallocate", true)] {
            findings.push("allocator_impl_method_set_changed");
        }
        match allocator_methods
            .iter()
            .find(|(name, _, _, _)| matches!(*name, "allocate"))
        {
            Some((_, _, _, body_open)) => {
                let body_close = matching_delimiter(&tokens, *body_open, close);
                let body = tokens.get(*body_open..=body_close).unwrap_or_default();
                let direct_pointer_path = [
                    "region",
                    ".",
                    "chunks",
                    "[",
                    "slot",
                    ".",
                    "chunk",
                    "]",
                    ".",
                    "data",
                    ".",
                    "as_mut_ptr",
                    "(",
                    ")",
                    ".",
                    "wrapping_add",
                    "(",
                    "slot",
                    ".",
                    "start",
                    ")",
                ];
                let forms_backing_reference = body
                    .iter()
                    .any(|lexeme| matches!(lexeme.text, "as_mut_slice" | "block_mut"));
                if lexeme_sequence_count(body, &direct_pointer_path) != 1
                    || lexeme_sequence_count(body, &["NonNull", "::", "new"]) != 1
                    || lexeme_sequence_count(body, &["NonNull", "::", "from"]) != 1
                    || forms_backing_reference
                {
                    findings.push("allocator_pointer_provenance_changed");
                }
            }
            None => findings.push("allocator_pointer_provenance_changed"),
        }
    }

    let private_adapter_declarations = (0..tokens.len())
        .filter(|&index| {
            lexeme_sequence_at(
                &tokens,
                index,
                &[
                    "struct",
                    "PrivateRegionAllocator",
                    "<",
                    "'",
                    "region",
                    ">",
                    "{",
                ],
            )
        })
        .count();
    if private_adapter_declarations != 1 {
        findings.push("allocator_adapter_declaration_changed");
    }

    let expected_methods: BTreeSet<&str> = PINNED_REGION_VEC_METHODS.into_iter().collect();
    let expected_traits: BTreeSet<&str> = ALLOWED_REGION_VEC_TRAITS.into_iter().collect();
    let gated: BTreeSet<&str> = QUERY_GATED_REGION_VEC_METHODS.into_iter().collect();
    let mut actual_methods = BTreeSet::new();
    let mut actual_traits = BTreeSet::new();

    for index in 0..tokens.len() {
        if !tokens[index].ident || tokens[index].text != "impl" {
            continue;
        }
        let Some(open) = impl_body_open(&tokens, index) else {
            continue;
        };
        let close = matching_delimiter(&tokens, open, tokens.len());
        if !tokens[index..open]
            .iter()
            .any(|lexeme| lexeme.ident && lexeme.text == "RegionVec")
        {
            continue;
        }
        let for_at = (index..open).find(|&at| tokens[at].ident && tokens[at].text == "for");
        if let Some(for_at) = for_at {
            let mut trait_start = index + 1;
            if tokens
                .get(trait_start)
                .is_some_and(|lexeme| lexeme.text == "<")
            {
                trait_start = matching_angle(&tokens, trait_start, for_at).saturating_add(1);
            }
            let trait_end = (trait_start..for_at)
                .find(|&at| tokens[at].text == "<")
                .unwrap_or(for_at);
            if let Some(name) = tokens[trait_start..trait_end]
                .iter()
                .rev()
                .find(|lexeme| lexeme.ident)
                .map(|lexeme| lexeme.text)
            {
                actual_traits.insert(name);
            }
            continue;
        }
        for (name, _, signature_start, signature_end) in
            top_level_methods(&tokens, open, close, true)
        {
            actual_methods.insert(name);
            if gated.contains(name)
                && !tokens[signature_start..signature_end]
                    .iter()
                    .any(|lexeme| lexeme.ident && lexeme.text == "QueryCx")
            {
                findings.push("region_vec_allocation_context_missing");
            }
        }
    }
    if actual_methods != expected_methods {
        findings.push("region_vec_public_method_set_changed");
    }
    if actual_traits != expected_traits {
        findings.push("region_vec_trait_set_changed");
    }

    findings.sort_unstable();
    findings.dedup();
    findings
}

/// What kind of item a unit's header declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Fn,
    Struct,
    Enum,
    Trait,
    Impl,
    Mod,
    TypeAlias,
    Const,
    Static,
    Use,
    ExternBlock,
    MacroRules,
    Unknown,
}

/// A unit header: everything from the visibility to the defining keyword.
#[derive(Debug, Clone, Copy)]
struct Head {
    kind: Kind,
    /// Index of the defining keyword, or of the `{` for an extern block.
    kw: usize,
    unsafe_mod: bool,
    safe_mod: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Vis {
    /// Unrestricted `pub`.
    Public,
    /// `pub(crate)`, `pub(super)`, `pub(self)`, `pub(in ...)`.
    Restricted,
    /// No visibility keyword.
    Inherited,
}

/// Where in the item tree the parser currently is.
#[derive(Debug, Clone, Copy)]
struct Scope {
    /// Items here are public without saying so: a `pub trait` body, or a trait
    /// impl body, where `pub` is not even legal.
    inherited_pub: bool,
    /// Report findings, or merely claim `pub` tokens. A function body is walked
    /// with `report` false: items declared there are unreachable, so flagging
    /// them would be a false alarm, but their `pub` tokens must still be claimed
    /// or the vacuity control fires on perfectly good source.
    report: bool,
    /// An `extern` block, where a `fn` is unsafe to call unless marked `safe`.
    extern_block: bool,
}

impl Scope {
    fn file() -> Self {
        Self {
            inherited_pub: false,
            report: true,
            extern_block: false,
        }
    }

    fn inner(self, inherited_pub: bool) -> Self {
        Self {
            inherited_pub,
            report: self.report,
            extern_block: false,
        }
    }

    /// A function body: claim tokens, report nothing.
    fn body(self) -> Self {
        Self {
            inherited_pub: false,
            report: false,
            extern_block: false,
        }
    }
}

struct ApiParser<'a> {
    toks: Vec<Token<'a>>,
    masked: &'a Masked,
    out: PublicApi,
}

impl ApiParser<'_> {
    fn tt(&self, i: usize) -> &str {
        self.toks.get(i).map_or("", |t| t.text)
    }

    fn is_kw(&self, i: usize, word: &str) -> bool {
        self.toks.get(i).is_some_and(|t| t.ident && t.text == word)
    }

    fn line(&self, i: usize) -> usize {
        self.toks.get(i).map_or(1, |t| self.masked.line_of(t.start))
    }

    /// Index of the bracket matching the opener at `open`, or `end`.
    fn matching(&self, open: usize, end: usize) -> usize {
        let (opener, closer) = match self.tt(open) {
            "(" => ("(", ")"),
            "[" => ("[", "]"),
            "{" => ("{", "}"),
            _ => return open,
        };
        let mut depth = 0usize;
        for i in open..end {
            let t = self.tt(i);
            if t == opener {
                depth += 1;
            } else if t == closer {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i;
                }
            }
        }
        end
    }

    fn skip_attrs(&self, mut i: usize, end: usize) -> usize {
        while i < end && self.tt(i) == "#" {
            let mut j = i + 1;
            if j < end && self.tt(j) == "!" {
                j += 1;
            }
            if j < end && self.tt(j) == "[" {
                i = self.matching(j, end) + 1;
            } else {
                break;
            }
        }
        i
    }

    /// Read a visibility, claiming its `pub` token.
    ///
    /// A `pub` followed by a parenthesis is a restriction only when the
    /// parenthesis opens with `crate`, `super`, `self` or `in`. That is what
    /// Rust itself accepts, and it keeps a tuple field whose type happens to be
    /// parenthesised from reading as `pub(…)`.
    fn read_vis(&mut self, i: &mut usize, end: usize) -> Vis {
        if !self.is_kw(*i, "pub") {
            return Vis::Inherited;
        }
        self.out.pub_tokens_claimed += 1;
        *i += 1;
        if *i < end && self.tt(*i) == "(" {
            let inner = self.tt(*i + 1);
            if matches!(inner, "crate" | "super" | "self" | "in") {
                *i = self.matching(*i, end) + 1;
                return Vis::Restricted;
            }
        }
        Vis::Public
    }

    /// Advance past the modifier keywords to the defining keyword.
    ///
    /// The loop only ever CONTINUES on a modifier; everything else settles a
    /// kind and breaks, so `kw` always names the token the caller must measure
    /// the item from.
    fn read_head(&self, mut i: usize, end: usize) -> Head {
        let mut unsafe_mod = false;
        let mut safe_mod = false;
        let mut kind = Kind::Unknown;
        let mut guard = 0;
        while i < end && guard < 8 {
            guard += 1;
            if !self.toks[i].ident {
                break;
            }
            match self.toks[i].text {
                "unsafe" => {
                    unsafe_mod = true;
                    i += 1;
                    continue;
                }
                "safe" => {
                    safe_mod = true;
                    i += 1;
                    continue;
                }
                "async" | "default" | "gen" => {
                    i += 1;
                    continue;
                }
                "extern" => {
                    // `extern "C" fn`, `unsafe extern "C" { … }`, or
                    // `extern crate`. The abi string is masked to its quotes.
                    i += 1;
                    while i < end && self.tt(i) == "\"" {
                        i += 1;
                    }
                    if i < end && self.tt(i) == "{" {
                        kind = Kind::ExternBlock;
                        break;
                    }
                    continue;
                }
                // `const` is a modifier only when a callable follows it;
                // otherwise it is the defining keyword of a constant.
                "const" if matches!(self.tt(i + 1), "fn" | "unsafe" | "extern" | "async") => {
                    i += 1;
                    continue;
                }
                "const" => kind = Kind::Const,
                "fn" => kind = Kind::Fn,
                "struct" => kind = Kind::Struct,
                "union" if self.toks.get(i + 1).is_some_and(|t| t.ident) => kind = Kind::Struct,
                "enum" => kind = Kind::Enum,
                "trait" => kind = Kind::Trait,
                "impl" => kind = Kind::Impl,
                "mod" => kind = Kind::Mod,
                "type" => kind = Kind::TypeAlias,
                "static" => kind = Kind::Static,
                "use" => kind = Kind::Use,
                "macro_rules" => kind = Kind::MacroRules,
                _ => {}
            }
            break;
        }
        Head {
            kind,
            kw: i.min(end.saturating_sub(1)),
            unsafe_mod,
            safe_mod,
        }
    }

    /// Where the unit beginning at `from` ends, and its brace body if it has
    /// one. A `;` at depth zero ends a unit without a body; a `{` at depth zero
    /// opens one and its matching `}` ends the unit.
    fn unit_extent(&self, from: usize, end: usize) -> (usize, Option<(usize, usize)>) {
        let mut depth = 0usize;
        let mut i = from;
        while i < end {
            match self.tt(i) {
                "(" | "[" => depth += 1,
                ")" | "]" => depth = depth.saturating_sub(1),
                "{" if depth == 0 => {
                    let close = self.matching(i, end);
                    return ((close + 1).min(end), Some((i, close)));
                }
                "{" => depth += 1,
                "}" if depth == 0 => return (i, None),
                "}" => depth = depth.saturating_sub(1),
                ";" if depth == 0 => return (i + 1, None),
                _ => {}
            }
            i += 1;
        }
        (end, None)
    }

    /// Where the unit beginning at `from` ends, counting only `;` — for items
    /// whose braces are content rather than a body (`use a::{b, c};`).
    fn unit_end_semi(&self, from: usize, end: usize) -> usize {
        let mut depth = 0usize;
        let mut i = from;
        while i < end {
            match self.tt(i) {
                "(" | "[" | "{" => depth += 1,
                ")" | "]" | "}" => depth = depth.saturating_sub(1),
                ";" if depth == 0 => return i + 1,
                _ => {}
            }
            i += 1;
        }
        end
    }

    /// The first parenthesis group at depth zero in `from..end`.
    fn first_paren(&self, from: usize, end: usize) -> Option<(usize, usize)> {
        let mut depth = 0usize;
        for i in from..end {
            match self.tt(i) {
                "(" if depth == 0 => return Some((i, self.matching(i, end))),
                "[" | "{" => depth += 1,
                "]" | "}" => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        None
    }

    /// Does a `for` appear at depth zero in `from..end` — i.e. is this
    /// `impl Trait for Type` rather than an inherent impl? A higher-ranked
    /// `for<'a>` inside a bound sits at angle depth and does not count.
    fn is_trait_impl(&self, from: usize, end: usize) -> bool {
        let (mut paren, mut brack, mut angle) = (0usize, 0usize, 0usize);
        for i in from..end {
            match self.tt(i) {
                "(" => paren += 1,
                ")" => paren = paren.saturating_sub(1),
                "[" => brack += 1,
                "]" => brack = brack.saturating_sub(1),
                "<" => angle += 1,
                ">" => angle = angle.saturating_sub(1),
                "for" if paren == 0 && brack == 0 && angle == 0 => return true,
                _ => {}
            }
        }
        false
    }

    /// The only foreign-trait impl whose methods are intentionally private to
    /// an island. The separate pinned-contract checker verifies its complete
    /// method set; this predicate only prevents those private callbacks from
    /// being mislabeled as public API.
    fn is_pinned_allocator_impl(&self, from: usize, end: usize, unsafe_mod: bool) -> bool {
        const HEADER: [&str; 12] = [
            "impl",
            "core",
            "::",
            "alloc",
            "::",
            "Allocator",
            "for",
            "PrivateRegionAllocator",
            "<",
            "'",
            "_",
            ">",
        ];
        unsafe_mod
            && end.saturating_sub(from) == HEADER.len()
            && self.toks[from..end]
                .iter()
                .map(|lexeme| lexeme.text)
                .eq(HEADER)
    }

    /// Where the type beginning at `from` ends: the first `,`, `;` or `=`
    /// outside every bracket, INCLUDING angle brackets.
    ///
    /// The angle counter is the whole point. `pub slots: Slots<u32, *mut u8>`
    /// has a comma that belongs to the generic argument list, and a reader that
    /// stopped there would hand the raw pointer to a "field" with no `pub` in
    /// front of it and report the struct clean — a SILENT ACCEPT, which is the
    /// direction that hides longest. `->` and `=>` are single tokens, so an
    /// arrow's `>` cannot close an argument list; a `>` with nothing open is
    /// ignored rather than driving the count negative.
    ///
    /// Stopping at a flat `=` is what keeps a constant's exported type separate
    /// from its initialiser: `const N: usize = size_of::<*mut u8>()` exports a
    /// `usize`, and the raw pointer in the turbofish is live code the mask does
    /// not blank.
    fn skip_type(&self, from: usize, end: usize) -> usize {
        let (mut paren, mut brack, mut brace, mut angle) = (0usize, 0usize, 0usize, 0usize);
        let mut i = from;
        while i < end {
            let flat = paren == 0 && brack == 0 && brace == 0 && angle == 0;
            match self.tt(i) {
                "(" => paren += 1,
                ")" if paren == 0 => return i,
                ")" => paren -= 1,
                "[" => brack += 1,
                "]" if brack == 0 => return i,
                "]" => brack -= 1,
                "{" => brace += 1,
                "}" if brace == 0 => return i,
                "}" => brace -= 1,
                "<" => angle += 1,
                ">" => angle = angle.saturating_sub(1),
                "," | ";" | "=" if flat => return i,
                _ => {}
            }
            i += 1;
        }
        end
    }

    /// Where an EXPRESSION ends: the first `,` or `;` outside `()`, `[]` and
    /// `{}`, with angle brackets deliberately NOT counted.
    ///
    /// An enum discriminant is an expression, and `Flag = 1 << 3` opens two
    /// angle brackets that never close. Balancing them there would swallow
    /// every remaining variant, which is how a reader reports a clean enum by
    /// never reaching the rest of it.
    fn skip_expr(&self, from: usize, end: usize) -> usize {
        let (mut paren, mut brack, mut brace) = (0usize, 0usize, 0usize);
        let mut i = from;
        while i < end {
            let flat = paren == 0 && brack == 0 && brace == 0;
            match self.tt(i) {
                "(" => paren += 1,
                ")" if paren == 0 => return i,
                ")" => paren -= 1,
                "[" => brack += 1,
                "]" if brack == 0 => return i,
                "]" => brack -= 1,
                "{" => brace += 1,
                "}" if brace == 0 => return i,
                "}" => brace -= 1,
                "," | ";" if flat => return i,
                _ => {}
            }
            i += 1;
        }
        end
    }

    /// Is there a raw pointer in `from..end`? A `*` is one only when the very
    /// next token is the `const` or `mut` keyword, so multiplication by a
    /// constant (`[u8; WIDTH * 2]`) is not mistaken for one.
    fn raw_pointer_in(&self, from: usize, end: usize) -> bool {
        (from..end).any(|i| {
            self.tt(i) == "*"
                && i + 1 < end
                && (self.is_kw(i + 1, "const") || self.is_kw(i + 1, "mut"))
        })
    }

    /// Pointer/allocator vocabulary that must stay sealed behind RegionVec.
    fn boundary_type_in(&self, from: usize, end: usize) -> bool {
        for i in from..end {
            if self.toks[i].ident
                && matches!(
                    self.toks[i].text,
                    "NonNull" | "Allocator" | "PrivateRegionAllocator"
                )
            {
                return true;
            }
            if !(self.is_kw(i, "Vec") && matches!(self.tt(i + 1), "<")) {
                continue;
            }
            let mut depth = 0_usize;
            for j in i + 1..end {
                match self.tt(j) {
                    "<" => depth += 1,
                    ">" => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            break;
                        }
                    }
                    "," if matches!(depth, 1) => return true,
                    _ => {}
                }
            }
        }
        false
    }

    fn record(&mut self, kind: &'static str, name: String, line: usize, hazards: PublicApiHazards) {
        if hazards.unsafe_fn || hazards.unsafe_impl || hazards.raw_pointer || hazards.boundary_type
        {
            self.out.findings.push(PublicApiFinding {
                kind,
                name,
                line,
                unsafe_fn: hazards.unsafe_fn,
                unsafe_impl: hazards.unsafe_impl,
                raw_pointer: hazards.raw_pointer,
                boundary_type: hazards.boundary_type,
            });
        }
    }

    fn name_after(&self, kw: usize) -> String {
        self.toks
            .get(kw + 1)
            .filter(|t| t.ident)
            .map_or_else(|| "<anonymous>".to_owned(), |t| t.text.to_owned())
    }

    fn items(&mut self, mut i: usize, end: usize, scope: Scope) {
        while i < end {
            let stall = i;
            i = self.skip_attrs(i, end);
            if i >= end {
                break;
            }
            if matches!(self.tt(i), ";" | ",") {
                i += 1;
                continue;
            }
            let vis = self.read_vis(&mut i, end);
            if i >= end {
                break;
            }
            let head = self.read_head(i, end);
            let public = vis == Vis::Public || scope.inherited_pub;
            if public && scope.report && head.kind != Kind::Unknown {
                self.out.public_items += 1;
            }
            let next = match head.kind {
                Kind::Fn => {
                    let (unit_end, body) = self.unit_extent(head.kw, end);
                    let sig_end = body.map_or(unit_end.saturating_sub(1), |(open, _)| open);
                    if public && scope.report {
                        let uf = head.unsafe_mod || (scope.extern_block && !head.safe_mod);
                        let raw = self.raw_pointer_in(head.kw, sig_end);
                        let boundary = self.boundary_type_in(head.kw, sig_end);
                        let name = self.name_after(head.kw);
                        let line = self.line(head.kw);
                        self.record(
                            "fn",
                            name,
                            line,
                            PublicApiHazards {
                                unsafe_fn: uf,
                                unsafe_impl: false,
                                raw_pointer: raw,
                                boundary_type: boundary,
                            },
                        );
                    }
                    if let Some((open, close)) = body {
                        self.items(open + 1, close, scope.body());
                    }
                    unit_end
                }
                Kind::Struct => {
                    let (unit_end, body) = self.unit_extent(head.kw, end);
                    let report = scope.report && public;
                    if report && self.name_after(head.kw) == "PrivateRegionAllocator" {
                        self.record(
                            "type",
                            "PrivateRegionAllocator".to_owned(),
                            self.line(head.kw),
                            PublicApiHazards {
                                unsafe_fn: false,
                                unsafe_impl: false,
                                raw_pointer: false,
                                boundary_type: true,
                            },
                        );
                    }
                    match body {
                        Some((open, close)) => self.named_fields(open + 1, close, false, report),
                        None => {
                            if let Some((open, close)) = self.first_paren(head.kw, unit_end) {
                                self.tuple_fields(open + 1, close, false, report);
                            }
                        }
                    }
                    unit_end
                }
                Kind::Enum => {
                    let (unit_end, body) = self.unit_extent(head.kw, end);
                    if let Some((open, close)) = body {
                        self.enum_body(open + 1, close, scope.report && public);
                    }
                    unit_end
                }
                Kind::Trait => {
                    let (unit_end, body) = self.unit_extent(head.kw, end);
                    if let Some((open, close)) = body {
                        self.items(open + 1, close, scope.inner(public));
                    }
                    unit_end
                }
                Kind::Impl => {
                    let (unit_end, body) = self.unit_extent(head.kw, end);
                    if let Some((open, close)) = body {
                        let trait_impl = self.is_trait_impl(head.kw, open);
                        let pinned_allocator = trait_impl
                            && self.is_pinned_allocator_impl(head.kw, open, head.unsafe_mod);
                        if trait_impl && head.unsafe_mod && !pinned_allocator && scope.report {
                            self.record(
                                "impl",
                                self.name_after(head.kw),
                                self.line(head.kw),
                                PublicApiHazards {
                                    unsafe_fn: false,
                                    unsafe_impl: true,
                                    raw_pointer: false,
                                    boundary_type: false,
                                },
                            );
                        }
                        self.items(
                            open + 1,
                            close,
                            scope.inner(trait_impl && !pinned_allocator),
                        );
                    }
                    unit_end
                }
                Kind::Mod => {
                    let (unit_end, body) = self.unit_extent(head.kw, end);
                    if let Some((open, close)) = body {
                        self.items(open + 1, close, scope.inner(false));
                    }
                    unit_end
                }
                Kind::ExternBlock => {
                    let close = self.matching(head.kw, end);
                    self.items(
                        head.kw + 1,
                        close,
                        Scope {
                            inherited_pub: false,
                            report: scope.report,
                            extern_block: true,
                        },
                    );
                    (close + 1).min(end)
                }
                Kind::TypeAlias => {
                    let unit_end = self.unit_end_semi(head.kw, end);
                    if public
                        && scope.report
                        && (self.raw_pointer_in(head.kw, unit_end)
                            || self.boundary_type_in(head.kw, unit_end))
                    {
                        let name = self.name_after(head.kw);
                        let line = self.line(head.kw);
                        self.record(
                            "type",
                            name,
                            line,
                            PublicApiHazards {
                                unsafe_fn: false,
                                unsafe_impl: false,
                                raw_pointer: self.raw_pointer_in(head.kw, unit_end),
                                boundary_type: self.boundary_type_in(head.kw, unit_end),
                            },
                        );
                    }
                    unit_end
                }
                Kind::Const | Kind::Static => {
                    let unit_end = self.unit_end_semi(head.kw, end);
                    if public && scope.report {
                        // Only the DECLARED type is exported; `skip_type` stops
                        // at the `=`, so the initialiser — an expression, where
                        // a `*` is a dereference — is outside the region.
                        let colon = (head.kw..unit_end).find(|i| self.tt(*i) == ":");
                        if let Some(colon) = colon {
                            let ty_end = self.skip_type(colon + 1, unit_end);
                            if self.raw_pointer_in(colon + 1, ty_end)
                                || self.boundary_type_in(colon + 1, ty_end)
                            {
                                let kind = if head.kind == Kind::Const {
                                    "const"
                                } else {
                                    "static"
                                };
                                let name = self.name_after(head.kw);
                                let line = self.line(head.kw);
                                self.record(
                                    kind,
                                    name,
                                    line,
                                    PublicApiHazards {
                                        unsafe_fn: false,
                                        unsafe_impl: false,
                                        raw_pointer: self.raw_pointer_in(colon + 1, ty_end),
                                        boundary_type: self.boundary_type_in(colon + 1, ty_end),
                                    },
                                );
                            }
                        }
                    }
                    unit_end
                }
                Kind::Use => {
                    let unit_end = self.unit_end_semi(head.kw, end);
                    if public && scope.report && self.boundary_type_in(head.kw, unit_end) {
                        self.record(
                            "use",
                            self.toks[head.kw..unit_end]
                                .iter()
                                .find(|lexeme| {
                                    lexeme.ident
                                        && matches!(
                                            lexeme.text,
                                            "NonNull" | "Allocator" | "PrivateRegionAllocator"
                                        )
                                })
                                .map_or("<allocator-boundary>", |lexeme| lexeme.text)
                                .to_owned(),
                            self.line(head.kw),
                            PublicApiHazards {
                                unsafe_fn: false,
                                unsafe_impl: false,
                                raw_pointer: false,
                                boundary_type: true,
                            },
                        );
                    }
                    unit_end
                }
                // A macro body is token soup this reader does not expand. It is
                // skipped WITHOUT claiming the `pub` tokens inside it, so an
                // island that grows one fails the claim control rather than
                // being reported clean over source nobody parsed.
                Kind::MacroRules => {
                    let (unit_end, _) = self.unit_extent(head.kw, end);
                    unit_end
                }
                Kind::Unknown => {
                    if vis == Vis::Public && scope.report {
                        self.out.parse_failures.push(self.line(head.kw));
                    }
                    let (unit_end, body) = self.unit_extent(head.kw, end);
                    if let Some((open, close)) = body {
                        self.items(open + 1, close, scope.body());
                    }
                    unit_end
                }
            };
            i = next.max(head.kw + 1);
            if i <= stall {
                i = stall + 1;
            }
        }
    }

    /// The named fields of a struct, a union, or a struct-shaped enum variant.
    /// `all_public` is set for a variant, whose fields are public with the enum
    /// and cannot say `pub`.
    fn named_fields(&mut self, mut i: usize, end: usize, all_public: bool, report: bool) {
        while i < end {
            let stall = i;
            i = self.skip_attrs(i, end);
            if i >= end {
                break;
            }
            if self.tt(i) == "," {
                i += 1;
                continue;
            }
            let vis = self.read_vis(&mut i, end);
            let public = all_public || vis == Vis::Public;
            if self.toks.get(i).is_some_and(|t| t.ident) && self.tt(i + 1) == ":" {
                let name = self.toks[i].text.to_owned();
                let line = self.line(i);
                let ty = i + 2;
                let ty_end = self.skip_type(ty, end);
                if report && public {
                    self.out.public_items += 1;
                    if self.raw_pointer_in(ty, ty_end) || self.boundary_type_in(ty, ty_end) {
                        self.record(
                            "field",
                            name,
                            line,
                            PublicApiHazards {
                                unsafe_fn: false,
                                unsafe_impl: false,
                                raw_pointer: self.raw_pointer_in(ty, ty_end),
                                boundary_type: self.boundary_type_in(ty, ty_end),
                            },
                        );
                    }
                }
                i = ty_end;
            } else {
                if vis == Vis::Public && report {
                    self.out.parse_failures.push(self.line(i.min(end - 1)));
                }
                i = self.skip_type(i, end);
            }
            if i < end && self.tt(i) == "," {
                i += 1;
            }
            if i <= stall {
                i = stall + 1;
            }
        }
    }

    /// The positional fields of a tuple struct or a tuple-shaped enum variant.
    fn tuple_fields(&mut self, mut i: usize, end: usize, all_public: bool, report: bool) {
        let mut ordinal = 0usize;
        while i < end {
            let stall = i;
            i = self.skip_attrs(i, end);
            if i >= end {
                break;
            }
            if self.tt(i) == "," {
                i += 1;
                continue;
            }
            let line = self.line(i);
            let vis = self.read_vis(&mut i, end);
            let public = all_public || vis == Vis::Public;
            let ty_end = self.skip_type(i, end);
            if report && public {
                self.out.public_items += 1;
                if self.raw_pointer_in(i, ty_end) || self.boundary_type_in(i, ty_end) {
                    self.record(
                        "field",
                        ordinal.to_string(),
                        line,
                        PublicApiHazards {
                            unsafe_fn: false,
                            unsafe_impl: false,
                            raw_pointer: self.raw_pointer_in(i, ty_end),
                            boundary_type: self.boundary_type_in(i, ty_end),
                        },
                    );
                }
            }
            ordinal += 1;
            i = ty_end;
            if i < end && self.tt(i) == "," {
                i += 1;
            }
            if i <= stall {
                i = stall + 1;
            }
        }
    }

    /// The variants of an enum. Every payload is public with the enum itself.
    fn enum_body(&mut self, mut i: usize, end: usize, report: bool) {
        while i < end {
            let stall = i;
            i = self.skip_attrs(i, end);
            if i >= end {
                break;
            }
            if self.tt(i) == "," {
                i += 1;
                continue;
            }
            if !self.toks[i].ident {
                i += 1;
                continue;
            }
            i += 1;
            match self.tt(i) {
                "(" => {
                    let close = self.matching(i, end);
                    self.tuple_fields(i + 1, close, true, report);
                    i = close + 1;
                }
                "{" => {
                    let close = self.matching(i, end);
                    self.named_fields(i + 1, close, true, report);
                    i = close + 1;
                }
                "=" => i = self.skip_expr(i + 1, end),
                _ => {}
            }
            if i < end && self.tt(i) == "," {
                i += 1;
            }
            if i <= stall {
                i = stall + 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One attribute, as the scanner sees it. Assembled from a `char` for the
    /// same reason the fixture is: a literal here would be a real site in this
    /// file. `fixture_is_not_visible_in_this_source` is what pins that.
    fn relaxes(attribute: &str) -> bool {
        let hash = '#';
        !scan_sites("<line>", &format!("{hash}{attribute}")).is_empty()
    }

    #[test]
    fn scanner_reproduces_its_own_fixture_exactly() {
        // If this ever drifts, every "zero sites" result elsewhere is unlicensed.
        let sites = scan_sites("<fixture>", &scanner_fixture());
        assert_eq!(sites.len(), SCANNER_FIXTURE_SITES, "fixture site count");
        let symbols: Vec<&str> = sites.iter().map(|s| s.symbol.as_str()).collect();
        assert_eq!(
            symbols,
            vec![
                "unsafe fn one() {}",
                "unsafe fn two() {}",
                "unsafe fn three() {}",
                "unsafe fn four() {}",
                "unsafe fn five() {}",
                MODULE_SCOPE_SYMBOL,
            ],
            "each site must name what it actually covers"
        );
    }

    #[test]
    fn scanner_rejects_comments_and_string_literals() {
        // The decoys in the fixture are the whole reason the count is 6 and not
        // 9; a naive `contains` scanner passes the count check by accident only
        // when the decoys are absent.
        let hash = '#';
        assert!(
            scan_sites("<line>", &format!("// {hash}[allow(unsafe_code)]")).is_empty(),
            "an allow inside a comment is not a site"
        );
        assert!(relaxes("[allow(unsafe_code)]"));
        assert!(relaxes("[allow(unsafe_code, clippy::x)]"));
        assert!(!relaxes("[allow(dead_code)]"));
        assert!(!relaxes("[deny(unsafe_code)]"));
        assert!(!relaxes("[doc = \"allow(unsafe_code)\"]"));
        assert!(!relaxes("[allow(clippy::unsafe_code_in_docs)]"));
    }

    /// The bypass this scanner was rewritten to close. Every one of these
    /// compiles `unsafe` under the `deny(unsafe_code)` root of an island, and
    /// every one of them was invisible to a scanner that required the attribute
    /// body to begin with `allow(`.
    #[test]
    fn wrapped_and_renamed_relaxations_are_all_counted() {
        assert!(relaxes(
            "[cfg_attr(target_arch = \"x86_64\", allow(unsafe_code))]"
        ));
        assert!(relaxes(
            "[cfg_attr(feature = \"a\", cfg_attr(feature = \"b\", allow(unsafe_code)))]"
        ));
        assert!(relaxes("[expect(unsafe_code)]"));
        assert!(relaxes("[warn(unsafe_code)]"));
        assert!(relaxes("![allow(unsafe_code)]"));
        assert!(relaxes("[ allow ( unsafe_code ) ]"));
        // …and the shapes that merely LOOK like the evasion must stay out, or
        // the ledger fills with rows describing nothing.
        assert!(!relaxes("[cfg_attr(feature = \"a\", forbid(unsafe_code))]"));
        assert!(!relaxes("[cfg_attr(feature = \"a\", deny(unsafe_code))]"));
        assert!(!relaxes("![forbid(unsafe_code)]"));
    }

    #[test]
    fn an_attribute_spread_across_lines_is_followed_to_its_bracket() {
        let hash = '#';
        let text = format!(
            "{hash}[cfg_attr(\n    all(target_os = \"linux\"),\n    allow(unsafe_code)\n)]\nunsafe fn spread() {{}}\n"
        );
        let sites = scan_sites("<multiline>", &text);
        assert_eq!(sites.len(), 1, "a wrapped allow does not stop being one");
        assert_eq!(sites[0].line, 1, "the site is reported where it opens");
        assert_eq!(
            sites[0].symbol, "unsafe fn spread() {}",
            "the symbol must skip the attribute's own arguments"
        );
    }

    /// A `]` or a quote inside a string used to be able to truncate the body or
    /// swallow the rest of it. Both directions are wrong, so both are pinned.
    #[test]
    fn string_and_char_literals_inside_an_attribute_do_not_confuse_the_scan() {
        assert!(relaxes("[cfg_attr(all(x = \"]\"), allow(unsafe_code))]"));
        assert!(relaxes("[cfg_attr(all(x = '\"'), allow(unsafe_code))]"));
        assert!(relaxes(
            "[cfg_attr(all(x = r#\"a \"quoted\" ]\"#), allow(unsafe_code))]"
        ));
        assert!(!relaxes("[doc = r#\"allow(unsafe_code)\"#]"));
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

    fn lint_level(manifest: &str) -> Option<String> {
        let table = crate::toml::parse(manifest).expect("manifest parses");
        let Some(crate::toml::Value::Table(workspace)) = table.get("workspace") else {
            return None;
        };
        workspace_unsafe_lint_level(workspace)
    }

    /// The workspace lint default is read as data, so every respelling that
    /// means the same thing agrees, and everything that merely CONTAINS the
    /// words does not.
    #[test]
    fn workspace_forbid_is_read_structurally_not_textually() {
        // Semantically identical spellings: all forbid.
        for manifest in [
            "[workspace]\n[workspace.lints.rust]\nunsafe_code = \"forbid\"\n",
            "[workspace]\n[workspace.lints.rust]\nunsafe_code=\"forbid\"\n",
            "[workspace]\n[workspace.lints.rust]\nunsafe_code = 'forbid'\n",
            "[workspace]\n[workspace.lints.rust]\n  unsafe_code   =   \"forbid\"  # pinned\n",
        ] {
            assert_eq!(
                lint_level(manifest).as_deref(),
                Some("forbid"),
                "spelling must not change the level: {manifest:?}"
            );
        }

        // Semantically different manifests: none of these forbid anything,
        // and every one of them satisfied the substring test that was here.
        for manifest in [
            // the shape of this repository's own header comment
            "# Workspace-level `unsafe_code = \"forbid\"` applies to all members.\n[workspace]\n",
            // a commented-out lint table
            "[workspace]\n[workspace.lints.rust]\n# unsafe_code = \"forbid\"\n",
            // the right level on the wrong lint namespace
            "[workspace]\n[workspace.lints.clippy]\nunsafe_code = \"forbid\"\n",
            // a package-level table, which no member inherits
            "[workspace]\n[lints.rust]\nunsafe_code = \"forbid\"\n",
            // present, but not forbidding
            "[workspace]\n[workspace.lints.rust]\nunsafe_code = \"deny\"\n",
        ] {
            assert_ne!(
                lint_level(manifest).as_deref(),
                Some("forbid"),
                "this manifest does not forbid unsafe_code: {manifest:?}"
            );
        }
    }

    /// The mutation proof, against the REAL manifest rather than a fixture.
    ///
    /// Deleting the live lint table must flip the verdict. Under the substring
    /// test it did not: the prose comment at the top of `Cargo.toml` carries
    /// the searched text, so the check could not fail on this repository.
    #[test]
    fn deleting_the_real_lint_table_flips_the_verdict() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("repo root")
            .to_path_buf();
        let real = fs::read_to_string(root.join("Cargo.toml")).expect("workspace manifest");
        assert_eq!(
            lint_level(&real).as_deref(),
            Some("forbid"),
            "the real workspace must forbid unsafe_code"
        );
        assert!(
            real.contains("unsafe_code = \"forbid\""),
            "the substring is present here in prose as well as in the lint table, \
             which is exactly why a substring test could not fail"
        );

        let gutted: String = real
            .lines()
            .filter(|line| {
                let t = line.trim();
                t != "[workspace.lints.rust]" && t != "unsafe_code = \"forbid\""
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            gutted.contains("unsafe_code = \"forbid\""),
            "the prose comment survives the deletion, so a substring test still passes"
        );
        assert_eq!(
            lint_level(&gutted),
            None,
            "with the lint table gone the workspace forbids nothing, and the checker \
             must say so"
        );
    }

    /// Install the lane plumbing needed by synthetic workspaces whose ledger
    /// intentionally contains no admitted sites. Keeping this fixture complete
    /// prevents a missing manifest from short-circuiting the older topology
    /// mutations before they reach the assertion they are meant to exercise.
    fn write_empty_verification_fixture(root: &Path) {
        fs::create_dir_all(root.join("scripts")).expect("scripts dir");
        fs::write(
            root.join("rust-toolchain.toml"),
            "[toolchain]\nchannel = \"nightly-2026-07-05\"\ncomponents = [\"miri\", \"rust-src\", \"llvm-tools-preview\"]\n",
        )
        .expect("toolchain");
        fs::write(
            root.join("registries/checker_index.toml"),
            "[registry]\nname = \"checker_index\"\n\n[[checker]]\nsymbol = \"w1_unsafe_tool_lanes\"\nkind = \"script\"\nstatus = \"live\"\nartifact = \"scripts/w1_unsafe_tool_lanes.sh\"\nunit = \"artifact\"\n",
        )
        .expect("checker index");
        fs::write(
            root.join("scripts/w1_unsafe_tool_lanes.sh"),
            "#!/usr/bin/env bash\nexit 1\n",
        )
        .expect("lane runner");
        fs::write(
            root.join(VERIFICATION_LANES_PATH),
            r#"schema_version = 1
cell = []

[[lane]]
tool = "miri"
status = "declared"
target = "x86_64-unknown-linux-gnu"
required_components = ["miri", "rust-src"]
runner = "scripts/w1_unsafe_tool_lanes.sh"
no_claim_boundary = "Synthetic zero-site fixture; no checked Miri claim."

[[lane]]
tool = "asan"
status = "declared"
target = "x86_64-unknown-linux-gnu"
required_components = ["rust-src", "llvm-tools-preview"]
runner = "scripts/w1_unsafe_tool_lanes.sh"
no_claim_boundary = "Synthetic zero-site fixture; no checked ASAN claim."

[[lane]]
tool = "tsan"
status = "declared"
target = "x86_64-unknown-linux-gnu"
required_components = ["rust-src", "llvm-tools-preview"]
runner = "scripts/w1_unsafe_tool_lanes.sh"
no_claim_boundary = "Synthetic zero-site fixture; no checked TSAN claim."
"#,
        )
        .expect("verification lanes");
    }

    /// Build a throwaway workspace and return `check_workspace`'s violation codes.
    fn synthetic_verdict(tag: &str, members_block: &str, member_manifest: &str) -> Vec<String> {
        let root = std::env::temp_dir().join(format!("fgdb-unsafe-ledger-{tag}"));
        let crate_dir = root.join("crates/fgdb-probe");
        fs::create_dir_all(crate_dir.join("src")).expect("crate dir");
        fs::create_dir_all(root.join("registries")).expect("registries dir");
        write_empty_verification_fixture(&root);
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[workspace]\nresolver = \"3\"\n{members_block}\n[workspace.lints.rust]\nunsafe_code = \"forbid\"\n"
            ),
        )
        .expect("workspace manifest");
        fs::write(
            root.join(LEDGER_PATH),
            "schema_version = 1\n\n[[island]]\nname = \"fgdb-unsafe-arena\"\ncharter = \"arena internals\"\nstatus = \"planned\"\n",
        )
        .expect("ledger");
        fs::write(crate_dir.join("Cargo.toml"), member_manifest).expect("member manifest");
        // An ordinary crate holding an unledgered relaxation. Assembled from a
        // `char` for the same reason `scanner_fixture` is.
        let hash = '#';
        fs::write(
            crate_dir.join("src/lib.rs"),
            format!("{hash}[allow(unsafe_code)]\npub unsafe fn probe() {{}}\n"),
        )
        .expect("member source");
        let (_report, violations) = check_workspace(&root);
        violations.into_iter().map(|v| v.code).collect()
    }

    /// The roster is resolved as data, so layout and quoting cannot shrink it.
    /// Under the line scan, the literal-quote and single-line forms resolved to
    /// ZERO members and the run reported a clean boundary with an unledgered
    /// `unsafe fn` sitting in the tree.
    #[test]
    fn member_roster_is_quote_and_layout_invariant() {
        let inherits =
            "[package]\nname = \"fgdb-probe\"\nedition = \"2024\"\n\n[lints]\nworkspace = true\n";
        for (tag, members_block) in [
            ("multiline", "members = [\n    \"crates/fgdb-probe\",\n]\n"),
            ("oneline", "members = [\"crates/fgdb-probe\"]\n"),
            ("literal", "members = [\n    'crates/fgdb-probe',\n]\n"),
            ("glob", "members = [\n    \"crates/*\",\n]\n"),
        ] {
            let codes = synthetic_verdict(tag, members_block, inherits);
            assert!(
                codes.contains(&"unsafe_allow_outside_island".to_string())
                    && codes.contains(&"site_unledgered".to_string()),
                "the unledgered site must be found however the roster is written \
                 ({tag}): got {codes:?}"
            );
            assert!(
                !codes.contains(&"workspace_has_no_members".to_string()),
                "the roster must resolve ({tag}): got {codes:?}"
            );
        }
    }

    /// Both directions of the inheritance verdict, end to end.
    #[test]
    fn inheriting_forbid_passes_and_omitting_it_fails() {
        let with_lints =
            "[package]\nname = \"fgdb-probe\"\nedition = \"2024\"\n\n[lints]\nworkspace = true\n";
        let without = "[package]\nname = \"fgdb-probe\"\nedition = \"2024\"\n";
        let members = "members = [\n    \"crates/fgdb-probe\",\n]\n";

        let inheriting = synthetic_verdict("inherits", members, with_lints);
        assert!(
            !inheriting.contains(&"member_does_not_inherit_forbid".to_string()),
            "a crate carrying `[lints] workspace = true` inherits forbid: {inheriting:?}"
        );
        let escaping = synthetic_verdict("escapes", members, without);
        assert!(
            escaping.contains(&"member_does_not_inherit_forbid".to_string()),
            "a crate omitting `[lints] workspace = true` must FAIL: {escaping:?}"
        );
    }

    /// The non-vacuity control: a roster that resolves to nothing is a failure,
    /// not a clean boundary over an empty set.
    #[test]
    fn an_empty_member_roster_is_a_violation() {
        let root = std::env::temp_dir().join("fgdb-unsafe-ledger-empty-roster");
        fs::create_dir_all(root.join("registries")).expect("registries dir");
        fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nresolver = \"3\"\nmembers = []\n\n[workspace.lints.rust]\nunsafe_code = \"forbid\"\n",
        )
        .expect("workspace manifest");
        fs::write(
            root.join(LEDGER_PATH),
            "schema_version = 1\n\n[[island]]\nname = \"fgdb-unsafe-arena\"\ncharter = \"arena internals\"\nstatus = \"planned\"\n",
        )
        .expect("ledger");
        write_empty_verification_fixture(&root);
        let (report, violations) = check_workspace(&root);
        assert_eq!(report.crates_scanned, 0);
        let codes: Vec<String> = violations.into_iter().map(|v| v.code).collect();
        assert!(
            codes.contains(&"workspace_has_no_members".to_string()),
            "zero resolved members must fail: {codes:?}"
        );
    }

    /// The real workspace is scanned, and the count is not zero. This is the
    /// number the substring/line-scan pair could silently drive to nothing.
    #[test]
    fn the_real_workspace_resolves_every_member() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("repo root")
            .to_path_buf();
        let text = fs::read_to_string(root.join("Cargo.toml")).expect("workspace manifest");
        let table = crate::toml::parse(&text).expect("workspace manifest parses");
        let workspace = table
            .get("workspace")
            .and_then(|value| match value {
                crate::toml::Value::Table(t) => Some(t),
                _ => None,
            })
            .expect("[workspace] section");
        let members = resolve_members(&root, workspace).expect("roster resolves");
        assert!(
            members.len() >= 10,
            "the real workspace has a substantial roster; got {}: {members:?}",
            members.len()
        );
        assert!(
            members.iter().any(|m| m.ends_with("tools/registry-check")),
            "the roster must include this crate: {members:?}"
        );
    }
}

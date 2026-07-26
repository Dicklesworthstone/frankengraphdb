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
//! three islands with six ledgered sites, and every crate outside them still
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

use crate::toml::{self, get_str, get_table_array};

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
struct Masked {
    text: String,
    line_starts: Vec<usize>,
}

impl Masked {
    /// The 1-based line containing `offset`.
    fn line_of(&self, offset: usize) -> usize {
        self.line_starts.partition_point(|start| *start <= offset)
    }
}

/// Mask a whole source file in one pass, so comment and literal state carries
/// across lines. Running the masker per candidate — which is what this scanner
/// used to do — cannot see that a line is already inside a `/*` opened three
/// lines above it, which is why commented-out attributes counted as real sites.
fn mask_source(text: &str) -> Masked {
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

    /// Build a throwaway workspace and return `check_workspace`'s violation codes.
    fn synthetic_verdict(tag: &str, members_block: &str, member_manifest: &str) -> Vec<String> {
        let root = std::env::temp_dir().join(format!("fgdb-unsafe-ledger-{tag}"));
        let crate_dir = root.join("crates/fgdb-probe");
        fs::create_dir_all(crate_dir.join("src")).expect("crate dir");
        fs::create_dir_all(root.join("registries")).expect("registries dir");
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

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
//! that signature. Four structural answers:
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
//!    — and it would have become live the day the first island landed, since an
//!    island root uses `deny`, which every one of those forms *can* lower. See
//!    [`LEVELS_BELOW_DENY`].
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
/// A line whose first non-whitespace is `//` is a comment and an attribute must
/// begin its trimmed line, so an occurrence inside a string or a nested
/// expression is excluded. Everything past that is decided **structurally**: the
/// attribute is followed to its matching `]` across lines, its comments and
/// string literals are blanked out, and it counts if any of
/// [`LEVELS_BELOW_DENY`] names `unsafe_code` at any depth inside it. Prefix
/// matching was the original bug — `cfg_attr(…, allow(unsafe_code))` walked
/// straight past a scanner that required the body to start with `allow(`.
pub fn scan_sites(path: &str, text: &str) -> Vec<ScannedSite> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (index, raw) in lines.iter().enumerate() {
        let line = raw.trim();
        if line.starts_with("//") {
            continue;
        }
        let inner = if line.starts_with("#![") {
            true
        } else if line.starts_with("#[") {
            false
        } else {
            continue;
        };
        let (body, end) = attribute_body(&lines, index);
        if !body_relaxes_unsafe_code(&body) {
            continue;
        }
        // The symbol is what a reviewer reads to know what the site covers: for
        // an outer attribute the item it precedes, for an inner one the module
        // it sits inside, which is broader and must not be reported as narrower.
        let symbol = if inner {
            MODULE_SCOPE_SYMBOL.to_owned()
        } else {
            symbol_after(&lines, end)
        };
        out.push(ScannedSite {
            path: path.to_owned(),
            line: index + 1,
            symbol,
        });
    }
    out
}

/// The item an outer attribute applies to: the next line that is not blank, a
/// comment, or another attribute. Intervening attributes are stepped over as
/// whole spans, so a multi-line one cannot leave its own arguments standing in
/// for the item.
fn symbol_after(lines: &[&str], attribute_end: usize) -> String {
    let mut index = attribute_end + 1;
    while index < lines.len() {
        let line = lines[index].trim();
        if line.is_empty() || line.starts_with("//") {
            index += 1;
            continue;
        }
        if line.starts_with("#[") || line.starts_with("#![") {
            index = attribute_body(lines, index).1 + 1;
            continue;
        }
        return line.to_owned();
    }
    String::new()
}

/// Collect one attribute starting at `start`, returning its body (the text
/// between the outermost brackets, with comments and string literals blanked
/// out) and the index of the line its `]` lands on.
///
/// The masking is what makes the structural match trustworthy in both
/// directions: without it a `]` inside a string truncates the body early (a
/// missed site) and an `allow(unsafe_code)` inside a `doc` string invents one
/// (a bogus ledger row).
fn attribute_body(lines: &[&str], start: usize) -> (String, usize) {
    let stop = lines.len().min(start + MAX_ATTRIBUTE_LINES);
    let mut masked = String::new();
    let mut line_starts = Vec::new();
    let mut state = MaskState::Code;
    let mut depth = 0usize;
    let mut open = None;
    let mut close = None;
    for (offset, raw) in lines[start..stop].iter().enumerate() {
        if offset > 0 {
            masked.push('\n');
        }
        line_starts.push(masked.len());
        let scan_from = masked.len();
        // The first line is trimmed so the opening bracket is the first `[`.
        let text = if offset == 0 { raw.trim_start() } else { raw };
        mask_line(text, &mut state, &mut masked);
        for (index, byte) in masked.as_bytes().iter().enumerate().skip(scan_from) {
            match byte {
                b'[' => {
                    open.get_or_insert(index);
                    depth += 1;
                }
                b']' if depth > 0 => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(index);
                        break;
                    }
                }
                _ => {}
            }
        }
        if close.is_some() {
            break;
        }
    }
    let Some(open) = open else {
        return (String::new(), start);
    };
    let close = close.unwrap_or(masked.len());
    let end = start
        + line_starts
            .iter()
            .rposition(|offset| *offset <= close)
            .unwrap_or(0);
    (masked[open + 1..close].to_owned(), end)
}

/// Does this attribute body set `unsafe_code` to a level below `deny`?
fn body_relaxes_unsafe_code(body: &str) -> bool {
    let bytes = body.as_bytes();
    for level in LEVELS_BELOW_DENY {
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
    }
    false
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
                    blank(out, 2);
                    i += 2;
                    *state = if depth <= 1 {
                        MaskState::Code
                    } else {
                        MaskState::Block(depth - 1)
                    };
                } else if c == '/' && next == Some('*') {
                    blank(out, 2);
                    i += 2;
                    *state = MaskState::Block(depth + 1);
                } else {
                    blank(out, 1);
                    i += 1;
                }
            }
            MaskState::Str => {
                if c == '\\' {
                    blank(out, if next.is_some() { 2 } else { 1 });
                    i += 2;
                } else if c == '"' {
                    out.push('"');
                    i += 1;
                    *state = MaskState::Code;
                } else {
                    blank(out, 1);
                    i += 1;
                }
            }
            MaskState::RawStr(hashes) => {
                let closes = c == '"'
                    && i + 1 + hashes <= chars.len()
                    && chars[i + 1..i + 1 + hashes].iter().all(|c| *c == '#');
                if closes {
                    out.push('"');
                    blank(out, hashes);
                    i += 1 + hashes;
                    *state = MaskState::Code;
                } else {
                    blank(out, 1);
                    i += 1;
                }
            }
            MaskState::Code => {
                if c == '/' && next == Some('/') {
                    blank(out, chars.len() - i);
                    i = chars.len();
                } else if c == '/' && next == Some('*') {
                    blank(out, 2);
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
                            blank(out, len - 2);
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

fn blank(out: &mut String, count: usize) {
    for _ in 0..count {
        out.push(' ');
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
                let inherits = match crate::topology::scan_manifest(member, &text) {
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
        assert!(relaxes("[cfg_attr(target_arch = \"x86_64\", allow(unsafe_code))]"));
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

    #[test]
    fn workspace_members_parses_the_real_shape() {
        let manifest = "[workspace]\nresolver = \"3\"\nmembers = [\n    \"crates/a\",\n    \"tools/b\",\n]\n\n[workspace.package]\n";
        assert_eq!(workspace_members(manifest), vec!["crates/a", "tools/b"]);
    }
}

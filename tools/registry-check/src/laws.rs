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

use crate::toml::{get_opt_str, get_opt_str_array, get_str, get_table_array, parse};
use std::collections::BTreeSet;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub const REGISTRY_PATH: &str = "registries/laws.toml";

/// Every key a `[[law]]` row may carry. A key outside this set is a violation,
/// not a shrug.
pub const KNOWN_LAW_KEYS: [&str; 8] = [
    "id",
    "name",
    "source_location",
    "statement",
    "enforcement",
    "status",
    "cited_as",
    "note",
];

/// The closed status vocabulary. `registered` licenses a citation; the others
/// do not, and the citation guard keys on exactly this distinction. `struck`
/// records a fabrication adjudicated by owner ruling (mv6g sitting,
/// 2026-08-01/02): the name licenses nothing, its citations were repaired away
/// in the same landing, and the row remains as the permanent record that the
/// phrase claims no authority.
pub const LAW_STATUSES: [&str; 4] = [
    "registered",
    "unadjudicated",
    "fabrication-candidate",
    "struck",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Law {
    pub id: String,
    pub name: String,
    pub source_location: String,
    pub statement: String,
    pub enforcement: String,
    pub status: String,
    /// Alternate surface phrases the catalog uses for THIS law.
    ///
    /// One rule is cited under more than one wording — the same
    /// `SequenceNeutralSpec` rule is cited both bare and as the "wrapper"
    /// form. Declaring the alternate phrasings here is what lets the citation
    /// guard resolve by registry lookup instead of by rewriting catalog prose,
    /// which would be a coordinated edit across the catalog and every
    /// projection generated from it.
    ///
    /// An alias shares the `name` namespace: it may not collide with another
    /// law's name or alias, or a citation would resolve ambiguously.
    pub cited_as: Vec<String>,
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
            cited_as: get_opt_str_array(row, "cited_as", &ctx)
                .map_err(|e| e.to_string())?
                .unwrap_or_default(),
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
    let Some((prefix, rest)) = value.split_once(':') else {
        return false;
    };
    match prefix {
        // "plan:LINE" — a normative statement outside the appendix slices
        // (owner ruling, mv6g sitting 2026-08-01/02: FG-LAW-05's statement
        // lives at §5.2, plan line 392, not in any aNN slice).
        "plan" => !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()),
        // "enforcement:name" — an enforcement-anchored registration (same
        // sitting: FG-LAW-06 is real but stated nowhere in the plan; its
        // authority is the checker law that enforces it). Grammar here,
        // resolution in the test suite — the same split the aNN anchors use.
        "enforcement" => {
            !rest.is_empty()
                && rest
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        }
        _ => {
            let mut chars = prefix.chars();
            let Some(first) = chars.next() else {
                return false;
            };
            first.is_ascii_lowercase()
                && chars.clone().count() == 2
                && chars.all(|c| c.is_ascii_digit())
                && !rest.is_empty()
                && rest.chars().all(|c| c.is_ascii_digit())
        }
    }
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
        // Aliases share the name namespace: the citation guard resolves a
        // surface phrase against names and aliases alike, so a collision makes
        // one citation resolve to two laws.
        for alias in &law.cited_as {
            if alias.trim().is_empty() {
                out.push(Violation::new(
                    "law_name_empty",
                    &law.id,
                    "an empty cited_as alias cannot be cited",
                ));
            } else if !seen_names.insert(alias.as_str()) {
                out.push(Violation::new(
                    "law_name_duplicate",
                    &law.id,
                    format!(
                        "cited_as alias {alias:?} collides with a law name or alias already declared; a citation would resolve ambiguously"
                    ),
                ));
            }
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
                        "registered law has source_location {:?}, which is not an aNN:LINE, plan:LINE, or enforcement:NAME anchor; the anchor is what makes the law falsifiable",
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

// ===========================================================================
// The citation guard — fgdb-law-citation-guard-ld8f
// ===========================================================================
//
// WHAT IT DECIDES. Catalog prose cites named laws to license the annotations
// it carries. This guard requires every such citation to resolve to a row in
// the registry above, and requires that row to be `registered`. A citation
// that resolves to nothing, and a `law` token whose shape the extractor cannot
// parse, are both failures with their own violation codes.
//
// THE DENOMINATOR, measured by this code at HEAD c25babd on 2026-07-27 and
// printed by `citation_census_is_not_vacuous` under `--nocapture`:
//
//   93 `law` tokens in the subject
//   92 citations, of 9 distinct names
//   86 licensed by a `registered` law (FG-LAW-01..04)
//    6 open adjudications, ceilinged below (FG-LAW-05..08)
//    1 generic ("No source law requires authenticated membership proofs")
//    0 unrecognised
//
// The registry adjudicates 10 names; the tenth, FG-LAW-09, has zero citations
// left in the catalog because fgdb-hvfn repaired the row that carried it.
//
// WHY IT IS NOT CIRCULAR, stated because the first law sweep WAS. The subject
// is `registries/appendix_a_catalog.toml`; the referent set is
// `registries/laws.toml`; and every `registered` row's `source_location`
// resolves against the plan, checked mechanically by
// `every_registered_anchor_resolves_in_the_plan`. So the chain grounds out in
// the normative source, not in a copy of the subject. The earlier attempt read
// three laws as RESOLVES because the checker held 30% of the catalog's own
// prose and answered every existence query from that mirror
// (fgdb-checker-mirrors-subject-prose-23u1, fgdb-n061). No law name or plan
// anchor is written into this checker: the names live in the registry, the
// citations live in the catalog, and this file only relates them.
//
// THE COMPLETENESS GUARD IS THE WHOLE INSTRUMENT. A fabricated law is by
// definition the one the extractor has no entry for, so an unparsed `law`
// token must FAIL rather than be skipped — otherwise the sweep is vacuous in
// exactly the region a fabrication lives in. Every `law`/`laws` token in the
// subject file lands in exactly one of three classes and the third is a
// failure; there is no fourth, silent class.
//
// THE ONE DISCRIMINATION THAT NEEDED PROVING. "No source law requires
// authenticated membership proofs" is ordinary English, not a citation, and a
// naive extractor lands red on it. The rule that separates it is structural
// rather than a per-sentence exemption: a citation is a DEFINITE reference to
// a named law, introduced by `the`, by an open paren, or by a source anchor. A
// negative or indefinite quantifier ("no ... law", "any ... law") governs a
// generic noun phrase that names nothing and therefore licenses nothing, so it
// has no referent to resolve. Both directions are red-proved in
// `tools/registry-check/tests/laws.rs`: a planted fabrication is caught, and
// turning that same sentence definite turns it into a citation that fails.

/// The artifact whose prose this guard reads. Scanned as TEXT, not through the
/// catalog parser: a field-by-field walk is only as total as its list of prose
/// fields, and a prose field nobody listed is the same hole as a citation shape
/// nobody parsed. Reading the whole file has no such list.
pub const CITATION_SUBJECT: &str = "registries/appendix_a_catalog.toml";

/// A law name is a noun phrase, never a sentence. The longest name in the
/// registry is four tokens; a longer run is not silently accepted, it is
/// reported as an unrecognised shape.
pub const MAX_NAME_TOKENS: usize = 4;

/// Determiners under which the head `law` is generic — it refers to no
/// particular law, so there is nothing for it to resolve to. Reachable only
/// when the neighbourhood carries neither a definite article nor a source
/// anchor; see `classify_law_token`.
pub const GENERIC_DETERMINERS: [&str; 9] = [
    "no", "any", "some", "every", "each", "a", "an", "another", "such",
];

/// Citations of laws the owner has not yet adjudicated, as a per-law CEILING.
///
/// EMPTIED 2026-08-02 by owner ruling (mv6g sitting; fgdb-u259): FG-LAW-05 and
/// FG-LAW-06 were registered, FG-LAW-07 and FG-LAW-08 were struck with their
/// citations repaired in the same landing. The machinery stays live: a future
/// adjudication may add entries, a NEW citation of any unregistered law fails
/// immediately at the default ceiling of zero, and the stale and over-ceiling
/// branches are proven to fire by the test suite through
/// `validate_citations_with_ceiling`.
///
/// It is a ceiling and it cannot go stale, which is what separates it from an
/// ordinary waiver. A NEW citation of a listed law exceeds the ceiling and
/// fails. Repairing one is free. Repairing the last one drops the observed
/// count to zero, and an entry with zero observed citations fails as stale — so
/// the entry must be deleted when its cause is gone, and the list can only
/// shrink.
pub const OPEN_ADJUDICATION_CEILING: [(&str, usize); 0] = [];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CitationFrame {
    /// `the <name> law` — optionally with the anchor between the two.
    Definite,
    /// `(<name> law` — the parenthetical form, anchor usually trailing.
    Paren,
    /// `<aNN:LINE> <name> law` — introduced by the source anchor, no article.
    Anchored,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CitationClass {
    /// Names a law. Must resolve, and must resolve to a `registered` row.
    Cited { name: String, frame: CitationFrame },
    /// Quantified generic reference. Names nothing; owes nothing.
    Generic { determiner: String },
    /// The extractor could not parse this occurrence. Fails closed.
    Unrecognised,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CitationToken {
    pub line: usize,
    /// The catalog row this token sits in, for diagnostics — the nearest
    /// preceding `row_id = ` line.
    pub row_id: String,
    pub class: CitationClass,
    pub excerpt: String,
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Is this token a bare `aNN:LINE` source anchor?
fn is_anchor_token(token: &str) -> bool {
    is_source_anchor(token)
}

/// A name token may not span a sentence, a clause, or a parenthesis — only the
/// first token may carry the opening paren of the parenthetical frame. This is
/// what bounds the leftward walk: without it the walk finds a `the` from an
/// earlier clause and manufactures a citation out of unrelated prose.
fn is_name_token(token: &str, first: bool) -> bool {
    let body = if first {
        token.strip_prefix('(').unwrap_or(token)
    } else {
        token
    };
    if body.is_empty() {
        return false;
    }
    if body.ends_with('.') {
        return false;
    }
    if body
        .chars()
        .any(|c| matches!(c, ';' | ',' | '(' | ')' | '"'))
    {
        return false;
    }
    // A colon belongs to an anchor and nowhere else; anywhere else it ends a
    // clause.
    if body.contains(':') && !is_anchor_token(body) {
        return false;
    }
    true
}

/// Classify one `law` occurrence from the text to its left.
fn classify_law_token(left: &str, plural: bool) -> CitationClass {
    // A citation names ONE law. A plural head is generic by construction.
    if plural {
        return CitationClass::Generic {
            determiner: "<plural>".to_string(),
        };
    }
    let toks: Vec<&str> = left.split_whitespace().collect();
    let n = toks.len();

    for k in 1..=MAX_NAME_TOKENS {
        if k > n {
            break;
        }
        let window = &toks[n - k..];
        if !window
            .iter()
            .enumerate()
            .all(|(i, t)| is_name_token(t, i == 0))
        {
            continue;
        }
        let prev = if n > k { Some(toks[n - k - 1]) } else { None };

        if let Some(head) = window[0].strip_prefix('(') {
            let mut name = String::from(head);
            for t in &window[1..] {
                name.push(' ');
                name.push_str(t);
            }
            return CitationClass::Cited {
                name,
                frame: CitationFrame::Paren,
            };
        }
        if prev == Some("the") {
            // `the a01:1412 flattened-rendering law` — the anchor is part of
            // the citation, not part of the name.
            let name_tokens = if window.len() > 1 && is_anchor_token(window[0]) {
                &window[1..]
            } else {
                window
            };
            if !name_tokens.is_empty() {
                return CitationClass::Cited {
                    name: name_tokens.join(" "),
                    frame: CitationFrame::Definite,
                };
            }
        }
        if prev.is_some_and(is_anchor_token) {
            return CitationClass::Cited {
                name: window.join(" "),
                frame: CitationFrame::Anchored,
            };
        }
    }

    // The generic bucket is the only class that owes nothing, so it must not be
    // reachable from anything citation-shaped. A definite article or a source
    // anchor anywhere in the neighbourhood means this occurrence WAS meant to
    // point at a particular law; if the frames above could not parse it, that
    // is an unrecognised shape and it fails, rather than being absorbed here.
    let neighbourhood = &toks[n.saturating_sub(MAX_NAME_TOKENS + 1)..];
    let definite_nearby = neighbourhood
        .iter()
        .any(|t| *t == "the" || is_anchor_token(t));
    if !definite_nearby {
        for k in 1..=MAX_NAME_TOKENS {
            if k >= n {
                break;
            }
            let candidate = toks[n - k - 1].trim_matches(|c: char| !c.is_ascii_alphanumeric());
            if GENERIC_DETERMINERS.contains(&candidate.to_ascii_lowercase().as_str()) {
                return CitationClass::Generic {
                    determiner: candidate.to_string(),
                };
            }
        }
    }

    CitationClass::Unrecognised
}

/// Every `law`/`laws` token in `text`, classified. Total over the text by
/// construction: it walks bytes, not a list of fields.
pub fn extract_citations(text: &str) -> Vec<CitationToken> {
    let mut out = Vec::new();
    let mut row_id = String::from("(no row_id above this line)");
    for (lineno, line) in text.lines().enumerate() {
        if let Some(rest) = line.strip_prefix("row_id = ") {
            row_id = rest.trim().trim_matches('"').to_string();
        }
        let bytes = line.as_bytes();
        let mut i = 0usize;
        while i + 3 <= bytes.len() {
            if &bytes[i..i + 3] != b"law" {
                i += 1;
                continue;
            }
            let prev_ok = i == 0 || !is_word_byte(bytes[i - 1]);
            let plural = i + 4 <= bytes.len() && bytes[i + 3] == b's';
            let end = if plural { i + 4 } else { i + 3 };
            let next_ok = end >= bytes.len() || !is_word_byte(bytes[end]);
            if !(prev_ok && next_ok) {
                i += 1;
                continue;
            }
            let lo = line[..i]
                .char_indices()
                .rev()
                .nth(90)
                .map_or(0, |(idx, _)| idx);
            let hi = line[end..]
                .char_indices()
                .nth(30)
                .map_or(line.len(), |(idx, _)| end + idx);
            out.push(CitationToken {
                line: lineno + 1,
                row_id: row_id.clone(),
                class: classify_law_token(&line[..i], plural),
                excerpt: line[lo..hi].to_string(),
            });
            i = end;
        }
    }
    out
}

/// Resolve a cited surface phrase against the registry, by `name` or by an
/// alternate phrasing declared in `cited_as`.
pub fn resolve_citation<'a>(registry: &'a LawRegistry, name: &str) -> Option<&'a Law> {
    registry
        .laws
        .iter()
        .find(|law| law.name == name || law.cited_as.iter().any(|alias| alias == name))
}

/// The guard. Fails closed on an unresolvable citation, on an unrecognised
/// citation shape, and on a citation of a law the owner has not registered.
pub fn validate_citations(registry: &LawRegistry, occurrences: &[CitationToken]) -> Vec<Violation> {
    validate_citations_with_ceiling(registry, occurrences, &OPEN_ADJUDICATION_CEILING)
}

/// The guard body, parameterized over the ceiling so the test suite can prove
/// the stale and over-ceiling branches fire even while the shipped ceiling is
/// empty. Production always enters through `validate_citations`.
pub fn validate_citations_with_ceiling(
    registry: &LawRegistry,
    occurrences: &[CitationToken],
    ceiling: &[(&str, usize)],
) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut unlicensed: Vec<(&str, &CitationToken)> = Vec::new();

    for occurrence in occurrences {
        let CitationClass::Cited { name, .. } = &occurrence.class else {
            continue;
        };
        let subject = format!(
            "{}:{} {}",
            CITATION_SUBJECT, occurrence.line, occurrence.row_id
        );
        match resolve_citation(registry, name) {
            None => out.push(Violation::new(
                "law_citation_unresolvable",
                subject,
                format!(
                    "cites {name:?}, which resolves to no row in {REGISTRY_PATH}; a citation with no referent manufactures authority that was never adopted: ...{}...",
                    occurrence.excerpt
                ),
            )),
            Some(law) if law.status != "registered" => {
                unlicensed.push((law.id.as_str(), occurrence));
            }
            Some(_) => {}
        }
    }

    for occurrence in occurrences {
        if occurrence.class == CitationClass::Unrecognised {
            out.push(Violation::new(
                "law_citation_shape_unrecognised",
                format!(
                    "{}:{} {}",
                    CITATION_SUBJECT, occurrence.line, occurrence.row_id
                ),
                format!(
                    "a `law` word this extractor cannot parse as either a citation or a generic reference; it is not skipped, because a fabricated law is the one the extractor has no entry for: ...{}...",
                    occurrence.excerpt
                ),
            ));
        }
    }

    for (law_id, limit) in ceiling.iter().copied() {
        let observed = unlicensed.iter().filter(|(id, _)| *id == law_id).count();
        if observed == 0 {
            out.push(Violation::new(
                "law_citation_open_adjudication_stale",
                law_id,
                format!(
                    "the open-adjudication ceiling carries {law_id} at {limit}, but the catalog cites it {observed} times; either the law is now registered or its citations were repaired, and a ceiling entry outlives its cause only by being deleted"
                ),
            ));
        }
    }

    for (law_id, occurrence) in &unlicensed {
        let limit = ceiling
            .iter()
            .find(|(id, _)| id == law_id)
            .map_or(0, |(_, n)| *n);
        let observed = unlicensed.iter().filter(|(id, _)| id == law_id).count();
        if observed > limit {
            let status = registry
                .laws
                .iter()
                .find(|law| law.id == *law_id)
                .map_or("<unknown>", |law| law.status.as_str());
            out.push(Violation::new(
                "law_citation_not_licensed",
                format!(
                    "{}:{} {}",
                    CITATION_SUBJECT, occurrence.line, occurrence.row_id
                ),
                format!(
                    "cites {law_id}, whose status is {status:?}; only a `registered` law licenses a citation, and this law is cited {observed} times against an open-adjudication ceiling of {limit}: ...{}...",
                    occurrence.excerpt
                ),
            ));
        }
    }

    out
}

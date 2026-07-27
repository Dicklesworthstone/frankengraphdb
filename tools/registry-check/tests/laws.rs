//! Mutation suite for the named-law registry (fgdb-law-citation-sweep-uzzh).
//!
//! Every validation rule in `registry_check::laws` gets a test that takes the
//! real registry, breaks exactly one thing, and asserts the exact violation
//! code. A checker nobody has watched go red is a checker nobody knows works.
//!
//! The load-bearing test is `every_registered_anchor_resolves_in_the_plan`.
//! The whole point of `source_location` is that a law becomes falsifiable — a
//! reader opens the cited line and checks — so the suite checks it mechanically
//! rather than trusting the row. Without that test the registry would be a
//! second place to write unverifiable prose, which is the defect it exists to
//! end.

use registry_check::laws::{
    CITATION_SUBJECT, CitationClass, Law, LawRegistry, OPEN_ADJUDICATION_CEILING,
    extract_citations, load_from_repo, resolve_citation, validate_citations, validate_laws,
};
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn registry() -> LawRegistry {
    load_from_repo(&repo_root()).expect("laws.toml loads")
}

fn codes(registry: &LawRegistry) -> Vec<String> {
    validate_laws(registry)
        .into_iter()
        .map(|violation| violation.code)
        .collect()
}

/// The control. Every mutation test below is meaningless if the unmutated
/// registry is not clean, so this failing first tells you the suite is lying.
#[test]
fn real_registry_is_clean() {
    let violations = validate_laws(&registry());
    assert!(
        violations.is_empty(),
        "the shipped law registry must validate clean, found: {violations:?}"
    );
}

#[test]
fn registry_declares_at_least_one_registered_law() {
    let registry = registry();
    assert!(
        registry.laws.iter().any(|law| law.status == "registered"),
        "a registry with no registered law licenses no citation at all"
    );
}

/// The reason `source_location` exists: a registered law must point at a plan
/// line a reader can open. This resolves every anchor against the real plan.
#[test]
fn every_registered_anchor_resolves_in_the_plan() {
    let plan = std::fs::read_to_string(
        repo_root().join("COMPREHENSIVE_PLAN_FOR_THE_DESIGN_OF_FRANKENGRAPHDB.md"),
    )
    .expect("plan is readable");
    let lines: Vec<&str> = plan.lines().collect();
    let registry = registry();
    let mut checked = 0usize;
    for law in registry.laws.iter().filter(|l| l.status == "registered") {
        let anchor = law.source_location.split_once(':');
        assert!(anchor.is_some(), "{} has no aNN:LINE anchor", law.id);
        let (_slice, digits) = anchor.expect("anchor presence asserted above");
        assert!(
            !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()),
            "{} anchor line {digits:?} is not a number",
            law.id
        );
        let line_no: usize = digits.parse().expect("digits asserted above");
        assert!(
            line_no >= 1 && line_no <= lines.len(),
            "{} cites line {line_no}, past the end of the plan ({} lines)",
            law.id,
            lines.len()
        );
        let text = lines[line_no - 1];
        assert!(
            !text.trim().is_empty(),
            "{} cites plan line {line_no}, which is blank",
            law.id
        );
        checked += 1;
    }
    assert!(checked > 0, "no registered anchors were checked");
}

fn mutate(f: impl FnOnce(&mut Law)) -> LawRegistry {
    let mut registry = registry();
    f(&mut registry.laws[0]);
    registry
}

#[test]
fn malformed_id_is_rejected() {
    let r = mutate(|law| law.id = "LAW-1".into());
    assert!(
        codes(&r).contains(&"law_id_malformed".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn duplicate_id_is_rejected() {
    let mut r = registry();
    let dup = r.laws[0].id.clone();
    r.laws[1].id = dup;
    assert!(
        codes(&r).contains(&"law_id_duplicate".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn duplicate_name_is_rejected() {
    let mut r = registry();
    let dup = r.laws[0].name.clone();
    r.laws[1].name = dup;
    assert!(
        codes(&r).contains(&"law_name_duplicate".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn empty_name_is_rejected() {
    let r = mutate(|law| law.name = "   ".into());
    assert!(
        codes(&r).contains(&"law_name_empty".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn unknown_status_is_rejected() {
    let r = mutate(|law| law.status = "probably-fine".into());
    assert!(
        codes(&r).contains(&"law_status_unknown".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn registered_law_without_statement_is_rejected() {
    let r = mutate(|law| {
        law.status = "registered".into();
        law.statement = String::new();
    });
    assert!(
        codes(&r).contains(&"law_statement_missing".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn registered_law_without_enforcement_is_rejected() {
    let r = mutate(|law| {
        law.status = "registered".into();
        law.enforcement = String::new();
    });
    assert!(
        codes(&r).contains(&"law_enforcement_missing".to_string()),
        "{:?}",
        codes(&r)
    );
}

/// The discriminator. A registered law with no anchor is exactly the shape
/// every fabricated citation in the catalog had.
#[test]
fn registered_law_without_anchor_is_rejected() {
    for bad in ["", "somewhere in the plan", "a01", "1412", "A01:1412"] {
        let r = mutate(|law| {
            law.status = "registered".into();
            law.source_location = bad.into();
        });
        assert!(
            codes(&r).contains(&"law_source_anchor_missing".to_string()),
            "anchor {bad:?} should have been rejected, got {:?}",
            codes(&r)
        );
    }
}

#[test]
fn a_real_anchor_is_accepted() {
    let r = mutate(|law| {
        law.status = "registered".into();
        law.source_location = "a01:1412".into();
    });
    assert!(
        !codes(&r).contains(&"law_source_anchor_missing".to_string()),
        "a well-formed anchor must not be rejected: {:?}",
        codes(&r)
    );
}

#[test]
fn unregistered_law_without_a_note_is_rejected() {
    let r = mutate(|law| {
        law.status = "fabrication-candidate".into();
        law.note = String::new();
    });
    assert!(
        codes(&r).contains(&"law_adjudication_note_missing".to_string()),
        "{:?}",
        codes(&r)
    );
}

#[test]
fn empty_registry_is_rejected() {
    let r = LawRegistry { laws: Vec::new() };
    assert!(
        codes(&r).contains(&"law_registry_empty".to_string()),
        "{:?}",
        codes(&r)
    );
}

/// Fail-closed reader: a key this reader does not understand is a row it has
/// not understood. Silently ignoring it is how a field gets added to some rows
/// and dropped on others.
#[test]
fn unknown_key_is_rejected_at_load() {
    let dir = std::env::temp_dir().join(format!("fgdb-laws-unknown-key-{}", std::process::id()));
    let registries = dir.join("registries");
    std::fs::create_dir_all(&registries).expect("scratch dir");
    let path = registries.join("laws.toml");
    std::fs::write(
        &path,
        "[[law]]\nid = \"FG-LAW-01\"\nname = \"x\"\nstatus = \"unadjudicated\"\nnote = \"n\"\nsurprise = \"y\"\n",
    )
    .expect("write fixture");
    let error = load_from_repo(&dir).expect_err("an unknown key must fail the load");
    assert!(
        error.message.contains("unknown key"),
        "expected an unknown-key load error, got {error}"
    );
}

#[test]
fn duplicate_alias_is_rejected() {
    let mut r = registry();
    let name = r.laws[0].name.clone();
    r.laws[1].cited_as = vec![name];
    assert!(
        codes(&r).contains(&"law_name_duplicate".to_string()),
        "an alias colliding with another law's name must be rejected: {:?}",
        codes(&r)
    );
}

#[test]
fn empty_alias_is_rejected() {
    let r = mutate(|law| law.cited_as = vec!["  ".into()]);
    assert!(
        codes(&r).contains(&"law_name_empty".to_string()),
        "{:?}",
        codes(&r)
    );
}

// =========================================================================
// The citation guard (fgdb-law-citation-guard-ld8f)
// =========================================================================
//
// The load-bearing test is `every_catalog_citation_resolves_and_is_licensed`.
// Everything below it exists to prove that test can go red — a guard nobody has
// watched fire is a guard nobody knows works, and this one in particular is the
// third attempt: the first two resolved citations against a copy of the very
// prose they were checking and read three laws as RESOLVES, all three false.

fn catalog_text() -> String {
    std::fs::read_to_string(repo_root().join(CITATION_SUBJECT)).expect("catalog is readable")
}

fn citation_codes(registry: &LawRegistry, text: &str) -> Vec<String> {
    validate_citations(registry, &extract_citations(text))
        .into_iter()
        .map(|violation| violation.code)
        .collect()
}

/// Codes over the REAL catalog with `planted` appended.
///
/// The open-adjudication ceiling is a whole-corpus property — a ceiling entry
/// with no matching citation fails as stale — so a control that asserts the
/// guard stays GREEN has to run against the whole corpus. A one-line fixture
/// would report four stale entries and prove nothing. Planting into the real
/// subject also answers the weaker question a fixture-only control leaves open:
/// whether the reader reaches the artifact at all.
fn planted_codes(registry: &LawRegistry, planted: &str) -> Vec<String> {
    let mut text = catalog_text();
    text.push_str(planted);
    citation_codes(registry, &text)
}

/// The gate. Every law citation in the catalog resolves to a registry ID, and
/// every `law` token is accounted for by one of the three classes.
#[test]
fn every_catalog_citation_resolves_and_is_licensed() {
    let violations = validate_citations(&registry(), &extract_citations(&catalog_text()));
    assert!(
        violations.is_empty(),
        "law citations in {CITATION_SUBJECT} must resolve to a registered law: {violations:#?}"
    );
}

/// The denominator, stated as an assertion rather than as a comment. A guard
/// over zero citations passes trivially, which is the failure mode a checker
/// this shape dies of: state the counts and let a collapse be red.
#[test]
fn citation_census_is_not_vacuous() {
    let registry = registry();
    let tokens = extract_citations(&catalog_text());
    let cited: Vec<&str> = tokens
        .iter()
        .filter_map(|t| match &t.class {
            CitationClass::Cited { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    let licensed = cited
        .iter()
        .filter(|name| {
            resolve_citation(&registry, name).is_some_and(|law| law.status == "registered")
        })
        .count();
    let unrecognised = tokens
        .iter()
        .filter(|t| t.class == CitationClass::Unrecognised)
        .count();
    let distinct: std::collections::BTreeSet<&str> = cited.iter().copied().collect();
    let generic = tokens
        .iter()
        .filter(|t| matches!(t.class, CitationClass::Generic { .. }))
        .count();
    // The denominator, printed so it is readable from the gate transcript under
    // `--nocapture` rather than only from a bead comment written once.
    println!(
        "law-citation census over {CITATION_SUBJECT}: {} law tokens = {} citations ({} distinct names, {licensed} licensed by a registered law, {} open adjudications) + {generic} generic + {unrecognised} unrecognised",
        tokens.len(),
        cited.len(),
        distinct.len(),
        cited.len() - licensed,
    );

    // MEASURED 2026-07-27: 93 law tokens, 92 citations of 9 distinct names, 1
    // generic ("No source law requires ..."), 0 unrecognised; 86 licensed by a
    // registered law and 6 open adjudications under the ceiling. These are
    // floors, not pins: a repair may lawfully shrink them, an extractor that
    // stops seeing the corpus may not.
    assert!(
        cited.len() >= 80,
        "citation extraction collapsed: {} citations found, expected the ~92 measured at HEAD",
        cited.len()
    );
    assert!(
        licensed >= 80,
        "only {licensed} of {} citations resolve to a registered law",
        cited.len()
    );
    assert!(
        distinct.len() >= 8,
        "distinct cited law names collapsed to {}: {distinct:?}",
        distinct.len()
    );
    assert_eq!(
        unrecognised, 0,
        "unrecognised law-token shapes in the catalog"
    );
    let ceiling_total: usize = OPEN_ADJUDICATION_CEILING.iter().map(|(_, n)| n).sum();
    assert_eq!(
        cited.len() - licensed,
        ceiling_total,
        "unlicensed citations must equal the declared open-adjudication ceiling exactly"
    );
}

/// CONTROL — a fabricated citation is caught. This is the whole reason the
/// guard exists: the row that started this arc justified an invented type by
/// citing a law that existed nowhere, and every gate passed.
#[test]
fn a_planted_fabrication_fails() {
    let planted = "rationale = \"a01:1412 the field is a u64 under the invented ceiling law\"\n";
    let codes = citation_codes(&registry(), planted);
    assert!(
        codes.contains(&"law_citation_unresolvable".to_string()),
        "a citation of a law that does not exist must fail: {codes:?}"
    );
}

/// CONTROL — the fabrication is caught inside the real corpus too, not only in
/// a one-line fixture. A guard proven only on a synthetic string has proven
/// that the reader works on the fixture, not that it reaches the subject.
#[test]
fn a_fabrication_planted_in_the_real_catalog_fails() {
    let mut text = catalog_text();
    text.push_str(
        "rationale = \"a10:1920 source-position tag 99; width forced by the Appendix A u64 ceiling law\"\n",
    );
    let codes = citation_codes(&registry(), &text);
    assert!(
        codes.contains(&"law_citation_unresolvable".to_string()),
        "a fabrication planted in the real catalog must fail: {codes:?}"
    );
}

/// CONTROL — an unparsed shape fails rather than being skipped. A fabricated
/// law is by definition the one the extractor has no entry for, so silence here
/// would make the guard vacuous exactly where it is needed.
#[test]
fn an_unrecognised_shape_fails() {
    for planted in [
        "rationale = \"the value is fixed by law\"\n",
        "rationale = \"retained pursuant to law and to the covering digest\"\n",
    ] {
        let codes = citation_codes(&registry(), planted);
        assert!(
            codes.contains(&"law_citation_shape_unrecognised".to_string()),
            "an unparsable `law` token must fail, not be skipped: {planted:?} gave {codes:?}"
        );
    }
}

/// CONTROL, the discrimination that had to be proved in BOTH directions. The
/// catalog contains "No source law requires authenticated membership proofs" —
/// ordinary English that a naive extractor reads as a citation and lands red
/// on. It must be classified generic. And the same sentence made definite must
/// become a citation and fail, which is what proves the generic class is a
/// structural rule and not a hole big enough to hide a fabrication in.
#[test]
fn the_generic_reference_is_not_a_citation_but_a_definite_one_is() {
    let generic = "rationale = \"rejecting duplicates before hashing. No source law requires authenticated membership proofs, so Merkle shapes are excluded\"\n";
    let tokens = extract_citations(generic);
    assert_eq!(tokens.len(), 1, "expected one law token: {tokens:#?}");
    assert!(
        matches!(tokens[0].class, CitationClass::Generic { .. }),
        "a negative-quantified generic must not be read as a citation: {:#?}",
        tokens[0]
    );
    assert!(
        planted_codes(&registry(), generic).is_empty(),
        "the generic reference must not make the guard red"
    );

    let definite = generic.replace("No source law", "the source law");
    let codes = planted_codes(&registry(), &definite);
    assert!(
        codes.contains(&"law_citation_unresolvable".to_string()),
        "the same phrase made definite names a law and must be resolved: {codes:?}"
    );
}

/// CONTROL — a citation of a law that IS in the registry but is not
/// `registered` fails. Recording a fabrication in the registry must not license
/// it; that would turn the registry into the laundering step.
#[test]
fn a_citation_of_an_unregistered_law_is_not_licensed() {
    let registry = registry();
    let unregistered = registry
        .laws
        .iter()
        .find(|law| law.status != "registered")
        .expect("the registry carries open adjudications");
    let planted = format!(
        "rationale = \"a01:1412 justified under the {} law\"\n",
        unregistered.name
    );
    let mut text = catalog_text();
    text.push_str(&planted);
    let codes = citation_codes(&registry, &text);
    assert!(
        codes.contains(&"law_citation_not_licensed".to_string()),
        "a citation of a {:?} law must not be licensed: {codes:?}",
        unregistered.status
    );
}

/// CONTROL — the open-adjudication ceiling cannot go stale. An entry whose
/// citations have all been repaired fails, so the list can only shrink. This is
/// what stops it from becoming the permanent waiver that every such list
/// becomes.
#[test]
fn a_stale_open_adjudication_entry_fails() {
    let registry = registry();
    let text = catalog_text();
    let ids: Vec<&str> = OPEN_ADJUDICATION_CEILING
        .iter()
        .map(|(id, _)| *id)
        .collect();
    // Repair every citation of the first ceilinged law by deleting the lines
    // that carry it, leaving its ceiling entry behind with nothing to cover.
    let target = registry
        .laws
        .iter()
        .find(|law| ids.contains(&law.id.as_str()))
        .expect("ceiling names a real law");
    let repaired: String = text
        .lines()
        .filter(|line| !line.contains(&format!("{} law", target.name)))
        .map(|line| format!("{line}\n"))
        .collect();
    let codes = citation_codes(&registry, &repaired);
    assert!(
        codes.contains(&"law_citation_open_adjudication_stale".to_string()),
        "a ceiling entry with no remaining citation must fail as stale: {codes:?}"
    );
}

/// CONTROL — the ceiling is a ceiling. One more citation of an unadjudicated
/// law than the census declares is a failure, so the pin cannot grow silently.
#[test]
fn exceeding_the_open_adjudication_ceiling_fails() {
    let registry = registry();
    let ceilinged = registry
        .laws
        .iter()
        .find(|law| {
            OPEN_ADJUDICATION_CEILING
                .iter()
                .any(|(id, _)| id == &law.id)
        })
        .expect("ceiling names a real law");
    let mut text = catalog_text();
    text.push_str(&format!(
        "rationale = \"a01:1412 one more citation under the {} law\"\n",
        ceilinged.name
    ));
    let codes = citation_codes(&registry, &text);
    assert!(
        codes.contains(&"law_citation_not_licensed".to_string()),
        "exceeding the ceiling must fail: {codes:?}"
    );
}

/// The alias path is load-bearing — one real citation resolves only through it
/// — so it gets its own witness rather than riding on the corpus test.
#[test]
fn an_alias_resolves_to_its_law() {
    let registry = registry();
    let (law, alias) = registry
        .laws
        .iter()
        .find_map(|law| law.cited_as.first().map(|alias| (law, alias)))
        .expect("the registry declares at least one alternate phrasing");
    let resolved = resolve_citation(&registry, alias).expect("alias resolves");
    assert_eq!(resolved.id, law.id);
    let planted = format!("rationale = \"a01:1412 under the {alias} law\"\n");
    assert!(
        planted_codes(&registry, &planted).is_empty(),
        "a citation by declared alias must be licensed"
    );
    // ...and the alias is not a free pass: a near-miss of it still fails.
    let codes = planted_codes(
        &registry,
        &planted.replace(alias, &format!("{alias} variant")),
    );
    assert!(
        codes.contains(&"law_citation_unresolvable".to_string()),
        "only the declared phrasings resolve: {codes:?}"
    );
}

//! liveness.rs — the ONE reader for "is this `status = "live"` checker row
//! actually live?"
//!
//! # The defect this module exists to remove
//!
//! AGENTS.md, *Spec-First Workflow* item 2: **"CI cross-checks that every ID has
//! a live checker."** And the hard rule directly beneath it: **"no subsystem
//! ships against an unenforced invariant. A workstream exit gate (G1–G4, §19)
//! cannot pass while any invariant it depends on lacks a live checker in
//! `invariants.toml`."** `registries/checker_index.toml`'s own header repeats
//! it: a row may be stub-registered pre-Genesis, *"but ... a \"live\" row's
//! artifact must exist in-repo."*
//!
//! Every G1–G4 gate therefore rests on the word `live`. Until
//! `fgdb-checker-index-live-is-only-file-existence-tl0o`, `live` was proved by
//! `Path::is_file()` — and by **two** readers that had already drifted apart:
//! `validate::validate_checker_index` called `root.join(artifact).is_file()`
//! with no path-safety guard, while `appendix_a::live_checker_artifact_exists`
//! had one. So a checker could be registered live, be named by a clause as its
//! enforcement mechanism, be invoked by no gate, and contain no code capable of
//! reporting a failure — and every registry gate stayed green.
//!
//! That is the same defect as the workspace `unsafe_code = "forbid"` predicate
//! that could not fail (`fgdb-regcheck-forbid-substring-vacuous-u9zp`), one
//! level up: **a checker whose answer is fixed before it reads anything.** The
//! generalisation in `tests/metamorphic.rs`'s header applies verbatim — a
//! weaker test standing in for the structural fact, inside a checker whose job
//! is to be unfoolable.
//!
//! # What `live` means now
//!
//! Three facts. Each has exactly ONE reader here. All three are required, and
//! the row is live only if none of them yields a [`Defect`].
//!
//! 1. **REGISTERED** — the declared artifact is a safe repository-relative path
//!    that exists, and the row's declared [`Unit`] resolves inside it. A
//!    `unit = "symbol"` row names one code symbol; renaming or deleting that
//!    symbol makes the row red instead of leaving it green over a file that
//!    merely still exists. A `unit = "artifact"` row declares out loud that its
//!    symbol is a law name for the whole file, which is what the eight
//!    scheme-prefixed and law-named rows in the registry actually are — the
//!    point is not that those are wrong, it is that until now **nothing
//!    distinguished them from a rotted one.**
//!
//! 2. **INVOKED** — the artifact is a unit that an executing gate can actually
//!    reach. This is a structural property of the build, not a second copy of
//!    `scripts/check.sh`'s dispatch table:
//!      * `cargo-test` — the artifact must be an auto-discovered integration
//!        test target: a direct child of a `tests/` directory belonging to a
//!        **workspace member**. `cargo test --workspace` compiles exactly those.
//!        A row pointing anywhere else is never run, and `check.sh` would still
//!        report it PASS, because `check.sh` credits every `cargo-test` row with
//!        the single workspace `cargo test` exit code.
//!      * `binary` — the artifact must be compiled INTO a binary target: either
//!        a bin root (`src/main.rs`, `src/bin/*.rs`) or a module reachable from
//!        one through `mod` declarations in LIVE code. A module no `mod`
//!        declares is dead source; registering it live is a claim about a
//!        program that does not contain it.
//!      * `script` — the artifact must be a shell program (`.sh`/`.bash` with a
//!        `#!` line), which is what `check.sh`'s registry-derived loop executes.
//!
//! 3. **CAPABLE OF FAILING** — the unit an executing gate reaches must have at
//!    least one path to a failing outcome, read from LIVE code only:
//!      * a `#[test]` fn fails by panicking (or by returning `Err`), so the
//!        vocabulary is the panicking operators;
//!      * a checker *binary* fails by exiting nonzero, and that is a property of
//!        its `main`, not of the module the row names — so a `binary` row is
//!        checked against the bin roots its artifact is compiled into. A gate
//!        whose `main` can only return `ExitCode::SUCCESS` cannot report a
//!        violation however much its modules compute;
//!      * a script fails by exiting nonzero.
//!
//! # The control that licenses a clean verdict
//!
//! Every reader below is a source-text reader, which is precisely the layer
//! where all four of this repository's "looks exactly like a pass" tooling bugs
//! lived. So none of them is trusted on its own word: [`self_test`] runs each
//! reader over synthetic text with a KNOWN answer — including the masked cases
//! (`// mod dead;`, a commented `exit 1`, an `assert!` inside a block comment)
//! that `fgdb-regcheck-scansites-line-anchored-ds45` and
//! `fgdb-regcheck-commented-arm-counts-live-ctv8` were — and reports which
//! cases it got wrong. A caller that finds zero defects must first check that
//! [`SelfTest::licensed`] holds; otherwise "every live row is live" is
//! indistinguishable from "the readers stopped reading". This is the same
//! discipline as `unsafe-ledger-check` reporting its scanner self-test before
//! any zero-site conclusion, and `ClosureReport::licensed`
//! (`fgdb-regcheck-closure-vacuous-no-control-hp0f`).
//!
//! # One reader per fact
//!
//! `mask_source` — the single reader for "which bytes of this Rust source are
//! live code" — is consumed here rather than re-derived, for the reason stated
//! in its own doc comment. The shell equivalent, [`mask_shell`], is new: no
//! reader for "which bytes of this shell script are live code" existed, and
//! without one a script whose only `exit 1` sits in a comment would be judged
//! capable of failing. Workspace membership comes from
//! `appendix_a::workspace_member_paths`, the roster reader `unsafe_ledger`
//! already shares.
//!
//! # The second registry: `proof_lanes.toml` (`…-checked-is-only-file-existence-0f1l`)
//!
//! `status = "checked"` on a proof lane was the SAME defect one artifact over.
//! `registries/proof_lanes.toml`'s header defines it as "the artifact exists
//! in-repo **and is CI-checked**"; `validate_proof_lanes` proved it with
//! `root.join(&lane.artifact).is_file()` — the pre-`tl0o` checker read, down to
//! the missing path-safety guard. Two laws in that header had no implementation
//! at all:
//!
//! 1. **Nothing checks the proof.** No `lake build`, no `lean`, no TLC
//!    invocation exists anywhere in the repository. A `.lean` file consisting of
//!    `theorem foo : False := sorry` satisfied `checked`, and so did an empty
//!    one. `sorry` is Lean's admit-anything tactic and is THE standard failure
//!    mode of a formal lane: the file typechecks, the build is green, and
//!    nothing is proven. For TLA+ it is worse — a `.tla` model with no `.cfg`
//!    naming an `INVARIANT` is a TLC run that checks nothing and can only pass.
//! 2. **"A declared lane may be cited only while the citing clause is `stub`"**
//!    existed as prose and as no code. That half is a cross-registry rule about
//!    clauses, so it lives in `validate`, next to the other clause laws; the
//!    lane-side half is here.
//!
//! So a `checked` lane is adjudicated by [`Prover::assess_lane`] against the
//! same three facts, and — this is the point of putting it in this module —
//! **the INVOKED fact is answered by delegating to [`Prover::assess`]**. A lane
//! declares `checked_by`, the `checker_index` symbol of the gate that runs its
//! prover; that gate must be a registered row, `status = "live"`, and live in
//! the full sense this module already defines. "Is CI-checked" is not a new
//! question. It is the question this file was written to answer, asked about a
//! different artifact, and a second implementation of it is exactly the
//! duplication that produced `tl0o` (two readers for `is_file`) and
//! `census-has-two-delimiter-readers` (two balance tests).
//!
//! [`mask_formal`] is the one genuinely new reader, and it is ONE reader for
//! two languages: Lean and TLA+ differ only in their comment delimiters, and
//! both nest. Writing two would be the same mistake in miniature.

use crate::model::{Checker, Lane};
use crate::unsafe_ledger::mask_source;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// The registry this module adjudicates.
pub const CHECKER_INDEX_PATH: &str = "registries/checker_index.toml";

/// The second registry this module adjudicates.
pub const PROOF_LANES_PATH: &str = "registries/proof_lanes.toml";

/// The two things a checker row's `symbol` can name.
///
/// Declared by the row, never guessed. Guessing was the status quo: eight of the
/// live rows carry law names (`architecture_source_coverage`,
/// `cargo-test:claims`, `topology_registry_source`, …) rather than code symbols,
/// and no spelling rule separates `threat_model` (a law name) from
/// `claims_neg_waiver_present` (a `#[test] fn`). A registry that declares which
/// one it means can be checked; one that does not, cannot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// The `symbol` is a code symbol inside the artifact. Its definition is the
    /// unit that must exist and must be capable of failing.
    Symbol,
    /// The `symbol` is a law/gate name for the whole artifact. The file is the
    /// unit.
    Artifact,
}

impl Unit {
    /// Parse the registry spelling.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "symbol" => Some(Self::Symbol),
            "artifact" => Some(Self::Artifact),
            _ => None,
        }
    }

    /// The registry spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Symbol => "symbol",
            Self::Artifact => "artifact",
        }
    }
}

/// Why a `status = "live"` row is not live.
///
/// One variant per fact, so a report says which of the three claims failed
/// rather than collapsing all of them into "artifact missing" — the collapse is
/// what let `is_file()` stand in for the whole doctrine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DefectKind {
    /// The declared artifact path is empty, absolute, or escapes the repository.
    ArtifactPathUnsafe,
    /// The declared artifact is not a file in this repository.
    ArtifactAbsent,
    /// A live row did not declare what its `symbol` names.
    UnitUndeclared,
    /// The artifact exists, but no gate runner can execute it.
    Uninvocable,
    /// The declared `unit` does not resolve in the artifact's live code.
    SymbolUnresolved,
    /// The unit a runner executes has no path to a failing outcome.
    CannotFail,
    /// A `checked` proof lane did not name the gate that checks it.
    LaneGateUndeclared,
    /// A `checked` proof lane's declared gate is not a registered checker row.
    LaneGateUnresolved,
    /// A `checked` proof lane's declared gate is not itself live.
    LaneGateNotLive,
    /// The lane names a formal system no reader here can adjudicate.
    LaneSystemUnreadable,
    /// The artifact states no proposition, or the model checks no property.
    LaneProvesNothing,
    /// The artifact admits its own conclusion rather than proving it.
    LaneAdmitsAnything,
}

impl DefectKind {
    /// The violation code a caller emits for this defect.
    ///
    /// `ArtifactAbsent` keeps the pre-existing `artifact_missing` code so the
    /// negative test that pins it (`tests/claims.rs`) keeps pinning the same
    /// fact. The other five are new because the facts are new.
    pub fn code(self) -> &'static str {
        match self {
            Self::ArtifactPathUnsafe => "checker_artifact_path_unsafe",
            Self::ArtifactAbsent => "artifact_missing",
            Self::UnitUndeclared => "checker_unit_undeclared",
            Self::Uninvocable => "checker_not_invocable",
            Self::SymbolUnresolved => "checker_symbol_unresolved",
            Self::CannotFail => "checker_cannot_fail",
            Self::LaneGateUndeclared => "proof_lane_gate_undeclared",
            Self::LaneGateUnresolved => "proof_lane_gate_unresolved",
            Self::LaneGateNotLive => "proof_lane_gate_not_live",
            Self::LaneSystemUnreadable => "proof_lane_system_unreadable",
            Self::LaneProvesNothing => "proof_lane_proves_nothing",
            Self::LaneAdmitsAnything => "proof_lane_admits_anything",
        }
    }
}

/// One reason a row failed, with the evidence that produced it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Defect {
    pub kind: DefectKind,
    pub detail: String,
}

impl Defect {
    fn new(kind: DefectKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Fact 0 — path safety and presence
// ---------------------------------------------------------------------------

/// Is `path` a safe repository-relative path?
///
/// Shared with `appendix_a`, which is where this predicate already lived; a
/// second copy is how `validate`'s liveness read came to have no guard at all.
fn artifact_path(repo_root: &Path, artifact: &str) -> Result<PathBuf, Defect> {
    if !crate::appendix_a::safe_repository_relative(artifact) {
        return Err(Defect::new(
            DefectKind::ArtifactPathUnsafe,
            format!("artifact {artifact:?} is not a safe repository-relative path"),
        ));
    }
    let absolute = repo_root.join(artifact);
    if !absolute.is_file() {
        return Err(Defect::new(
            DefectKind::ArtifactAbsent,
            format!("artifact {artifact:?} is not a file in this repository"),
        ));
    }
    Ok(absolute)
}

// ---------------------------------------------------------------------------
// Fact 1 — invocability
// ---------------------------------------------------------------------------

/// What an executing gate runs when it reaches this row.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Invocation {
    /// An auto-discovered integration test target; `cargo test --workspace`
    /// compiles and runs it.
    IntegrationTest,
    /// A Rust source compiled into at least one binary target. The roots are
    /// carried because a binary fails by exiting nonzero from `main`, which is a
    /// property of the root and not of the named module.
    Binary { roots: Vec<PathBuf> },
    /// A shell program `check.sh`'s registry-derived loop executes with `bash`.
    Script,
}

/// The workspace member roster, through the reader `unsafe_ledger` already
/// shares with `appendix_a` so a third copy cannot drift.
fn workspace_members(repo_root: &Path) -> Result<Vec<PathBuf>, String> {
    let text = fs::read_to_string(repo_root.join("Cargo.toml"))
        .map_err(|error| format!("Cargo.toml: {error}"))?;
    let manifest = crate::toml::parse(&text).map_err(|error| format!("Cargo.toml: {error}"))?;
    let workspace = crate::toml::get_table(&manifest, "workspace", "Cargo.toml")
        .map_err(|error| error.to_string())?;
    let members = crate::toml::get_str_array(workspace, "members", "Cargo.toml.workspace")
        .map_err(|error| error.to_string())?;
    let excludes = crate::appendix_a::workspace_exact_excludes(workspace)?;
    crate::appendix_a::workspace_member_paths(repo_root, &members, &excludes)
}

/// The member directory owning `artifact`, if any.
fn owning_member(members: &[PathBuf], artifact: &Path) -> Option<PathBuf> {
    members
        .iter()
        .filter(|member| artifact.starts_with(member))
        .max_by_key(|member| member.components().count())
        .cloned()
}

/// Every `mod <ident>;` declared in LIVE code.
///
/// Read from the mask, so a `mod` declaration inside a comment declares nothing
/// — which is the whole `fgdb-regcheck-commented-arm-counts-live-ctv8` class. An
/// inline `mod x { … }` declares no file and is deliberately not returned.
fn declared_modules(source: &str) -> Vec<String> {
    let masked = mask_source(source);
    let bytes = masked.text().as_bytes();
    let mut out = Vec::new();
    for at in 0..bytes.len() {
        if !bytes[at..].starts_with(b"mod") {
            continue;
        }
        if at > 0 && is_ident_byte(bytes[at - 1]) {
            continue;
        }
        let mut cursor = at + 3;
        if !matches!(bytes.get(cursor), Some(b) if b.is_ascii_whitespace()) {
            continue;
        }
        while matches!(bytes.get(cursor), Some(b) if b.is_ascii_whitespace()) {
            cursor += 1;
        }
        let start = cursor;
        while matches!(bytes.get(cursor), Some(b) if is_ident_byte(*b)) {
            cursor += 1;
        }
        if cursor == start {
            continue;
        }
        let name = masked.text()[start..cursor].to_owned();
        while matches!(bytes.get(cursor), Some(b) if b.is_ascii_whitespace()) {
            cursor += 1;
        }
        // `mod x;` declares a file; `mod x { … }` declares an inline module.
        if bytes.get(cursor) == Some(&b';') {
            out.push(name);
        }
    }
    out
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The directory a source file's own submodules live in.
///
/// `src/lib.rs`, `src/main.rs` and `src/foo/mod.rs` own their parent directory;
/// every other module `src/foo.rs` owns `src/foo/`.
fn module_directory(file: &Path) -> PathBuf {
    let parent = file.parent().unwrap_or(Path::new("")).to_path_buf();
    match file.file_stem().and_then(|stem| stem.to_str()) {
        Some("mod") | Some("lib") | Some("main") => parent,
        Some(stem) => parent.join(stem),
        None => parent,
    }
}

/// Every source file compiled into a binary target of `member`, keyed by the bin
/// roots that reach it.
///
/// A crate's bins link its lib, so when the crate has both, the lib's module
/// tree is compiled into every bin. That is why `appendix_a.rs` — a `pub mod` of
/// `lib.rs`, named by three live `binary` rows — is correctly invocable while a
/// module no `mod` declares is not.
///
/// The library closure is computed ONCE and unioned into each bin, not
/// recomputed per root: this crate's library is 1.7 MB of Rust and every module
/// of it is masked to find its `mod` declarations, so the difference is ten
/// seconds per registry sweep against three tenths of one.
fn binary_module_map(repo_root: &Path, member: &Path) -> BTreeMap<PathBuf, Vec<PathBuf>> {
    let src = member.join("src");
    let mut roots = Vec::new();
    if repo_root.join(&src).join("main.rs").is_file() {
        roots.push(src.join("main.rs"));
    }
    if let Ok(entries) = fs::read_dir(repo_root.join(&src).join("bin")) {
        let mut bins: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "rs") && path.is_file())
            .filter_map(|path| path.file_name().map(|name| src.join("bin").join(name)))
            .collect();
        bins.sort();
        roots.extend(bins);
    }
    let lib = src.join("lib.rs");
    let library = if repo_root.join(&lib).is_file() {
        module_closure(repo_root, &[lib])
    } else {
        BTreeSet::new()
    };

    let mut map: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for root in &roots {
        // A bin root reaches its own module tree, and — because it links the
        // crate's library — the library's module tree as well.
        for reached in module_closure(repo_root, std::slice::from_ref(root))
            .into_iter()
            .chain(library.iter().cloned())
        {
            let entry = map.entry(reached).or_default();
            if !entry.contains(root) {
                entry.push(root.clone());
            }
        }
    }
    map
}

/// Every source file reachable from `seeds` through `mod` declarations.
fn module_closure(repo_root: &Path, seeds: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut queue: Vec<PathBuf> = seeds.to_vec();
    while let Some(file) = queue.pop() {
        if !seen.insert(file.clone()) {
            continue;
        }
        let Ok(source) = fs::read_to_string(repo_root.join(&file)) else {
            continue;
        };
        let directory = module_directory(&file);
        for name in declared_modules(&source) {
            let flat = directory.join(format!("{name}.rs"));
            let nested = directory.join(&name).join("mod.rs");
            if repo_root.join(&flat).is_file() {
                queue.push(flat);
            } else if repo_root.join(&nested).is_file() {
                queue.push(nested);
            }
        }
    }
    seen
}

/// Decide what — if anything — runs this row.
fn invocation(
    prover: &Prover<'_>,
    checker: &Checker,
    relative: &Path,
) -> Result<Invocation, Defect> {
    let repo_root = prover.repo_root;
    let members = match prover.members() {
        Ok(members) => members,
        Err(error) => {
            // An unreadable workspace is a defect, never a skip: a reader that
            // silently does nothing is indistinguishable from one that passed.
            return Err(Defect::new(
                DefectKind::Uninvocable,
                format!("cannot resolve the workspace member roster: {error}"),
            ));
        }
    };
    match checker.kind.as_str() {
        "cargo-test" => {
            let parent = relative.parent().unwrap_or(Path::new(""));
            let is_test_dir = parent.file_name().is_some_and(|name| name == "tests");
            let member = parent.parent().unwrap_or(Path::new(""));
            if !is_test_dir
                || relative.extension().is_none_or(|ext| ext != "rs")
                || !members.iter().any(|candidate| candidate == member)
            {
                return Err(Defect::new(
                    DefectKind::Uninvocable,
                    format!(
                        "{:?} is not an integration test target of a workspace member, so \
                         `cargo test --workspace` never compiles it",
                        checker.artifact
                    ),
                ));
            }
            Ok(Invocation::IntegrationTest)
        }
        "binary" => {
            let Some(member) = owning_member(&members, relative) else {
                return Err(Defect::new(
                    DefectKind::Uninvocable,
                    format!(
                        "{:?} belongs to no workspace member, so no cargo target contains it",
                        checker.artifact
                    ),
                ));
            };
            let map = prover.module_map(&member);
            match map.get(relative) {
                Some(roots) if !roots.is_empty() => Ok(Invocation::Binary {
                    roots: roots.clone(),
                }),
                _ => Err(Defect::new(
                    DefectKind::Uninvocable,
                    format!(
                        "{:?} is not compiled into any binary target of {}: no `mod` \
                         declaration in live code reaches it",
                        checker.artifact,
                        member.display()
                    ),
                )),
            }
        }
        "script" => {
            let extension = relative.extension().and_then(|ext| ext.to_str());
            if !matches!(extension, Some("sh") | Some("bash")) {
                return Err(Defect::new(
                    DefectKind::Uninvocable,
                    format!("{:?} is not a shell deliverable", checker.artifact),
                ));
            }
            let Ok(source) = fs::read_to_string(repo_root.join(relative)) else {
                return Err(Defect::new(
                    DefectKind::Uninvocable,
                    format!("{:?} could not be read", checker.artifact),
                ));
            };
            if !source.starts_with("#!") {
                return Err(Defect::new(
                    DefectKind::Uninvocable,
                    format!(
                        "{:?} has no `#!` line, so it is not an executable program",
                        checker.artifact
                    ),
                ));
            }
            Ok(Invocation::Script)
        }
        other => Err(Defect::new(
            DefectKind::Uninvocable,
            format!("kind {other:?} has no gate runner, so a live row of it is never executed"),
        )),
    }
}

// ---------------------------------------------------------------------------
// Fact 2 — the symbol resolves
// ---------------------------------------------------------------------------

/// The body of `#[test] fn <symbol>`, read from live code.
///
/// Returns the raw source of the body, spanning from the opening brace to its
/// match. The `#[test]` attribute is required: a plain `fn` of the right name is
/// not a thing `cargo test` runs, and accepting one would let a deleted test
/// keep its row green because a helper of the same name survived.
fn test_body<'a>(source: &'a str, symbol: &str) -> Option<&'a str> {
    let masked = mask_source(source);
    let text = masked.text();
    let bytes = text.as_bytes();
    let needle = format!("fn {symbol}");
    let mut from = 0;
    while let Some(offset) = text[from..].find(&needle) {
        let at = from + offset;
        from = at + needle.len();
        if at > 0 && is_ident_byte(bytes[at - 1]) {
            continue;
        }
        let after = at + needle.len();
        // `fn foo` must end here: `fn foo_bar` is a different function.
        if matches!(bytes.get(after), Some(b) if is_ident_byte(*b)) {
            continue;
        }
        if !preceded_by_test_attribute(text, at) {
            continue;
        }
        let Some(open) = text[after..].find('{').map(|index| after + index) else {
            continue;
        };
        let close = matching_brace(bytes, open);
        return Some(&source[open..close]);
    }
    None
}

/// Does a `#[test]` attribute apply to the item starting at `at`?
///
/// Walks backwards over the attributes and whitespace immediately preceding the
/// item. Reading the mask means a commented-out `#[test]` attaches to nothing,
/// which is `fgdb-regcheck-scansites-line-anchored-ds45` in its own habitat.
fn preceded_by_test_attribute(masked: &str, at: usize) -> bool {
    let bytes = masked.as_bytes();
    let mut cursor = at;
    loop {
        while cursor > 0 && bytes[cursor - 1].is_ascii_whitespace() {
            cursor -= 1;
        }
        // Item modifiers may sit between the attribute and `fn`.
        let mut modifier_end = cursor;
        while modifier_end > 0 && is_ident_byte(bytes[modifier_end - 1]) {
            modifier_end -= 1;
        }
        if modifier_end < cursor
            && matches!(
                &masked[modifier_end..cursor],
                "pub" | "async" | "const" | "unsafe" | "extern"
            )
        {
            cursor = modifier_end;
            continue;
        }
        if cursor == 0 || bytes[cursor - 1] != b']' {
            return false;
        }
        let close = cursor - 1;
        let Some(open) = matching_open_bracket(bytes, close) else {
            return false;
        };
        if masked[open + 1..close].trim() == "test" {
            return true;
        }
        if open == 0 || bytes[open - 1] != b'#' {
            return false;
        }
        cursor = open - 1;
    }
}

fn matching_open_bracket(bytes: &[u8], close: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut index = close;
    loop {
        match bytes[index] {
            b']' => depth += 1,
            b'[' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        if index == 0 {
            return None;
        }
        index -= 1;
    }
}

/// The offset just past the `}` closing the brace at `open`, over masked bytes.
fn matching_brace(bytes: &[u8], open: usize) -> usize {
    let mut depth = 0i32;
    for (index, byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return index + 1;
                }
            }
            _ => {}
        }
    }
    bytes.len()
}

// ---------------------------------------------------------------------------
// Fact 3 — capable of failing
// ---------------------------------------------------------------------------

/// How a Rust `#[test]` reports failure: it panics.
///
/// Every entry is matched as a macro invocation token — the byte before it must
/// not be an identifier byte — so `debug_assert!` does not satisfy `assert!`
/// and is listed in its own right.
const PANIC_MACROS: &[&str] = &[
    "assert!",
    "assert_eq!",
    "assert_ne!",
    "debug_assert!",
    "debug_assert_eq!",
    "debug_assert_ne!",
    "panic!",
    "unreachable!",
    "todo!",
    "unimplemented!",
];

/// The panicking methods. Matched with their opening paren so a field or a
/// method reference is not counted.
const PANIC_METHODS: &[&str] = &[".unwrap(", ".expect("];

/// Does this Rust source contain a way to panic, in live code?
fn rust_can_panic(source: &str) -> bool {
    let masked = mask_source(source);
    let text = masked.text();
    PANIC_MACROS
        .iter()
        .any(|macro_name| contains_token(text, macro_name))
        || PANIC_METHODS.iter().any(|method| text.contains(method))
}

/// Does this Rust source contain a nonzero exit, in live code?
///
/// `ExitCode::from(` and `process::exit(` are read with their argument: a
/// `main` that can only ever return `ExitCode::from(0)` cannot report a
/// violation, and the whole point of this module is to stop reading a call as
/// evidence of what it does.
fn rust_can_exit_nonzero(source: &str) -> bool {
    let masked = mask_source(source);
    let text = masked.text();
    if contains_token(text, "ExitCode::FAILURE") {
        return true;
    }
    for opener in ["ExitCode::from(", "process::exit("] {
        let mut from = 0;
        while let Some(offset) = text[from..].find(opener) {
            let at = from + offset;
            from = at + opener.len();
            let argument: String = text[from..]
                .chars()
                .take_while(|character| *character != ')')
                .filter(|character| !character.is_whitespace())
                .collect();
            // A non-literal argument may take any value, so it is a failure path.
            match argument.parse::<i64>() {
                Ok(0) => continue,
                _ => return true,
            }
        }
    }
    // A panic is also a nonzero exit.
    rust_can_panic(source)
}

/// Match `needle` only where it begins a token.
fn contains_token(text: &str, needle: &str) -> bool {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(offset) = text[from..].find(needle) {
        let at = from + offset;
        from = at + needle.len();
        if at == 0 || !is_ident_byte(bytes[at - 1]) {
            return true;
        }
    }
    false
}

/// A shell source with its comments, quoted spans and heredoc bodies blanked.
///
/// Byte-exact, like `mask_source`: a blanked character becomes one space, so an
/// offset in the mask names the same column of the same line of the source. No
/// reader for this fact existed, and without one a script whose only `exit 1`
/// sits inside a comment or a heredoc reads as capable of failing — which is
/// exactly the shape of `fgdb-regcheck-scansites-line-anchored-ds45`.
pub fn mask_shell(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut pending_heredocs: Vec<String> = Vec::new();
    let mut active_heredoc: Option<String> = None;
    for (index, line) in source.lines().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if let Some(terminator) = active_heredoc.clone() {
            if line.trim() == terminator {
                active_heredoc = None;
            }
            blank(line, &mut out);
            continue;
        }
        let (masked, heredocs) = mask_shell_line(line);
        if masked.len() == line.len() {
            out.push_str(&masked);
        } else {
            // The byte-for-byte correspondence is the whole contract: a caller
            // reads structure from the mask and the VALUE from the raw line at
            // the same offset. A line the masker could not reproduce at its own
            // length is treated as dead code rather than trusted at a shifted
            // offset — that direction can only produce a spurious "cannot fail",
            // which is loud, never a silent "can fail", which is the failure
            // this module exists to remove.
            blank(line, &mut out);
        }
        pending_heredocs.extend(heredocs);
        if active_heredoc.is_none() && !pending_heredocs.is_empty() {
            active_heredoc = Some(pending_heredocs.remove(0));
        }
    }
    out
}

/// A heredoc delimiter is a word: `<<EOF`, `<<'EOF'`, `<<-_end`. A number is
/// not, which is what keeps `$(( x << 2 ))` from opening one.
fn is_heredoc_delimiter(terminator: &str) -> bool {
    let mut characters = terminator.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn blank(line: &str, out: &mut String) {
    for character in line.chars() {
        for _ in 0..character.len_utf8() {
            out.push(' ');
        }
    }
}

/// Mask one shell line, returning it and any heredoc terminators it opened.
fn mask_shell_line(line: &str) -> (String, Vec<String>) {
    let bytes = line.as_bytes();
    let mut out = vec![b' '; bytes.len()];
    let mut heredocs = Vec::new();
    let mut index = 0;
    let mut previous_significant: Option<u8> = None;
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'\\' => {
                out[index] = byte;
                if index + 1 < bytes.len() {
                    out[index + 1] = bytes[index + 1];
                    index += 1;
                }
                previous_significant = Some(byte);
            }
            b'\'' | b'"' => {
                // A quoted span carries no shell control flow; blank the body
                // and keep the delimiters so token boundaries survive.
                out[index] = byte;
                let quote = byte;
                index += 1;
                while index < bytes.len() && bytes[index] != quote {
                    if quote == b'"' && bytes[index] == b'\\' {
                        index += 1;
                    }
                    index += 1;
                }
                if index < bytes.len() {
                    out[index] = quote;
                }
                previous_significant = Some(quote);
            }
            b'#' if previous_significant.is_none_or(|previous| {
                previous.is_ascii_whitespace() || matches!(previous, b';' | b'(' | b'&' | b'|')
            }) =>
            {
                // Comment to end of line.
                break;
            }
            // `<<` opens a heredoc — but only when what follows is a delimiter
            // word. `$(( bits << 2 ))` is a left shift and `<<<` is a
            // herestring; treating either as a heredoc would blank the entire
            // rest of the file waiting for a terminator that never arrives, and
            // a script read as empty reports as incapable of failing.
            b'<' if bytes.get(index + 1) == Some(&b'<') => {
                // Consume the whole run: `<<<` is a herestring, not two
                // redirections, and stepping into it one byte at a time would
                // read its last two `<` as a heredoc opener.
                let mut cursor = index;
                while bytes.get(cursor) == Some(&b'<') {
                    out[cursor] = b'<';
                    cursor += 1;
                }
                if cursor - index != 2 {
                    previous_significant = Some(b'<');
                    index = cursor;
                    continue;
                }
                if bytes.get(cursor) == Some(&b'-') {
                    cursor += 1;
                }
                while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                    cursor += 1;
                }
                let start = cursor;
                while cursor < bytes.len()
                    && !bytes[cursor].is_ascii_whitespace()
                    && bytes[cursor] != b';'
                {
                    cursor += 1;
                }
                let terminator: String = String::from_utf8_lossy(&bytes[start..cursor])
                    .trim_matches(|character| matches!(character, '"' | '\'' | '\\'))
                    .to_owned();
                if is_heredoc_delimiter(&terminator) {
                    out[index..cursor].copy_from_slice(&bytes[index..cursor]);
                    heredocs.push(terminator);
                    index = cursor;
                    previous_significant = Some(b'w');
                    continue;
                }
                // Not a heredoc after all: the `<<` run is already copied, and
                // the delimiter candidate is ordinary text handled from here.
                previous_significant = Some(b'<');
                index = cursor.min(start);
                continue;
            }
            _ => {
                out[index] = byte;
                previous_significant = Some(byte);
            }
        }
        index += 1;
    }
    (String::from_utf8_lossy(&out).into_owned(), heredocs)
}

/// Does this shell program contain a nonzero exit, in live code?
///
/// STRUCTURE FROM THE MASK, VALUE FROM THE RAW LINE — the idiom `Masked`'s own
/// doc comment prescribes, and the reason [`mask_shell`] is byte-exact. The mask
/// says whether an `exit` token is live shell rather than the inside of a
/// comment, a quoted string or a heredoc; the raw line at the same offset says
/// what its argument is. Reading the argument from the mask instead would blank
/// the `$rc` of `exit "$rc"` and report a script that propagates a failure
/// status as incapable of failing.
///
/// A literal `0` is not a failure path. A variable or command substitution may
/// hold any status, so it is.
fn shell_can_exit_nonzero(source: &str) -> bool {
    let masked = mask_shell(source);
    for (mask_line, raw_line) in masked.lines().zip(source.lines()) {
        for keyword in ["exit", "return"] {
            let mut from = 0;
            while let Some(offset) = mask_line[from..].find(keyword) {
                let at = from + offset;
                from = at + keyword.len();
                let bytes = mask_line.as_bytes();
                // The keyword must be a token of its own: `exit_code=1` assigns
                // a variable, and `--exit 1` is somebody's flag.
                if at > 0 && (is_ident_byte(bytes[at - 1]) || bytes[at - 1] == b'-') {
                    continue;
                }
                if bytes.get(from).is_some_and(|byte| is_ident_byte(*byte)) {
                    continue;
                }
                let Some(rest) = raw_line.get(from..) else {
                    continue;
                };
                let rest = rest.trim_start();
                let argument: String = rest
                    .chars()
                    .take_while(|character| !character.is_whitespace() && *character != ';')
                    .collect();
                match argument.parse::<i64>() {
                    // A bare `exit`/`return`, or an explicit success.
                    Ok(0) => continue,
                    Ok(_) => return true,
                    Err(_) => {
                        // Any expansion may carry a nonzero status.
                        if argument.contains('$') {
                            return true;
                        }
                    }
                }
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Fact 3, restated for a proof artifact — the prover can still say no
// ---------------------------------------------------------------------------

/// The comment syntax of one formal language.
///
/// ONE reader serves both lanes. Lean and TLA+ differ in exactly three tokens
/// and agree on everything that matters — both nest their block comments, and
/// both carry double-quoted string literals — so writing `mask_lean` and
/// `mask_tla` would be two implementations of one fact, which is the duplication
/// this module's header is about.
pub struct CommentSyntax {
    /// Comment to end of line.
    pub line: &'static str,
    /// Block comment open; nests.
    pub open: &'static str,
    /// Block comment close.
    pub close: &'static str,
}

/// Lean 4: `--` to end of line, `/- … -/` nesting (`/-!` and `/--` are block
/// comments that happen to start with `/-`, so they need no special case).
pub const LEAN_SYNTAX: CommentSyntax = CommentSyntax {
    line: "--",
    open: "/-",
    close: "-/",
};

/// TLA+ and its TLC configuration files: `\*` to end of line, `(* … *)`
/// nesting.
pub const TLA_SYNTAX: CommentSyntax = CommentSyntax {
    line: "\\*",
    open: "(*",
    close: "*)",
};

/// A formal source with its comments and string literals blanked.
///
/// Byte-exact, like `mask_source` and [`mask_shell`]: a blanked byte becomes one
/// space, so an offset in the mask names the same column of the same line of the
/// source. Without this, `-- the `sorry` below is gone now` reads as an admit and
/// `\* INVARIANT Foo` reads as a checked property — which is
/// `fgdb-regcheck-scansites-line-anchored-ds45` and
/// `fgdb-regcheck-commented-arm-counts-live-ctv8` transposed into a language
/// nobody has looked at yet. Block comments NEST, and a masker that stopped at
/// the first `-/` would treat the tail of a nested doc comment as live code.
///
/// Deliberately byte-oriented throughout: `str` slicing at a non-boundary
/// panics, and a masker that panics on a UTF-8 proof script — Lean sources are
/// full of `∀`, `≤`, `⟨⟩` — is a checker that cannot read its own subject.
pub fn mask_formal(source: &str, syntax: &CommentSyntax) -> String {
    let bytes = source.as_bytes();
    let mut out = vec![b' '; bytes.len()];
    let (line, open, close) = (
        syntax.line.as_bytes(),
        syntax.open.as_bytes(),
        syntax.close.as_bytes(),
    );
    let mut depth = 0usize;
    let mut in_string = false;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'\n' {
            out[index] = b'\n';
            // A string literal does not span a line in either language; a block
            // comment does.
            in_string = false;
            index += 1;
            continue;
        }
        if depth > 0 {
            if bytes[index..].starts_with(open) {
                depth += 1;
                index += open.len();
            } else if bytes[index..].starts_with(close) {
                depth -= 1;
                index += close.len();
            } else {
                index += 1;
            }
            continue;
        }
        if in_string {
            if byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == b'"' {
                out[index] = b'"';
                in_string = false;
            }
            index += 1;
            continue;
        }
        if bytes[index..].starts_with(open) {
            depth = 1;
            index += open.len();
            continue;
        }
        if bytes[index..].starts_with(line) {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if byte == b'"' {
            out[index] = b'"';
            in_string = true;
            index += 1;
            continue;
        }
        out[index] = byte;
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The ways a Lean file can assert its conclusion instead of proving it.
///
/// `sorry` and `admit` are pure admits: the file typechecks, `lake build` exits
/// zero, and the theorem is assumed. They are the exact analogue of a `main`
/// that can only return `ExitCode::SUCCESS` — an outcome fixed before the prover
/// reads anything.
///
/// `axiom` and `native_decide` are here for the same reason and with a caveat
/// stated out loud: both CAN be legitimate, and neither is legitimate silently.
/// An axiom is an unproven assumption and `native_decide` trusts the compiler's
/// evaluator; a lane resting on either is making a claim its `model_scope` must
/// state, and no machine-checkable disclosure field exists yet. So they are
/// reported. That direction produces a loud false alarm on a lane that meant it
/// and disclosed it in prose — which is recoverable — rather than a silent pass
/// on one that did not, which is the failure this module exists to remove.
const LEAN_ADMITS: &[&str] = &["sorry", "admit", "axiom", "native_decide"];

/// The Lean keywords that state a proposition to be proved.
///
/// A file with none of these proves nothing whatever it contains — the empty
/// file the bead names is only the smallest member of that class. `example` is
/// included: an anonymous theorem is still a checked proposition.
const LEAN_PROPOSITIONS: &[&str] = &["theorem", "lemma", "example"];

/// The TLC configuration keywords that name something to check.
///
/// A `.cfg` without one of these is a model checker run that explores the state
/// space and asserts nothing about it: it can only pass. `SPECIFICATION` is
/// deliberately NOT required here — a config missing it is a loud TLC error, and
/// this module's subject is the silent pass.
const TLA_CHECKED_PROPERTIES: &[&str] = &["INVARIANT", "INVARIANTS", "PROPERTY", "PROPERTIES"];

/// Every admit token present in this Lean source's live code.
fn lean_admits(source: &str) -> Vec<&'static str> {
    let masked = mask_formal(source, &LEAN_SYNTAX);
    LEAN_ADMITS
        .iter()
        .filter(|admit| contains_whole_token(&masked, admit))
        .copied()
        .collect()
}

/// Does this Lean source state a proposition in live code?
fn lean_states_a_proposition(source: &str) -> bool {
    let masked = mask_formal(source, &LEAN_SYNTAX);
    LEAN_PROPOSITIONS
        .iter()
        .any(|keyword| contains_whole_token(&masked, keyword))
}

/// Does this TLC configuration name something to check, in live code?
///
/// The keyword alone is not enough: `INVARIANT` with nothing after it names no
/// operator, so the reader requires an operand. That is the same rule as
/// [`rust_can_exit_nonzero`] reading the ARGUMENT of `ExitCode::from(` rather
/// than counting the call — a checker that stops at the keyword is reading a
/// call as evidence of what it does.
fn tla_config_checks_something(source: &str) -> bool {
    let masked = mask_formal(source, &TLA_SYNTAX);
    let bytes = masked.as_bytes();
    for keyword in TLA_CHECKED_PROPERTIES {
        let mut from = 0;
        while let Some(offset) = masked[from..].find(keyword) {
            let at = from + offset;
            from = at + keyword.len();
            if at > 0 && is_ident_byte(bytes[at - 1]) {
                continue;
            }
            let mut cursor = from;
            while matches!(bytes.get(cursor), Some(b) if b.is_ascii_whitespace()) {
                cursor += 1;
            }
            if matches!(bytes.get(cursor), Some(b) if is_ident_byte(*b)) {
                return true;
            }
        }
    }
    false
}

/// Match `needle` only where it is a WHOLE token.
///
/// Stricter than [`contains_token`], which anchors only the left edge because
/// its needles end in `!` or `(`. These needles are bare words, so both edges
/// matter: `sorry` must not fire on `sorryAx`, and `axiom` must not fire on
/// `axioms_used`.
fn contains_whole_token(text: &str, needle: &str) -> bool {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(offset) = text[from..].find(needle) {
        let at = from + offset;
        from = at + needle.len();
        let left_clear = at == 0 || !is_ident_byte(bytes[at - 1]);
        let right_clear = !matches!(bytes.get(at + needle.len()), Some(b) if is_ident_byte(*b));
        if left_clear && right_clear {
            return true;
        }
    }
    false
}

/// The TLC configuration a `.tla` model is checked under.
///
/// TLC takes a model and a config; the config is what names the invariant. The
/// registry records one artifact path, so the companion is derived — the same
/// stem with a `.cfg` extension, which is TLC's own convention.
fn tla_config_path(artifact: &Path) -> PathBuf {
    artifact.with_extension("cfg")
}

// ---------------------------------------------------------------------------
// The adjudication
// ---------------------------------------------------------------------------

/// A liveness prover that remembers what it has already read.
///
/// One registry sweep asks the same two questions of the filesystem over and
/// over — "what are the workspace members" and "what is compiled into this
/// crate's binaries" — and answering the second means masking every module of
/// the crate. Answering it once per row took ten seconds per sweep; the cache
/// takes it to three tenths of one, and `appendix_a` calls the prover once per
/// evidence row, of which the catalog has many.
///
/// The cache is keyed inside one prover, whose lifetime is one sweep of one
/// repository root. It is deliberately NOT a process-global: a stale cache in a
/// checker is the same family of defect as everything else this module exists to
/// remove — a verdict about something other than what is on disk.
pub struct Prover<'a> {
    repo_root: &'a Path,
    members: RefCell<Option<Result<Vec<PathBuf>, String>>>,
    module_maps: RefCell<BTreeMap<PathBuf, BTreeMap<PathBuf, Vec<PathBuf>>>>,
}

impl<'a> Prover<'a> {
    /// A prover for one sweep of `repo_root`.
    pub fn new(repo_root: &'a Path) -> Self {
        Self {
            repo_root,
            members: RefCell::new(None),
            module_maps: RefCell::new(BTreeMap::new()),
        }
    }

    /// The repository this prover reads.
    pub fn repo_root(&self) -> &Path {
        self.repo_root
    }

    fn members(&self) -> Result<Vec<PathBuf>, String> {
        let mut slot = self.members.borrow_mut();
        slot.get_or_insert_with(|| workspace_members(self.repo_root))
            .clone()
    }

    fn module_map(&self, member: &Path) -> BTreeMap<PathBuf, Vec<PathBuf>> {
        if let Some(cached) = self.module_maps.borrow().get(member) {
            return cached.clone();
        }
        let computed = binary_module_map(self.repo_root, member);
        self.module_maps
            .borrow_mut()
            .insert(member.to_path_buf(), computed.clone());
        computed
    }

    /// Every reason `checker` is not the live checker it claims to be.
    ///
    /// An empty result means live — but only if [`self_test`] is licensed. A
    /// caller that reports "no defects" without checking that has learned
    /// nothing: the readers above are the same source-text layer where all four
    /// of this repository's "looks exactly like a pass" tooling bugs lived.
    pub fn assess(&self, checker: &Checker) -> Vec<Defect> {
        assess_with(self, checker)
    }

    /// Every reason `lane` is not the checked proof lane it claims to be.
    ///
    /// `checkers` is the `checker_index` roster, because the INVOKED fact for a
    /// proof lane IS a checker-liveness question and is answered by
    /// [`Prover::assess`] rather than by a second reader — see the module
    /// header. As with [`Prover::assess`], an empty result means checked only if
    /// [`self_test`] is licensed.
    pub fn assess_lane(&self, lane: &Lane, checkers: &[Checker]) -> Vec<Defect> {
        assess_lane_with(self, lane, checkers)
    }
}

/// Every reason `checker` is not the live checker it claims to be.
///
/// The single-row entry point. A caller adjudicating more than one row should
/// build a [`Prover`] and reuse it.
pub fn assess(repo_root: &Path, checker: &Checker) -> Vec<Defect> {
    Prover::new(repo_root).assess(checker)
}

/// Every reason `lane` is not the checked proof lane it claims to be.
///
/// The single-row entry point. A caller adjudicating more than one row should
/// build a [`Prover`] and reuse it.
pub fn assess_lane(repo_root: &Path, lane: &Lane, checkers: &[Checker]) -> Vec<Defect> {
    Prover::new(repo_root).assess_lane(lane, checkers)
}

fn assess_with(prover: &Prover<'_>, checker: &Checker) -> Vec<Defect> {
    let repo_root = prover.repo_root;
    if checker.status != "live" {
        return Vec::new();
    }
    let relative = Path::new(&checker.artifact);
    let absolute = match artifact_path(repo_root, &checker.artifact) {
        Ok(path) => path,
        Err(defect) => return vec![defect],
    };
    let invocation = match invocation(prover, checker, relative) {
        Ok(invocation) => invocation,
        Err(defect) => return vec![defect],
    };
    let Some(unit) = checker.unit.as_deref().and_then(Unit::parse) else {
        return vec![Defect::new(
            DefectKind::UnitUndeclared,
            format!(
                "live row {:?} must declare unit = \"symbol\" or unit = \"artifact\"; \
                 found {:?}",
                checker.symbol, checker.unit
            ),
        )];
    };

    match invocation {
        Invocation::IntegrationTest => {
            let Ok(source) = fs::read_to_string(&absolute) else {
                return vec![Defect::new(
                    DefectKind::ArtifactAbsent,
                    format!("artifact {:?} could not be read", checker.artifact),
                )];
            };
            match unit {
                Unit::Symbol => match test_body(&source, &checker.symbol) {
                    None => vec![Defect::new(
                        DefectKind::SymbolUnresolved,
                        format!(
                            "no `#[test] fn {}` in the live code of {:?}",
                            checker.symbol, checker.artifact
                        ),
                    )],
                    Some(body) => {
                        if rust_can_panic(body) || returns_result(&source, &checker.symbol) {
                            Vec::new()
                        } else {
                            vec![Defect::new(
                                DefectKind::CannotFail,
                                format!(
                                    "`#[test] fn {}` has no assertion, panic or `?` in its body, \
                                     so running it cannot report a violation",
                                    checker.symbol
                                ),
                            )]
                        }
                    }
                },
                Unit::Artifact => {
                    if !source.contains("#[test]") {
                        return vec![Defect::new(
                            DefectKind::SymbolUnresolved,
                            format!(
                                "{:?} declares no `#[test]` function, so `cargo test` runs \
                                 nothing in it",
                                checker.artifact
                            ),
                        )];
                    }
                    if rust_can_panic(&source) {
                        Vec::new()
                    } else {
                        vec![Defect::new(
                            DefectKind::CannotFail,
                            format!(
                                "no test in {:?} contains an assertion or panic, so the whole \
                                 artifact cannot report a violation",
                                checker.artifact
                            ),
                        )]
                    }
                }
            }
        }
        Invocation::Binary { roots } => {
            let mut defects = Vec::new();
            if unit == Unit::Symbol {
                let Ok(source) = fs::read_to_string(&absolute) else {
                    return vec![Defect::new(
                        DefectKind::ArtifactAbsent,
                        format!("artifact {:?} could not be read", checker.artifact),
                    )];
                };
                if !defines_symbol(&source, &checker.symbol) {
                    defects.push(Defect::new(
                        DefectKind::SymbolUnresolved,
                        format!(
                            "no `fn {}` in the live code of {:?}",
                            checker.symbol, checker.artifact
                        ),
                    ));
                }
            }
            let failing_root = roots.iter().any(|root| {
                fs::read_to_string(repo_root.join(root))
                    .is_ok_and(|source| rust_can_exit_nonzero(&source))
            });
            if !failing_root {
                defects.push(Defect::new(
                    DefectKind::CannotFail,
                    format!(
                        "every binary target containing {:?} ({}) returns success \
                         unconditionally, so the gate cannot report a violation",
                        checker.artifact,
                        roots
                            .iter()
                            .map(|root| root.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }
            defects
        }
        Invocation::Script => {
            let Ok(source) = fs::read_to_string(&absolute) else {
                return vec![Defect::new(
                    DefectKind::ArtifactAbsent,
                    format!("artifact {:?} could not be read", checker.artifact),
                )];
            };
            if shell_can_exit_nonzero(&source) {
                Vec::new()
            } else {
                vec![Defect::new(
                    DefectKind::CannotFail,
                    format!(
                        "{:?} has no nonzero exit in live code, so running it cannot report \
                         a violation",
                        checker.artifact
                    ),
                )]
            }
        }
    }
}

fn assess_lane_with(prover: &Prover<'_>, lane: &Lane, checkers: &[Checker]) -> Vec<Defect> {
    let repo_root = prover.repo_root;
    // PATH SAFETY APPLIES TO EVERY LANE ROW, whatever its status. `Path::join`
    // with an absolute path DISCARDS the root, so `artifact = "/etc/hosts"` on a
    // declared lane is a row that passes `is_file()` the instant somebody
    // promotes it — the escape `appendix_a::safe_repository_relative` exists for,
    // and which the lane reader never had. This is the one check a `declared`
    // lane owes: its artifact does not exist yet, so nothing else about it can be
    // read, but the SHAPE of the path is checkable today.
    if !crate::appendix_a::safe_repository_relative(&lane.artifact) {
        return vec![Defect::new(
            DefectKind::ArtifactPathUnsafe,
            format!(
                "artifact {:?} is not a safe repository-relative path",
                lane.artifact
            ),
        )];
    }
    if lane.status != "checked" {
        return Vec::new();
    }
    let relative = Path::new(&lane.artifact);
    let absolute = match artifact_path(repo_root, &lane.artifact) {
        Ok(path) => path,
        Err(defect) => return vec![defect],
    };

    let mut defects = Vec::new();

    // INVOKED — "and is CI-checked". Delegated, in full, to the reader that
    // already answers "is this gate live". A lane whose prover no gate runs is
    // the proof-lane spelling of an artifact `cargo test` never compiles.
    match lane.checked_by.as_deref() {
        None => defects.push(Defect::new(
            DefectKind::LaneGateUndeclared,
            format!(
                "checked lane {:?} declares no `checked_by`, so nothing says which gate \
                 runs its prover",
                lane.id
            ),
        )),
        Some(symbol) => match checkers.iter().find(|row| row.symbol == symbol) {
            None => defects.push(Defect::new(
                DefectKind::LaneGateUnresolved,
                format!("checked_by {symbol:?} does not resolve in checker_index.toml"),
            )),
            Some(gate) if gate.status != "live" => defects.push(Defect::new(
                DefectKind::LaneGateNotLive,
                format!(
                    "checked_by {symbol:?} is a checker_index row with status {:?}; a lane \
                     is CI-checked only if the gate that runs it is live",
                    gate.status
                ),
            )),
            Some(gate) => {
                let gate_defects = prover.assess(gate);
                if !gate_defects.is_empty() {
                    defects.push(Defect::new(
                        DefectKind::LaneGateNotLive,
                        format!(
                            "checked_by {symbol:?} claims `status = \"live\"` but is not live: {}",
                            gate_defects
                                .iter()
                                .map(|defect| defect.detail.clone())
                                .collect::<Vec<_>>()
                                .join("; ")
                        ),
                    ));
                }
            }
        },
    }

    // CAPABLE OF FAILING — the prover must still be able to say no.
    let Ok(source) = fs::read_to_string(&absolute) else {
        defects.push(Defect::new(
            DefectKind::ArtifactAbsent,
            format!("artifact {:?} could not be read", lane.artifact),
        ));
        return defects;
    };
    match lane.lane.as_str() {
        "lean" => {
            if !lean_states_a_proposition(&source) {
                defects.push(Defect::new(
                    DefectKind::LaneProvesNothing,
                    format!(
                        "{:?} declares no `theorem`, `lemma` or `example` in live code, so \
                         building it proves nothing",
                        lane.artifact
                    ),
                ));
            }
            let admits = lean_admits(&source);
            if !admits.is_empty() {
                defects.push(Defect::new(
                    DefectKind::LaneAdmitsAnything,
                    format!(
                        "{:?} contains {} in live code: the build succeeds whether or not the \
                         theorem holds",
                        lane.artifact,
                        admits
                            .iter()
                            .map(|admit| format!("`{admit}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }
        }
        "tlaplus" => {
            let config = tla_config_path(relative);
            match fs::read_to_string(repo_root.join(&config)) {
                Err(_) => defects.push(Defect::new(
                    DefectKind::LaneProvesNothing,
                    format!(
                        "no TLC configuration at {:?}: a model with no config declares no \
                         instance bounds and no property, so nothing is checked",
                        config.display()
                    ),
                )),
                Ok(config_source) => {
                    if !tla_config_checks_something(&config_source) {
                        defects.push(Defect::new(
                            DefectKind::LaneProvesNothing,
                            format!(
                                "{:?} names no INVARIANT or PROPERTY in live text, so TLC \
                                 explores the state space and asserts nothing about it",
                                config.display()
                            ),
                        ));
                    }
                }
            }
        }
        // COMPLETENESS GUARD. `validate` rejects an unknown `lane` value in its
        // own schema pass, but a reader that falls through to "no defects" on a
        // row type it does not understand fails OPEN — and the two readers would
        // then have to be kept in step by hand, which is how this whole class
        // starts. A system with no reader here is never checked.
        other => defects.push(Defect::new(
            DefectKind::LaneSystemUnreadable,
            format!(
                "lane names formal system {other:?}, which no reader here adjudicates; a \
                 checked lane of an unreadable system cannot be distinguished from an \
                 unchecked one"
            ),
        )),
    }
    defects
}

/// Does `fn <symbol>` exist in live code, whatever its attributes?
fn defines_symbol(source: &str, symbol: &str) -> bool {
    let masked = mask_source(source);
    let text = masked.text();
    let bytes = text.as_bytes();
    let needle = format!("fn {symbol}");
    let mut from = 0;
    while let Some(offset) = text[from..].find(&needle) {
        let at = from + offset;
        from = at + needle.len();
        if at > 0 && is_ident_byte(bytes[at - 1]) {
            continue;
        }
        if !matches!(bytes.get(at + needle.len()), Some(b) if is_ident_byte(*b)) {
            return true;
        }
    }
    false
}

/// Does `#[test] fn <symbol>` declare a `Result` return, making `?` a failure
/// path?
fn returns_result(source: &str, symbol: &str) -> bool {
    let masked = mask_source(source);
    let text = masked.text();
    let needle = format!("fn {symbol}");
    let Some(at) = text.find(&needle) else {
        return false;
    };
    let after = at + needle.len();
    let Some(open) = text[after..].find('{').map(|index| after + index) else {
        return false;
    };
    text[after..open].contains("->") && text[after..open].contains("Result")
}

// ---------------------------------------------------------------------------
// The control
// ---------------------------------------------------------------------------

/// Which of the readers above got a known answer wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelfTest {
    /// The name of every case whose known answer the readers failed to produce.
    pub failures: Vec<&'static str>,
    /// How many cases ran. A zero here is itself a broken control.
    pub cases: usize,
}

impl SelfTest {
    /// May a caller treat "no defects" as evidence?
    pub fn licensed(&self) -> bool {
        self.cases > 0 && self.failures.is_empty()
    }
}

/// Run every reader over synthetic text whose answer is known.
///
/// Text-only and filesystem-free, exactly like `unsafe_ledger::scanner_fixture`,
/// so it can run inside a checker on every invocation and license — or refuse to
/// license — that run's conclusions. Half the cases are MASKED cases, because
/// "the reader saw a comment" is the specific failure this repository keeps
/// shipping.
pub fn self_test() -> SelfTest {
    let mut failures = Vec::new();
    let mut cases = 0usize;
    let mut check = |name: &'static str, actual: bool, expected: bool| {
        cases += 1;
        if actual != expected {
            failures.push(name);
        }
    };

    // --- the Rust panic reader -------------------------------------------
    check(
        "rust_panic_present",
        rust_can_panic("fn t() { assert!(x); }"),
        true,
    );
    check(
        "rust_panic_absent",
        rust_can_panic("fn t() { let _ = x; }"),
        false,
    );
    check(
        "rust_panic_in_line_comment",
        rust_can_panic("fn t() { // assert!(x);\n}"),
        false,
    );
    check(
        "rust_panic_in_block_comment",
        rust_can_panic("fn t() {\n/*\nassert!(x);\n*/\n}"),
        false,
    );
    check(
        "rust_panic_in_string_literal",
        rust_can_panic("fn t() { let s = \"assert!(x)\"; }"),
        false,
    );
    check(
        "rust_panic_debug_variant",
        rust_can_panic("fn t() { debug_assert_eq!(a, b); }"),
        true,
    );

    // --- the Rust nonzero-exit reader ------------------------------------
    check(
        "rust_exit_success_only",
        rust_can_exit_nonzero("fn main() -> ExitCode { ExitCode::SUCCESS }"),
        false,
    );
    check(
        "rust_exit_zero_literal",
        rust_can_exit_nonzero("fn main() -> ExitCode { ExitCode::from(0) }"),
        false,
    );
    check(
        "rust_exit_nonzero_literal",
        rust_can_exit_nonzero("fn main() -> ExitCode { ExitCode::from(1) }"),
        true,
    );
    check(
        "rust_exit_failure_constant",
        rust_can_exit_nonzero("fn main() -> ExitCode { ExitCode::FAILURE }"),
        true,
    );

    // --- the module-declaration reader -----------------------------------
    check(
        "mod_live",
        declared_modules("pub mod live;\n") == vec!["live".to_owned()],
        true,
    );
    check(
        "mod_commented",
        declared_modules("// mod dead;\n").is_empty(),
        true,
    );
    check(
        "mod_inline_declares_no_file",
        declared_modules("mod inline { }\n").is_empty(),
        true,
    );
    check(
        "mod_not_a_prefix_match",
        declared_modules("let modest = 1;\n").is_empty(),
        true,
    );

    // --- the test-body reader --------------------------------------------
    check(
        "test_body_found",
        test_body("#[test]\nfn t() { assert!(x); }", "t").is_some(),
        true,
    );
    check(
        "test_body_requires_test_attribute",
        test_body("fn t() { assert!(x); }", "t").is_none(),
        true,
    );
    check(
        "test_body_ignores_commented_attribute",
        test_body("// #[test]\nfn t() { assert!(x); }", "t").is_none(),
        true,
    );
    check(
        "test_body_is_not_a_prefix_match",
        test_body("#[test]\nfn t_extra() { assert!(x); }", "t").is_none(),
        true,
    );
    check(
        "test_body_stops_at_its_own_brace",
        test_body("#[test]\nfn t() { }\nfn other() { assert!(x); }", "t")
            .is_some_and(|body| !body.contains("assert")),
        true,
    );

    // --- the shell reader -------------------------------------------------
    check(
        "shell_exit_nonzero",
        shell_can_exit_nonzero("#!/usr/bin/env bash\nexit 1\n"),
        true,
    );
    check(
        "shell_exit_zero_only",
        shell_can_exit_nonzero("#!/usr/bin/env bash\necho hi\nexit 0\n"),
        false,
    );
    check(
        "shell_exit_in_comment",
        shell_can_exit_nonzero("#!/usr/bin/env bash\n# exit 1\nexit 0\n"),
        false,
    );
    check(
        "shell_exit_in_single_quotes",
        shell_can_exit_nonzero("#!/usr/bin/env bash\necho 'exit 1'\nexit 0\n"),
        false,
    );
    check(
        "shell_exit_in_heredoc",
        shell_can_exit_nonzero("#!/usr/bin/env bash\ncat <<'EOF'\nexit 1\nEOF\nexit 0\n"),
        false,
    );
    check(
        "shell_return_nonzero",
        shell_can_exit_nonzero("#!/usr/bin/env bash\nf() { return 1; }\n"),
        true,
    );
    check(
        "shell_exit_variable_status",
        shell_can_exit_nonzero("#!/usr/bin/env bash\nexit \"$rc\"\n"),
        true,
    );
    check(
        "shell_exit_is_not_a_prefix_match",
        shell_can_exit_nonzero("#!/usr/bin/env bash\nexit_code=1\n"),
        false,
    );
    check(
        "shell_arithmetic_shift_is_not_a_heredoc",
        shell_can_exit_nonzero("#!/usr/bin/env bash\nn=$(( 1 << 2 ))\nexit 1\n"),
        true,
    );
    check(
        "shell_herestring_is_not_a_heredoc",
        shell_can_exit_nonzero("#!/usr/bin/env bash\ncat <<<\"x\"\nexit 1\n"),
        true,
    );
    check(
        "shell_double_quoted_exit_is_not_a_call",
        shell_can_exit_nonzero("#!/usr/bin/env bash\necho \"exit 1\"\nexit 0\n"),
        false,
    );
    // The mask must stay byte-for-byte aligned with the source across non-ASCII
    // text, or every "value from the raw line" read lands at a shifted offset.
    check(
        "shell_mask_is_byte_aligned_over_non_ascii",
        mask_shell("echo 'héllo — wörld'\nexit 1\n").len()
            == "echo 'héllo — wörld'\nexit 1\n".trim_end().len(),
        true,
    );
    check(
        "shell_exit_survives_non_ascii_neighbours",
        shell_can_exit_nonzero("#!/usr/bin/env bash\necho \"héllo — wörld\"\nexit 1\n"),
        true,
    );

    // --- the Lean readers --------------------------------------------------
    //
    // The admit tokens are assembled from a `char` for the same reason
    // `scanner_fixture` assembles its attribute that way: `LEAN_ADMITS` is
    // matched over this crate's own sources by nothing today, but a literal
    // `sorry` in a file named by a future lane is a real site, and a control
    // that plants real sites is a control that will one day be wrong about
    // itself.
    let s = 's';
    check(
        "lean_admit_present",
        lean_admits(&format!("theorem t : True := {s}orry\n")) == vec!["sorry"],
        true,
    );
    check(
        "lean_admit_absent",
        lean_admits("theorem t : True := trivial\n").is_empty(),
        true,
    );
    check(
        "lean_admit_in_line_comment",
        lean_admits(&format!("-- was {s}orry\ntheorem t : True := trivial\n")).is_empty(),
        true,
    );
    check(
        "lean_admit_in_block_comment",
        lean_admits(&format!("/- {s}orry -/\ntheorem t : True := trivial\n")).is_empty(),
        true,
    );
    check(
        "lean_admit_in_nested_block_comment",
        lean_admits(&format!(
            "/- outer /- inner -/ {s}orry -/\ntheorem t : True := trivial\n"
        ))
        .is_empty(),
        true,
    );
    check(
        "lean_admit_in_string_literal",
        lean_admits(&format!(
            "theorem t : True := by trace \"{s}orry\"; trivial\n"
        ))
        .is_empty(),
        true,
    );
    check(
        "lean_admit_is_not_a_prefix_match",
        lean_admits(&format!("theorem t : True := {s}orryAx_free\n")).is_empty(),
        true,
    );
    check(
        "lean_admit_axiom_is_reported",
        lean_admits("axiom trust_me : True\n") == vec!["axiom"],
        true,
    );
    check(
        "lean_proposition_present",
        lean_states_a_proposition("theorem t : True := trivial\n"),
        true,
    );
    check(
        "lean_proposition_absent_in_empty_file",
        lean_states_a_proposition(""),
        false,
    );
    check(
        "lean_proposition_in_comment_is_not_a_proposition",
        lean_states_a_proposition("-- theorem t : True := trivial\n"),
        false,
    );
    // The mask must stay byte-aligned across the notation a Lean source is
    // written in, or every offset read from it lands in the wrong column.
    check(
        "lean_mask_is_byte_aligned_over_notation",
        mask_formal("theorem t : ∀ n, n ≤ n := by simp\n", &LEAN_SYNTAX).len()
            == "theorem t : ∀ n, n ≤ n := by simp\n".len(),
        true,
    );
    check(
        "lean_admit_survives_notation_neighbours",
        lean_admits(&format!("theorem t : ∀ n, n ≤ n := {s}orry\n")) == vec!["sorry"],
        true,
    );

    // --- the TLA+ config reader -------------------------------------------
    check(
        "tla_config_checks_an_invariant",
        tla_config_checks_something("SPECIFICATION Spec\nINVARIANT TypeOK\n"),
        true,
    );
    check(
        "tla_config_checks_a_property",
        tla_config_checks_something("SPECIFICATION Spec\nPROPERTIES Liveness\n"),
        true,
    );
    check(
        "tla_config_checks_nothing",
        tla_config_checks_something("SPECIFICATION Spec\nCONSTANTS N = 3\n"),
        false,
    );
    check(
        "tla_config_invariant_in_comment",
        tla_config_checks_something("SPECIFICATION Spec\n\\* INVARIANT TypeOK\n"),
        false,
    );
    check(
        "tla_config_invariant_in_block_comment",
        tla_config_checks_something("SPECIFICATION Spec\n(* INVARIANT TypeOK *)\n"),
        false,
    );
    check(
        "tla_config_keyword_without_an_operand",
        tla_config_checks_something("SPECIFICATION Spec\nINVARIANT\n"),
        false,
    );
    check(
        "tla_config_keyword_is_not_a_suffix_match",
        tla_config_checks_something("SPECIFICATION Spec\nMY_INVARIANT Foo\n"),
        false,
    );

    SelfTest { failures, cases }
}

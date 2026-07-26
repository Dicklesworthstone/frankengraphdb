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

use crate::model::Checker;
use crate::unsafe_ledger::mask_source;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// The registry this module adjudicates.
pub const CHECKER_INDEX_PATH: &str = "registries/checker_index.toml";

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
}

/// Every reason `checker` is not the live checker it claims to be.
///
/// The single-row entry point. A caller adjudicating more than one row should
/// build a [`Prover`] and reuse it.
pub fn assess(repo_root: &Path, checker: &Checker) -> Vec<Defect> {
    Prover::new(repo_root).assess(checker)
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

    SelfTest { failures, cases }
}

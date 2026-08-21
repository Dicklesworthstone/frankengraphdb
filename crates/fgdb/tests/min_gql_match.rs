//! **The pinned GQL statement on the product path** (`fgdb-w5-parsers-nje.1`).
//!
//! Genesis criterion 2: `MATCH (a)-[:R]->(b) RETURN b` binds, plans, and
//! executes on `fgdb::Database`. This file is the product-level acceptance for
//! exactly that one statement — no labels, no WHERE, no variable-length, no
//! TCK. Everything off that grammar is a TYPED parse error, never a best
//! effort.
//!
//! **API CONTRACT THIS FILE COMPILES AGAINST** (the implementation slice's
//! in-flight surface, read from `lib.rs` at authoring time):
//! - `Database::execute_gql(&self, src: &str, bind: &RelationBind)
//!    -> Result<Vec<VId>, GqlError>` — one method, sync like every other
//!   product read; rows ARE destination vids.
//! - `RelationBind`: the caller-supplied `"R" -> RelationId` map, built via
//!   `RelationBind::new().with_relation(...)`, re-exported from `fgdb-gql`.
//! - `GqlError::Parse(_)` is the typed off-grammar arm (`Bind` and `Read` are
//!   its siblings, three arms because three remedies).
//! Until `fgdb-gql` lands this file fails to compile — deliberately. It is
//! the executable acceptance criteria; do not weaken it to make it compile.
//!
//! **THE PLANTED NEGATIVE (constitutional, Doctrine 7).** A
//! parser-interprets-AST engine is prohibited: the executor's only input is
//! the `BoundPlan` the binder produced — `execute(plan: &BoundPlan, …)` — and
//! the parse tree must be unrepresentable there. These tests reach execution
//! ONLY through `execute_gql`, whose pipeline is parse → bind → execute; a
//! cheat that walks AST nodes at runtime cannot satisfy the `BoundPlan`
//! signature the executor exposes, and it cannot fake the determinism law
//! below either, because CGSE ordering is a property of the plan's scan, not
//! of parse-tree traversal order. If `execute_gql` ever grows an AST-taking
//! twin, that twin is the defect this comment exists to make loud.

use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, GqlError, RelationBind, WriteBatch};
use fgdb_delta_types::{LabelId, RelationId};
use fgdb_types::context::{CommitCx, PurposeContexts};
use fgdb_types::ids::DatabaseSecurityNamespaceId;
use fgdb_types::{EId, VId};
use std::path::PathBuf;

const R: RelationId = RelationId(1);
const OTHER: RelationId = RelationId(2);
const PINNED: &str = "MATCH (a)-[:R]->(b) RETURN b";
const K_OID: [u8; 32] = [0x5a; 32];
const NAMESPACE: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x77; 32]);

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(K_OID, NAMESPACE, [0x3c; 32])
}

/// A scratch directory that does not yet exist, so `create` owns making it.
/// Pid-qualified because concurrent panes share `/tmp`; nothing is removed
/// (rule 1 carves out no exception for test code).
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fgdb-min-gql-{}-{name}", std::process::id()))
}

fn under_lab<T, Fut>(seed: u64, test: impl FnOnce(CommitCx) -> Fut + Send + 'static) -> T
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (output, report) = run_async_under_lab(seed, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        test(contexts.commit()).await
    });
    assert!(
        report.lab_test_passed(),
        "lab run failed (quiescence, oracle, or invariant channel): {report:?}"
    );
    output
}

/// The one bind every test uses: the statement's `R` resolves to
/// `RelationId(1)` exactly as the spine example does, through the landed
/// builder API (`crates/fgdb-gql`).
fn bind_r() -> RelationBind {
    RelationBind::new().with_relation("R", R)
}

/// One `:R` edge: the pinned statement returns exactly the destination vid,
/// and a cold reopen answers identically from disk.
#[test]
fn pinned_match_returns_the_destination_and_survives_reopen() {
    under_lab(0x91_01, |cx| async move {
        let cx = &cx;
        let dir = scratch("one-edge");
        {
            let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
            let mut batch = WriteBatch::new(R);
            batch.create_vertex(VId(1), vec![LabelId(3)], vec![]);
            batch.create_vertex(VId(2), vec![], vec![]);
            batch.add_edge(EId(10), VId(1), VId(2), vec![]);
            db.write(cx, batch).await.expect("commits");

            let rows = db.execute_gql(PINNED, &bind_r()).expect("pinned statement executes");
            assert_eq!(rows, vec![VId(2)]);
        }

        // NOTHING crosses this line except the path and the keys: the answer
        // below comes from the durable stream, not the writer's fold.
        let db = Database::open(cx, &dir, keys()).await.expect("reopens");
        let rows = db.execute_gql(PINNED, &bind_r()).expect("executes after reopen");
        assert_eq!(rows, vec![VId(2)]);
    });
}

/// Two disjoint `:R` edges, written in DESCENDING destination order so an
/// implementation that echoes insertion or scan-discovery order fails: the
/// result is CGSE-deterministic, destination `VId` ascending.
#[test]
fn two_edges_return_both_destinations_sorted_ascending() {
    under_lab(0x91_02, |cx| async move {
        let cx = &cx;
        let dir = scratch("two-edges");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut batch = WriteBatch::new(R);
        batch.create_vertex(VId(1), vec![], vec![]);
        batch.create_vertex(VId(3), vec![], vec![]);
        batch.create_vertex(VId(9), vec![], vec![]);
        batch.create_vertex(VId(4), vec![], vec![]);
        // The 9-destination edge first: ascending output must be a SORT, not
        // an accident of write order.
        batch.add_edge(EId(10), VId(1), VId(9), vec![]);
        batch.add_edge(EId(11), VId(3), VId(4), vec![]);
        db.write(cx, batch).await.expect("commits");

        let rows = db.execute_gql(PINNED, &bind_r()).expect("executes");
        assert_eq!(
            rows,
            vec![VId(4), VId(9)],
            "destination vids, CGSE-sorted ascending — not insertion order"
        );

        // Determinism is a product feature (B5): same graph, same statement,
        // same bind ⇒ identical rows, every time. An AST-walking executor has
        // no plan to hang this law on; the BoundPlan scan does.
        let again = db.execute_gql(PINNED, &bind_r()).expect("executes again");
        assert_eq!(rows, again);
    });
}

/// A graph with vertices and an edge in a DIFFERENT relation than the bind:
/// the match is empty, and empty is `Ok(vec![])`, never an error. The
/// off-relation edge is the control that the scan honours the bound
/// `RelationId` rather than matching any edge shape.
#[test]
fn no_matching_edge_is_an_empty_result_not_an_error() {
    under_lab(0x91_03, |cx| async move {
        let cx = &cx;
        let dir = scratch("no-match");
        let mut db = Database::create(cx, &dir, keys()).await.expect("creates");
        let mut batch = WriteBatch::new(OTHER);
        batch.create_vertex(VId(1), vec![], vec![]);
        batch.create_vertex(VId(2), vec![], vec![]);
        batch.add_edge(EId(10), VId(1), VId(2), vec![]);
        db.write(cx, batch).await.expect("commits");

        let rows = db
            .execute_gql(PINNED, &bind_r())
            .expect("an empty match is a result, not a failure");
        assert!(
            rows.is_empty(),
            "the bind maps R to RelationId(1); the only edge is RelationId(2): {rows:?}"
        );
    });
}

/// Everything off the pinned grammar is a TYPED parse error. The grammar is
/// EXACTLY one statement; "close enough" inputs are the ones a lenient parser
/// would quietly accept, so each mutation here is a distinct leniency to kill:
/// a shorter pattern, trailing tokens, a missing RETURN, and empty input.
#[test]
fn off_grammar_inputs_are_typed_parse_errors() {
    under_lab(0x91_04, |cx| async move {
        let cx = &cx;
        let dir = scratch("off-grammar");
        let db = Database::create(cx, &dir, keys()).await.expect("creates");

        for off_grammar in [
            "MATCH (a) RETURN a",
            "MATCH (a)-[:R]->(b) RETURN b EXTRA",
            "MATCH (a)-[:R]->(b)",
            "",
        ] {
            let err = db
                .execute_gql(off_grammar, &bind_r())
                .expect_err(off_grammar);
            assert!(
                matches!(err, GqlError::Parse(_)),
                "{off_grammar:?} must be the typed parse arm, got {err:?}"
            );
        }
    });
}

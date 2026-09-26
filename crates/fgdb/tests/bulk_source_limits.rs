//! Fallible decoders, complete-source budgets and chunk bytes use the same
//! real loader as ordinary ingestion. No source error is an EOF or commit.

use asupersync::lab::run_async_under_lab;
use fgdb::{
    BulkEdge, BulkLoadErrorKind, BulkLoadPolicy, BulkRow, BulkVertex, Database, DatabaseKeys,
};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_types::{CanonicalScalar, DatabaseSecurityNamespaceId, PurposeContexts};
use std::error::Error;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Debug)]
struct DecoderFailure(usize); // Deliberately not Clone: readers create errors on demand.
impl std::fmt::Display for DecoderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "decoder stopped at record {}", self.0)
    }
}
impl Error for DecoderFailure {}
struct Decoder {
    rows: Arc<[BulkRow]>,
    at: usize,
    audit: bool,
    audit_error: Option<usize>,
    replay_error: Option<usize>,
}
impl Clone for Decoder {
    fn clone(&self) -> Self {
        Self {
            rows: self.rows.clone(),
            at: self.at,
            audit: true,
            audit_error: self.audit_error,
            replay_error: self.replay_error,
        }
    }
}
impl Iterator for Decoder {
    type Item = Result<BulkRow, DecoderFailure>;
    fn next(&mut self) -> Option<Self::Item> {
        let at = self.at;
        self.at += 1;
        if (if self.audit {
            self.audit_error
        } else {
            self.replay_error
        }) == Some(at)
        {
            return Some(Err(DecoderFailure(at)));
        }
        self.rows.get(at).cloned().map(Ok)
    }
}
fn decoder(audit_error: Option<usize>, replay_error: Option<usize>) -> Decoder {
    Decoder {
        rows: rows().into(),
        at: 0,
        audit: false,
        audit_error,
        replay_error,
    }
}
fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0xb4; 32],
        DatabaseSecurityNamespaceId([0xb5; 32]),
        [0xb6; 32],
    )
}
fn vertex(key: &str) -> BulkRow {
    BulkRow::Vertex(BulkVertex {
        key: key.into(),
        labels: vec![LabelId(1)],
        props: vec![(PropertyKeyId(1), CanonicalScalar::Int(7))],
    })
}
fn edge(key: &str, source: &str, destination: &str) -> BulkRow {
    BulkRow::Edge(BulkEdge {
        key: key.into(),
        source: source.into(),
        destination: destination.into(),
        relation: RelationId(1),
        props: vec![(PropertyKeyId(1), CanonicalScalar::Int(9))],
    })
}
fn rows() -> Vec<BulkRow> {
    vec![
        vertex("a"),
        vertex("b"),
        edge("ab", "a", "b"),
        edge("ba", "b", "a"),
    ]
}
fn policy() -> BulkLoadPolicy {
    BulkLoadPolicy::new(2, RelationId(1))
}

#[test]
fn every_preflight_decoder_failure_leaves_the_graph_and_frontier_unchanged() {
    let ((), report) = run_async_under_lab(0x000b_5201, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for at in 0..=4 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let error = db
                .try_bulk_load(&cx, &commit, decoder(Some(at), None), policy())
                .await
                .unwrap_err();
            assert!(matches!(error.kind, BulkLoadErrorKind::Source { row, .. } if row == at));
            assert_eq!(
                error
                    .source()
                    .unwrap()
                    .downcast_ref::<DecoderFailure>()
                    .unwrap()
                    .0,
                at
            );
            assert_eq!(error.committed.next_row, 0);
            assert!(error.pending.is_none());
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn replay_reader_errors_preserve_exact_progress_and_resume_without_duplicates() {
    let ((), report) = run_async_under_lab(0x000b_5202, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for at in 0..=4 {
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let mut acknowledged = 0;
            let error = db
                .try_bulk_load_with_checkpoint(
                    &cx,
                    &commit,
                    decoder(None, Some(at)),
                    policy(),
                    None,
                    |_| {
                        acknowledged += 1;
                        Ok(())
                    },
                )
                .await
                .unwrap_err();
            assert!(matches!(error.kind, BulkLoadErrorKind::Source { row, .. } if row == at));
            let prefix = if at < 2 { 0 } else { 2 };
            assert_eq!(error.committed.next_row, prefix);
            assert_eq!(acknowledged, prefix / 2);
            assert!(error.pending.is_none());
            assert!(db.edges().unwrap().is_empty());
            let mut resumed = policy();
            resumed.resume = Some(error.committed);
            let done = db
                .try_bulk_load(&cx, &commit, decoder(None, None), resumed)
                .await
                .unwrap();
            assert_eq!(done.next_row, 4);
            assert_eq!(done.committed_chunks, 2);
            assert_eq!(db.vertices().unwrap().len(), 2);
            assert_eq!(db.edges().unwrap().len(), 2);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn source_row_and_key_bounds_are_inclusive_and_apply_before_any_commit() {
    let ((), report) = run_async_under_lab(0x000b_5203, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let exact = policy().with_source_limits(4, 2, 6, 4096);
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let done = db
            .try_bulk_load(&cx, &commit, decoder(None, None), exact.clone())
            .await
            .unwrap();
        assert_eq!(done.next_row, 4);
        for dimension in ["source_rows", "key_bytes", "total_key_bytes"] {
            let mut limited = exact.clone();
            match dimension {
                "source_rows" => limited.max_source_rows -= 1,
                "key_bytes" => limited.max_key_bytes -= 1,
                _ => limited.max_total_key_bytes -= 1,
            }
            let mut db = Database::open_memory(&commit, keys()).await.unwrap();
            let before = db.frontier().unwrap();
            let error = db
                .bulk_load(&cx, &commit, rows(), limited)
                .await
                .unwrap_err();
            assert!(
                matches!(error.kind, BulkLoadErrorKind::SourceLimit { dimension: found, .. } if found == dimension)
            );
            assert_eq!(error.committed.next_row, 0);
            assert!(error.pending.is_none());
            assert_eq!(db.frontier().unwrap(), before);
            assert!(db.vertices().unwrap().is_empty());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

fn row_bytes(row: &BulkRow) -> usize {
    let (fixed, props) = match row {
        BulkRow::Vertex(v) => (
            1 + 8
                + v.key.len()
                + 8
                + v.labels
                    .iter()
                    .map(|id| std::mem::size_of_val(&id.0))
                    .sum::<usize>(),
            &v.props,
        ),
        BulkRow::Edge(e) => (
            1 + 24
                + e.key.len()
                + e.source.len()
                + e.destination.len()
                + std::mem::size_of_val(&e.relation.0),
            &e.props,
        ),
    };
    fixed
        + 8
        + props
            .iter()
            .map(|(key, value)| std::mem::size_of_val(&key.0) + 8 + value.encode().unwrap().len())
            .sum::<usize>()
}

#[test]
fn canonical_chunk_bytes_are_exact_and_never_silently_change_transaction_boundaries() {
    let ((), report) = run_async_under_lab(0x000b_5204, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let input = rows();
        let maximum = input
            .chunks(2)
            .map(|chunk| chunk.iter().map(row_bytes).sum::<usize>())
            .max()
            .unwrap();
        let mut exact = policy();
        exact.max_chunk_bytes = maximum;
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let done = db
            .bulk_load(&cx, &commit, input.clone(), exact.clone())
            .await
            .unwrap();
        assert_eq!(done.committed_chunks, 2);
        exact.max_chunk_bytes -= 1;
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let error = db
            .bulk_load(&cx, &commit, input.clone(), exact)
            .await
            .unwrap_err();
        assert!(matches!(
            error.kind,
            BulkLoadErrorKind::SourceLimit {
                dimension: "chunk_bytes",
                ..
            }
        ));
        assert_eq!(error.committed.next_row, 0);
        let mut one_chunk = BulkLoadPolicy::new(4, RelationId(1));
        one_chunk.max_chunk_bytes = maximum;
        let error = db
            .bulk_load(&cx, &commit, input, one_chunk)
            .await
            .unwrap_err();
        assert!(matches!(
            error.kind,
            BulkLoadErrorKind::SourceLimit {
                dimension: "chunk_bytes",
                ..
            }
        ));
        assert!(
            db.vertices().unwrap().is_empty(),
            "do not turn one transaction into smaller committed chunks"
        );
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn a_never_ending_source_stops_at_its_first_over_budget_row() {
    let ((), report) = run_async_under_lab(0x000b_5205, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let input = (0usize..).map(move |i| {
            observed.fetch_add(1, Ordering::SeqCst);
            Ok::<_, DecoderFailure>(vertex(&format!("v{i}")))
        });
        let mut limited = policy();
        limited.max_source_rows = 3;
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let error = db
            .try_bulk_load(&cx, &commit, input, limited)
            .await
            .unwrap_err();
        assert!(matches!(
            error.kind,
            BulkLoadErrorKind::SourceLimit {
                row: 3,
                dimension: "source_rows",
                limit: 3,
                observed: 4,
            }
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(error.committed.next_row, 0);
        assert!(db.vertices().unwrap().is_empty());
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn resume_cannot_refresh_whole_source_row_or_key_allowances() {
    let ((), report) = run_async_under_lab(0x000b_5206, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let error = db
            .bulk_load_with_checkpoint(&cx, &commit, rows(), policy(), None, |_| {
                Err(std::io::Error::other("retain prefix"))
            })
            .await
            .unwrap_err();
        assert_eq!(error.committed.next_row, 2);
        for dimension in ["source_rows", "total_key_bytes"] {
            let mut resumed = policy();
            resumed.resume = Some(error.committed.clone());
            if dimension == "source_rows" {
                resumed.max_source_rows = 3;
            } else {
                resumed.max_total_key_bytes = 5;
            }
            let refused = db
                .try_bulk_load(&cx, &commit, decoder(None, None), resumed)
                .await
                .unwrap_err();
            assert!(
                matches!(refused.kind, BulkLoadErrorKind::SourceLimit { dimension: found, .. } if found == dimension)
            );
            assert_eq!(refused.committed.frontier, error.committed.frontier);
            assert_eq!(refused.committed.next_row, 2);
            assert!(refused.pending.is_none());
            assert!(db.edges().unwrap().is_empty());
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn zero_limits_and_invalid_chunk_widths_are_checked_even_for_empty_inputs() {
    let ((), report) = run_async_under_lab(0x000b_5207, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let mut db = Database::open_memory(&commit, keys()).await.unwrap();
        let empty = policy().with_source_limits(0, 0, 0, 0);
        assert_eq!(
            db.bulk_load(&cx, &commit, vec![], empty.clone())
                .await
                .unwrap()
                .next_row,
            0
        );
        assert!(matches!(
            db.bulk_load(&cx, &commit, rows(), empty)
                .await
                .unwrap_err()
                .kind,
            BulkLoadErrorKind::SourceLimit {
                dimension: "source_rows",
                ..
            }
        ));
        for width in [0, BulkLoadPolicy::MAX_ROWS_PER_CHUNK + 1, usize::MAX] {
            assert!(matches!(
                db.bulk_load(
                    &cx,
                    &commit,
                    vec![],
                    BulkLoadPolicy::new(width, RelationId(1))
                )
                .await
                .unwrap_err()
                .kind,
                BulkLoadErrorKind::InvalidPolicy
            ));
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn decoder_failure_prefix_reopens_from_real_chronicle_before_continuation() {
    use asupersync::{Budget, runtime::RuntimeBuilder};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "fgdb-fallible-bulk-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let contexts = PurposeContexts::narrow_runtime_root(&root);
    let cx = contexts.query();
    let commit = contexts.commit();
    runtime.block_on(async {
        let mut db = Database::create(&commit, &path, keys()).await.unwrap();
        let error = db
            .try_bulk_load(&cx, &commit, decoder(None, Some(3)), policy())
            .await
            .unwrap_err();
        assert!(matches!(
            error.kind,
            BulkLoadErrorKind::Source { row: 3, .. }
        ));
        assert_eq!(error.committed.next_row, 2);
        assert!(error.pending.is_none());
        let checkpoint = error.committed;
        drop(db);
        let mut db = Database::open(&commit, &path, keys()).await.unwrap();
        assert_eq!(db.frontier().unwrap(), checkpoint.frontier);
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert!(db.edges().unwrap().is_empty());
        let mut resumed = policy();
        resumed.resume = Some(checkpoint);
        let done = db
            .try_bulk_load(&cx, &commit, decoder(None, None), resumed)
            .await
            .unwrap();
        drop(db);
        let db = Database::open(&commit, &path, keys()).await.unwrap();
        assert_eq!(db.frontier().unwrap(), done.frontier);
        assert_eq!(db.vertices().unwrap().len(), 2);
        assert_eq!(db.edges().unwrap().len(), 2);
        let ab = db.edge(done.edges["ab"]).unwrap().unwrap();
        assert_eq!(ab.entry.src, done.vertices["a"]);
        assert_eq!(ab.entry.dst, done.vertices["b"]);
    });
}

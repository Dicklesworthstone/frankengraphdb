use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb::{Database, DatabaseKeys, EmbeddedReadView, MemVfs, WriteBatch};
use fgdb_delta_types::{LabelId, PropertyKeyId, RelationId};
use fgdb_gql::algebra::GraphValue;
use fgdb_strata::tiered::memory::MemoryCharge;
use fgdb_types::{
    CanonicalScalar, CommitCx, DatabaseSecurityNamespaceId, EId, PurposeContexts, VId,
};
use std::io;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn okay<T>(result: Result<T, Failure>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.class, error.message))
}

fn keys() -> DatabaseKeys {
    DatabaseKeys::new(
        [0x81; 32],
        DatabaseSecurityNamespaceId([0x82; 32]),
        [0x83; 32],
    )
}

fn options(extra: &[&str]) -> Options {
    let mut args = [
        "--db",
        "unused",
        "--key-file",
        "unused",
        "--label",
        "L=1",
        "--relation",
        "R=1",
        "--property",
        "p=1",
        "--property",
        "missing=2",
        "--buffered",
        "--buffer-memory-bytes",
        "8388608",
    ]
    .map(str::to_owned)
    .to_vec();
    args.extend(extra.iter().map(|value| (*value).to_owned()));
    okay(crate::parse(&args, "query"))
}

async fn fixture(cx: &CommitCx) -> (MemVfs, EmbeddedReadView) {
    let vfs = MemVfs::new().unwrap();
    let mut db = Database::create_with_vfs(cx, vfs.clone(), vfs.database_dir(), keys())
        .await
        .unwrap();
    let mut batch = WriteBatch::new(RelationId(1));
    for vid in 1..=3 {
        batch.create_vertex(
            VId(vid),
            vec![LabelId(1)],
            vec![(PropertyKeyId(1), CanonicalScalar::Int(vid as i64))],
        );
    }
    for (eid, source, target, value) in [(1, 1, 2, 7), (2, 1, 2, 8), (3, 2, 2, 9)] {
        batch.add_edge(
            EId(eid),
            VId(source),
            VId(target),
            vec![(PropertyKeyId(1), CanonicalScalar::Int(value))],
        );
    }
    assert_eq!(db.write(cx, batch).await.unwrap(), CommitSeq(1));
    let mut change = WriteBatch::new(RelationId(1));
    change.set_vertex_property(VId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(11)));
    change.set_edge_property(EId(1), PropertyKeyId(1), Some(CanonicalScalar::Int(17)));
    assert_eq!(db.write(cx, change).await.unwrap(), CommitSeq(2));
    (vfs, db.read_session().unwrap())
}

fn rows(bytes: &[u8]) -> Vec<&str> {
    std::str::from_utf8(bytes)
        .unwrap()
        .lines()
        .filter(|line| line.contains(r#""event":"row""#))
        .collect()
}

#[test]
fn buffered_vertex_edge_and_temporal_cli_delivery_matches_native_rows() {
    let ((), report) = run_async_under_lab(0x636c_7501, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        let (vfs, resident) = fixture(&commit).await;
        for (text, seq, count) in [
            ("MATCH (n:L) RETURN n,n.p,n.missing,n.p", 2, 3),
            (
                "MATCH (n:L) WHERE n.p > 2 RETURN DISTINCT n,n.p SKIP 1 LIMIT 1",
                2,
                1,
            ),
            ("MATCH (n) FOR SYSTEM_TIME AS OF SEQ 1 RETURN n,n.p", 1, 3),
            ("MATCH (n) FOR SYSTEM_TIME AS OF SEQ 0 RETURN n,n.p", 0, 0),
            ("MATCH (a)-[r:R]->(b) RETURN r,a,b,r.p,a.p", 2, 3),
            ("MATCH (a)<-[r:R]-(b) RETURN r,a,b,r.p", 2, 3),
            ("MATCH p=(a)-[r:R]-(b) RETURN r,a,b,p,r.p", 2, 5),
            (
                "MATCH (a)-[r:R]->(b) FOR SYSTEM_TIME AS OF SEQ 1 RETURN r,a,b,r.p",
                1,
                3,
            ),
        ] {
            let options = options(&[text]);
            let prepared = okay(prepare(&options));
            let (pool, limits) = okay(options.buffered.admission(options.budget.policy()));
            let mut view = Database::open_buffered_read_view_with_vfs(
                &commit,
                vfs.clone(),
                vfs.database_dir(),
                keys(),
                pool.clone(),
                limits,
            )
            .await
            .unwrap();
            let eager = resident
                .query(
                    &cx,
                    text,
                    &options.params,
                    &options,
                    options.budget.policy(),
                )
                .unwrap();
            let mut expected = Vec::new();
            okay(crate::render(eager, seq, "rows", true, &mut expected));
            let mut output = Vec::new();
            okay(run(&mut view, &cx, &options, &prepared, true, &mut output).await);
            assert_eq!(rows(&output), rows(&expected), "{text}");
            assert_eq!(rows(&output).len(), count, "{text}");
            let output = std::str::from_utf8(&output).unwrap();
            assert!(output.starts_with(&format!(
                r#"{{"v":1,"event":"columns","stream":true,"seq":{seq},"#
            )));
            assert!(output.ends_with(&format!(
                "{}\n",
                stream::summary_line(seq, count as u64, true)
            )));
            drop(view);
            assert_eq!(pool.used(), 0);
        }
        let mut options = options(&["MATCH (n) WHERE n.p > $floor RETURN n,n.p"]);
        options.params = fgdb_gql::GqlParameters::new()
            .with_int64("floor", 10)
            .unwrap();
        let prepared = okay(prepare(&options));
        let (pool, limits) = okay(options.buffered.admission(options.budget.policy()));
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs.clone(),
            vfs.database_dir(),
            keys(),
            pool.clone(),
            limits,
        )
        .await
        .unwrap();
        let mut output = Vec::new();
        okay(run(&mut view, &cx, &options, &prepared, false, &mut output).await);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("vertex 1\t11"));
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn unsupported_definitions_and_open_budgets_refuse_before_delivery_while_late_errors_do_not_complete()
 {
    let ((), report) = run_async_under_lab(0x636c_7502, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let commit = contexts.commit();
        for text in [
            "MATCH (n) RETURN n.p LIMIT 0",
            "MATCH (n) RETURN n ORDER BY n DESC LIMIT 0",
            "MATCH (n) WHERE EXISTS { MATCH (n)-[:R]->(m) } RETURN n LIMIT 0",
            "MATCH (a)-[r:R]->(b)-[:R]->(c) RETURN r,a,c LIMIT 0",
            "MATCH (n) RETURN count(*) LIMIT 0",
        ] {
            let options = options(&[text]);
            assert!(prepare(&options).is_err(), "{text}");
        }
        let (vfs, _resident) = fixture(&commit).await;
        for mode in 0..3 {
            let mut options = options(&["MATCH (n) RETURN n"]);
            match mode {
                0 => options.buffered.memory = Some(0),
                1 => options.buffered.source = Some(0),
                _ => options.budget.work = Some(0),
            }
            let (pool, limits) = okay(options.buffered.admission(options.budget.policy()));
            let opened = Database::open_buffered_read_view_with_vfs(
                &commit,
                vfs.clone(),
                vfs.database_dir(),
                keys(),
                pool.clone(),
                limits,
            )
            .await;
            let error = match opened {
                Err(error) => open_failure(error),
                Ok(_) => panic!("mode {mode} should refuse"),
            };
            assert_eq!(error.class, "open");
            assert_eq!(pool.used(), 0);
        }
        for text in [
            "MATCH (n) RETURN n,n.p",
            "MATCH (n) WHERE 10/(n.p-2)>0 RETURN n,n.p",
        ] {
            let mut options = options(&[text]);
            if text == "MATCH (n) RETURN n,n.p" {
                options.budget.rows = Some(1);
            }
            let prepared = okay(prepare(&options));
            let (pool, limits) = okay(options.buffered.admission(options.budget.policy()));
            let mut view = Database::open_buffered_read_view_with_vfs(
                &commit,
                vfs.clone(),
                vfs.database_dir(),
                keys(),
                pool.clone(),
                limits,
            )
            .await
            .unwrap();
            let mut output = Vec::new();
            let error = run(&mut view, &cx, &options, &prepared, true, &mut output)
                .await
                .err()
                .expect("late refusal");
            assert_eq!(error.class, "query");
            assert!(
                error
                    .message
                    .starts_with("stream incomplete after 1 fully flushed row(s)")
            );
            assert_eq!(rows(&output).len(), 1);
            assert!(
                !std::str::from_utf8(&output)
                    .unwrap()
                    .contains(r#""event":"result""#)
            );
            drop(view);
            assert_eq!(pool.used(), 0);
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

#[test]
fn buffered_flags_are_query_only_unique_and_do_not_change_ordinary_streaming() {
    let base = ["--db", "unused", "--key-file", "unused"];
    for (command, extra) in [
        (
            "query",
            vec!["--buffered", "--buffered", "MATCH (n) RETURN n"],
        ),
        (
            "query",
            vec!["--buffer-memory-bytes", "1", "MATCH (n) RETURN n"],
        ),
        (
            "query",
            vec!["--buffer-source-bytes", "1", "MATCH (n) RETURN n"],
        ),
        (
            "query",
            vec!["--buffered", "--stream", "MATCH (n) RETURN n"],
        ),
        (
            "query",
            vec!["--buffered", "--spill-dir", "unused", "MATCH (n) RETURN n"],
        ),
        (
            "query",
            vec!["--buffered", "--certify-to", "unused", "MATCH (n) RETURN n"],
        ),
        (
            "query",
            vec!["--buffered", "CALL fnx.pagerank() YIELD vertex,score"],
        ),
        ("write", vec!["--buffered", "CREATE (n)"]),
        ("create", vec!["--buffered"]),
        (
            "query",
            vec![
                "--buffered",
                "--buffer-memory-bytes",
                "1",
                "--buffer-memory-bytes",
                "2",
                "MATCH (n) RETURN n",
            ],
        ),
    ] {
        let args = base
            .into_iter()
            .chain(extra)
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert!(crate::parse(&args, command).is_err());
    }
    for flag in BufferedOptions::FLAGS {
        for value in ["", "-1", "+1", "1.5", " 1", "18446744073709551616"] {
            let args = base
                .into_iter()
                .chain(["--buffered", flag, value, "MATCH (n) RETURN n"])
                .map(str::to_owned)
                .collect::<Vec<_>>();
            assert!(crate::parse(&args, "query").is_err());
        }
    }
    let args = base
        .into_iter()
        .chain(["--stream", "MATCH (n) RETURN n"])
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let options = okay(crate::parse(&args, "query"));
    assert!(options.stream);
    assert!(!options.buffered.enabled());
}

#[test]
fn buffered_cli_reads_vertex_payloads_larger_than_its_graph_memory_cap() {
    const COUNT: usize = 320;
    const PAYLOAD: usize = 14_000;
    const MEMORY: u64 = 4 * 1024 * 1024;
    let ((), report) = run_async_under_lab(0x636c_7504, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let commit = contexts.commit();
        let cx = contexts.query();
        let vfs = MemVfs::new().unwrap();
        let mut db = Database::create_with_vfs(&commit, vfs.clone(), vfs.database_dir(), keys())
            .await
            .unwrap();
        let mut batch = WriteBatch::new(RelationId(1));
        for index in 1..=COUNT {
            let mut payload = vec![0x47; PAYLOAD];
            payload[..8].copy_from_slice(&(index as u64).to_le_bytes());
            batch.create_vertex(
                VId(index as u128),
                vec![],
                vec![(PropertyKeyId(1), CanonicalScalar::bytes(payload).unwrap())],
            );
        }
        assert_eq!(db.write(&commit, batch).await.unwrap(), CommitSeq(1));
        drop(db);
        assert!(
            COUNT * PAYLOAD > MEMORY as usize,
            "unique committed payload bytes exceed the graph pool"
        );
        let mut options = options(&["MATCH (n) RETURN n"]);
        options.buffered.memory = Some(MEMORY);
        let prepared = okay(prepare(&options));
        let (pool, limits) = okay(options.buffered.admission(options.budget.policy()));
        let mut view = Database::open_buffered_read_view_with_vfs(
            &commit,
            vfs.clone(),
            vfs.database_dir(),
            keys(),
            pool.clone(),
            limits,
        )
        .await
        .unwrap();
        let mut output = Vec::new();
        okay(run(&mut view, &cx, &options, &prepared, true, &mut output).await);
        let actual = rows(&output);
        assert_eq!(actual.len(), COUNT);
        for (index, line) in actual.into_iter().enumerate() {
            let cell = okay(crate::value_cell(&GraphValue::Vertex(VId(
                index as u128 + 1
            ))));
            assert_eq!(line, format!(r#"{{"v":1,"event":"row","cells":[{cell}]}}"#));
        }
        assert!(
            view.buffer_stats().bypasses >= COUNT as u64,
            "delivery used the extent source"
        );
        drop(view);
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

struct GuardedRow {
    row: GraphValueRow,
    _charge: MemoryCharge,
}
impl AsRef<GraphValueRow> for GuardedRow {
    fn as_ref(&self) -> &GraphValueRow {
        &self.row
    }
}

struct GuardedOutput {
    bytes: Vec<u8>,
    pool: MemoryPool,
    flushes: usize,
    fail_at: Option<usize>,
}
impl Write for GuardedOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.flushes += 1;
        let text = std::str::from_utf8(&self.bytes).unwrap();
        let row = text.lines().last().unwrap().contains(r#""event":"row""#);
        assert_eq!(
            self.pool.used(),
            if row { 1024 } else { 0 },
            "row guard must survive flush"
        );
        if self.fail_at == Some(self.flushes) {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "receiver closed"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn async_delivery_retains_row_admission_until_flush_and_stops_on_broken_output() {
    let ((), report) = run_async_under_lab(0x636c_7503, |root| async move {
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        for fail_at in [Some(1), Some(2), Some(3), None] {
            let pool = MemoryPool::new(1024, 0).unwrap();
            let pulls = Arc::new(AtomicUsize::new(0));
            let mut output = GuardedOutput {
                bytes: Vec::new(),
                pool: pool.clone(),
                flushes: 0,
                fail_at,
            };
            let next = async || {
                let at = pulls.fetch_add(1, Ordering::SeqCst);
                if at == 2 {
                    return None;
                }
                assert_eq!(pool.used(), 0, "previous row released before next demand");
                let charge = pool.reserve(&cx, 1024).unwrap();
                Some(Ok::<_, io::Error>(GuardedRow {
                    row: GraphValueRow::from_owned_values(vec![GraphValue::Vertex(VId(at
                        as u128
                        + 1))]),
                    _charge: charge,
                }))
            };
            let result = deliver(&["n".to_owned()], 2, next, true, &mut output, || Ok(())).await;
            assert_eq!(result.is_err(), fail_at.is_some());
            assert_eq!(pulls.load(Ordering::SeqCst), fail_at.map_or(3, |at| at - 1));
            assert_eq!(pool.used(), 0);
            if fail_at.is_some() {
                assert!(
                    !std::str::from_utf8(&output.bytes)
                        .unwrap()
                        .contains(r#""event":"result""#)
                );
            }
        }
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

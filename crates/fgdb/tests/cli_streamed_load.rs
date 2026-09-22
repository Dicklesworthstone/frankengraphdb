//! Real-process coverage of replayed file input and canonical bulk continuation.
//! Reader unit tests cover block mutation/cancellation without timing races.
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb::{Database, DatabaseKeys};
use fgdb_types::{CommitSeq, DatabaseSecurityNamespaceId, PurposeContexts};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

struct Fixture {
    home: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let home = std::env::temp_dir().join(format!(
            "fgdb-stream-load-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&home).unwrap();
        std::fs::write(
            home.join("keys"),
            format!(
                "{}\n{}\n{}\n",
                "5a".repeat(32),
                "77".repeat(32),
                "3c".repeat(32)
            ),
        )
        .unwrap();
        let fixture = Self { home };
        success(&fixture.command("create").output().unwrap());
        fixture
    }
    fn command(&self, verb: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fgdb"));
        command
            .env_remove("FGDB_LOAD_CRASH_CHUNK")
            .env_remove("FGDB_LOAD_CRASH_POINT")
            .args(["--robot", verb, "--db"])
            .arg(self.home.join("db"))
            .arg("--key-file")
            .arg(self.home.join("keys"))
            .args(["--relation", "R=1", "--relation", "S=2"]);
        command
    }
    fn load(&self) -> Command {
        let mut command = self.command("load");
        command
            .arg("--input")
            .arg(self.home.join("input"))
            .arg("--checkpoint")
            .arg(self.home.join("checkpoint"))
            .args(["--rows-per-chunk", "3"]);
        command
    }
    fn inspect<T>(&self, check: impl FnOnce(&Database) -> T) -> T {
        let runtime = RuntimeBuilder::new().build().unwrap();
        let root = runtime.request_cx_with_budget(Budget::INFINITE);
        let cx = PurposeContexts::narrow_runtime_root(&root).commit();
        runtime.block_on(async {
            let keys = DatabaseKeys::new(
                [0x5a; 32],
                DatabaseSecurityNamespaceId([0x77; 32]),
                [0x3c; 32],
            );
            let db = Database::open(&cx, &self.home.join("db"), keys)
                .await
                .unwrap();
            check(&db)
        })
    }
    fn frontier(&self) -> CommitSeq {
        self.inspect(|db| db.frontier().unwrap())
    }
    fn source(&self) -> Vec<u8> {
        // Whitespace crosses several read blocks without manufacturing giant
        // stored properties; the test exercises ten real creation effects.
        let mut source = String::new();
        let padding = " ".repeat(10_000);
        for id in 0..6 {
            source.push_str(&format!(
                "{padding}{{\"kind\":\"vertex\",\"key\":\"v{id}λ\",\"labels\":[]}}\r\n"
            ));
        }
        for id in 0..4 {
            source.push_str(&format!("{padding}{{\"kind\":\"edge\",\"key\":\"e{id}\",\"source\":\"v{id}λ\",\"destination\":\"v{}λ\",\"relation\":\"{}\"}}{}",
                id + 1, if id % 2 == 0 { "R" } else { "S" }, if id == 3 { "" } else { "\n" }));
        }
        let bytes = source.into_bytes();
        assert!(bytes.len() > 64 * 1024);
        std::fs::write(self.home.join("input"), &bytes).unwrap();
        bytes
    }
}
fn success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn large_input_streams_across_blocks_and_completed_resume_is_a_noop() {
    let fixture = Fixture::new();
    fixture.source();
    let basis = fixture.frontier();
    success(&fixture.load().output().unwrap());
    let before = fixture.inspect(|db| {
        assert_eq!(db.frontier().unwrap(), CommitSeq(basis.0 + 4));
        assert_eq!(db.vertices().unwrap().len(), 6);
        let edges = db.edges().unwrap();
        assert_eq!(edges.len(), 4);
        assert_eq!(
            edges.iter().filter(|row| row.entry.relation.0 == 1).count(),
            2
        );
        assert_eq!(
            edges.iter().filter(|row| row.entry.relation.0 == 2).count(),
            2
        );
        (db.vertices().unwrap(), edges)
    });
    let checkpoint = std::fs::read(fixture.home.join("checkpoint")).unwrap();
    success(&fixture.load().output().unwrap());
    assert_eq!(fixture.frontier(), CommitSeq(basis.0 + 4));
    assert_eq!(
        fixture.inspect(|db| (db.vertices().unwrap(), db.edges().unwrap())),
        before
    );
    assert_eq!(
        std::fs::read(fixture.home.join("checkpoint")).unwrap(),
        checkpoint
    );
}

#[test]
fn marker_to_checkpoint_crash_window_reconciles_using_streamed_source_rows() {
    let fixture = Fixture::new();
    fixture.source();
    let basis = fixture.frontier();
    let stopped = fixture
        .load()
        .env("FGDB_LOAD_CRASH_CHUNK", "1")
        .env("FGDB_LOAD_CRASH_POINT", "after-marker-sync")
        .output()
        .unwrap();
    assert!(!stopped.status.success());
    assert!(
        String::from_utf8_lossy(&std::fs::read(fixture.home.join("checkpoint")).unwrap())
            .contains("\"next_row\":3")
    );
    success(&fixture.load().output().unwrap());
    fixture.inspect(|db| {
        assert_eq!(db.frontier().unwrap(), CommitSeq(basis.0 + 4));
        assert_eq!(db.vertices().unwrap().len(), 6);
        assert_eq!(db.edges().unwrap().len(), 4);
        assert_eq!(db.delta_since(basis).unwrap().count(), 4);
    });
}

#[test]
fn a_late_decoder_failure_does_not_create_a_checkpoint_or_commit_a_prefix() {
    let fixture = Fixture::new();
    let mut source = fixture.source();
    source.extend_from_slice(b"\n{broken}");
    std::fs::write(fixture.home.join("input"), source).unwrap();
    let basis = fixture.frontier();
    let failed = fixture.load().output().unwrap();
    assert_eq!(failed.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&failed.stdout).contains("line 11:"));
    assert!(!fixture.home.join("checkpoint").exists());
    assert_eq!(fixture.frontier(), basis);
    assert_eq!(fixture.inspect(|db| db.vertices().unwrap().len()), 0);
}

#[test]
fn overlong_records_and_sparse_oversized_files_refuse_before_checkpointing() {
    for oversized_file in [false, true] {
        let fixture = Fixture::new();
        let basis = fixture.frontier();
        let mut file = std::fs::File::create(fixture.home.join("input")).unwrap();
        if oversized_file {
            // Sparse metadata, not 8 GiB of test payload. The admission must
            // refuse before a bulk read/allocation can be attempted.
            file.set_len(8 * 1024 * 1024 * 1024 + 1).unwrap();
        } else {
            file.write_all(&vec![b' '; 1024 * 1024 + 1]).unwrap();
        }
        drop(file);
        let failed = fixture.load().output().unwrap();
        assert_eq!(failed.status.code(), Some(3));
        assert!(String::from_utf8_lossy(&failed.stdout).contains("SourceLimit"));
        assert!(!fixture.home.join("checkpoint").exists());
        assert_eq!(fixture.frontier(), basis);
    }
}

#[test]
fn changing_source_after_a_completed_load_does_not_relabel_checkpoint_identity() {
    let fixture = Fixture::new();
    let mut source = fixture.source();
    success(&fixture.load().output().unwrap());
    let frontier = fixture.frontier();
    let saved = std::fs::read(fixture.home.join("checkpoint")).unwrap();
    source[0] = b'\t'; // still valid, logically identical JSON; raw source identity differs
    std::fs::write(fixture.home.join("input"), source).unwrap();
    let refused = fixture.load().output().unwrap();
    assert_eq!(refused.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&refused.stdout).contains("InvalidResume"));
    assert_eq!(fixture.frontier(), frontier);
    assert_eq!(
        std::fs::read(fixture.home.join("checkpoint")).unwrap(),
        saved
    );
}

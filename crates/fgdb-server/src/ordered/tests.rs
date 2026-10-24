use super::*;
use asupersync::lab::run_async_under_lab;
use fgdb_types::PurposeContexts;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

fn accepted<T>(result: Result<T, Refusal>) -> T {
    result.unwrap_or_else(|error| panic!("{}: {}", error.code.name(), error.message))
}

#[test]
fn scratch_is_anonymous_private_distinct_and_session_capacity_is_refunded() {
    let ((), report) = run_async_under_lab(0x79a0_0601, |root| async move {
        let directory =
            std::env::temp_dir().join(format!("fgdb-order-host-{}", std::process::id()));
        asupersync::fs::create_dir(&directory).await.unwrap();
        let inspected = directory.clone();
        spawn_blocking_io(move || {
            let file = anonymous_file(&inspected, [0x66; 16], 'a')?;
            let metadata = file.metadata()?;
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
            assert_eq!(metadata.nlink(), 0);
            assert_eq!(metadata.len(), 0);
            Ok(())
        })
        .await
        .unwrap();
        let mut config = OrderedReadConfig::new(&directory);
        config.max_concurrent = 1;
        config.memory_bytes = 1024;
        let host = OrderedReads::open(&root, config).await.unwrap();
        let contexts = PurposeContexts::narrow_runtime_root(&root);
        let query = contexts.query();
        let (mut a, mut b, lease) = accepted(host.files(&root, &query).await);
        assert_eq!(host.active.load(Ordering::Acquire), 1);
        assert_eq!(
            host.files(&root, &query).await.err().unwrap().code,
            ErrorCode::Budget
        );
        let run_a = a.append(&query, b"private sort input").await.unwrap();
        let run_b = b.append(&query, b"different final rows").await.unwrap();
        let restored = a.restore(&query, &run_a).await.unwrap();
        assert_eq!(restored.as_ref(), b"private sort input");
        drop(restored);
        assert!(matches!(
            a.restore(&query, &run_b).await,
            Err(fgdb_strata::tiered::memory::SpillError::ForeignRun)
        ));
        assert!(
            asupersync::fs::read_dir(&directory)
                .await
                .unwrap()
                .next_entry()
                .await
                .unwrap()
                .is_none()
        );
        drop((a, b, lease));
        assert_eq!(host.active.load(Ordering::Acquire), 0);
        assert_eq!(host.pool.used(), 0);
        drop(accepted(host.files(&root, &query).await));
        assert_eq!(host.active.load(Ordering::Acquire), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

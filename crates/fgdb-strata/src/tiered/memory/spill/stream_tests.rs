// Included in spill::tests to reuse its transport, failure seams and lab host.

#[test]
fn windows_restore_under_memory_pressure_and_verify_nonzero_file_offsets() {
    let root = MemoryPool::new(128, 0).unwrap();
    let pool = root.child(256, 0).unwrap();
    let mut file = scratch(pool.clone());
    complete(file.append_inner(b"other run", || Ok(()))).unwrap();
    let bytes: Vec<u8> = (0..96).collect();
    let run = complete(file.append_inner(&bytes, || Ok(()))).unwrap();
    assert_eq!(run.offset(), 9);
    let pressure = pool.allocate_inner(112, 0).unwrap();
    assert!(matches!(complete(file.restore_inner(&run, || Ok(()))), Err(SpillError::Memory(_))));
    assert_eq!(file.file.read_calls, 0);
    for (start, len) in [(0, 8), (7, 8), (31, 5), (95, 1), (96, 0)] {
        let result = complete(file.restore_window_inner(&run, start, len, || Ok(()))).unwrap();
        assert_eq!(result.as_ref(), &bytes[start..start + len]);
        assert_eq!(pool.used(), pressure.charged_bytes() + result.charged_bytes());
        assert_eq!(root.used(), pool.used());
        drop(result);
        assert_eq!(pool.used(), pressure.charged_bytes());
    }
    assert_eq!(file.stats().restored_runs, 5);
    drop(pressure);
    assert_eq!(root.used(), 0);
}

#[test]
fn corruption_outside_even_an_empty_window_refuses_without_releasing_bytes() {
    let pool = MemoryPool::new(64, 0).unwrap();
    let mut file = scratch(pool.clone());
    let run = complete(file.append_inner(b"verified window with an unrequested suffix", || Ok(()))).unwrap();
    for index in 0..run.len() {
        file.file.data.get_mut()[index] ^= 1;
        for (start, len) in [(3, 5), (0, 0), (run.len(), 0)] {
            assert!(matches!(
                complete(file.restore_window_inner(&run, start, len, || Ok(()))),
                Err(SpillError::ChecksumMismatch)
            ));
            assert_eq!(pool.used(), 0);
            assert!(!file.is_poisoned());
            assert_eq!(file.stats().restored_runs, 0);
        }
        file.file.data.get_mut()[index] ^= 1;
    }
    let result = complete(file.restore_window_inner(&run, 3, 5, || Ok(()))).unwrap();
    assert_eq!(result.as_ref(), b"ified");
}

#[test]
fn foreign_invalid_and_unaffordable_windows_refuse_before_reads() {
    let pool = MemoryPool::new(16, 0).unwrap();
    let mut file = scratch(pool.clone());
    let run = complete(file.append_inner(b"abcdefgh", || Ok(()))).unwrap();
    let mut foreign = scratch(pool.clone());
    assert!(matches!(
        complete(foreign.restore_window_inner(&run, 0, 1, || Ok(()))),
        Err(SpillError::ForeignRun)
    ));
    for (start, len) in [(9, 0), (8, 1), (usize::MAX, 1), (1, usize::MAX)] {
        assert!(matches!(
            complete(file.restore_window_inner(&run, start, len, || Ok(()))),
            Err(SpillError::InvalidRun)
        ));
    }
    let pressure = pool.allocate_inner(12, 0).unwrap();
    assert!(matches!(
        complete(file.restore_window_inner(&run, 0, 4, || Ok(()))),
        Err(SpillError::Memory(_))
    )); // output fits, but there is no byte left for the checksum scan
    assert_eq!(pool.used(), pressure.charged_bytes());
    assert_eq!((file.file.read_calls, foreign.file.read_calls), (0, 0));
    assert!(!file.is_poisoned());
}

#[test]
fn full_and_empty_run_windows_share_existing_single_allocation_restore() {
    let pool = MemoryPool::new(8, 0).unwrap();
    let mut file = scratch(pool.clone());
    let run = complete(file.append_inner(b"abcdefgh", || Ok(()))).unwrap();
    let full = complete(file.restore_window_inner(&run, 0, 8, || Ok(()))).unwrap();
    assert_eq!(full.as_ref(), b"abcdefgh");
    assert_eq!(pool.used(), full.charged_bytes());
    drop(full);
    let empty = complete(file.append_inner(b"", || Ok(()))).unwrap();
    let result = complete(file.restore_window_inner(&empty, 0, 0, || Ok(()))).unwrap();
    assert!(result.is_empty());
    assert_eq!(pool.used(), 0);
    assert_eq!(file.stats().restored_runs, 2);
}

#[test]
fn every_window_checkpoint_failure_refunds_output_and_scratch() {
    let pool = MemoryPool::new(64, 0).unwrap();
    let mut initial = scratch(pool.clone());
    let bytes: Vec<u8> = (0..64).collect();
    let run = complete(initial.append_inner(&bytes, || Ok(()))).unwrap();
    let pressure = pool.allocate_inner(48, 0).unwrap();
    let mut total = 0;
    let result = complete(initial.restore_window_inner(&run, 10, 8, || {
        total += 1;
        Ok(())
    })).unwrap();
    assert_eq!(result.as_ref(), &bytes[10..18]);
    drop(result);
    assert!(total > 3);
    drop(pressure);
    for stop in 1..=total {
        let mut file = scratch(pool.clone());
        let run = complete(file.append_inner(&bytes, || Ok(()))).unwrap();
        let pressure = pool.allocate_inner(48, 0).unwrap();
        let mut calls = 0;
        assert!(complete(file.restore_window_inner(&run, 10, 8, || {
            calls += 1;
            if calls == stop { Err(SpillError::Io(io::Error::other("injected control refusal"))) }
            else { Ok(()) }
        })).is_err());
        assert_eq!(calls, stop);
        assert_eq!(file.stats().restored_runs, 0);
        assert_eq!(pool.used(), pressure.charged_bytes());
        assert_eq!(file.is_poisoned(), stop > 1 && stop < total);
        drop(pressure);
        assert_eq!(pool.used(), 0);
    }
}

#[test]
fn short_window_source_and_unwind_cannot_leak_allocations_or_reuse_pending_io() {
    let pool = MemoryPool::new(16, 0).unwrap();
    for panic in [false, true] {
        let mut file = scratch(pool.clone());
        let run = complete(file.append_inner(b"abcdefghijklmnop", || Ok(()))).unwrap();
        if panic {
            let mut calls = 0;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                complete(file.restore_window_inner(&run, 1, 2, || {
                    calls += 1;
                    assert_ne!(calls, 2, "injected in-flight unwind");
                    Ok(())
                }))
            }));
            assert!(result.is_err());
        } else {
            file.file.data.get_mut().truncate(15);
            assert!(matches!(
                complete(file.restore_window_inner(&run, 1, 2, || Ok(()))),
                Err(SpillError::Io(_))
            ));
        }
        assert!(file.is_poisoned());
        assert_eq!(pool.used(), 0);
        assert_eq!(file.stats().restored_runs, 0);
        assert!(matches!(complete(file.restore_inner(&run, || Ok(()))), Err(SpillError::PoisonedFile)));
    }
}

#[test]
fn public_window_read_obeys_the_same_hierarchical_budget() {
    under_lab(|cx| async move {
        let root = MemoryPool::new(64, 0).unwrap();
        let pool = root.child(128, 0).unwrap();
        let mut file = SpillFile::new(&cx, TestFile::default(), pool.clone(), limits()).await.unwrap();
        let run = file.append(&cx, &[42; 64]).await.unwrap();
        let pressure = pool.allocate_zeroed(&cx, 48).unwrap();
        let bytes = file.restore_window(&cx, &run, 29, 8).await.unwrap();
        assert_eq!(bytes.as_ref(), &[42; 8]);
        assert_eq!(root.used(), pressure.charged_bytes() + bytes.charged_bytes());
        drop(bytes);
        drop(pressure);
        assert_eq!(root.used(), 0);
    });
}

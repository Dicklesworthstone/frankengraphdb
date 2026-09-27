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
    assert!(matches!(
        complete(file.restore_inner(&run, || Ok(()))),
        Err(SpillError::Memory(_))
    ));
    assert_eq!(file.file.read_calls, 0);
    for (start, len) in [(0, 8), (7, 8), (31, 5), (95, 1), (96, 0)] {
        let result = complete(file.restore_window_inner(&run, start, len, || Ok(()))).unwrap();
        assert_eq!(result.as_ref(), &bytes[start..start + len]);
        assert_eq!(
            pool.used(),
            pressure.charged_bytes() + result.charged_bytes()
        );
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
    let run = complete(file.append_inner(b"verified window with an unrequested suffix", || Ok(())))
        .unwrap();
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
    }))
    .unwrap();
    assert_eq!(result.as_ref(), &bytes[10..18]);
    drop(result);
    assert!(total > 3);
    drop(pressure);
    for stop in 1..=total {
        let mut file = scratch(pool.clone());
        let run = complete(file.append_inner(&bytes, || Ok(()))).unwrap();
        let pressure = pool.allocate_inner(48, 0).unwrap();
        let mut calls = 0;
        assert!(
            complete(file.restore_window_inner(&run, 10, 8, || {
                calls += 1;
                if calls == stop {
                    Err(SpillError::Io(io::Error::other("injected control refusal")))
                } else {
                    Ok(())
                }
            }))
            .is_err()
        );
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
        assert!(matches!(
            complete(file.restore_inner(&run, || Ok(()))),
            Err(SpillError::PoisonedFile)
        ));
    }
}

#[test]
fn public_window_read_obeys_the_same_hierarchical_budget() {
    under_lab(|cx| async move {
        let root = MemoryPool::new(64, 0).unwrap();
        let pool = root.child(128, 0).unwrap();
        let mut file = SpillFile::new(&cx, TestFile::default(), pool.clone(), limits())
            .await
            .unwrap();
        let run = file.append(&cx, &[42; 64]).await.unwrap();
        let pressure = pool.allocate_zeroed(&cx, 48).unwrap();
        let bytes = file.restore_window(&cx, &run, 29, 8).await.unwrap();
        assert_eq!(bytes.as_ref(), &[42; 8]);
        assert_eq!(
            root.used(),
            pressure.charged_bytes() + bytes.charged_bytes()
        );
        drop(bytes);
        drop(pressure);
        assert_eq!(root.used(), 0);
    });
}

fn producer(bytes: &[u8]) -> TestFile {
    TestFile {
        data: Cursor::new(bytes.to_vec()),
        ..TestFile::default()
    }
}

#[test]
fn streamed_runs_exceed_the_pool_and_round_trip_through_verified_windows() {
    let root = MemoryPool::new(16, 0).unwrap();
    let pool = root.child(128, 0).unwrap();
    let mut file = scratch(pool.clone());
    let bytes: Vec<u8> = (0..=255).collect();
    let mut input = producer(&bytes);
    let run = complete(file.append_from_inner(&mut input, bytes.len(), || Ok(()))).unwrap();
    assert_eq!(run.len(), 256);
    assert_eq!(input.data.position(), 256);
    assert_eq!(input.read_calls, 16);
    assert_eq!(root.used(), 0);
    assert_eq!(file.stats().reserved_bytes, 256);
    assert!(matches!(
        complete(file.restore_inner(&run, || Ok(()))),
        Err(SpillError::Memory(_))
    ));
    assert_eq!(file.file.read_calls, 0);
    for start in (0..256).step_by(8) {
        let window = complete(file.restore_window_inner(&run, start, 8, || Ok(()))).unwrap();
        assert_eq!(window.as_ref(), &bytes[start..start + 8]);
        assert_eq!(root.used(), window.charged_bytes());
    }
    assert_eq!(root.used(), 0);
    assert!(!file.is_poisoned());
}

#[test]
fn streamed_checksum_is_chunk_independent_and_input_suffix_is_untouched() {
    for len in [0, 1, 15, 16, 17, IO_CHUNK_BYTES + 3] {
        let bytes: Vec<_> = (0..len + 7).map(|i| (i % 251) as u8).collect();
        let mut input = producer(&bytes);
        let mut small = scratch(MemoryPool::new(31, 0).unwrap());
        let mut large = scratch(MemoryPool::new(len + 31, 0).unwrap());
        complete(small.append_inner(b"prefix", || Ok(()))).unwrap();
        complete(large.append_inner(b"prefix", || Ok(()))).unwrap();
        let streamed = complete(small.append_from_inner(&mut input, len, || Ok(()))).unwrap();
        let borrowed = complete(large.append_inner(&bytes[..len], || Ok(()))).unwrap();
        assert_eq!(streamed.checksum, borrowed.checksum);
        assert_eq!(
            (streamed.id(), streamed.offset(), streamed.len()),
            (borrowed.id(), borrowed.offset(), borrowed.len())
        );
        assert_eq!(small.file.data.get_ref(), large.file.data.get_ref());
        assert_eq!(input.data.position(), len as u64);
        assert_eq!(small.pool.used(), 0);
        let start = len.saturating_sub(5);
        let result =
            complete(small.restore_window_inner(&streamed, start, len - start, || Ok(()))).unwrap();
        assert_eq!(result.as_ref(), &bytes[start..len]);
    }
}

#[test]
fn stream_admission_limits_do_not_consume_input_or_reserve_extents() {
    for case in 0..4 {
        let pool = MemoryPool::new(8, 0).unwrap();
        let mut file = scratch(pool.clone());
        let mut input = producer(&[7; 32]);
        let held = if case == 3 {
            Some(pool.allocate_inner(8, 0).unwrap())
        } else {
            None
        };
        match case {
            0 => file.limits.max_run_bytes = 15,
            1 => file.limits.max_file_bytes = 15,
            2 => file.limits.max_runs = 0,
            _ => {}
        }
        let result = complete(file.append_from_inner(&mut input, 16, || Ok(())));
        match case {
            0 => assert!(matches!(
                result,
                Err(SpillError::RunTooLarge { limit: 15, .. })
            )),
            1 => assert!(matches!(
                result,
                Err(SpillError::FileLimit { available: 15, .. })
            )),
            2 => assert!(matches!(result, Err(SpillError::RunLimit { limit: 0 }))),
            _ => assert!(matches!(result, Err(SpillError::Memory(_)))),
        }
        assert_eq!(file.stats(), SpillStats::default());
        assert_eq!((input.read_calls, input.data.position()), (0, 0));
        assert!(file.file.data.get_ref().is_empty());
        assert!(!file.is_poisoned());
        drop(held);
        assert_eq!(pool.used(), 0);
    }
}

#[test]
fn short_producer_seek_write_and_flush_failures_burn_but_never_publish() {
    for case in 0..4 {
        let pool = MemoryPool::new(8, 0).unwrap();
        let mut file = scratch(pool.clone());
        let mut input = producer(&[9; 24]);
        match case {
            0 => input.data.get_mut().truncate(10),
            1 => file.file.wrong_seek = true,
            2 => file.file.write_limit = Some(3),
            _ => file.file.fail_flush = true,
        }
        let result = complete(file.append_from_inner(&mut input, 24, || Ok(())));
        assert!(result.is_err());
        assert_eq!(
            (file.stats().reserved_runs, file.stats().reserved_bytes),
            (1, 24)
        );
        assert_eq!(file.stats().published_runs, 0);
        assert_eq!(pool.used(), 0);
        assert!(file.is_poisoned());
        let calls = input.read_calls;
        assert!(matches!(
            complete(file.append_from_inner(&mut input, 1, || Ok(()))),
            Err(SpillError::PoisonedFile)
        ));
        assert_eq!(input.read_calls, calls);
    }
}

#[test]
fn every_stream_control_cut_preserves_extent_and_buffer_ownership() {
    let pool = MemoryPool::new(8, 0).unwrap();
    let mut file = scratch(pool.clone());
    let mut input = producer(&[1; 24]);
    let mut total = 0;
    complete(file.append_from_inner(&mut input, 24, || {
        total += 1;
        Ok(())
    }))
    .unwrap();
    assert!(total > 6);
    for stop in 1..=total {
        let mut file = scratch(pool.clone());
        let mut input = producer(&[1; 24]);
        let mut calls = 0;
        assert!(
            complete(file.append_from_inner(&mut input, 24, || {
                calls += 1;
                if calls == stop {
                    Err(SpillError::Io(io::Error::other("injected control refusal")))
                } else {
                    Ok(())
                }
            }))
            .is_err()
        );
        assert_eq!(calls, stop);
        assert_eq!(pool.used(), 0);
        assert_eq!(file.stats().published_runs, 0);
        assert_eq!(file.stats().reserved_runs, u64::from(stop != 1));
        assert_eq!(file.stats().reserved_bytes, if stop == 1 { 0 } else { 24 });
        assert_eq!(file.is_poisoned(), stop > 1 && stop < total);
        if stop == total {
            // Completed but not accepted extent stays burned, without a fence
            // on subsequent I/O. The next run must begin AFTER those bytes.
            let next = complete(file.append_from_inner(&mut producer(b"z"), 1, || Ok(()))).unwrap();
            assert_eq!(next.offset(), 24);
            assert_eq!(next.id(), 2);
        }
    }
}

#[test]
fn dropping_pending_stream_producer_or_destination_refunds_and_fences() {
    struct PendingInput;
    impl AsyncRead for PendingInput {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Pending
        }
    }
    let pool = MemoryPool::new(8, 0).unwrap();
    let mut file = scratch(pool.clone());
    {
        let mut input = PendingInput;
        let future = file.append_from_inner(&mut input, 24, || Ok(()));
        let mut future = std::pin::pin!(future);
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    }
    assert_eq!(pool.used(), 0);
    assert!(file.is_poisoned());
    assert_eq!(
        (file.stats().reserved_bytes, file.stats().published_runs),
        (24, 0)
    );
    let mut file = scratch(pool.clone());
    file.file.pending_write = true;
    {
        let mut input = producer(&[0; 24]);
        let future = file.append_from_inner(&mut input, 24, || Ok(()));
        let mut future = std::pin::pin!(future);
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    }
    assert_eq!(pool.used(), 0);
    assert!(file.is_poisoned());
    assert_eq!(
        (file.stats().reserved_bytes, file.stats().published_runs),
        (24, 0)
    );
}

#[test]
fn public_stream_and_empty_run_use_exact_ceilings_without_resident_run_allocation() {
    under_lab(|cx| async move {
        let root = MemoryPool::new(16, 0).unwrap();
        let pool = root.child(128, 0).unwrap();
        let limits = SpillLimits {
            max_file_bytes: 256,
            max_runs: 2,
            max_run_bytes: 256,
        };
        let mut file = SpillFile::new(&cx, TestFile::default(), pool.clone(), limits)
            .await
            .unwrap();
        let mut input = producer(&[43; 256]);
        let run = file.append_from(&cx, &mut input, 256).await.unwrap();
        let bytes = file.restore_window(&cx, &run, 125, 8).await.unwrap();
        assert_eq!(bytes.as_ref(), &[43; 8]);
        drop(bytes);
        assert_eq!(root.used(), 0);
        let pressure = pool.allocate_zeroed(&cx, 16).unwrap();
        let empty = file.append_from(&cx, &mut input, 0).await.unwrap();
        assert!(empty.is_empty());
        assert!(
            file.restore_window(&cx, &empty, 0, 0)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(input.data.position(), 256);
        assert!(matches!(
            file.append_from(&cx, &mut input, 0).await,
            Err(SpillError::RunLimit { .. })
        ));
        assert_eq!(root.used(), pressure.charged_bytes());
        drop(pressure);
        assert_eq!(root.used(), 0);
    });
}

#[test]
fn dropping_a_pending_window_read_refunds_both_buffers_and_fences_the_file() {
    struct PausingFile {
        inner: TestFile,
        reads: usize,
    }
    impl AsyncSeek for PausingFile {
        fn poll_seek(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            pos: SeekFrom,
        ) -> Poll<io::Result<u64>> {
            Pin::new(&mut self.inner).poll_seek(cx, pos)
        }
    }
    impl AsyncWrite for PausingFile {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, bytes)
        }
        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }
        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }
    impl AsyncRead for PausingFile {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            out: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.reads == 0 {
                return Poll::Pending;
            }
            self.reads -= 1;
            Pin::new(&mut self.inner).poll_read(cx, out)
        }
    }
    let pool = MemoryPool::new(24, 0).unwrap();
    let mut file = complete(SpillFile::new_inner(
        PausingFile {
            inner: TestFile::default(),
            reads: 1,
        },
        pool.clone(),
        limits(),
        || Ok(()),
    ))
    .unwrap();
    let run = complete(file.append_from_inner(&mut producer(&[31; 64]), 64, || Ok(()))).unwrap();
    {
        let future = file.restore_window_inner(&run, 1, 8, || Ok(()));
        let mut future = std::pin::pin!(future);
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(pool.used(), 24); // output plus the reusable read buffer
    }
    assert_eq!(pool.used(), 0);
    assert!(file.is_poisoned());
    assert_eq!(file.stats().restored_runs, 0);
}

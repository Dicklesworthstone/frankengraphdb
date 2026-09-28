use super::*;
use asupersync::io::ReadBuf;
use std::future::Future;
use std::io::{Cursor, Seek};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

#[derive(Default)]
struct File {
    data: Cursor<Vec<u8>>,
    read_bytes: usize,
    read_calls: usize,
    write_limit: Option<usize>,
    pending_read: bool,
    pending_write: bool,
    pending_seek: bool,
    wrong_seek: bool,
    fail_flush: bool,
}
impl AsyncRead for File {
    fn poll_read(mut self: Pin<&mut Self>, _: &mut Context<'_>, out: &mut ReadBuf<'_>)
        -> Poll<io::Result<()>>
    {
        if self.pending_read { return Poll::Pending; }
        let at = self.data.position() as usize;
        let count = out.remaining().min(self.data.get_ref().len().saturating_sub(at));
        if count != 0 {
            out.put_slice(&self.data.get_ref()[at..at + count]);
            self.data.set_position((at + count) as u64);
        }
        self.read_calls += 1;
        self.read_bytes += count;
        Poll::Ready(Ok(()))
    }
}
impl AsyncWrite for File {
    fn poll_write(mut self: Pin<&mut Self>, _: &mut Context<'_>, bytes: &[u8])
        -> Poll<io::Result<usize>>
    {
        if self.pending_write { return Poll::Pending; }
        let count = self.write_limit.unwrap_or(bytes.len()).min(bytes.len());
        if count == 0 && !bytes.is_empty() { return Poll::Ready(Err(io::Error::other("write cut"))); }
        let result = std::io::Write::write(&mut self.data, &bytes[..count]);
        if let Some(left) = &mut self.write_limit { *left -= count; }
        Poll::Ready(result)
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(if self.fail_flush { Err(io::Error::other("flush cut")) } else { Ok(()) })
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
impl AsyncSeek for File {
    fn poll_seek(mut self: Pin<&mut Self>, _: &mut Context<'_>, position: SeekFrom)
        -> Poll<io::Result<u64>>
    {
        if self.pending_seek { return Poll::Pending; }
        let result = self.data.seek(position);
        Poll::Ready(if self.wrong_seek { result.map(|at| at + 1) } else { result })
    }
}
fn complete<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    match future.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("unexpected suspension"),
    }
}
fn file(pool: MemoryPool) -> SpillFile<File> {
    complete(SpillFile::new_inner(File::default(), pool, SpillLimits {
        max_file_bytes: 4_000_000, max_runs: 100, max_run_bytes: 1_000_000,
    }, || Ok(()))).unwrap()
}
fn append(file: &mut SpillFile<File>, bytes: &[u8], size: usize, chunk: usize) -> PagedSpillRun {
    let mut writer = file.paged_writer_inner(size).unwrap();
    for bytes in bytes.chunks(chunk) { complete(writer.write_inner(bytes, &mut || Ok(()))).unwrap(); }
    complete(writer.finish_inner(&mut || Ok(()))).unwrap()
}
fn read(file: &mut SpillFile<File>, run: &PagedSpillRun) -> Vec<u8> {
    let mut bytes = Vec::new();
    for page in 0..run.page_count() {
        let part = complete(file.restore_page_inner(run, page, || Ok(()))).unwrap();
        bytes.extend_from_slice(part.as_ref());
    }
    bytes
}

#[test]
fn paged_runs_exceed_resident_memory_and_need_only_a_logarithmic_proof_per_read() {
    let pool = MemoryPool::new(WRITER_METADATA + 1024, 0).unwrap();
    let mut scratch = file(pool.clone());
    let input: Vec<_> = (0..320_017).map(|i| (i % 251) as u8).collect();
    let run = append(&mut scratch, &input, 1024, 137);
    assert!(run.len() > pool.limit());
    assert_eq!(pool.used(), 0);
    assert_eq!(scratch.stats.reserved_bytes, input.len() as u64 + (run.page_count() - 1) * 128);
    assert_eq!(scratch.stats.published_runs, 1);
    for page in (0..run.page_count()).rev() {
        let before = scratch.file.read_bytes;
        let output = complete(scratch.restore_page_inner(&run, page, || Ok(()))).unwrap();
        let start = page as usize * run.page_bytes();
        assert_eq!(output.as_ref(), &input[start..start + output.len()]);
        let height = run.page_count().next_power_of_two().ilog2() as usize;
        assert!(scratch.file.read_bytes - before <= output.len() + height * BRANCH_BYTES);
        assert_eq!(pool.used(), output.charged_bytes());
        drop(output);
        assert_eq!(pool.used(), 0);
    }
}

#[test]
fn corruption_before_finish_cannot_be_blessed_by_rereading_untrusted_metadata() {
    let pool = MemoryPool::new(WRITER_METADATA + 16, 0).unwrap();
    let mut scratch = file(pool.clone());
    let mut writer = scratch.paged_writer_inner(8).unwrap();
    complete(writer.write_inner(b"abcdefghijklmnop", &mut || Ok(()))).unwrap();
    // Two leaves and their branch already exist. The retained root must come
    // from original writes, not from this now-corrupted physical record.
    writer.scratch.file.data.get_mut()[0] ^= 1;
    let run = complete(writer.finish_inner(&mut || Ok(()))).unwrap();
    assert_eq!(scratch.file.read_calls, 0);
    assert!(matches!(complete(scratch.restore_page_inner(&run, 0, || Ok(()))),
        Err(SpillError::ChecksumMismatch)));
    assert_eq!(complete(scratch.restore_page_inner(&run, 1, || Ok(()))).unwrap().as_ref(), b"ijklmnop");
    assert_eq!(pool.used(), 0);
}

#[test]
fn all_small_tree_shapes_partial_pages_and_fragmentations_preserve_bytes() {
    for page_bytes in [1, 3, 16, 63] {
        for len in 0..=page_bytes * 17 {
            let input: Vec<_> = (0..len).map(|i| (i % 239) as u8).collect();
            let mut a = file(MemoryPool::new(32_768, 0).unwrap());
            let mut b = file(MemoryPool::new(32_768, 0).unwrap());
            let ra = append(&mut a, &input, page_bytes, 1);
            let rb = append(&mut b, &input, page_bytes, 71);
            assert_eq!(read(&mut a, &ra), input);
            assert_eq!(read(&mut b, &rb), input);
            assert_eq!(a.file.data.get_ref(), b.file.data.get_ref());
            assert_eq!(ra.root.map(|r| r.digest), rb.root.map(|r| r.digest));
            assert_eq!(ra.page_count(), len.div_ceil(page_bytes) as u64);
            assert_eq!(ra.is_empty(), len == 0);
        }
    }
}

#[test]
fn every_stored_payload_and_branch_byte_is_detected_by_complete_consumption() {
    let pool = MemoryPool::new(32_768, 0).unwrap();
    let mut scratch = file(pool.clone());
    complete(scratch.append_inner(b"unrelated prior run", || Ok(()))).unwrap();
    let run = append(&mut scratch, b"abcdefghijklmnopqrstuvwxyz0123456789", 7, 5);
    for byte in run.start as usize..run.end as usize {
        scratch.file.data.get_mut()[byte] ^= 1;
        let mut refused = false;
        for page in 0..run.page_count() {
            match complete(scratch.restore_page_inner(&run, page, || Ok(()))) {
                Ok(bytes) => drop(bytes),
                Err(SpillError::ChecksumMismatch) => { refused = true; break; }
                other => panic!("unexpected corruption result: {other:?}"),
            }
        }
        assert!(refused, "unprotected byte {byte}");
        assert!(!scratch.is_poisoned());
        assert_eq!(pool.used(), 0);
        scratch.file.data.get_mut()[byte] ^= 1;
    }
    assert_eq!(read(&mut scratch, &run), b"abcdefghijklmnopqrstuvwxyz0123456789");
}

#[test]
fn page_authentication_is_not_mislabeled_as_unread_payload_verification() {
    let mut scratch = file(MemoryPool::new(32_768, 0).unwrap());
    let run = append(&mut scratch, b"first__second", 7, 100);
    // Two leaves precede their parent. Corrupt only the second payload, not
    // the authenticated descriptor the first page uses as its sibling proof.
    scratch.file.data.get_mut()[7] ^= 1;
    let first = complete(scratch.restore_page_inner(&run, 0, || Ok(()))).unwrap();
    assert_eq!(first.as_ref(), b"first__");
    assert!(matches!(complete(scratch.restore_page_inner(&run, 1, || Ok(()))),
        Err(SpillError::ChecksumMismatch)));
}

#[test]
fn foreign_empty_out_of_range_and_memory_refusals_do_no_io() {
    let pool = MemoryPool::new(32_768, 0).unwrap();
    let mut a = file(pool.clone());
    let mut b = file(pool.clone());
    let run = append(&mut a, b"abcdef", 3, 7);
    assert!(matches!(complete(b.restore_page_inner(&run, 0, || Ok(()))), Err(SpillError::ForeignRun)));
    for page in [run.page_count(), u64::MAX] {
        assert!(matches!(complete(a.restore_page_inner(&run, page, || Ok(()))), Err(SpillError::InvalidRun)));
    }
    let empty = append(&mut a, b"", 3, 7);
    assert!(matches!(complete(a.restore_page_inner(&empty, 0, || Ok(()))), Err(SpillError::InvalidRun)));
    let occupied = pool.allocate_inner(pool.available(), 0).unwrap();
    assert!(matches!(complete(a.restore_page_inner(&run, 0, || Ok(()))), Err(SpillError::Memory(_))));
    assert_eq!((a.file.read_calls, b.file.read_calls), (0, 0));
    assert!(!a.is_poisoned() && !b.is_poisoned());
    drop(occupied);
    assert_eq!(read(&mut a, &run), b"abcdef");
}

#[test]
fn full_quotas_include_branch_records_and_unaccepted_attempts() {
    let mut scratch = file(MemoryPool::new(32_768, 0).unwrap());
    scratch.limits.max_file_bytes = 5 + BRANCH_BYTES as u64;
    scratch.limits.max_runs = 1;
    let run = append(&mut scratch, b"abcde", 3, 100);
    assert_eq!(scratch.stats.reserved_bytes, scratch.limits.max_file_bytes);
    assert_eq!(read(&mut scratch, &run), b"abcde");
    assert!(matches!(scratch.paged_writer_inner(3), Err(SpillError::RunLimit { limit: 1 })));
    let mut scratch = file(MemoryPool::new(32_768, 0).unwrap());
    scratch.limits.max_file_bytes = 5 + BRANCH_BYTES as u64 - 1;
    let mut writer = scratch.paged_writer_inner(3).unwrap();
    complete(writer.write_inner(b"abcde", &mut || Ok(()))).unwrap();
    assert!(matches!(complete(writer.finish_inner(&mut || Ok(()))), Err(SpillError::FileLimit { .. })));
    assert_eq!(scratch.stats.published_runs, 0);
    assert!(scratch.is_poisoned());
    assert_eq!(scratch.pool.used(), 0);
    let mut scratch = file(MemoryPool::new(32_768, 0).unwrap());
    scratch.limits.max_run_bytes = 4;
    let mut writer = scratch.paged_writer_inner(3).unwrap();
    assert!(matches!(complete(writer.write_inner(b"abcde", &mut || Ok(()))), Err(SpillError::RunTooLarge { .. })));
    assert!(matches!(complete(writer.finish_inner(&mut || Ok(()))), Err(SpillError::PoisonedFile)));
    assert_eq!(scratch.stats.reserved_bytes, 0);
    assert_eq!(scratch.stats.reserved_runs, 1);
    assert!(scratch.is_poisoned());
}

#[test]
fn every_write_finish_and_read_checkpoint_refuses_without_leaking_a_prefix_or_charge() {
    fn execute(stop: usize) -> (bool, usize, SpillStats, bool, usize) {
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let mut scratch = file(pool.clone());
        let mut calls = 0;
        let mut gate = || {
            calls += 1;
            if calls == stop { Err(SpillError::Io(io::Error::other("control cut"))) } else { Ok(()) }
        };
        let result = complete(async {
            let mut writer = scratch.paged_writer_inner(4)?;
            writer.write_inner(b"a partial final page", &mut gate).await?;
            writer.finish_inner(&mut gate).await
        });
        (result.is_ok(), calls, scratch.stats(), scratch.is_poisoned(), pool.used())
    }
    let (_, count, _, _, _) = execute(usize::MAX);
    for stop in 1..=count {
        let (ok, calls, stats, poisoned, used) = execute(stop);
        assert!(!ok);
        assert_eq!(calls, stop);
        assert_eq!(stats.published_runs, 0);
        assert_eq!(poisoned, stop != count); // final checkpoint is after quiescent flush
        assert_eq!(used, 0);
    }
    let pool = MemoryPool::new(32_768, 0).unwrap();
    let mut scratch = file(pool.clone());
    let run = append(&mut scratch, b"a partial final page", 4, 9);
    let mut count = 0;
    drop(complete(scratch.restore_page_inner(&run, 4, || { count += 1; Ok(()) })).unwrap());
    for stop in 1..=count {
        let mut scratch = file(pool.clone());
        let run = append(&mut scratch, b"a partial final page", 4, 9);
        let mut calls = 0;
        assert!(complete(scratch.restore_page_inner(&run, 4, || {
            calls += 1;
            if calls == stop { Err(SpillError::Io(io::Error::other("read cut"))) } else { Ok(()) }
        })).is_err());
        assert_eq!(calls, stop);
        assert_eq!(scratch.stats.restored_runs, 0);
        assert_eq!(scratch.is_poisoned(), stop != 1 && stop != count);
        assert_eq!(pool.used(), 0);
    }
}

#[test]
fn dropped_pending_io_and_abandoned_buffered_writers_fence_the_file() {
    for stage in 0..3 {
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let mut scratch = file(pool.clone());
        scratch.file.pending_seek = stage == 0;
        scratch.file.pending_write = stage == 1;
        let mut writer = scratch.paged_writer_inner(8).unwrap();
        if stage == 2 {
            complete(writer.write_inner(b"buffer", &mut || Ok(()))).unwrap();
        } else {
            let mut gate = || Ok(());
            let mut future = std::pin::pin!(writer.write_inner(b"one full page", &mut gate));
            assert!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        }
        drop(writer);
        assert!(scratch.is_poisoned());
        assert_eq!(scratch.stats.published_runs, 0);
        assert_eq!(pool.used(), 0);
    }
    let pool = MemoryPool::new(32_768, 0).unwrap();
    let mut scratch = file(pool.clone());
    let run = append(&mut scratch, b"verified data", 8, 99);
    scratch.file.pending_read = true;
    {
        let mut future = std::pin::pin!(scratch.restore_page_inner(&run, 0, || Ok(())));
        assert!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
    }
    assert!(scratch.is_poisoned());
    assert_eq!(pool.used(), 0);
}

#[test]
fn actual_io_failures_and_unwind_never_issue_a_run_handle() {
    for case in 0..4 {
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let mut scratch = file(pool.clone());
        scratch.file.write_limit = (case == 0).then_some(2);
        scratch.file.fail_flush = case == 1;
        scratch.file.wrong_seek = case == 2;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            complete(async {
                let mut writer = scratch.paged_writer_inner(4)?;
                writer.write_inner(b"not a successful prefix", &mut || Ok(())).await?;
                writer.finish_inner(&mut || {
                    assert_ne!(case, 3, "injected finish unwind");
                    Ok(())
                }).await
            })
        }));
        if case == 3 { assert!(outcome.is_err()); } else { assert!(outcome.unwrap().is_err()); }
        assert!(scratch.is_poisoned());
        assert_eq!(scratch.stats.published_runs, 0);
        assert_eq!(pool.used(), 0);
    }
}

#[test]
fn public_query_context_path_composes_with_existing_run_formats() {
    let ((), report) = asupersync::lab::run_async_under_lab(0x5b11_9001, |root| async move {
        let contexts = fgdb_types::PurposeContexts::narrow_runtime_root(&root);
        let cx = contexts.query();
        let pool = MemoryPool::new(32_768, 0).unwrap();
        let mut scratch = SpillFile::new(&cx, File::default(), pool.clone(), SpillLimits {
            max_file_bytes: 1_000_000, max_runs: 10, max_run_bytes: 200_000,
        }).await.unwrap();
        let old = scratch.append(&cx, b"old format").await.unwrap();
        let mut writer = scratch.paged_writer(&cx, 512).unwrap();
        for _ in 0..1000 { writer.write(&cx, b"new format fragment").await.unwrap(); }
        assert_eq!(writer.len(), 19_000);
        let run = writer.finish(&cx).await.unwrap();
        let later = scratch.append(&cx, b"later format").await.unwrap();
        let mut bytes = Vec::new();
        for page in 0..run.page_count() {
            let output = scratch.restore_page(&cx, &run, page).await.unwrap();
            bytes.extend_from_slice(output.as_ref());
        }
        assert_eq!(bytes, b"new format fragment".repeat(1000));
        assert_eq!(scratch.restore(&cx, &old).await.unwrap().as_ref(), b"old format");
        assert_eq!(scratch.restore(&cx, &later).await.unwrap().as_ref(), b"later format");
        assert_eq!(pool.used(), 0);
    });
    assert!(report.lab_test_passed(), "{report:?}");
}

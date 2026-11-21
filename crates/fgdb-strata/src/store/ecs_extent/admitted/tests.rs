use super::*;
use asupersync::fs::{Metadata, Permissions, UnixVfs, UnixVfsFile, Vfs};
use asupersync::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use asupersync::{Budget, runtime::RuntimeBuilder};
use fgdb_types::PurposeContexts;
use std::future::{Future, poll_fn};
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};

static NEXT: AtomicU64 = AtomicU64::new(0);
const NS: DatabaseSecurityNamespaceId = DatabaseSecurityNamespaceId([0x81; 32]);
const ID: ObjectId = ObjectId([0x82; 32]);

fn fixture(bytes: &[u8]) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "fgdb-admitted-extent-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
    ));
    std::fs::write(&path, bytes).unwrap();
    path
}

fn geometry(config: &StrataEcsConfig, kind: ExtentObjectKind, bytes: &[u8]) -> ExtentGeometry {
    let coords = coord_transcript(config, NS, kind, ID);
    let key = object_subkey("fgdb:strata-ecs:hdr-key:v1", config, &coords);
    ExtentGeometry::compute(
        &verify_header_bytes(&bytes[..HEADER_BYTES], kind, config.epoch, &key, false).unwrap(),
    )
    .unwrap()
}

// Like the pack reader's HeldFile, but park at one selected seek offset so
// header admission can finish before the frame or repair I/O becomes pending.
struct HeldRead {
    inner: UnixVfsFile,
    offset: u64,
    park: bool,
    reached: Arc<AtomicBool>,
}

impl AsyncRead for HeldRead {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.park {
            self.reached.store(true, Ordering::SeqCst);
            Poll::Pending
        } else {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }
}

impl AsyncWrite for HeldRead {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl AsyncSeek for HeldRead {
    fn poll_seek(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        pos: SeekFrom,
    ) -> Poll<io::Result<u64>> {
        match Pin::new(&mut self.inner).poll_seek(cx, pos) {
            Poll::Ready(Ok(offset)) => {
                self.park = offset == self.offset;
                Poll::Ready(Ok(offset))
            }
            other => other,
        }
    }
}

impl VfsFile for HeldRead {
    async fn metadata(&self) -> io::Result<Metadata> {
        self.inner.metadata().await
    }
    async fn sync_all(&self) -> io::Result<()> {
        self.inner.sync_all().await
    }
    async fn sync_data(&self) -> io::Result<()> {
        self.inner.sync_data().await
    }
    async fn set_len(&self, size: u64) -> io::Result<()> {
        self.inner.set_len(size).await
    }
    async fn set_permissions(&self, perm: Permissions) -> io::Result<()> {
        self.inner.set_permissions(perm).await
    }
}

#[test]
fn dropping_pending_frame_or_repair_reads_refunds_query_and_ancestor_charges() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = PurposeContexts::narrow_runtime_root(&root).query();
    let config = StrataEcsConfig::new([0x87; 32]);
    let plain = vec![0x51; 2 * 16_384 + 17];
    let encoded = encode_extent(&config, NS, ExtentObjectKind::AdjRun, ID, &plain).unwrap();
    let geom = geometry(&config, ExtentObjectKind::AdjRun, &encoded);
    let path = fixture(&encoded);
    let vfs = UnixVfs::new();
    let ancestor = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
    let pool = ancestor.child(8 * 1024 * 1024, 0).unwrap();
    let baseline_charge = pool.reserve(&cx, 1337).unwrap();
    let baseline = pool.used();

    for repair in [false, true] {
        let offset = if repair {
            let mut damaged = encoded.clone();
            damaged[geom.frame_offset(0) + FRAME_DIGEST_BYTES + 3] ^= 0xff;
            std::fs::write(&path, damaged).unwrap();
            geom.stripe_trailer_span(0).0
        } else {
            geom.frame_offset(0)
        };
        let reached = Arc::new(AtomicBool::new(false));
        let file = HeldRead {
            inner: runtime.block_on(vfs.open_read(&path)).unwrap(),
            offset: offset as u64,
            park: false,
            reached: Arc::clone(&reached),
        };
        let mut pending = Box::pin(read_admitted_object(
            &cx,
            file,
            &config,
            NS,
            ExtentObjectKind::AdjRun,
            ID,
            plain.len() as u64,
            &pool,
        ));
        runtime.block_on(poll_fn(|task| {
            assert!(pending.as_mut().poll(task).is_pending());
            if reached.load(Ordering::SeqCst) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }));
        assert!(pool.used() > baseline, "the pending read owns live scratch");
        if repair {
            assert!(pool.used() >= baseline + repair_bytes(&geom, 0).unwrap());
        }
        assert_eq!(ancestor.used(), pool.used());
        drop(pending);
        assert_eq!(
            pool.used(),
            baseline,
            "drop refunds this read, not its caller's charge"
        );
        assert_eq!(ancestor.used(), baseline);
    }

    // The same hierarchy remains usable after both cancellations.
    runtime.block_on(async {
        let file = vfs.open_read(&path).await.unwrap();
        assert_eq!(
            read_admitted_object(
                &cx,
                file,
                &config,
                NS,
                ExtentObjectKind::AdjRun,
                ID,
                plain.len() as u64,
                &pool,
            )
            .await
            .unwrap(),
            plain
        );
    });
    assert_eq!(pool.used(), baseline);
    drop(baseline_charge);
    assert_eq!(pool.used(), 0);
    assert_eq!(ancestor.used(), 0);
}

#[test]
fn admitted_ranges_read_only_touched_frames_and_charge_repair_before_decoding() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = PurposeContexts::narrow_runtime_root(&root).query();
    let config = StrataEcsConfig::new([0x83; 32]);
    let plain: Vec<u8> = (0..(6 * 16_384 + 13)).map(|i| (i % 251) as u8).collect();
    let mut encoded = encode_extent(&config, NS, ExtentObjectKind::Pack, ID, &plain).unwrap();
    let geom = geometry(&config, ExtentObjectKind::Pack, &encoded);
    // An untouched frame is corrupt. Healthy range admission cannot read the
    // whole pack or spend the repair decoder's larger workspace.
    encoded[geom.frame_offset(5) + FRAME_DIGEST_BYTES + 3] ^= 0xff;
    let path = fixture(&encoded);
    let vfs = UnixVfs::new();
    let small = MemoryPool::new(80 * 1024, 0).unwrap();
    runtime.block_on(async {
        let file = vfs.open_read(&path).await.unwrap();
        assert_eq!(
            read_admitted_range(
                &cx,
                file,
                &config,
                NS,
                ExtentObjectKind::Pack,
                ID,
                7,
                19,
                &small
            )
            .await
            .unwrap(),
            plain[7..26],
        );
    });
    assert_eq!(small.used(), 0);

    encoded[geom.frame_offset(0) + FRAME_DIGEST_BYTES + 3] ^= 0xff;
    std::fs::write(&path, &encoded).unwrap();
    runtime.block_on(async {
        let file = vfs.open_read(&path).await.unwrap();
        let error = read_admitted_range(
            &cx,
            file,
            &config,
            NS,
            ExtentObjectKind::Pack,
            ID,
            7,
            19,
            &small,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            Ok(ExtentError::Memory(MemoryError::ResourceExhausted { .. }))
        ));
    });
    assert_eq!(
        small.used(),
        0,
        "failed repair refunds frame and header capacity"
    );

    let large = MemoryPool::new(16 * 1024 * 1024, 0).unwrap();
    runtime.block_on(async {
        let file = vfs.open_read(&path).await.unwrap();
        assert_eq!(
            read_admitted_range(
                &cx,
                file,
                &config,
                NS,
                ExtentObjectKind::Pack,
                ID,
                7,
                19,
                &large
            )
            .await
            .unwrap(),
            plain[7..26],
        );
        let file = vfs.open_read(&path).await.unwrap();
        assert_eq!(
            read_admitted_object(
                &cx,
                file,
                &config,
                NS,
                ExtentObjectKind::Pack,
                ID,
                plain.len() as u64,
                &large,
            )
            .await
            .unwrap(),
            plain,
        );
    });
    assert_eq!(
        large.used(),
        0,
        "successful repair retains no decoder scratch"
    );
}

#[test]
fn admitted_objects_use_authenticated_geometry_and_refuse_false_merkle_roots() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = PurposeContexts::narrow_runtime_root(&root).query();
    let reader = StrataEcsConfig::new([0x84; 32]);
    let mut writer = reader.clone().with_frame_size(ExtentFrameSize::KiB64);
    writer.stripe_frames = 8;
    writer.stripe_k = 2;
    writer.stripe_r = 2;
    let plain = vec![0x42; 80 * 1024];
    let mut encoded = encode_extent(&writer, NS, ExtentObjectKind::AdjRun, ID, &plain).unwrap();
    let path = fixture(&encoded);
    let vfs = UnixVfs::new();
    let small = MemoryPool::new(192 * 1024, 0).unwrap();
    let large = MemoryPool::new(32 * 1024 * 1024, 0).unwrap();
    runtime.block_on(async {
        let file = vfs.open_read(&path).await.unwrap();
        assert!(matches!(
            read_admitted_object(
                &cx,
                file,
                &reader,
                NS,
                ExtentObjectKind::AdjRun,
                ID,
                plain.len() as u64,
                &small,
            )
            .await,
            Err(Ok(ExtentError::Memory(
                MemoryError::ResourceExhausted { .. }
            ))),
        ));
        let file = vfs.open_read(&path).await.unwrap();
        assert_eq!(
            read_admitted_object(
                &cx,
                file,
                &reader,
                NS,
                ExtentObjectKind::AdjRun,
                ID,
                plain.len() as u64,
                &large,
            )
            .await
            .unwrap(),
            plain,
        );
        let file = vfs.open_read(&path).await.unwrap();
        assert!(matches!(
            read_admitted_object(
                &cx,
                file,
                &reader,
                NS,
                ExtentObjectKind::AdjRun,
                ID,
                5,
                &large
            )
            .await,
            Err(Ok(ExtentError::PlaintextTooLarge { limit: 5, .. })),
        ));
    });
    assert_eq!(small.used(), 0);
    assert_eq!(large.used(), 0);

    // Frames remain independently authentic, but full-object readers must also
    // verify the authenticated header's commitment to all frame digests.
    let coords = coord_transcript(&reader, NS, ExtentObjectKind::AdjRun, ID);
    let key = object_subkey("fgdb:strata-ecs:hdr-key:v1", &reader, &coords);
    let mut header = verify_header_bytes(
        &encoded[..HEADER_BYTES],
        ExtentObjectKind::AdjRun,
        reader.epoch,
        &key,
        false,
    )
    .unwrap();
    header.merkle_root[0] ^= 0xff;
    encoded[..HEADER_BYTES].copy_from_slice(&encode_header_bytes(
        &header,
        ExtentObjectKind::AdjRun,
        &key,
        false,
    ));
    std::fs::write(&path, &encoded).unwrap();
    runtime.block_on(async {
        let file = vfs.open_read(&path).await.unwrap();
        assert!(matches!(
            read_admitted_object(
                &cx,
                file,
                &reader,
                NS,
                ExtentObjectKind::AdjRun,
                ID,
                plain.len() as u64,
                &large,
            )
            .await,
            Err(Ok(ExtentError::MerkleMismatch)),
        ));
    });
    assert_eq!(large.used(), 0);
}

#[test]
fn admitted_empty_objects_still_authenticate_their_frame_and_header() {
    let runtime = RuntimeBuilder::new().build().unwrap();
    let root = runtime.request_cx_with_budget(Budget::INFINITE);
    let cx = PurposeContexts::narrow_runtime_root(&root).query();
    let config = StrataEcsConfig::new([0x85; 32]);
    let encoded = encode_extent(&config, NS, ExtentObjectKind::Block, ID, &[]).unwrap();
    let path = fixture(&encoded);
    let vfs = UnixVfs::new();
    let pool = MemoryPool::new(1024 * 1024, 0).unwrap();
    runtime.block_on(async {
        let file = vfs.open_read(&path).await.unwrap();
        assert!(
            read_admitted_object(
                &cx,
                file,
                &config,
                NS,
                ExtentObjectKind::Block,
                ID,
                0,
                &pool,
            )
            .await
            .unwrap()
            .is_empty()
        );
        let file = vfs.open_read(&path).await.unwrap();
        assert!(matches!(
            read_admitted_object(
                &cx,
                file,
                &StrataEcsConfig::new([0x86; 32]),
                NS,
                ExtentObjectKind::Block,
                ID,
                0,
                &pool,
            )
            .await,
            Err(Ok(ExtentError::AuthenticationFailed))
        ));
    });
    assert_eq!(pool.used(), 0);
}

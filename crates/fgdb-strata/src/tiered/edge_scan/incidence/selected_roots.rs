//! A selected vertex domain for the ordinary globally ordered edge root.
//! Descriptor routing selects complete histories; compact row positions retain
//! the selection after the caller releases its bounded identity batch.
//! Initial admission proves that an EId's source, relation and target never
//! change across versions, so endpoint membership cannot change the winner.

use super::*;

impl<V: Vfs> BufferedEdgeJoinScan<'_, V> {
    /// Restrict an unstarted edge root to edges incident to a sorted, distinct
    /// vertex domain in the requested direction. The root's relation selector
    /// remains in force. Nested incidence and vertex reads stay independent.
    ///
    /// Initialization routes through admitted descriptors and decodes topology
    /// only. It retains one EId merge head and a bounded position permutation
    /// per matching block, never a neighborhood bag or the caller's identities.
    /// Later root pulls emit each selected historical EId exactly once, in
    /// increasing order, before resolving its complete history and endpoints.
    /// Undirected edges matching both ends still occupy one root position.
    ///
    /// The caller may release the identity batch as soon as this returns. All
    /// work and memory admission uses this driver's existing cut and controls.
    /// A repeated selection, unsorted/duplicate domain, source error, or drop of
    /// a polled pending initialization closes the entire driver. No partial
    /// position can resume. An already closed driver remains inert.
    pub async fn select_root_vertices_with<C: Send>(
        &mut self,
        cx: &QueryCx,
        vertices: &[VId],
        direction: BufferedEdgeDirection,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<(), BufferedScanError<C>> {
        let Some(mut driver) = self.driver.take() else {
            return Ok(());
        };
        self.continuation = None;
        let used = &mut self.work;
        let result = driver
            .select_roots(cx, vertices, direction, &mut |event| {
                observe(event)?;
                if event == BufferedEdgeScanEvent::Work {
                    *used = used.saturating_add(1);
                }
                Ok(())
            })
            .await;
        self.work = driver.root.work.used;
        if result.is_ok() {
            self.driver = Some(driver);
        }
        result
    }
}

impl<V: Vfs> Driver<'_, V> {
    async fn select_roots<C: Send>(
        &mut self,
        cx: &QueryCx,
        vertices: &[VId],
        direction: BufferedEdgeDirection,
        observe: &mut (impl FnMut(BufferedEdgeScanEvent) -> Result<(), C> + Send),
    ) -> Result<(), BufferedScanError<C>> {
        self.root.work.step(cx, observe)?;
        let state = self.root.state.take().ok_or_else(invalid_load)?;
        if state.initialized {
            return Err(invalid_load().into());
        }
        drop(state);
        for pair in vertices.windows(2) {
            self.root.work.step(cx, observe)?;
            if pair[0] >= pair[1] {
                return Err(invalid_load().into());
            }
        }
        if vertices.is_empty() {
            return Ok(());
        }

        // One domain cursor yields each selected descriptor exactly once,
        // including overlaps across incoming ranges and undirected sides.
        // Count first with fixed-size scratch, then replay the immutable
        // directory. No bitmap or candidate list scales with the corpus.
        let mut blocks =
            self.root
                .partition
                .edge_scan_root_blocks(vertices, self.root.relation, direction);
        let _scratch = self
            .root
            .partition
            .reserve_scan_bytes(cx, size_of_val(&blocks))?;
        let mut capacity = 0usize;
        while let Some(block) = self.root.partition.edge_scan_next_root_block(
            &mut blocks,
            cx,
            &mut self.root.work,
            observe,
        )? {
            self.root.work.step(cx, observe)?;
            if block >= self.root.blocks {
                return Err(invalid_load().into());
            }
            capacity = capacity
                .checked_add(1)
                .ok_or(BufferedReadError::SizeOverflow)?;
        }
        if capacity == 0 {
            return Ok(());
        }

        // Admit the full backing once through the cache-aware reservation;
        // incremental charge growth would not reclaim unpinned extent frames.
        let bytes = capacity
            .checked_mul(512)
            .and_then(|bytes| bytes.checked_add(1024))
            .ok_or(BufferedReadError::SizeOverflow)?;
        self.root.work.step(cx, observe)?;
        let charge = self.root.partition.reserve_scan_bytes(cx, bytes)?;
        let mut state = State {
            heads: Vec::new(),
            orders: Vec::new(),
            order_bytes: 0,
            retain_orders: true,
            active: None,
            initialized: true,
            incidence: None,
            _head_charge: Some(charge),
        };
        state.heads.try_reserve_exact(capacity).map_err(|_| {
            BufferedReadError::Memory(MemoryError::AllocationFailed { requested: bytes })
        })?;
        state.orders.try_reserve_exact(capacity).map_err(|_| {
            BufferedReadError::Memory(MemoryError::AllocationFailed { requested: bytes })
        })?;
        state.check_retained_bytes()?;
        blocks = self
            .root
            .partition
            .edge_scan_root_blocks(vertices, self.root.relation, direction);
        let mut visited = 0usize;
        while let Some(block) = self.root.partition.edge_scan_next_root_block(
            &mut blocks,
            cx,
            &mut self.root.work,
            observe,
        )? {
            self.root.work.step(cx, observe)?;
            if visited == capacity || block >= self.root.blocks {
                return Err(invalid_load().into());
            }
            visited += 1;
            self.root
                .load(
                    &mut state,
                    cx,
                    block,
                    None,
                    EdgePropertyAccess::Skip,
                    observe,
                )
                .await?;
            let active = state.active.as_mut().ok_or_else(invalid_load)?;
            let mut keep = 0;
            for at in 0..active.order.len() {
                self.root.work.step(cx, observe)?;
                let key = active.order[at];
                let entry = active.image.0.get(key.2).ok_or_else(invalid_load)?;
                let mut control = || self.root.work.step(cx, observe);
                let matches = self.root.relation.is_none_or(|r| r == entry.relation)
                    && match direction {
                        BufferedEdgeDirection::Outgoing => {
                            contains(vertices, entry.src, &mut control)?
                        }
                        BufferedEdgeDirection::Incoming => {
                            contains(vertices, entry.dst, &mut control)?
                        }
                        BufferedEdgeDirection::Undirected => {
                            contains(vertices, entry.src, &mut control)?
                                || contains(vertices, entry.dst, &mut control)?
                        }
                    };
                if matches {
                    active.order[keep] = key;
                    keep += 1;
                }
            }
            active.order.truncate(keep);
            let Some(&(eid, created, _)) = active.order.first() else {
                continue;
            };
            let slot = state.orders.len();
            // Unlike ordinary incidence memoization, even a one/two-row
            // block MUST retain its filtered positions: the borrowed root
            // domain is gone before any later full-payload refault.
            let available = bytes
                .checked_sub(
                    state
                        .retained_bytes()
                        .ok_or(BufferedReadError::SizeOverflow)?,
                )
                .ok_or(BufferedReadError::SizeOverflow)?;
            let active = state.active.as_ref().ok_or_else(invalid_load)?;
            let positions = compact_positions(&active.order, available, &mut || {
                self.root.work.step(cx, observe)
            })?;
            state.order_bytes = state
                .order_bytes
                .checked_add(positions.capacity())
                .ok_or(BufferedReadError::SizeOverflow)?;
            state.orders.push(Some(positions));
            state.check_retained_bytes()?;
            heap_push(
                &mut state.heads,
                Reverse(Head {
                    eid,
                    created,
                    block,
                    ordinal: 0,
                    slot,
                }),
                &mut || self.root.work.step(cx, observe),
            )?;
        }
        if visited != capacity {
            return Err(invalid_load().into());
        }
        // Do not carry a topology decoder or the domain cursor across the
        // first candidate admission. Only selected positions and heads survive.
        state.active = None;
        if !state.heads.is_empty() {
            self.root.state = Some(state);
        }
        Ok(())
    }
}

fn contains<C>(
    vertices: &[VId],
    vertex: VId,
    control: &mut impl FnMut() -> Result<(), BufferedScanError<C>>,
) -> Result<bool, BufferedScanError<C>> {
    let mut start = 0;
    let mut end = vertices.len();
    while start < end {
        control()?;
        let middle = start + (end - start) / 2;
        match vertices[middle].cmp(&vertex) {
            core::cmp::Ordering::Less => start = middle + 1,
            core::cmp::Ordering::Equal => return Ok(true),
            core::cmp::Ordering::Greater => end = middle,
        }
    }
    Ok(false)
}

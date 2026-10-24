//! Separate completed record ownership from format-wide decode scratch.
//! All decoders and replacement clones still acquire their original workspace
//! BEFORE allocation. Only completed, immutable images reach this module.

use super::*;

fn add(left: usize, right: usize) -> Result<usize, BufferedReadError> {
    left.checked_add(right)
        .ok_or(BufferedReadError::SizeOverflow)
}

fn vector_bytes<T>(values: &Vec<T>) -> Result<usize, BufferedReadError> {
    values
        .capacity()
        .checked_mul(size_of::<T>())
        .ok_or(BufferedReadError::SizeOverflow)
}

// Value owners measure actual capacities at every depth, including collection
// vectors, map keys, text sort keys and spare byte capacity. Encoded length and
// collection length are not bounds on their retained heap allocations.
fn properties<C>(
    values: &EdgePropertyRow,
    control: &mut impl FnMut() -> Result<(), BufferedScanError<C>>,
) -> Result<usize, BufferedScanError<C>> {
    control()?;
    let mut bytes = vector_bytes(values)?;
    for (_, value) in values {
        bytes = add(
            bytes,
            value
                .owned_heap_bytes_controlled(control)?
                .ok_or(BufferedReadError::SizeOverflow)?,
        )?;
    }
    Ok(bytes)
}

// The affine split transfers admission without allocating or reacquiring it.
// Keep value and charge in one owner while measuring and on every refusal.
// No bytes are refunded until ALL measurement succeeds and the new charge
// covers the complete surviving owner. An underestimated workspace refuses;
// it never silently publishes an under-accounted allocation.
fn retain<T>(
    mut value: BufferedValue<T>,
    bytes: usize,
) -> Result<BufferedValue<T>, BufferedReadError> {
    let retained = value
        ._charge
        .split_off(bytes)
        .ok_or(BufferedReadError::SizeOverflow)?;
    value._charge = retained;
    Ok(value)
}

pub(super) fn block<C>(
    image: BufferedValue<BufferedBlockImage>,
    control: &mut impl FnMut() -> Result<(), BufferedScanError<C>>,
) -> Result<BufferedValue<BufferedBlockImage>, BufferedScanError<C>> {
    control()?;
    let mut bytes = add(size_of::<BufferedBlockImage>(), vector_bytes(&image.0)?)?;
    if let Some(props) = &image.1 {
        bytes = add(bytes, vector_bytes(&props.locators)?)?;
        bytes = add(bytes, vector_bytes(&props.rows)?)?;
        for row in &props.rows {
            bytes = add(bytes, properties(row, control)?)?;
        }
    }
    control()?;
    Ok(retain(image, bytes)?)
}

impl BufferedValue<VertexRow> {
    /// Keep only the completed vertex's owned capacity after all historical
    /// replacements and decoder temporaries have finished.
    pub(crate) fn retain_scan_vertex<C>(
        self,
        control: &mut impl FnMut() -> Result<(), BufferedScanError<C>>,
    ) -> Result<Self, BufferedScanError<C>> {
        control()?;
        let bytes = add(
            add(size_of::<VertexRow>(), vector_bytes(&self.labels)?)?,
            properties(&self.props, control)?,
        )?;
        control()?;
        Ok(retain(self, bytes)?)
    }
}

impl BufferedValue<BufferedEdge> {
    /// Called after resolving the complete edge history and before retaining
    /// it through endpoint reads or another nested traversal. In-flight winner
    /// replacement still has the original conservative workspace reservation.
    pub(crate) fn retain_scan_edge<C>(
        self,
        control: &mut impl FnMut() -> Result<(), BufferedScanError<C>>,
    ) -> Result<Self, BufferedScanError<C>> {
        control()?;
        let bytes = add(size_of::<BufferedEdge>(), properties(&self.props, control)?)?;
        control()?;
        Ok(retain(self, bytes)?)
    }
}

#[cfg(test)]
mod tests;

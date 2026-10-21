//! Separate completed scalar-record ownership from format-wide decode scratch.
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

// Scalar owners expose actual heap capacities, including text sort keys and
// spare byte capacity. Collection owners do not expose their Vec capacities.
// Never substitute length or encoded length for those unknown allocations:
// retain the preexisting full workspace for any image containing a collection.
fn properties<C>(
    values: &EdgePropertyRow,
    control: &mut impl FnMut() -> Result<(), BufferedScanError<C>>,
) -> Result<Option<usize>, BufferedScanError<C>> {
    control()?;
    let mut bytes = vector_bytes(values)?;
    for (_, value) in values {
        control()?;
        let Some(scalar) = value.as_scalar() else {
            return Ok(None);
        };
        bytes = add(
            bytes,
            scalar
                .owned_heap_bytes()
                .ok_or(BufferedReadError::SizeOverflow)?,
        )?;
    }
    Ok(Some(bytes))
}

// The affine split transfers admission without allocating or reacquiring it.
// Keep value and charge in one owner while measuring and on every refusal.
// No bytes are refunded until ALL measurement succeeds and the new charge
// covers the complete surviving owner. An underestimated workspace refuses;
// it never silently publishes an under-accounted allocation.
fn retain<T>(
    mut value: BufferedValue<T>,
    bytes: Option<usize>,
) -> Result<BufferedValue<T>, BufferedReadError> {
    if let Some(bytes) = bytes {
        let retained = value
            ._charge
            .split_off(bytes)
            .ok_or(BufferedReadError::SizeOverflow)?;
        value._charge = retained;
    }
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
            let Some(row_bytes) = properties(row, control)? else {
                return Ok(image);
            };
            bytes = add(bytes, row_bytes)?;
        }
    }
    control()?;
    Ok(retain(image, Some(bytes))?)
}

pub(super) fn vertex<C>(
    row: BufferedValue<VertexRow>,
    control: &mut impl FnMut() -> Result<(), BufferedScanError<C>>,
) -> Result<BufferedValue<VertexRow>, BufferedScanError<C>> {
    control()?;
    let bytes = properties(&row.props, control)?
        .map(|props| {
            add(
                add(size_of::<VertexRow>(), vector_bytes(&row.labels)?)?,
                props,
            )
        })
        .transpose()?;
    control()?;
    Ok(retain(row, bytes)?)
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
        let bytes = properties(&self.props, control)?
            .map(|props| add(size_of::<BufferedEdge>(), props))
            .transpose()?;
        control()?;
        Ok(retain(self, bytes)?)
    }
}

#[cfg(test)]
mod tests;

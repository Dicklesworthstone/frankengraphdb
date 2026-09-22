//! Derived sealed-image framing. This format has no durable object-kind ID.
//! The opaque source anchor authenticates bytes before this parser is exposed.
//! All multibyte integers are LE; payload lengths are exact, not skip hints.

use super::super::inline::{INLINE_CAPACITY, InlineAdjacency, MAX_INLINE_PAYLOAD_BYTES};
use super::image::{Image, Row, Run, Storage, identity_limits, reserved};
use super::{SealedError, SealedLimits, check_limit};
use crate::edge_props::{self, MAX_PROPERTY_PATCH_ROWS};
use crate::{DescriptorKey, Direction, VisibilityInterval};
use fgdb_codec::ef_payload::{self, EfPayloadLimits};
use fgdb_codec::elias_fano::EliasFano;
use fgdb_codec::identity::{ElementIdentity, IdentityColumn, IdentityColumnDescriptor};
use fgdb_delta_types::RelationId;
use fgdb_types::{CanonicalScalarResolver, CommitSeq, EId, VId};

const MAGIC: &[u8; 4] = b"FGSI";
const VERSION: u16 = 1;

struct Output {
    bytes: Vec<u8>,
    limit: usize,
}

impl Output {
    fn put(&mut self, bytes: &[u8]) -> Result<(), SealedError> {
        let wanted = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or(SealedError::SizeOverflow)?;
        check_limit("image bytes", wanted, self.limit)?;
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|_| SealedError::AllocationFailed)?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn count(&mut self, count: usize) -> Result<(), SealedError> {
        self.put(
            &u32::try_from(count)
                .map_err(|_| SealedError::SizeOverflow)?
                .to_le_bytes(),
        )
    }

    fn identity<T: ElementIdentity>(
        &mut self,
        column: &IdentityColumn<T>,
    ) -> Result<(), SealedError> {
        let descriptor = column.descriptor();
        let payload = column
            .try_scalar_payload(self.limit)
            .map_err(SealedError::Identity)?;
        self.put(&[crate::representation_tag(descriptor.representation())])?;
        self.count(descriptor.prefixes())?;
        self.count(payload.len())?;
        self.put(&payload)
    }

    fn ef(&mut self, column: &EliasFano) -> Result<(), SealedError> {
        let payload = ef_payload::encode(
            column,
            EfPayloadLimits {
                max_entries: column.len(),
                max_bytes: self.limit,
            },
        )
        .map_err(SealedError::Payload)?;
        self.put(&column.max_value().unwrap_or(0).to_le_bytes())?;
        self.count(payload.len())?;
        self.put(&payload)
    }
}

pub(super) fn encode(
    image: &Image,
    limits: SealedLimits,
    checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
) -> Result<Vec<u8>, SealedError> {
    checkpoint()?;
    check_limit("descriptors", image.rows.len(), limits.max_rows)?;
    check_limit("incidences", image.incidences, limits.max_incidences)?;
    let mut output = Output {
        bytes: Vec::new(),
        limit: limits.max_image_bytes,
    };
    output.put(MAGIC)?;
    output.put(&VERSION.to_le_bytes())?;
    output.count(image.rows.len())?;
    output.count(image.incidences)?;
    output.count(image.dictionary.len())?;
    output.count(image.properties.len())?;
    output.identity(&image.dictionary)?;
    output.ef(&image.offsets)?;
    for row in &image.rows {
        checkpoint()?;
        match row.storage {
            Storage::Inline(index) => {
                let mut payload = [0; MAX_INLINE_PAYLOAD_BYTES];
                let len = image.inlines[index]
                    .encode_into(&mut payload)
                    .map_err(SealedError::Inline)?;
                output.put(&[0])?;
                output.count(len)?;
                output.put(&payload[..len])?;
            }
            Storage::Run(index) => {
                let run = &image.runs[index];
                output.put(&[1])?;
                output.put(&row.key.src.0.to_le_bytes())?;
                output.put(&row.key.relation.0.to_le_bytes())?;
                output.put(&[row.key.direction as u8])?;
                output.ef(&run.neighbors)?;
                output.identity(&run.edge_ids)?;
                output.count(run.spans.len())?;
                for span in &run.spans {
                    output.put(&span.end_row.to_le_bytes())?;
                    output.put(&span.created_at.0.to_le_bytes())?;
                    output.put(&span.retired_at.map_or(0, |seq| seq.0).to_le_bytes())?;
                }
                for (at, locator) in run.locators.iter().enumerate() {
                    if at % 256 == 0 {
                        checkpoint()?;
                    }
                    output.put(&locator.to_le_bytes())?;
                }
            }
        }
    }
    let mut property_bytes = 0usize;
    for chunk in image.properties.chunks(MAX_PROPERTY_PATCH_ROWS as usize) {
        checkpoint()?;
        // One existing, canonical FGSP patch per chunk. Never introduce a
        // second scalar encoder or exceed its 255-row format ceiling.
        let payload = edge_props::encode_property_patch(chunk).map_err(SealedError::Property)?;
        property_bytes = property_bytes
            .checked_add(
                payload
                    .len()
                    .checked_sub(10)
                    .ok_or(SealedError::NonCanonical)?,
            )
            .ok_or(SealedError::SizeOverflow)?;
        check_limit("property bytes", property_bytes, limits.max_property_bytes)?;
        output.count(payload.len())?;
        output.put(&payload)?;
    }
    checkpoint()?;
    Ok(output.bytes)
}

struct Input<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Input<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], SealedError> {
        let end = self
            .at
            .checked_add(count)
            .filter(|&end| end <= self.bytes.len())
            .ok_or(SealedError::Truncated)?;
        let bytes = &self.bytes[self.at..end];
        self.at = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8, SealedError> {
        Ok(self.take(1)?[0])
    }

    fn count(&mut self) -> Result<usize, SealedError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("bounded count")) as usize)
    }

    fn u64(&mut self) -> Result<u64, SealedError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("bounded u64"),
        ))
    }

    fn identity<T: ElementIdentity>(
        &mut self,
        count: usize,
        limits: SealedLimits,
    ) -> Result<IdentityColumn<T>, SealedError> {
        let representation =
            crate::representation_from_tag(self.byte()?).ok_or(SealedError::NonCanonical)?;
        let prefixes = self.count()?;
        let length = self.count()?;
        let payload = self.take(length)?;
        IdentityColumn::try_from_scalar_payload(
            payload,
            IdentityColumnDescriptor::new(representation, count, prefixes),
            identity_limits(count, limits),
        )
        .map_err(SealedError::Identity)
    }

    fn ef(
        &mut self,
        count: usize,
        maximum_bound: u64,
        limits: SealedLimits,
    ) -> Result<EliasFano, SealedError> {
        let maximum = self.u64()?;
        if maximum > maximum_bound {
            return Err(SealedError::NonCanonical);
        }
        let length = self.count()?;
        ef_payload::decode(
            self.take(length)?,
            count,
            maximum,
            EfPayloadLimits {
                max_entries: count,
                max_bytes: limits.max_image_bytes,
            },
        )
        .map_err(SealedError::Payload)
    }
}

pub(super) fn decode(
    bytes: &[u8],
    limits: SealedLimits,
    resolver: Option<&dyn CanonicalScalarResolver>,
    checkpoint: &mut impl FnMut() -> Result<(), SealedError>,
) -> Result<Image, SealedError> {
    checkpoint()?;
    check_limit("image bytes", bytes.len(), limits.max_image_bytes)?;
    let mut input = Input { bytes, at: 0 };
    if input.take(4)? != MAGIC
        || u16::from_le_bytes(input.take(2)?.try_into().expect("bounded version")) != VERSION
    {
        return Err(SealedError::InvalidFormat);
    }
    let row_count = input.count()?;
    let incidence_count = input.count()?;
    let destination_count = input.count()?;
    let property_count = input.count()?;
    check_limit("descriptors", row_count, limits.max_rows)?;
    check_limit("incidences", incidence_count, limits.max_incidences)?;
    if row_count > incidence_count
        || destination_count > incidence_count
        || property_count > incidence_count
    {
        return Err(SealedError::NonCanonical);
    }
    let dictionary = input.identity::<VId>(destination_count, limits)?;
    let offsets = input.ef(
        row_count.checked_add(1).ok_or(SealedError::SizeOverflow)?,
        incidence_count as u64,
        limits,
    )?;
    if offsets.select(0) != Some(0) || offsets.select(row_count) != Some(incidence_count as u64) {
        return Err(SealedError::NonCanonical);
    }
    let mut rows = reserved(row_count)?;
    let mut inlines = Vec::new();
    let mut runs = Vec::new();
    for row_index in 0..row_count {
        checkpoint()?;
        let start = offsets.select(row_index).ok_or(SealedError::NonCanonical)?;
        let end = offsets
            .select(row_index + 1)
            .ok_or(SealedError::NonCanonical)?;
        let count = end
            .checked_sub(start)
            .and_then(|count| usize::try_from(count).ok())
            .filter(|&count| count != 0)
            .ok_or(SealedError::NonCanonical)?;
        match input.byte()? {
            0 => {
                if count > INLINE_CAPACITY {
                    return Err(SealedError::NonCanonical);
                }
                let length = input.count()?;
                let inline =
                    InlineAdjacency::decode(input.take(length)?).map_err(SealedError::Inline)?;
                if inline.len() != count {
                    return Err(SealedError::NonCanonical);
                }
                let key = inline.descriptor();
                let index = inlines.len();
                inlines
                    .try_reserve(1)
                    .map_err(|_| SealedError::AllocationFailed)?;
                inlines.push(inline);
                rows.push(Row {
                    key,
                    count,
                    storage: Storage::Inline(index),
                });
            }
            1 => {
                if count <= INLINE_CAPACITY {
                    return Err(SealedError::NonCanonical);
                }
                let src = VId(u128::from_le_bytes(
                    input.take(16)?.try_into().expect("bounded source"),
                ));
                let relation = RelationId(input.u64()?);
                if input.byte()? != 0 {
                    return Err(SealedError::NonCanonical);
                }
                let key = DescriptorKey {
                    src,
                    relation,
                    direction: Direction::Outbound,
                };
                let neighbors =
                    input.ef(count, destination_count.saturating_sub(1) as u64, limits)?;
                let edge_ids = input.identity::<EId>(count, limits)?;
                let span_count = input.count()?;
                if span_count == 0 || span_count > count {
                    return Err(SealedError::NonCanonical);
                }
                // Prove the minimum remaining framing before allocating arrays.
                let minimum = span_count
                    .checked_mul(20)
                    .and_then(|n| count.checked_mul(4).and_then(|m| n.checked_add(m)))
                    .ok_or(SealedError::SizeOverflow)?;
                if minimum > input.bytes.len() - input.at {
                    return Err(SealedError::Truncated);
                }
                let mut spans = reserved(span_count)?;
                let mut start_row = 0;
                for _ in 0..span_count {
                    let end_row =
                        u32::try_from(input.count()?).map_err(|_| SealedError::SizeOverflow)?;
                    let created_at = CommitSeq(input.u64()?);
                    let retired = input.u64()?;
                    spans.push(VisibilityInterval {
                        start_row,
                        end_row,
                        created_at,
                        retired_at: (retired != 0).then_some(CommitSeq(retired)),
                    });
                    start_row = end_row;
                }
                crate::validate_spans(&spans, count).map_err(SealedError::Entry)?;
                let mut locators = reserved(count)?;
                for at in 0..count {
                    if at % 256 == 0 {
                        checkpoint()?;
                    }
                    locators.push(
                        u32::try_from(input.count()?).map_err(|_| SealedError::SizeOverflow)?,
                    );
                }
                let index = runs.len();
                runs.try_reserve(1)
                    .map_err(|_| SealedError::AllocationFailed)?;
                runs.push(Run {
                    neighbors,
                    edge_ids,
                    spans,
                    locators,
                });
                rows.push(Row {
                    key,
                    count,
                    storage: Storage::Run(index),
                });
            }
            _ => return Err(SealedError::NonCanonical),
        }
    }
    let mut properties = reserved(property_count)?;
    let mut property_bytes = 0usize;
    while properties.len() < property_count {
        checkpoint()?;
        let expected = (property_count - properties.len()).min(MAX_PROPERTY_PATCH_ROWS as usize);
        let length = input.count()?;
        property_bytes = property_bytes
            .checked_add(length.checked_sub(10).ok_or(SealedError::NonCanonical)?)
            .ok_or(SealedError::SizeOverflow)?;
        check_limit("property bytes", property_bytes, limits.max_property_bytes)?;
        let payload = input.take(length)?;
        let chunk = match resolver {
            Some(resolver) => edge_props::decode_property_patch_with_resolver(payload, resolver),
            None => edge_props::decode_property_patch(payload),
        }
        .map_err(SealedError::Property)?;
        if chunk.len() != expected {
            return Err(SealedError::NonCanonical);
        }
        properties.extend(chunk);
    }
    if input.at != bytes.len() {
        return Err(SealedError::TrailingBytes);
    }
    checkpoint()?;
    Ok(Image {
        dictionary,
        offsets,
        rows,
        inlines,
        runs,
        properties,
        incidences: incidence_count,
    })
}

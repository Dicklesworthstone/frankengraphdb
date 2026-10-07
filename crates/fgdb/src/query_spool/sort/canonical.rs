//! Borrowed comparison of frames produced by GraphValueRow::canonical_bytes.
//! Only the private native-spool path calls this after page authentication.
//! Scalar bytes keep fgdb-types' memcomparable ordering; we neither decode
//! scalars nor admit arbitrary external scalar claims. This is NOT a public
//! deserializer/authentication boundary. Structural lengths are still checked.

use super::*;
use fgdb_gql::algebra::GraphValue;

pub(super) const ROW: &[u8] = b"fgdb:graph-row:v1\0";
const VALUE: &[u8] = b"fgdb:graph-value:v1\0";

fn invalid() -> NativeSpoolError {
    SpillError::InvalidRun.into()
}

fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Result<&'a [u8]> {
    let value = bytes.get(..len).ok_or_else(invalid)?;
    *bytes = &bytes[len..];
    Ok(value)
}
fn count(bytes: &mut &[u8]) -> Result<usize> {
    let mut word = [0; 8];
    word.copy_from_slice(take(bytes, 8)?);
    usize::try_from(u64::from_be_bytes(word)).map_err(|_| invalid())
}
fn frame<'a>(bytes: &mut &'a [u8]) -> Result<&'a [u8]> {
    let len = count(bytes)?;
    take(bytes, len)
}
fn row(bytes: &[u8], columns: usize) -> Result<&[u8]> {
    let mut bytes = bytes.strip_prefix(ROW).ok_or_else(invalid)?;
    if count(&mut bytes)? != columns || columns > MAX_PATTERN_VERTICES {
        return Err(invalid());
    }
    Ok(bytes)
}
fn root<'a>(bytes: &mut &'a [u8]) -> Result<Cell<'a>> {
    let mut value = frame(bytes)?.strip_prefix(VALUE).ok_or_else(invalid)?;
    let cell = Cell::next(&mut value)?;
    if !value.is_empty() {
        return Err(invalid());
    }
    Ok(cell)
}

#[derive(Clone, Copy)]
struct Cell<'a> {
    tag: u8,
    body: &'a [u8],
}
impl<'a> Cell<'a> {
    fn next(bytes: &mut &'a [u8]) -> Result<Self> {
        let (tag, body) = frame(bytes)?.split_first().ok_or_else(invalid)?;
        if *tag > 6 {
            return Err(invalid());
        }
        Ok(Self { tag: *tag, body })
    }
    fn scalar(self) -> Result<&'a [u8]> {
        let mut body = self.body;
        let scalar = frame(&mut body)?;
        if !body.is_empty()
            || scalar.is_empty()
            || scalar[0] > 7
            || (scalar[0] == 0 && scalar.len() != 1)
        {
            return Err(invalid());
        }
        Ok(scalar)
    }
    fn is_null(self) -> Result<bool> {
        Ok(self.tag == 0 && self.scalar()? == [0])
    }
    fn validate(self, depth: usize, nodes: &mut usize, work: &mut Work<'_>) -> Result<()> {
        work.charge(1)?;
        if depth > GraphValue::MAX_LIST_DEPTH || *nodes == 0 {
            return Err(invalid());
        }
        *nodes -= 1;
        let mut body = self.body;
        match self.tag {
            0 => {
                self.scalar()?;
            }
            1 | 5 => {
                if body.len() != 16 {
                    return Err(invalid());
                }
            }
            2 => {
                take(&mut body, 16)?;
                let steps = count(&mut body)?;
                if steps.checked_mul(32) != Some(body.len()) {
                    return Err(invalid());
                }
            }
            3 | 4 => {
                let len = count(&mut body)?;
                if len.checked_mul(16) != Some(body.len()) {
                    return Err(invalid());
                }
            }
            6 => {
                let len = count(&mut body)?;
                if len > *nodes {
                    return Err(invalid());
                }
                for _ in 0..len {
                    Self::next(&mut body)?.validate(depth + 1, nodes, work)?;
                }
                if !body.is_empty() {
                    return Err(invalid());
                }
            }
            _ => return Err(invalid()),
        }
        Ok(())
    }
}

pub(super) fn validate(bytes: &[u8], columns: usize, work: &mut Work<'_>) -> Result<()> {
    work.charge(1)?;
    let mut bytes = row(bytes, columns)?;
    for _ in 0..columns {
        let mut nodes = GraphValue::MAX_LIST_NODES;
        root(&mut bytes)?.validate(0, &mut nodes, work)?;
    }
    if !bytes.is_empty() {
        return Err(invalid());
    }
    Ok(())
}

// Return the original framed cells, borrowing the authenticated input row.
// Validate the COMPLETE evaluation row first, including every hidden cell and
// its nested frames. A discarded tail must never turn malformed input into a
// successful visible row. Only the domain/count header changes on output.
pub(super) fn visible_prefix<'a>(
    bytes: &'a [u8],
    columns: usize,
    visible: usize,
    work: &mut Work<'_>,
) -> Result<&'a [u8]> {
    work.charge(1)?;
    if visible == 0 || visible > columns {
        return Err(invalid());
    }
    validate(bytes, columns, work)?;
    let frames = row(bytes, columns)?;
    let mut remaining = frames;
    for _ in 0..visible {
        work.charge(1)?;
        frame(&mut remaining)?;
    }
    let prefix = frames.len() - remaining.len();
    Ok(&frames[..prefix])
}

// Bounded chunks admit comparison work before inspecting variable payloads.
fn lex(left: &[u8], right: &[u8], work: &mut Work<'_>) -> Result<Ordering> {
    let common = left.len().min(right.len());
    for (a, b) in left[..common]
        .chunks(1024)
        .zip(right[..common].chunks(1024))
    {
        work.charge(a.len())?;
        let cmp = a.cmp(b);
        if cmp != Ordering::Equal {
            return Ok(cmp);
        }
    }
    work.charge(1)?;
    Ok(left.len().cmp(&right.len()))
}
fn compare_cells(a: Cell<'_>, b: Cell<'_>, depth: usize, work: &mut Work<'_>) -> Result<Ordering> {
    work.charge(1)?;
    if depth > GraphValue::MAX_LIST_DEPTH {
        return Err(invalid());
    }
    if a.tag != b.tag {
        return Ok(a.tag.cmp(&b.tag));
    }
    let (mut left, mut right) = (a.body, b.body);
    match a.tag {
        0 => lex(a.scalar()?, b.scalar()?, work),
        1 | 5 => lex(left, right, work),
        2 => {
            let result = lex(take(&mut left, 16)?, take(&mut right, 16)?, work)?;
            if result != Ordering::Equal {
                return Ok(result);
            }
            count(&mut left)?;
            count(&mut right)?;
            lex(left, right, work)
        }
        3 | 4 => {
            count(&mut left)?;
            count(&mut right)?;
            lex(left, right, work)
        }
        6 => {
            let a_len = count(&mut left)?;
            let b_len = count(&mut right)?;
            for _ in 0..a_len.min(b_len) {
                let cmp = compare_cells(
                    Cell::next(&mut left)?,
                    Cell::next(&mut right)?,
                    depth + 1,
                    work,
                )?;
                if cmp != Ordering::Equal {
                    return Ok(cmp);
                }
            }
            Ok(a_len.cmp(&b_len))
        }
        _ => Err(invalid()),
    }
}
fn column<'a>(
    bytes: &'a [u8],
    width: usize,
    column: usize,
    work: &mut Work<'_>,
) -> Result<Cell<'a>> {
    let mut bytes = row(bytes, width)?;
    for at in 0..width {
        work.charge(1)?;
        let cell = root(&mut bytes)?;
        if at == column {
            return Ok(cell);
        }
    }
    Err(invalid())
}

pub(super) fn compare(
    a: &[u8],
    b: &[u8],
    order: &[GraphValueOrder],
    columns: usize,
    work: &mut Work<'_>,
) -> Result<Ordering> {
    for key in order {
        let a = column(a, columns, key.column, work)?;
        let b = column(b, columns, key.column, work)?;
        let cmp = match (a.is_null()?, b.is_null()?) {
            (true, false) => {
                if key.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if key.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            _ => {
                let value = compare_cells(a, b, 0, work)?;
                if key.descending {
                    value.reverse()
                } else {
                    value
                }
            }
        };
        if cmp != Ordering::Equal {
            return Ok(cmp);
        }
    }
    // GLA's tie-break is the entire TYPED row, not the framed byte transcript.
    let (mut left, mut right) = (row(a, columns)?, row(b, columns)?);
    for _ in 0..columns {
        let cmp = compare_cells(root(&mut left)?, root(&mut right)?, 0, work)?;
        if cmp != Ordering::Equal {
            return Ok(cmp);
        }
    }
    Ok(Ordering::Equal)
}

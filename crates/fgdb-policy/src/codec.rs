//! Canonical application-level caveat encoding. This is not an Appendix A
//! durable object codec and does not allocate a registered ObjectKind.

use super::*;

fn check_set<T: Ord>(items: &[T]) -> Result<(), PolicyError> {
    if items.len() > MAX_SCOPE_ITEMS { return Err(PolicyError::Limit); }
    if items.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(PolicyError::NonCanonical);
    }
    Ok(())
}

fn put_set<T: Ord>(out: &mut Vec<u8>, items: &[T], mut put: impl FnMut(&mut Vec<u8>, &T)) -> Result<(), PolicyError> {
    check_set(items)?;
    out.extend_from_slice(&(items.len() as u16).to_le_bytes());
    for item in items { put(out, item); }
    Ok(())
}

fn put_predicate(out: &mut Vec<u8>, predicate: &PropertyPredicate) -> Result<(), PolicyError> {
    if predicate.value.canonical_encoded_len().map_err(|_| PolicyError::InvalidScalar)? > MAX_POLICY_SCALAR_BYTES {
        return Err(PolicyError::Limit);
    }
    if matches!(predicate.value, CanonicalScalar::Null) { return Err(PolicyError::InvalidScalar); }
    let scalar = predicate.value.encode().map_err(|_| PolicyError::InvalidScalar)?;
    // Artifact-bound values require a separately pinned resolver language arm;
    // never reinterpret them using the host's locale or timezone database.
    if CanonicalScalar::decode(&scalar).map_err(|_| PolicyError::Unsupported)? != predicate.value {
        return Err(PolicyError::NonCanonical);
    }
    out.extend_from_slice(&predicate.key.0.to_le_bytes());
    out.push(match predicate.comparison {
        Comparison::Equal => 0, Comparison::NotEqual => 1,
        Comparison::Less => 2, Comparison::LessOrEqual => 3,
        Comparison::Greater => 4, Comparison::GreaterOrEqual => 5,
    });
    out.extend_from_slice(&(scalar.len() as u16).to_le_bytes());
    out.extend_from_slice(&scalar);
    Ok(())
}

impl ReadCaveat {
    /// Version/tag and fixed-width little-endian fields. Lists must already be
    /// strictly sorted; decoding never repairs a noncanonical signed payload.
    pub fn to_bytes(&self) -> Result<Vec<u8>, PolicyError> {
        let mut out = vec![1];
        match self {
            Self::Graphs(items) => {
                out.push(1);
                put_set(&mut out, items, |out, id| out.extend_from_slice(&id.0.to_le_bytes()))?;
            }
            Self::Branches(items) => {
                out.push(2);
                put_set(&mut out, items, |out, id| out.extend_from_slice(&id.0.to_le_bytes()))?;
            }
            Self::Labels(items) => {
                out.push(3);
                put_set(&mut out, items, |out, id| out.extend_from_slice(&id.0.to_le_bytes()))?;
            }
            Self::EdgeTypes(items) => {
                out.push(4);
                put_set(&mut out, items, |out, id| out.extend_from_slice(&id.0.to_le_bytes()))?;
            }
            Self::Properties(items) => {
                out.push(5);
                put_set(&mut out, items, |out, id| out.extend_from_slice(&id.0.to_le_bytes()))?;
            }
            Self::Vertices(items) => {
                out.push(6);
                put_set(&mut out, items, |out, id| out.extend_from_slice(&id.0.to_le_bytes()))?;
            }
            Self::HasLabel(id) => { out.push(7); out.extend_from_slice(&id.0.to_le_bytes()); }
            Self::VertexProperty(predicate) => { out.push(8); put_predicate(&mut out, predicate)?; }
            Self::EdgeProperty(predicate) => { out.push(9); put_predicate(&mut out, predicate)?; }
            Self::TimeWindow { not_before, expires_at } => {
                if not_before >= expires_at { return Err(PolicyError::InvalidWindow); }
                out.push(10);
                out.extend_from_slice(&not_before.to_le_bytes());
                out.extend_from_slice(&expires_at.to_le_bytes());
            }
            Self::Snapshots { first, last } => {
                if first > last { return Err(PolicyError::InvalidWindow); }
                out.push(11);
                out.extend_from_slice(&first.0.to_le_bytes());
                out.extend_from_slice(&last.0.to_le_bytes());
            }
            Self::Limits(limits) => {
                out.push(12);
                out.extend_from_slice(&limits.rows.to_le_bytes());
                out.extend_from_slice(&limits.work.to_le_bytes());
                out.extend_from_slice(&limits.scratch.to_le_bytes());
            }
        }
        if out.len() > MAX_CAVEAT_BYTES { return Err(PolicyError::Limit); }
        Ok(out)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, PolicyError> {
        if bytes.len() > MAX_CAVEAT_BYTES { return Err(PolicyError::Limit); }
        let mut input = Reader(bytes);
        if input.byte()? != 1 { return Err(PolicyError::Unsupported); }
        let result = match input.byte()? {
            1 => Self::Graphs(input.set(|input| Ok(GraphId(input.u128()?)))?),
            2 => Self::Branches(input.set(|input| Ok(BranchId(input.u128()?)))?),
            3 => Self::Labels(input.set(|input| Ok(LabelId(input.u64()?)))?),
            4 => Self::EdgeTypes(input.set(|input| Ok(RelationId(input.u64()?)))?),
            5 => Self::Properties(input.set(|input| Ok(PropertyKeyId(input.u64()?)))?),
            6 => Self::Vertices(input.set(|input| Ok(VId(input.u128()?)))?),
            7 => Self::HasLabel(LabelId(input.u64()?)),
            8 => Self::VertexProperty(input.predicate()?),
            9 => Self::EdgeProperty(input.predicate()?),
            10 => Self::TimeWindow { not_before: input.u128()?, expires_at: input.u128()? },
            11 => Self::Snapshots { first: CommitSeq(input.u64()?), last: CommitSeq(input.u64()?) },
            12 => Self::Limits(ReadLimits { rows: input.u64()?, work: input.u64()?, scratch: input.u64()? }),
            _ => return Err(PolicyError::Unsupported),
        };
        if !input.0.is_empty() || result.to_bytes()?.as_slice() != bytes {
            return Err(PolicyError::NonCanonical);
        }
        Ok(result)
    }
}

struct Reader<'a>(&'a [u8]);
impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], PolicyError> {
        let (head, tail) = self.0.split_at_checked(len).ok_or(PolicyError::NonCanonical)?;
        self.0 = tail;
        Ok(head)
    }
    fn byte(&mut self) -> Result<u8, PolicyError> { Ok(self.take(1)?[0]) }
    fn u16(&mut self) -> Result<u16, PolicyError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().map_err(|_| PolicyError::NonCanonical)?))
    }
    fn u64(&mut self) -> Result<u64, PolicyError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| PolicyError::NonCanonical)?))
    }
    fn u128(&mut self) -> Result<u128, PolicyError> {
        Ok(u128::from_le_bytes(self.take(16)?.try_into().map_err(|_| PolicyError::NonCanonical)?))
    }
    fn set<T: Ord>(&mut self, mut read: impl FnMut(&mut Self) -> Result<T, PolicyError>) -> Result<Vec<T>, PolicyError> {
        let len = usize::from(self.u16()?);
        if len > MAX_SCOPE_ITEMS || len > self.0.len() / 8 { return Err(PolicyError::Limit); }
        let mut items = Vec::with_capacity(len);
        for _ in 0..len { items.push(read(self)?); }
        check_set(&items)?;
        Ok(items)
    }
    fn predicate(&mut self) -> Result<PropertyPredicate, PolicyError> {
        let key = PropertyKeyId(self.u64()?);
        let comparison = match self.byte()? {
            0 => Comparison::Equal, 1 => Comparison::NotEqual,
            2 => Comparison::Less, 3 => Comparison::LessOrEqual,
            4 => Comparison::Greater, 5 => Comparison::GreaterOrEqual,
            _ => return Err(PolicyError::Unsupported),
        };
        let len = usize::from(self.u16()?);
        if len > MAX_POLICY_SCALAR_BYTES { return Err(PolicyError::Limit); }
        let value = CanonicalScalar::decode(self.take(len)?).map_err(|_| PolicyError::InvalidScalar)?;
        Ok(PropertyPredicate { key, comparison, value })
    }
}
